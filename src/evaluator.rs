use std::collections::HashMap;

use chrono::{DateTime, Local};
use sqlparser::ast::{
    BinaryOperator, CaseWhen, DataType as SqlDataType, Expr, TrimWhereField, UnaryOperator,
    Value as SqlValue, ValueWithSpan,
};

use crate::functions::eval_function;
use crate::value::values_eq;
use crate::value::values_partial_cmp;
use crate::value::Value;

pub struct EvalContext<'a> {
    pub columns: &'a HashMap<String, usize>,
    pub all_rows: &'a [Vec<Value>],
    pub group_rows: &'a [usize],
    pub now: DateTime<Local>,
}

static EMPTY_COLUMNS: std::sync::OnceLock<HashMap<String, usize>> = std::sync::OnceLock::new();
static EMPTY_ROWS: [Vec<Value>; 0] = [];
static EMPTY_GROUP: [usize; 0] = [];

impl<'a> EvalContext<'a> {
    pub fn new(
        columns: &'a HashMap<String, usize>,
        all_rows: &'a [Vec<Value>],
        group_rows: &'a [usize],
        now: DateTime<Local>,
    ) -> Self {
        Self {
            columns,
            all_rows,
            group_rows,
            now,
        }
    }

    pub fn scalar() -> Self {
        Self {
            columns: EMPTY_COLUMNS.get_or_init(HashMap::new),
            all_rows: &EMPTY_ROWS,
            group_rows: &EMPTY_GROUP,
            now: Local::now(),
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
        Expr::Identifier(ident) => resolve_column(ctx, &ident.value, current),
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
            escape_char,
            ..
        } => {
            let value = eval_expr(ctx, expr, current)?;
            let pattern = eval_expr(ctx, pattern, current)?;
            if value.is_null() || pattern.is_null() {
                return Ok(Value::Null);
            }
            let escape = match escape_char {
                Some(v) => match eval_sql_value(&v.value) {
                    Ok(Value::Text(s)) => s.chars().next(),
                    _ => None,
                },
                None => None,
            };
            let matched = like_values(&value, &pattern, false, escape)?;
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            ..
        } => {
            let value = eval_expr(ctx, expr, current)?;
            let pattern = eval_expr(ctx, pattern, current)?;
            if value.is_null() || pattern.is_null() {
                return Ok(Value::Null);
            }
            let escape = match escape_char {
                Some(v) => match eval_sql_value(&v.value) {
                    Ok(Value::Text(s)) => s.chars().next(),
                    _ => None,
                },
                None => None,
            };
            let matched = like_values(&value, &pattern, true, escape)?;
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
    if let Some(index) = ctx.columns.get(name) {
        return Ok(current.get(*index).cloned().unwrap_or(Value::Null));
    }
    let lowered = name.to_lowercase();
    match ctx.columns.get(&lowered) {
        Some(index) => Ok(current.get(*index).cloned().unwrap_or(Value::Null)),
        None => Err(format!("Column `{lowered}` not found")),
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
                    BinaryOperator::Plus => a.checked_add(b),
                    BinaryOperator::Minus => a.checked_sub(b),
                    BinaryOperator::Multiply => a.checked_mul(b),
                    _ => unreachable!(),
                }
                .ok_or_else(|| format!("Integer overflow in `{op}` for `{a}` and `{b}`"))?;
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

fn like_values(value: &Value, pattern: &Value, case_insensitive: bool, escape: Option<char>) -> Result<bool, String> {
    if value.is_null() || pattern.is_null() {
        return Ok(false);
    }
    let text = value.to_display_string();
    let pattern_text = pattern.to_display_string();
    Ok(like_match(&text, &pattern_text, case_insensitive, escape))
}

pub fn like_match(text: &str, pattern: &str, case_insensitive: bool, escape: Option<char>) -> bool {
    let text: Vec<char> = if case_insensitive {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let pattern_norm: String = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };

    enum Token {
        Percent,
        Underscore,
        Literal(char),
    }

    let mut tokens: Vec<Token> = Vec::new();
    let mut chars = pattern_norm.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            if let Some(next) = chars.next() {
                tokens.push(Token::Literal(next));
            }
            // A trailing escape character with nothing after it is ignored.
        } else if c == '%' {
            tokens.push(Token::Percent);
        } else if c == '_' {
            tokens.push(Token::Underscore);
        } else {
            tokens.push(Token::Literal(c));
        }
    }

    let n = tokens.len();
    let mut dp = vec![vec![false; n + 1]; text.len() + 1];
    dp[0][0] = true;
    for j in 0..n {
        if matches!(tokens[j], Token::Percent) {
            dp[0][j + 1] = dp[0][j];
        }
    }

    for i in 0..text.len() {
        for j in 0..n {
            match tokens[j] {
                Token::Percent => dp[i + 1][j + 1] = dp[i + 1][j] || dp[i][j + 1],
                Token::Underscore => dp[i + 1][j + 1] = dp[i][j],
                Token::Literal(ch) => dp[i + 1][j + 1] = dp[i][j] && text[i] == ch,
            }
        }
    }

    dp[text.len()][n]
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
    fn unary_operators() {
        assert_eq!(scalar("-5"), Value::Int(-5));
        assert_eq!(scalar("+5"), Value::Int(5));
        assert_eq!(scalar("NOT TRUE"), Value::Bool(false));
    }

    #[test]
    fn unknown_function_errors() {
        assert!(scalar_result("NOPE(1)").is_err());
    }
}