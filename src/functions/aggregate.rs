use std::cmp::Ordering;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Visit, Visitor};

use crate::error::Error;
use crate::evaluator::eval_expr;
use crate::evaluator::{AggregateSummary, EvalContext};
use crate::functions::FnArgs;
use crate::functions::require_arity_len;
use crate::value::Value;
use crate::value::values_partial_cmp;

pub const AGGREGATE_FUNCTIONS: [&str; 5] = ["count", "sum", "avg", "min", "max"];

pub(crate) fn eval(ctx: &EvalContext, name: &str, args: &FnArgs) -> Result<Value, Error> {
    // Every aggregate accepts either `*` (COUNT only) or exactly one argument.
    if !matches!(args, FnArgs::Star) {
        require_arity_len(name, args.len(), 1)?;
    }
    match name {
        "count" => eval_count(ctx, args),
        "sum" => eval_sum(ctx, args),
        "avg" => eval_avg(ctx, args),
        "min" => eval_min_max(ctx, args, false),
        "max" => eval_min_max(ctx, args, true),
        _ => unreachable!("`{name}` is not an aggregate function"),
    }
}

fn cached_argument_values(
    ctx: &EvalContext,
    expr: &Expr,
) -> Result<std::sync::Arc<[Value]>, Error> {
    let key = ctx.expr_id(expr);
    let state = ctx
        .group_state
        .ok_or("Aggregate evaluation requires a group state")?;
    if let Some(values) = state.borrow().argument_values.get(&key) {
        return Ok(std::sync::Arc::clone(values));
    }

    let mut values = Vec::with_capacity(ctx.group_rows.len());
    let planned_expr = ctx.planned_expr(key);
    for &row_index in ctx.group_rows {
        let Some(row) = ctx.all_rows.get(row_index) else {
            continue;
        };
        values.push(eval_expr(ctx, planned_expr, row)?);
    }
    let values: std::sync::Arc<[Value]> = values.into();
    state
        .borrow_mut()
        .argument_values
        .insert(key, std::sync::Arc::clone(&values));
    Ok(values)
}

fn cached_summary(ctx: &EvalContext, expr: &Expr) -> Result<AggregateSummary, Error> {
    let key = ctx.expr_id(expr);
    let state = ctx
        .group_state
        .ok_or("Aggregate evaluation requires a group state")?;
    if let Some(summary) = state.borrow().summaries.get(&key) {
        return Ok(summary.clone());
    }
    let summary = AggregateSummary::from_values(cached_argument_values(ctx, expr)?.iter());
    state.borrow_mut().summaries.insert(key, summary.clone());
    Ok(summary)
}

fn eval_count(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let expr = args.first_expr();
    if let Some(expr) = expr {
        let summary = cached_summary(ctx, expr)?;
        let count = if matches!(args, FnArgs::Distinct(_)) {
            summary.distinct_count
        } else {
            summary.count
        };
        return Ok(Value::Int(count));
    }
    let mut count = 0i64;
    for &row_index in ctx.group_rows {
        if ctx.all_rows.get(row_index).is_some() {
            count += 1;
        }
    }
    Ok(Value::Int(count))
}

fn eval_sum(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let Some(expr) = args.first_expr() else {
        return Err("Aggregate expects one argument".into());
    };
    let summary = cached_summary(ctx, expr)?;
    if let Some(value) = summary.non_numeric {
        return Err(format!("Aggregate expects numeric values, got `{value}`").into());
    }
    let numeric_count = if matches!(args, FnArgs::Distinct(_)) {
        summary.distinct_numeric_count
    } else {
        summary.numeric_count
    };
    if numeric_count == 0 {
        return Ok(Value::Null);
    }
    let distinct = matches!(args, FnArgs::Distinct(_));
    let is_float = if distinct {
        summary.distinct_is_float
    } else {
        summary.is_float
    };
    if !is_float
        && if distinct {
            summary.distinct_sum_int_overflow
        } else {
            summary.sum_int_overflow
        }
    {
        return Err("Integer overflow in SUM".into());
    }
    if is_float {
        Ok(Value::Float(if distinct {
            summary.distinct_sum_float
        } else {
            summary.sum_float
        }))
    } else {
        Ok(Value::Int(if distinct {
            summary.distinct_sum_int.unwrap_or(0)
        } else {
            summary.sum_int.unwrap_or(0)
        }))
    }
}

fn eval_avg(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let Some(expr) = args.first_expr() else {
        return Err("Aggregate expects one argument".into());
    };
    let summary = cached_summary(ctx, expr)?;
    if let Some(value) = summary.non_numeric {
        return Err(format!("Aggregate expects numeric values, got `{value}`").into());
    }
    let numeric_count = if matches!(args, FnArgs::Distinct(_)) {
        summary.distinct_numeric_count
    } else {
        summary.numeric_count
    };
    if numeric_count == 0 {
        return Ok(Value::Null);
    }
    let sum = if matches!(args, FnArgs::Distinct(_)) {
        summary.distinct_sum_float
    } else {
        summary.sum_float
    };
    Ok(Value::Float(sum / numeric_count as f64))
}

