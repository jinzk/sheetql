use std::collections::HashSet;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Visit, Visitor};

use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::functions::FnArgs;
use crate::functions::require_arity_len;
use crate::value::GroupKey;
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

fn eval_count(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let expr = args.first_expr();
    let mut count = 0i64;
    let mut distinct = HashSet::new();
    for &row_index in ctx.group_rows {
        let Some(row) = ctx.all_rows.get(row_index) else {
            continue;
        };
        match expr {
            None => count += 1,
            Some(expr) => {
                let value = eval_expr(ctx, expr, row)?;
                if !value.is_null() {
                    count += 1;
                    if matches!(args, FnArgs::Distinct(_)) {
                        distinct.insert(group_key(&value));
                    }
                }
            }
        }
    }
    if matches!(args, FnArgs::Distinct(_)) {
        count = distinct.len() as i64;
    }
    Ok(Value::Int(count))
}

fn collect_numeric(ctx: &EvalContext, args: &FnArgs) -> Result<(Vec<Value>, bool), Error> {
    let Some(expr) = args.first_expr() else {
        return Err("Aggregate expects one argument".to_string().into());
    };
    let distinct = matches!(args, FnArgs::Distinct(_));
    let mut values = vec![];
    let mut seen: HashSet<GroupKey> = HashSet::new();
    let mut is_float = false;
    for &row_index in ctx.group_rows {
        let Some(row) = ctx.all_rows.get(row_index) else {
            continue;
        };
        let value = eval_expr(ctx, expr, row)?;
        if value.is_null() {
            continue;
        }
        if distinct && !seen.insert(group_key(&value)) {
            continue;
        }
        if let Value::Float(number) = value
            && number.is_nan()
        {
            continue;
        }
        if matches!(value, Value::Float(_)) {
            is_float = true;
        }
        if matches!(value, Value::Int(_) | Value::Float(_)) {
            values.push(value);
        } else {
            return Err(format!("Aggregate expects numeric values, got `{}`", value).into());
        }
    }
    Ok((values, is_float))
}

fn eval_sum(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let (values, is_float) = collect_numeric(ctx, args)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    if is_float {
        let sum: f64 = values.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum();
        Ok(Value::Float(sum))
    } else {
        let sum: i64 = values
            .iter()
            .try_fold(0i64, |acc, v| acc.checked_add(v.as_i64().unwrap_or(0)))
            .ok_or("Integer overflow in SUM")?;
        Ok(Value::Int(sum))
    }
}

fn eval_avg(ctx: &EvalContext, args: &FnArgs) -> Result<Value, Error> {
    let (values, _) = collect_numeric(ctx, args)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let sum: f64 = values.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum();
    Ok(Value::Float(sum / values.len() as f64))
}

fn eval_min_max(ctx: &EvalContext, args: &FnArgs, is_max: bool) -> Result<Value, Error> {
    let Some(expr) = args.first_expr() else {
        return Err("Aggregate expects one argument".to_string().into());
    };
    let mut best: Option<Value> = None;
    for &row_index in ctx.group_rows {
        let Some(row) = ctx.all_rows.get(row_index) else {
            continue;
        };
        let value = eval_expr(ctx, expr, row)?;
        if value.is_null() {
            continue;
        }
        best = match best {
            None => Some(value),
            Some(current) => {
                let ordering = values_partial_cmp(&value, &current);
                match ordering {
                    Some(std::cmp::Ordering::Less) => {
                        if is_max {
                            Some(current)
                        } else {
                            Some(value)
                        }
                    }
                    Some(std::cmp::Ordering::Greater) => {
                        if is_max {
                            Some(value)
                        } else {
                            Some(current)
                        }
                    }
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
