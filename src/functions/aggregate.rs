use std::collections::HashSet;

use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::functions::FnArgs;
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;
use crate::value::values_partial_cmp;

pub const AGGREGATE_FUNCTIONS: [&str; 5] = ["count", "sum", "avg", "min", "max"];

pub(crate) fn eval(ctx: &EvalContext, name: &str, args: &FnArgs) -> Result<Value, Error> {
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
    let mut count = 0i64;
    let mut distinct = HashSet::new();
    for &row_index in ctx.group_rows {
        let row = ctx.all_rows.get(row_index);
        match args {
            FnArgs::Star => count += 1,
            FnArgs::All(exprs) | FnArgs::Distinct(exprs) => {
                if let (Some(expr), Some(row)) = (exprs.first(), row) {
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
    }
    if matches!(args, FnArgs::Distinct(_)) {
        count = distinct.len() as i64;
    }
    Ok(Value::Int(count))
}

fn collect_numeric(ctx: &EvalContext, args: &FnArgs) -> Result<(Vec<Value>, bool), Error> {
    let distinct = matches!(args, FnArgs::Distinct(_));
    let mut values = vec![];
    let mut seen: HashSet<GroupKey> = HashSet::new();
    let mut is_float = false;
    for &row_index in ctx.group_rows {
        let row = ctx.all_rows.get(row_index);
        if let (Some(expr), Some(row)) = (args.first_expr(), row) {
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
    }
    Ok((values, is_float))
}

impl FnArgs {
    fn first_expr(&self) -> Option<&Expr> {
        match self {
            FnArgs::Star => None,
            FnArgs::All(exprs) | FnArgs::Distinct(exprs) => exprs.first(),
        }
    }
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
    let mut best: Option<Value> = None;
    for &row_index in ctx.group_rows {
        let row = ctx.all_rows.get(row_index);
        if let (Some(expr), Some(row)) = (args.first_expr(), row) {
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
    }
    Ok(best.unwrap_or(Value::Null))
}

/// Detect whether `expr` (transitively) contains an aggregate function call.
pub fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(func) => {
            let mut name = func.name.to_string();
            name.make_ascii_lowercase();
            if AGGREGATE_FUNCTIONS.contains(&name.as_str()) {
                return true;
            }
            function_args_contain_aggregate(&func.args)
        }
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { expr, .. } => contains_aggregate(expr),
        Expr::Nested(expr) => contains_aggregate(expr),
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            contains_aggregate(expr)
                || substring_from
                    .as_ref()
                    .map(|e| contains_aggregate(e))
                    .unwrap_or(false)
                || substring_for
                    .as_ref()
                    .map(|e| contains_aggregate(e))
                    .unwrap_or(false)
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            contains_aggregate(expr)
                || trim_what
                    .as_ref()
                    .map(|e| contains_aggregate(e))
                    .unwrap_or(false)
                || trim_characters
                    .as_ref()
                    .map(|chars| chars.iter().any(contains_aggregate))
                    .unwrap_or(false)
        }
        Expr::Case {
            conditions,
            else_result,
            ..
        } => {
            conditions.iter().any(|case_when| {
                contains_aggregate(&case_when.condition) || contains_aggregate(&case_when.result)
            }) || else_result
                .as_ref()
                .map(|e| contains_aggregate(e))
                .unwrap_or(false)
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            contains_aggregate(expr) || contains_aggregate(pattern)
        }
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsNotFalse(expr) => contains_aggregate(expr),
        _ => false,
    }
}

fn function_args_contain_aggregate(args: &FunctionArguments) -> bool {
    match args {
        FunctionArguments::List(list) => list.args.iter().any(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => contains_aggregate(expr),
            FunctionArg::Named {
                arg: FunctionArgExpr::Expr(expr),
                ..
            } => contains_aggregate(expr),
            _ => false,
        }),
        _ => false,
    }
}