fn eval_min_max(ctx: &EvalContext, args: &FnArgs, is_max: bool) -> Result<Value, Error> {
    let Some(expr) = args.first_expr() else {
        return Err("Aggregate expects one argument".to_string().into());
    };
    // Keep MIN/MAX on the value cache: unlike numeric aggregates, their
    // ordering semantics include mixed temporal/text values and NaN handling.
    Ok(min_max(&cached_argument_values(ctx, expr)?, is_max))
}

/// Compute MIN/MAX over a set of values, preserving the ordering semantics
/// (mixed temporal/text values and NaN handling) used by both grouped and
/// window aggregates.
pub(crate) fn min_max(values: &[Value], is_max: bool) -> Value {
    let mut best: Option<Value> = None;
    for value in values.iter().filter(|value| !value.is_null()) {
        best = match best {
            None => Some(value.clone()),
            Some(current) => {
                let ordering = values_partial_cmp(value, &current);
                match ordering {
                    Some(Ordering::Less) if !is_max => Some(value.clone()),
                    Some(Ordering::Greater) if is_max => Some(value.clone()),
                    _ => Some(current),
                }
            }
        };
    }
    best.unwrap_or(Value::Null)
}

/// Detect whether `expr` (transitively) contains an aggregate function call.
/// Uses the sqlparser visitor so every expression kind is covered.
pub fn contains_aggregate(expr: &Expr) -> bool {
    struct AggregateCallDetector {
        query_depth: usize,
    }

    impl Visitor for AggregateCallDetector {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth += 1;
            ControlFlow::Continue(())
        }

        fn post_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth -= 1;
            ControlFlow::Continue(())
        }

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.query_depth > 0 {
                return ControlFlow::Continue(());
            }
            if let Expr::Function(func) = expr {
                // Window functions such as `SUM(x) OVER (...)` are not grouping
                // aggregates; they are computed over the active rows instead.
                if func.over.is_some() {
                    return ControlFlow::Continue(());
                }
                let mut name = func.name.to_string();
                name.make_ascii_lowercase();
                if AGGREGATE_FUNCTIONS.contains(&name.as_str()) {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        }
    }

    expr.visit(&mut AggregateCallDetector { query_depth: 0 })
        .is_break()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_max_selects_extremes() {
        let items = vec![Value::Int(3), Value::Int(1), Value::Int(2)];
        assert_eq!(min_max(&items, false), Value::Int(1));
        assert_eq!(min_max(&items, true), Value::Int(3));
    }

    #[test]
    fn min_max_ignores_null_values() {
        let items = vec![Value::Null, Value::Int(3), Value::Null, Value::Int(1)];
        assert_eq!(min_max(&items, false), Value::Int(1));
        assert_eq!(min_max(&items, true), Value::Int(3));
    }

    #[test]
    fn min_max_empty_or_all_null_returns_null() {
        assert_eq!(min_max(&[], false), Value::Null);
        assert_eq!(min_max(&[], true), Value::Null);
        let all_null = vec![Value::Null, Value::Null];
        assert_eq!(min_max(&all_null, false), Value::Null);
        assert_eq!(min_max(&all_null, true), Value::Null);
    }

    #[test]
    fn min_max_orders_text() {
        let items = vec![
            Value::Text("b".into()),
            Value::Text("a".into()),
            Value::Text("c".into()),
        ];
        assert_eq!(min_max(&items, false), Value::Text("a".into()));
        assert_eq!(min_max(&items, true), Value::Text("c".into()));
    }

    #[test]
    fn min_max_compares_across_int_and_float() {
        let items = vec![Value::Int(1), Value::Float(2.5), Value::Int(0)];
        assert_eq!(min_max(&items, false), Value::Int(0));
        assert_eq!(min_max(&items, true), Value::Float(2.5));
    }

    #[test]
    fn min_max_keeps_first_value_on_ties() {
        let items = vec![Value::Float(1.0), Value::Int(1)];
        assert_eq!(min_max(&items, false), Value::Float(1.0));
        assert_eq!(min_max(&items, true), Value::Float(1.0));
    }

    #[test]
    fn min_max_skips_nan_unless_it_is_already_best() {
        // NaN yields no ordering in partial_cmp, so a later NaN cannot replace
        // an existing best, while a NaN seen first stays.
        let items = vec![Value::Float(3.0), Value::Float(f64::NAN), Value::Float(1.0)];
        assert_eq!(min_max(&items, false), Value::Float(1.0));
        assert_eq!(min_max(&items, true), Value::Float(3.0));
        let leading_nan = vec![Value::Float(f64::NAN), Value::Float(1.0)];
        assert!(matches!(
            min_max(&leading_nan, false),
            Value::Float(n) if n.is_nan()
        ));
    }
}
