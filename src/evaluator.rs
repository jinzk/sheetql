use std::collections::HashMap;
use std::collections::HashSet;

use sqlparser::ast::{
    BinaryOperator, CaseWhen, DataType as SqlDataType, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, TrimWhereField, UnaryOperator, Value as SqlValue, ValueWithSpan,
};

use crate::value::values_eq;
use crate::value::values_partial_cmp;
use crate::value::Value;

pub const AGGREGATE_FUNCTIONS: [&str; 5] = ["count", "sum", "avg", "min", "max"];

pub struct EvalContext<'a> {
    pub columns: &'a HashMap<String, usize>,
    pub all_rows: &'a [Vec<Value>],
    pub group_rows: &'a [usize],
}

static EMPTY_COLUMNS: std::sync::OnceLock<HashMap<String, usize>> = std::sync::OnceLock::new();
static EMPTY_ROWS: [Vec<Value>; 0] = [];
static EMPTY_GROUP: [usize; 0] = [];

impl<'a> EvalContext<'a> {
    pub fn new(
        columns: &'a HashMap<String, usize>,
        all_rows: &'a [Vec<Value>],
        group_rows: &'a [usize],
    ) -> Self {
        Self {
            columns,
            all_rows,
            group_rows,
        }
    }

    pub fn scalar() -> Self {
        Self {
            columns: EMPTY_COLUMNS.get_or_init(HashMap::new),
            all_rows: &EMPTY_ROWS,
            group_rows: &EMPTY_GROUP,
        }
    }
}

pub fn eval_expr(
    ctx: &EvalContext,
    expr: &Expr,
    current: &[Value],
) -> Result<Value, String> {
    match expr {
        Expr::Value(ValueWithSpan { value, .. }) => eval_sql_value(value),
        Expr::Identifier(ident) => {
            let name = ident.value.to_lowercase();
            resolve_column(ctx, &name, current)
        }
        Expr::CompoundIdentifier(parts) => {
            let mut name_parts: Vec<String> =
                parts.iter().map(|p| p.value.to_lowercase()).collect();
            if name_parts.len() < 2 {
                return Err("Invalid compound identifier".to_string());
            }
            let column = name_parts.pop().unwrap();
            let qualifier = name_parts.join(".");
            resolve_column(ctx, &format!("{}.{}", qualifier, column), current)
        }
        Expr::Nested(inner) => eval_expr(ctx, inner, current),
        Expr::BinaryOp { left, op, right } => {
            let lhs = eval_expr(ctx, left, current)?;
            let rhs = eval_expr(ctx, right, current)?;
            eval_binary(op, lhs, rhs)
        }
        Expr::UnaryOp { op, expr } => {
            let value = eval_expr(ctx, expr, current)?;
            eval_unary(op, value)
        }
        Expr::IsNull(inner) => Ok(Value::Bool(eval_expr(ctx, inner, current)?.is_null())),
        Expr::IsNotNull(inner) => Ok(Value::Bool(!eval_expr(ctx, inner, current)?.is_null())),
        Expr::IsTrue(inner) => Ok(Value::Bool(eval_expr(ctx, inner, current)?.truthy())),
        Expr::IsFalse(inner) => Ok(Value::Bool(!eval_expr(ctx, inner, current)?.truthy())),
        Expr::IsNotTrue(inner) => Ok(Value::Bool(!eval_expr(ctx, inner, current)?.truthy())),
        Expr::IsNotFalse(inner) => Ok(Value::Bool(eval_expr(ctx, inner, current)?.truthy())),
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in_list(ctx, expr, list, *negated, current),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => eval_between(ctx, expr, low, high, *negated, current),
        Expr::Like {
            negated,
            expr,
            pattern,
            ..
        } => {
            let value = eval_expr(ctx, expr, current)?;
            let pattern = eval_expr(ctx, pattern, current)?;
            let matched = like_values(&value, &pattern, false)?;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            ..
        } => {
            let value = eval_expr(ctx, expr, current)?;
            let pattern = eval_expr(ctx, pattern, current)?;
            let matched = like_values(&value, &pattern, true)?;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => eval_case(ctx, operand, conditions, else_result, current),
        Expr::Cast { expr, data_type, .. } => {
            let value = eval_expr(ctx, expr, current)?;
            cast_value(value, data_type)
        }
        Expr::Function(func) => eval_function(ctx, func, current),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let text = eval_expr(ctx, expr, current)?.to_display_string();
            let chars: Vec<char> = text.chars().collect();
            let from = match substring_from {
                Some(from) => eval_expr(ctx, from, current)?
                    .as_i64()
                    .ok_or("SUBSTRING start must be a number")?,
                None => 1,
            };
            let start_index = if from >= 0 { (from - 1) as usize } else { 0 };
            let end_index = match substring_for {
                Some(length) => {
                    let length = eval_expr(ctx, length, current)?
                        .as_i64()
                        .ok_or("SUBSTRING length must be a number")?;
                    start_index.saturating_add(length.max(0) as usize)
                }
                None => chars.len(),
            };
            let result: String = chars
                .get(start_index..end_index.min(chars.len()))
                .unwrap_or(&[])
                .iter()
                .collect();
            Ok(Value::Text(result))
        }
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => {
            let value = eval_expr(ctx, expr, current)?.to_display_string();
            let side = trim_where.unwrap_or(TrimWhereField::Both);
            let what = match (trim_what, trim_characters) {
                (Some(what), _) => Some(eval_expr(ctx, what, current)?.to_display_string()),
                (None, Some(characters)) => {
                    let mut joined = String::new();
                    for character in characters {
                        joined.push_str(&eval_expr(ctx, character, current)?.to_display_string());
                    }
                    Some(joined)
                }
                (None, None) => None,
            };
            Ok(Value::Text(trim_string(&value, side, what.as_deref())))
        }
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            Err("Subqueries are not supported".to_string())
        }
        other => Err(format!("Unsupported expression: {}", expr_display(other))),
    }
}

