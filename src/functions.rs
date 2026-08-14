use std::collections::HashSet;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::evaluator::eval_expr;
use crate::evaluator::EvalContext;
use crate::value::values_partial_cmp;
use crate::value::Value;

pub const AGGREGATE_FUNCTIONS: [&str; 5] = ["count", "sum", "avg", "min", "max"];

pub enum FnArgs {
    Star,
    All(Vec<Expr>),
    Distinct(Vec<Expr>),
}

pub fn parse_function_args(args: &FunctionArguments) -> Result<FnArgs, String> {
    match args {
        FunctionArguments::None => Ok(FnArgs::All(vec![])),
        FunctionArguments::List(list) => {
            let mut exprs = vec![];
            for arg in &list.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                        exprs.push(expr.clone());
                    }
                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => return Ok(FnArgs::Star),
                    FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(expr),
                        ..
                    } => exprs.push(expr.clone()),
                    _ => return Err("Unsupported function argument".to_string()),
                }
            }
            match list.duplicate_treatment {
                Some(sqlparser::ast::DuplicateTreatment::Distinct) => {
                    Ok(FnArgs::Distinct(exprs))
                }
                _ => Ok(FnArgs::All(exprs)),
            }
        }
        FunctionArguments::Subquery(_) => {
            Err("Subquery function arguments are not supported".to_string())
        }
    }
}

pub fn eval_function(
    ctx: &EvalContext,
    func: &Function,
    current: &[Value],
) -> Result<Value, String> {
    let name = func.name.to_string().to_lowercase();
    if func.over.is_some() {
        return Err(format!("Window function `{name}` is not supported yet"));
    }
    let args = parse_function_args(&func.args)?;

    match name.as_str() {
        "count" => eval_count(ctx, &args),
        "sum" => eval_sum(ctx, &args),
        "avg" => eval_avg(ctx, &args),
        "min" => eval_min_max(ctx, &args, false),
        "max" => eval_min_max(ctx, &args, true),
        "ifnull" | "isnull" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.len() != 2 {
                return Err(format!("Function `{name}` expects 2 arguments"));
            }
            if values[0].is_null() {
                Ok(values[1].clone())
            } else {
                Ok(values[0].clone())
            }
        }
        "coalesce" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            for value in values {
                if !value.is_null() {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        "len" | "length" | "char_length" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Int(values[0].to_display_string().chars().count() as i64))
        }
        "lower" | "lcase" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(values[0].to_display_string().to_lowercase()))
        }
        "upper" | "ucase" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(values[0].to_display_string().to_uppercase()))
        }
        "trim" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(values[0].to_display_string().trim().to_string()))
        }
        "ltrim" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(
                values[0].to_display_string().trim_start().to_string(),
            ))
        }
        "rtrim" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(
                values[0].to_display_string().trim_end().to_string(),
            ))
        }
        "concat" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            Ok(Value::Text(
                values
                    .iter()
                    .map(Value::to_display_string)
                    .collect::<Vec<_>>()
                    .join(""),
            ))
        }
        "substring" | "substr" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.len() != 2 && values.len() != 3 {
                return Err(format!("Function `{name}` expects 2 or 3 arguments"));
            }
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let start = values[1].as_i64().ok_or("SUBSTRING start must be a number")?;
            let start_index = if start >= 0 { (start - 1) as usize } else { 0 };
            let end_index = if values.len() == 3 {
                let length = values[2].as_i64().ok_or("SUBSTRING length must be a number")?;
                start_index.saturating_add(length.max(0) as usize)
            } else {
                text.len()
            };
            let result: String = text
                .get(start_index..end_index.min(text.len()))
                .unwrap_or(&[])
                .iter()
                .collect();
            Ok(Value::Text(result))
        }
        "replace" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 3)?;
            Ok(Value::Text(values[0].to_display_string().replace(
                &values[1].to_display_string(),
                &values[2].to_display_string(),
            )))
        }
        "abs" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            if let Some(parsed) = values[0].as_i64() {
                Ok(Value::Int(parsed.abs()))
            } else if let Some(parsed) = values[0].as_f64() {
                Ok(Value::Float(parsed.abs()))
            } else {
                Err(format!("ABS expects a numeric value, got `{}`", values[0]))
            }
        }
        "round" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.len() != 1 && values.len() != 2 {
                return Err(format!("Function `{name}` expects 1 or 2 arguments"));
            }
            let number = values[0].as_f64().ok_or("ROUND expects a numeric value")?;
            let decimals = if values.len() == 2 {
                values[1].as_i64().unwrap_or(0) as i32
            } else {
                0
            };
            let factor = 10f64.powi(decimals);
            Ok(Value::Float((number * factor).round() / factor))
        }
        "floor" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Float(values[0].as_f64().ok_or("FLOOR expects a numeric value")?.floor()))
        }
        "ceil" | "ceiling" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Float(values[0].as_f64().ok_or("CEIL expects a numeric value")?.ceil()))
        }
        "mod" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            Ok(Value::Int(
                values[0].as_i64().ok_or("MOD expects numeric values")?
                    % values[1].as_i64().ok_or("MOD expects numeric values")?,
            ))
        }
        "left" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let count = values[1].as_i64().ok_or("LEFT length must be a number")?.max(0) as usize;
            Ok(Value::Text(text.iter().take(count).collect()))
        }
        "right" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let count = values[1].as_i64().ok_or("RIGHT length must be a number")?.max(0) as usize;
            let start = text.len().saturating_sub(count);
            Ok(Value::Text(text.iter().skip(start).collect()))
        }
        "instr" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let haystack = values[0].to_display_string();
            let needle = values[1].to_display_string();
            let position = haystack
                .find(&needle)
                .map(|index| haystack[..index].chars().count() as i64 + 1)
                .unwrap_or(0);
            Ok(Value::Int(position))
        }
        "now" | "current_timestamp" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 0)?;
            Ok(Value::Text(now_string()))
        }
        "date" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.is_empty() {
                return Ok(Value::Text(today_string()));
            }
            require_arity(&name, &values, 1)?;
            Ok(Value::Text(extract_date(&values[0].to_display_string())))
        }
        "power" | "pow" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let base = values[0].as_f64().ok_or("POWER base must be a number")?;
            let exponent = values[1].as_f64().ok_or("POWER exponent must be a number")?;
            Ok(Value::Float(base.powf(exponent)))
        }
        "sqrt" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            let number = values[0].as_f64().ok_or("SQRT expects a numeric value")?;
            if number < 0.0 {
                return Err("SQRT expects a non-negative value".to_string());
            }
            Ok(Value::Float(number.sqrt()))
        }
        "greatest" | "least" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.is_empty() {
                return Err(format!("Function `{name}` expects at least 1 argument"));
            }
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let mut best = values[0].clone();
            for value in &values[1..] {
                let ordering = values_partial_cmp(&best, value)
                    .ok_or_else(|| format!("Cannot compare values for `{name}`"))?;
                if (name == "greatest" && ordering == std::cmp::Ordering::Less)
                    || (name == "least" && ordering == std::cmp::Ordering::Greater)
                {
                    best = value.clone();
                }
            }
            Ok(best)
        }
        _ => Err(format!("Unknown function `{name}`")),
    }
}

