use std::cmp::Ordering;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Visit, Visitor};

use crate::error::Error;
use crate::evaluator::eval_expr;
use crate::evaluator::{AggregateSummary, EvalContext};
use crate::functions::FnArgs;
use crate::functions::require_arity_len;
use crate::value::Value;
use crate::value::group_key;
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
    let mut summary = AggregateSummary::default();
    for value in cached_argument_values(ctx, expr)?.iter() {
        if value.is_null() {
            continue;
        }
        summary.count += 1;
        let is_new_distinct = summary.distinct.insert(group_key(value));
        let is_nan = matches!(value, Value::Float(number) if number.is_nan());
        if let Value::Float(_) = value
            && !is_nan
        {
            summary.is_float = true;
        }
        if matches!(value, Value::Int(_) | Value::Float(_)) && !is_nan {
            summary.numeric_count += 1;
            summary.sum_float += value.as_f64().unwrap_or(0.0);
            if let Value::Int(number) = value {
                summary.sum_int = Some(match summary.sum_int.unwrap_or(0).checked_add(*number) {
                    Some(sum) => sum,
                    None => {
                        summary.sum_int_overflow = true;
                        0
                    }
                });
            }
            if is_new_distinct {
                summary.distinct_numeric_count += 1;
                if let Value::Float(_) = value {
                    summary.distinct_is_float = true;
                }
                summary.distinct_sum_float += value.as_f64().unwrap_or(0.0);
                if let Value::Int(number) = value {
                    summary.distinct_sum_int = Some(
                        match summary.distinct_sum_int.unwrap_or(0).checked_add(*number) {
                            Some(sum) => sum,
                            None => {
                                summary.distinct_sum_int_overflow = true;
                                0
                            }
                        },
                    );
                }
            }
        } else if !is_nan && summary.non_numeric.is_none() {
            summary.non_numeric = Some(value.clone());
        }
    }
    summary.distinct_count = summary.distinct.len() as i64;
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
    let mut best: Option<Value> = None;
    for value in cached_argument_values(ctx, expr)?.iter() {
        if value.is_null() {
            continue;
        }
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
    Ok(best.unwrap_or(Value::Null))
}

/// Detect whether `expr` (transitively) contains an aggregate function call.
/// Uses the sqlparser visitor so every expression kind is covered.
pub fn contains_aggregate(expr: &Expr) -> bool {
    struct AggregateCallDetector;

    impl Visitor for AggregateCallDetector {
        type Break = ();

        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if let Expr::Function(func) = expr {
                let mut name = func.name.to_string();
                name.make_ascii_lowercase();
                if AGGREGATE_FUNCTIONS.contains(&name.as_str()) {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        }
    }

    expr.visit(&mut AggregateCallDetector).is_break()
}