fn expr_display(expr: &Expr) -> String {
    expr.to_string()
}

fn eval_sql_value(value: &SqlValue) -> Result<Value, String> {
    match value {
        SqlValue::Number(number, _) => {
            if let Ok(parsed) = number.parse::<i64>() {
                Ok(Value::Int(parsed))
            } else if let Ok(parsed) = number.parse::<f64>() {
                Ok(Value::Float(parsed))
            } else {
                Err(format!("Invalid number literal `{number}`"))
            }
        }
        SqlValue::SingleQuotedString(s) => Ok(Value::Text(s.clone())),
        SqlValue::DoubleQuotedString(s) => Ok(Value::Text(s.clone())),
        SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
        SqlValue::Null => Ok(Value::Null),
        other => Err(format!("Unsupported literal: {other}")),
    }
}

fn resolve_column(
    ctx: &EvalContext,
    name: &str,
    current: &[Value],
) -> Result<Value, String> {
    match ctx.columns.get(name) {
        Some(index) => Ok(current
            .get(*index)
            .cloned()
            .unwrap_or(Value::Null)),
        None => Err(format!("Column `{name}` not found")),
    }
}

fn eval_binary(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, String> {
    match op {
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => eval_arithmetic(op, lhs, rhs),
        BinaryOperator::StringConcat => {
            if lhs.is_null() || rhs.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Text(format!(
                "{}{}",
                lhs.to_display_string(),
                rhs.to_display_string()
            )))
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => eval_compare(op, lhs, rhs),
        BinaryOperator::And | BinaryOperator::Or => eval_logic(op, lhs, rhs),
        other => Err(format!("Unsupported operator: {other}")),
    }
}

fn eval_arithmetic(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, String> {
    if lhs.is_null() || rhs.is_null() {
        return Ok(Value::Null);
    }

    match op {
        BinaryOperator::Divide => {
            let b = rhs.as_f64().ok_or_else(|| format!("Cannot divide by `{}`", rhs))?;
            if b == 0.0 {
                return Err("Division by zero".to_string());
            }
            let a = lhs.as_f64().ok_or_else(|| format!("Cannot divide `{}`", lhs))?;
            Ok(Value::Float(a / b))
        }
        BinaryOperator::Modulo => {
            let b = rhs
                .as_i64()
                .ok_or_else(|| format!("Cannot modulo by `{}`", rhs))?;
            if b == 0 {
                return Err("Modulo by zero".to_string());
            }
            let a = lhs.as_i64().ok_or_else(|| format!("Cannot modulo `{}`", lhs))?;
            Ok(Value::Int(a % b))
        }
        _ => {
            if matches!(lhs, Value::Int(_)) && matches!(rhs, Value::Int(_)) {
                let a = lhs.as_i64().expect("int value");
                let b = rhs.as_i64().expect("int value");
                let value = match op {
                    BinaryOperator::Plus => a.wrapping_add(b),
                    BinaryOperator::Minus => a.wrapping_sub(b),
                    BinaryOperator::Multiply => a.wrapping_mul(b),
                    _ => unreachable!(),
                };
                Ok(Value::Int(value))
            } else {
                let a = lhs
                    .as_f64()
                    .ok_or_else(|| format!("Cannot apply arithmetic to `{}`", lhs))?;
                let b = rhs
                    .as_f64()
                    .ok_or_else(|| format!("Cannot apply arithmetic to `{}`", rhs))?;
                let value = match op {
                    BinaryOperator::Plus => a + b,
                    BinaryOperator::Minus => a - b,
                    BinaryOperator::Multiply => a * b,
                    _ => unreachable!(),
                };
                Ok(Value::Float(value))
            }
        }
    }
}

fn eval_compare(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, String> {
    if lhs.is_null() || rhs.is_null() {
        return Ok(Value::Null);
    }

    match op {
        BinaryOperator::Eq => Ok(Value::Bool(values_eq(&lhs, &rhs))),
        BinaryOperator::NotEq => Ok(Value::Bool(!values_eq(&lhs, &rhs))),
        BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq => {
            let ordering = values_partial_cmp(&lhs, &rhs)
                .ok_or_else(|| format!("Cannot compare `{}` and `{}`", lhs, rhs))?;
            use std::cmp::Ordering;
            let result = match op {
                BinaryOperator::Lt => ordering == Ordering::Less,
                BinaryOperator::LtEq => ordering != Ordering::Greater,
                BinaryOperator::Gt => ordering == Ordering::Greater,
                BinaryOperator::GtEq => ordering != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        _ => unreachable!(),
    }
}

fn eval_logic(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, String> {
    let a = tri_bool(&lhs);
    let b = tri_bool(&rhs);

    match op {
        BinaryOperator::And => Ok(match (a, b) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        }),
        BinaryOperator::Or => Ok(match (a, b) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        }),
        _ => unreachable!(),
    }
}

fn tri_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        other => Some(other.truthy()),
    }
}

fn eval_unary(op: &UnaryOperator, value: Value) -> Result<Value, String> {
    match op {
        UnaryOperator::Plus => Ok(value),
        UnaryOperator::Minus => {
            if value.is_null() {
                return Ok(Value::Null);
            }
            if let Some(parsed) = value.as_i64() {
                Ok(Value::Int(-parsed))
            } else if let Some(parsed) = value.as_f64() {
                Ok(Value::Float(-parsed))
            } else {
                Err(format!("Cannot negate `{value}`"))
            }
        }
        UnaryOperator::Not => {
            if value.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Bool(!value.truthy()))
        }
        other => Err(format!("Unsupported unary operator: {other}")),
    }
}

fn eval_in_list(
    ctx: &EvalContext,
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    current: &[Value],
) -> Result<Value, String> {
    let value = eval_expr(ctx, expr, current)?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut found = false;
    for item in list {
        let item_value = eval_expr(ctx, item, current)?;
        if values_eq(&value, &item_value) {
            found = true;
            break;
        }
    }
    Ok(Value::Bool(if negated { !found } else { found }))
}

fn eval_between(
    ctx: &EvalContext,
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    current: &[Value],
) -> Result<Value, String> {
    let value = eval_expr(ctx, expr, current)?;
    let low = eval_expr(ctx, low, current)?;
    let high = eval_expr(ctx, high, current)?;

    let result = match (
        values_partial_cmp(&value, &low),
        values_partial_cmp(&value, &high),
    ) {
        (Some(a), Some(b)) => {
            a != std::cmp::Ordering::Less && b != std::cmp::Ordering::Greater
        }
        _ => false,
    };

    Ok(Value::Bool(if negated { !result } else { result }))
}

fn eval_case(
    ctx: &EvalContext,
    operand: &Option<Box<Expr>>,
    conditions: &[CaseWhen],
    else_result: &Option<Box<Expr>>,
    current: &[Value],
) -> Result<Value, String> {
    let operand_value = match operand {
        Some(operand) => Some(eval_expr(ctx, operand, current)?),
        None => None,
    };

    for case_when in conditions {
        let matched = match &operand_value {
            Some(expected) => {
                let actual = eval_expr(ctx, &case_when.condition, current)?;
                values_eq(expected, &actual)
            }
            None => eval_expr(ctx, &case_when.condition, current)?.truthy(),
        };
        if matched {
            return eval_expr(ctx, &case_when.result, current);
        }
    }

    match else_result {
        Some(result) => eval_expr(ctx, result, current),
        None => Ok(Value::Null),
    }
}

fn trim_string(value: &str, side: TrimWhereField, what: Option<&str>) -> String {
    match what {
        None => match side {
            TrimWhereField::Both => value.trim().to_string(),
            TrimWhereField::Leading => value.trim_start().to_string(),
            TrimWhereField::Trailing => value.trim_end().to_string(),
        },
        Some(characters) => {
            let trimmed = if matches!(side, TrimWhereField::Leading | TrimWhereField::Both) {
                value.trim_start_matches(|c| characters.contains(c))
            } else {
                value
            };
            let trimmed = if matches!(side, TrimWhereField::Trailing | TrimWhereField::Both) {
                trimmed.trim_end_matches(|c| characters.contains(c))
            } else {
                trimmed
            };
            trimmed.to_string()
        }
    }
}

fn like_values(value: &Value, pattern: &Value, case_insensitive: bool) -> Result<bool, String> {
    if value.is_null() || pattern.is_null() {
        return Ok(false);
    }
    let text = value.to_display_string();
    let pattern_text = pattern.to_display_string();
    Ok(like_match(&text, &pattern_text, case_insensitive))
}

pub fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let text: Vec<char> = if case_insensitive {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let pattern: Vec<char> = if case_insensitive {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };

    let mut dp = vec![vec![false; pattern.len() + 1]; text.len() + 1];
    dp[0][0] = true;
    for j in 0..pattern.len() {
        if pattern[j] == '%' {
            dp[0][j + 1] = dp[0][j];
        }
    }

    for i in 0..text.len() {
        for j in 0..pattern.len() {
            if pattern[j] == '%' {
                dp[i + 1][j + 1] = dp[i + 1][j] || dp[i][j + 1];
            } else if pattern[j] == '_' {
                dp[i + 1][j + 1] = dp[i][j];
            } else {
                dp[i + 1][j + 1] = dp[i][j] && text[i] == pattern[j];
            }
        }
    }

    dp[text.len()][pattern.len()]
}

fn cast_value(value: Value, data_type: &SqlDataType) -> Result<Value, String> {
    match data_type {
        SqlDataType::Int(_) | SqlDataType::Integer(_) | SqlDataType::SmallInt(_)
        | SqlDataType::SmallIntUnsigned(_) | SqlDataType::Int2Unsigned(_)
        | SqlDataType::TinyInt(_) | SqlDataType::TinyIntUnsigned(_) | SqlDataType::UTinyInt
        | SqlDataType::USmallInt | SqlDataType::BigInt(_) | SqlDataType::BigIntUnsigned(_)
        | SqlDataType::Int8Unsigned(_) | SqlDataType::Int4Unsigned(_)
        | SqlDataType::IntegerUnsigned(_) | SqlDataType::IntUnsigned(_)
        | SqlDataType::MediumIntUnsigned(_) | SqlDataType::Unsigned | SqlDataType::UnsignedInteger => {
            if let Some(parsed) = value.as_i64() {
                return Ok(Value::Int(parsed));
            }
            if let Some(text) = value.as_text()
                && let Ok(parsed) = text.parse::<i64>()
            {
                return Ok(Value::Int(parsed));
            }
            Err(format!("Cannot cast `{}` to integer", value))
        }
        SqlDataType::Float(_) | SqlDataType::Real | SqlDataType::Double(_)
        | SqlDataType::DoublePrecision | SqlDataType::FloatUnsigned(_) | SqlDataType::RealUnsigned
        | SqlDataType::DoubleUnsigned(_) | SqlDataType::DoublePrecisionUnsigned => {
            if let Some(parsed) = value.as_f64() {
                return Ok(Value::Float(parsed));
            }
            if let Some(text) = value.as_text()
                && let Ok(parsed) = text.parse::<f64>()
            {
                return Ok(Value::Float(parsed));
            }
            Err(format!("Cannot cast `{}` to float", value))
        }
        SqlDataType::Boolean | SqlDataType::Bool => {
            if let Some(parsed) = value.as_bool() {
                return Ok(Value::Bool(parsed));
            }
            if let Some(text) = value.as_text() {
                let lower = text.to_lowercase();
                if lower == "true" || lower == "1" {
                    return Ok(Value::Bool(true));
                }
                if lower == "false" || lower == "0" {
                    return Ok(Value::Bool(false));
                }
            }
            Err(format!("Cannot cast `{}` to boolean", value))
        }
        SqlDataType::Text | SqlDataType::String(_) | SqlDataType::Char(_)
        | SqlDataType::Varchar(_) | SqlDataType::Character(_) | SqlDataType::CharacterVarying(_)
        | SqlDataType::TinyText | SqlDataType::MediumText | SqlDataType::LongText => {
            Ok(Value::Text(value.to_display_string()))
        }
        _ => Err(format!("Unsupported cast target type `{}`", data_type)),
    }
}

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

fn eval_function(ctx: &EvalContext, func: &Function, current: &[Value]) -> Result<Value, String> {
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

    fn scalar_result(expr: &str) -> Result<Value, String> {
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[])
    }

    #[test]
    fn arithmetic() {
        assert_eq!(scalar("1 + 2"), Value::Int(3));
        assert_eq!(scalar("10 - 4"), Value::Int(6));
        assert_eq!(scalar("2 * 3"), Value::Int(6));
        assert_eq!(scalar("10 / 4"), Value::Float(2.5));
        assert_eq!(scalar("7 % 3"), Value::Int(1));
        assert_eq!(scalar("2 * 3.5"), Value::Float(7.0));
    }

    #[test]
    fn arithmetic_null_and_zero() {
        assert_eq!(scalar("1 + NULL"), Value::Null);
        assert!(scalar_result("1 / 0").is_err());
        assert!(scalar_result("1 % 0").is_err());
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(scalar("1 < 2"), Value::Bool(true));
        assert_eq!(scalar("1 = 1"), Value::Bool(true));
        assert_eq!(scalar("1 != 2"), Value::Bool(true));
        assert_eq!(scalar("2 <= 2"), Value::Bool(true));
        assert_eq!(scalar("3 > 4"), Value::Bool(false));
        assert_eq!(scalar("TRUE AND FALSE"), Value::Bool(false));
        assert_eq!(scalar("TRUE OR FALSE"), Value::Bool(true));
        assert_eq!(scalar("NOT FALSE"), Value::Bool(true));
        assert_eq!(scalar("1 = NULL"), Value::Null);
    }

    #[test]
    fn string_operators() {
        assert_eq!(scalar("'SheetQL' LIKE '%QL'"), Value::Bool(true));
        assert_eq!(scalar("'SheetQL' LIKE '%missing%'"), Value::Bool(false));
        assert_eq!(scalar("'abc' ILIKE 'ABC'"), Value::Bool(true));
        assert_eq!(scalar("'a' IN ('a', 'b')"), Value::Bool(true));
        assert_eq!(scalar("'c' IN ('a', 'b')"), Value::Bool(false));
        assert_eq!(scalar("5 BETWEEN 1 AND 10"), Value::Bool(true));
        assert_eq!(scalar("15 BETWEEN 1 AND 10"), Value::Bool(false));
    }

    #[test]
    fn case_expression() {
        assert_eq!(
            scalar("CASE WHEN 2 > 1 THEN 'yes' ELSE 'no' END"),
            Value::Text("yes".to_string())
        );
        assert_eq!(
            scalar("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END"),
            Value::Text("two".to_string())
        );
        assert_eq!(scalar("CASE WHEN FALSE THEN 1 ELSE 2 END"), Value::Int(2));
    }

    #[test]
    fn cast_function() {
        assert_eq!(scalar("CAST('42' AS INTEGER)"), Value::Int(42));
        assert_eq!(scalar("CAST(3.5 AS TEXT)"), Value::Text("3.5".to_string()));
        assert_eq!(scalar("CAST('true' AS BOOLEAN)"), Value::Bool(true));
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
    fn unary_operators() {
        assert_eq!(scalar("-5"), Value::Int(-5));
        assert_eq!(scalar("+5"), Value::Int(5));
        assert_eq!(scalar("NOT TRUE"), Value::Bool(false));
    }

    #[test]
    fn unknown_function_errors() {
        assert!(scalar_result("NOPE(1)").is_err());
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