fn require_arity(name: &str, values: &[Value], expected: usize) -> Result<(), String> {
    if values.len() != expected {
        return Err(format!(
            "Function `{name}` expects {expected} argument(s), got {}",
            values.len()
        ));
    }
    Ok(())
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today_string() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn extract_date(input: &str) -> String {
    let trimmed = input.trim();
    let bytes = trimmed.as_bytes();
    let is_digit = |byte: u8| byte.is_ascii_digit();
    let valid = bytes.len() >= 10
        && is_digit(bytes[0])
        && is_digit(bytes[1])
        && is_digit(bytes[2])
        && is_digit(bytes[3])
        && matches!(bytes[4], b'-' | b'/' | b'.')
        && is_digit(bytes[5])
        && is_digit(bytes[6])
        && matches!(bytes[7], b'-' | b'/' | b'.')
        && is_digit(bytes[8])
        && is_digit(bytes[9]);
    if !valid {
        return trimmed.to_string();
    }
    let month = trimmed[5..7].parse::<u32>().unwrap_or(0);
    let day = trimmed[8..10].parse::<u32>().unwrap_or(0);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return trimmed.to_string();
    }
    format!("{}-{}-{}", &trimmed[0..4], &trimmed[5..7], &trimmed[8..10])
}

fn eval_scalar_args(ctx: &EvalContext, args: &FnArgs, current: &[Value]) -> Result<Vec<Value>, String> {
    let exprs = match args {
        FnArgs::Star => return Err("Wildcard is not allowed here".to_string()),
        FnArgs::All(exprs) | FnArgs::Distinct(exprs) => exprs,
    };
    let mut values = vec![];
    for expr in exprs {
        values.push(eval_expr(ctx, expr, current)?);
    }
    Ok(values)
}

fn eval_count(ctx: &EvalContext, args: &FnArgs) -> Result<Value, String> {
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
                            distinct.insert(format!("{:?}", value));
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

fn collect_numeric(
    ctx: &EvalContext,
    args: &FnArgs,
) -> Result<(Vec<Value>, bool), String> {
    let mut values = vec![];
    let mut is_float = false;
    for &row_index in ctx.group_rows {
        let row = ctx.all_rows.get(row_index);
        if let (Some(expr), Some(row)) = (args.first_expr(), row) {
            let value = eval_expr(ctx, expr, row)?;
            if value.is_null() {
                continue;
            }
            if matches!(value, Value::Float(_)) {
                is_float = true;
            }
            if matches!(value, Value::Int(_) | Value::Float(_)) {
                values.push(value);
            } else {
                return Err(format!("Aggregate expects numeric values, got `{}`", value));
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

fn eval_sum(ctx: &EvalContext, args: &FnArgs) -> Result<Value, String> {
    let (values, is_float) = collect_numeric(ctx, args)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    if is_float {
        let sum: f64 = values
            .iter()
            .map(|v| v.as_f64().unwrap_or(0.0))
            .sum();
        Ok(Value::Float(sum))
    } else {
        let sum: i64 = values.iter().map(|v| v.as_i64().unwrap_or(0)).sum();
        Ok(Value::Int(sum))
    }
}

fn eval_avg(ctx: &EvalContext, args: &FnArgs) -> Result<Value, String> {
    let (values, _) = collect_numeric(ctx, args)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let sum: f64 = values
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0))
        .sum();
    Ok(Value::Float(sum / values.len() as f64))
}

fn eval_min_max(ctx: &EvalContext, args: &FnArgs, is_max: bool) -> Result<Value, String> {
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

pub fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(func) => {
            let name = func.name.to_string().to_lowercase();
            if AGGREGATE_FUNCTIONS.contains(&name.as_str()) {
                return true;
            }
            function_args_contain_aggregate(&func.args)
        }
        Expr::BinaryOp { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
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
                || trim_what.as_ref().map(|e| contains_aggregate(e)).unwrap_or(false)
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
            conditions
                .iter()
                .any(|case_when| contains_aggregate(&case_when.condition) || contains_aggregate(&case_when.result))
                || else_result
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
        } => {
            contains_aggregate(expr)
                || contains_aggregate(low)
                || contains_aggregate(high)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> Expr {
        Parser::new(&MySqlDialect {})
            .try_with_sql(sql)
            .expect("parse sql")
            .parse_expr()
            .expect("parse expr")
    }

    fn scalar(expr: &str) -> Value {
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[])
            .expect("eval scalar")
    }

    #[test]
    fn scalar_functions() {
        assert_eq!(scalar("LEN('SheetQL')"), Value::Int(7));
        assert_eq!(scalar("LOWER('ABC')"), Value::Text("abc".to_string()));
        assert_eq!(scalar("UPPER('abc')"), Value::Text("ABC".to_string()));
        assert_eq!(scalar("TRIM('  x  ')"), Value::Text("x".to_string()));
        assert_eq!(scalar("CONCAT('a', 'b', 'c')"), Value::Text("abc".to_string()));
        assert_eq!(scalar("SUBSTRING('SheetQL', 1, 4)"), Value::Text("Shee".to_string()));
        assert_eq!(scalar("REPLACE('a-b-a', 'a', 'x')"), Value::Text("x-b-x".to_string()));
        assert_eq!(scalar("ABS(-5)"), Value::Int(5));
        assert_eq!(scalar("ROUND(3.146, 2)"), Value::Float(3.15));
        assert_eq!(scalar("MOD(10, 3)"), Value::Int(1));
        assert_eq!(scalar("IFNULL(NULL, 'fallback')"), Value::Text("fallback".to_string()));
        assert_eq!(scalar("IFNULL(5, 'fallback')"), Value::Int(5));
        assert_eq!(scalar("COALESCE(NULL, NULL, 7)"), Value::Int(7));
    }

    #[test]
    fn contains_aggregate_detection() {
        assert!(contains_aggregate(&parse_expr("COUNT(*) > 1")));
        assert!(contains_aggregate(&parse_expr("SUM(amount) + 1")));
        assert!(contains_aggregate(&parse_expr("CASE WHEN MAX(x) > 0 THEN 1 ELSE 0 END")));
        assert!(!contains_aggregate(&parse_expr("LOWER(name)")));
        assert!(!contains_aggregate(&parse_expr("age + 1")));
    }
}