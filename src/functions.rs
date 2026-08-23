use std::collections::HashSet;

use chrono::{DateTime, Local};
use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;
use crate::value::values_partial_cmp;
use serde_json::Value as JsonValue;

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
                Some(sqlparser::ast::DuplicateTreatment::Distinct) => Ok(FnArgs::Distinct(exprs)),
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
        "url_scheme" | "url_host" | "url_port" | "url_path" | "url_query" | "url_fragment" => {
            eval_url_field(ctx, &args, current, &name)
        }
        "url_param" => eval_url_param(ctx, &args, current),
        "email_local" | "email_domain" | "email_valid" => {
            eval_email_function(ctx, &args, current, &name)
        }
        "format" => eval_format(ctx, &args, current),
        "json_valid" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Bool(parse_json_input(&values[0]).is_ok()))
        }
        "json_value" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let json = parse_json_input(&values[0])?;
            let selected = json_path(&json, &values[1])?.cloned();
            match selected {
                None | Some(JsonValue::Null) => Ok(Value::Null),
                Some(JsonValue::String(value)) => Ok(Value::Text(value)),
                Some(JsonValue::Bool(value)) => Ok(Value::Bool(value)),
                Some(JsonValue::Number(value)) => json_number_to_value(value),
                Some(JsonValue::Object(_)) | Some(JsonValue::Array(_)) => {
                    Err("JSON_VALUE path must return a scalar".to_string())
                }
            }
        }
        "json_parse" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Json(parse_json_input(&values[0])?))
        }
        "json_query" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let json = parse_json_input(&values[0])?;
            match json_path(&json, &values[1])? {
                Some(JsonValue::Object(value)) => Ok(Value::Json(JsonValue::Object(value.clone()))),
                Some(JsonValue::Array(value)) => Ok(Value::Json(JsonValue::Array(value.clone()))),
                _ => Ok(Value::Null),
            }
        }
        "json_exists" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let json = parse_json_input(&values[0])?;
            Ok(Value::Bool(json_path(&json, &values[1])?.is_some()))
        }
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
            Ok(Value::Int(
                values[0].to_display_string().chars().count() as i64
            ))
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
            Ok(Value::Text(
                values[0].to_display_string().trim().to_string(),
            ))
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
            let start = values[1]
                .as_i64()
                .ok_or("SUBSTRING start must be a number")?;
            let start_index = if start >= 0 { (start - 1) as usize } else { 0 };
            let end_index = if values.len() == 3 {
                let length = values[2]
                    .as_i64()
                    .ok_or("SUBSTRING length must be a number")?;
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
                parsed
                    .checked_abs()
                    .map(Value::Int)
                    .ok_or_else(|| format!("ABS overflow for `{parsed}`"))
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
            Ok(Value::Float(
                values[0]
                    .as_f64()
                    .ok_or("FLOOR expects a numeric value")?
                    .floor(),
            ))
        }
        "ceil" | "ceiling" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 1)?;
            Ok(Value::Float(
                values[0]
                    .as_f64()
                    .ok_or("CEIL expects a numeric value")?
                    .ceil(),
            ))
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
            let count = values[1]
                .as_i64()
                .ok_or("LEFT length must be a number")?
                .max(0) as usize;
            Ok(Value::Text(text.iter().take(count).collect()))
        }
        "right" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let count = values[1]
                .as_i64()
                .ok_or("RIGHT length must be a number")?
                .max(0) as usize;
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
        "startswith" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let text = values[0].to_display_string();
            let prefix = values[1].to_display_string();
            Ok(Value::Bool(text.starts_with(&prefix)))
        }
        "endswith" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let text = values[0].to_display_string();
            let suffix = values[1].to_display_string();
            Ok(Value::Bool(text.ends_with(&suffix)))
        }
        "split" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 3)?;
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let text = values[0].to_display_string();
            let sep = values[1].to_display_string();
            let index = values[2].as_i64().ok_or("SPLIT index must be a number")?;
            if index < 1 {
                return Ok(Value::Null);
            }
            let parts: Vec<&str> = if sep.is_empty() {
                vec![text.as_str()]
            } else {
                text.split(&sep).collect()
            };
            match parts.get((index - 1) as usize) {
                Some(part) => Ok(Value::Text(part.to_string())),
                None => Ok(Value::Null),
            }
        }
        "now" | "current_timestamp" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 0)?;
            Ok(Value::DateTime(now_string(&ctx.now)))
        }
        "date" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            if values.is_empty() {
                return Ok(Value::Date(today_string(&ctx.now)));
            }
            require_arity(&name, &values, 1)?;
            Ok(Value::Date(extract_date(&values[0].to_display_string())))
        }
        "power" | "pow" => {
            let values = eval_scalar_args(ctx, &args, current)?;
            require_arity(&name, &values, 2)?;
            let base = values[0].as_f64().ok_or("POWER base must be a number")?;
            let exponent = values[1]
                .as_f64()
                .ok_or("POWER exponent must be a number")?;
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

fn eval_url_field(
    ctx: &EvalContext,
    args: &FnArgs,
    current: &[Value],
    name: &str,
) -> Result<Value, String> {
    let values = eval_scalar_args(ctx, args, current)?;
    require_arity(name, &values, 1)?;
    if values[0].is_null() {
        return Ok(Value::Null);
    }
    let text = values[0]
        .as_text()
        .ok_or_else(|| format!("Function `{name}` expects a text URL, got `{}`", values[0]))?;
    let parsed = url::Url::parse(text).map_err(|error| format!("Invalid URL `{text}`: {error}"))?;
    let value = match name {
        "url_scheme" => Some(parsed.scheme().to_string()),
        "url_host" => parsed.host_str().map(str::to_string),
        "url_port" => parsed.port().map(|port| port.to_string()),
        "url_path" => Some(parsed.path().to_string()),
        "url_query" => parsed.query().map(str::to_string),
        "url_fragment" => parsed.fragment().map(str::to_string),
        _ => unreachable!(),
    };
    Ok(value.map(Value::Text).unwrap_or(Value::Null))
}

fn eval_url_param(ctx: &EvalContext, args: &FnArgs, current: &[Value]) -> Result<Value, String> {
    let values = eval_scalar_args(ctx, args, current)?;
    require_arity("url_param", &values, 2)?;
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let text = values[0]
        .as_text()
        .ok_or("Function `url_param` expects a text URL")?;
    let name = values[1]
        .as_text()
        .ok_or("Function `url_param` expects a text parameter name")?;
    let parsed = url::Url::parse(text).map_err(|error| format!("Invalid URL `{text}`: {error}"))?;
    Ok(parsed
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| Value::Text(value.into_owned()))
        .unwrap_or(Value::Null))
}

fn eval_email_function(
    ctx: &EvalContext,
    args: &FnArgs,
    current: &[Value],
    name: &str,
) -> Result<Value, String> {
    let values = eval_scalar_args(ctx, args, current)?;
    require_arity(name, &values, 1)?;
    if values[0].is_null() {
        return Ok(Value::Null);
    }
    let email = values[0]
        .as_text()
        .ok_or_else(|| format!("Function `{name}` expects a text email address"))?;
    let Some((local, domain)) = split_email(email) else {
        return if name == "email_valid" {
            Ok(Value::Bool(false))
        } else {
            Ok(Value::Null)
        };
    };
    match name {
        "email_local" => Ok(Value::Text(local.to_string())),
        "email_domain" => Ok(Value::Text(domain.to_string())),
        "email_valid" => Ok(Value::Bool(true)),
        _ => unreachable!(),
    }
}

fn split_email(email: &str) -> Option<(&str, &str)> {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
        || local.chars().any(|character| {
            character.is_whitespace()
                || matches!(character, '(' | ')' | '<' | '>' | ',' | ';' | ':')
        })
        || !domain.contains('.')
        || domain.chars().any(|character| {
            character.is_whitespace()
                || matches!(character, '(' | ')' | '<' | '>' | ',' | ';' | ':')
        })
    {
        return None;
    }
    Some((local, domain))
}

fn eval_format(ctx: &EvalContext, args: &FnArgs, current: &[Value]) -> Result<Value, String> {
    let values = eval_scalar_args(ctx, args, current)?;
    if values.is_empty() {
        return Err("Function `format` expects at least 1 argument".to_string());
    }
    if values[0].is_null() {
        return Ok(Value::Null);
    }
    let template = values[0]
        .as_text()
        .ok_or("Function `format` expects a text template")?;
    let arguments = &values[1..];
    let mut output = String::new();
    let mut chars = template.chars().peekable();
    let mut sequential = 0usize;
    while let Some(character) = chars.next() {
        if character == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                output.push('{');
                continue;
            }
            let mut placeholder = String::new();
            let mut closed = false;
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    closed = true;
                    break;
                }
                placeholder.push(next);
            }
            if !closed {
                return Err("FORMAT contains an unterminated placeholder".to_string());
            }
            let index = if placeholder.is_empty() {
                let index = sequential;
                sequential += 1;
                index
            } else {
                placeholder
                    .parse::<usize>()
                    .map_err(|_| format!("FORMAT has invalid placeholder `{{{placeholder}}}`"))?
            };
            let value = arguments
                .get(index)
                .ok_or_else(|| format!("FORMAT argument {index} is missing"))?;
            output.push_str(&value.to_display_string());
        } else if character == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                output.push('}');
            } else {
                return Err("FORMAT contains an unmatched `}`".to_string());
            }
        } else {
            output.push(character);
        }
    }
    Ok(Value::Text(output))
}

fn parse_json_input(value: &Value) -> Result<JsonValue, String> {
    match value {
        Value::Json(value) => Ok(value.clone()),
        Value::Text(text) => {
            serde_json::from_str(text).map_err(|error| format!("Invalid JSON: {error}"))
        }
        Value::Null => Err("JSON value cannot be NULL".to_string()),
        _ => Err(format!("JSON function expects text or JSON, got `{value}`")),
    }
}

fn json_number_to_value(value: serde_json::Number) -> Result<Value, String> {
    if let Some(value) = value.as_i64() {
        Ok(Value::Int(value))
    } else if let Some(value) = value.as_f64() {
        if value.is_finite() {
            Ok(Value::Float(value))
        } else {
            Err("JSON number is not finite".to_string())
        }
    } else {
        Err("JSON integer is outside the supported numeric range".to_string())
    }
}

fn json_path<'a>(value: &'a JsonValue, path: &Value) -> Result<Option<&'a JsonValue>, String> {
    let path = match path {
        Value::Text(path) => path,
        _ => return Err("JSON path must be text".to_string()),
    };
    if path == "$" {
        return Ok(Some(value));
    }
    if !path.starts_with('$') {
        return Err(format!("Invalid JSON path `{path}`"));
    }

    let chars: Vec<char> = path.chars().collect();
    let mut index = 1;
    let mut current = value;
    while index < chars.len() {
        match chars[index] {
            '.' => {
                index += 1;
                let start = index;
                while index < chars.len() && chars[index] != '.' && chars[index] != '[' {
                    index += 1;
                }
                if start == index {
                    return Err(format!("Invalid JSON path `{path}`"));
                }
                let key: String = chars[start..index].iter().collect();
                current = match current.get(&key) {
                    Some(value) => value,
                    None => return Ok(None),
                };
            }
            '[' => {
                index += 1;
                let start = index;
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                }
                if start == index || index >= chars.len() || chars[index] != ']' {
                    return Err(format!("Invalid JSON path `{path}`"));
                }
                let position: usize = chars[start..index]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|_| format!("Invalid JSON path `{path}`"))?;
                index += 1;
                current = match current.get(position) {
                    Some(value) => value,
                    None => return Ok(None),
                };
            }
            _ => return Err(format!("Invalid JSON path `{path}`")),
        }
    }
    Ok(Some(current))
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

fn now_string(now: &DateTime<Local>) -> String {
    now.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today_string(now: &DateTime<Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

fn extract_date(input: &str) -> String {
    let trimmed = input.trim();
    let date_part = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let has_separator = date_part.chars().any(|c| matches!(c, '-' | '/' | '.'));
    if !has_separator {
        return trimmed.to_string();
    }
    let parts: Vec<&str> = date_part.split(['-', '/', '.']).collect();
    if parts.len() != 3 {
        return trimmed.to_string();
    }
    let (year, month, day) = (parts[0], parts[1], parts[2]);
    if year.len() != 4
        || month.is_empty()
        || month.len() > 2
        || day.is_empty()
        || day.len() > 2
        || !year.chars().all(|c| c.is_ascii_digit())
        || !month.chars().all(|c| c.is_ascii_digit())
        || !day.chars().all(|c| c.is_ascii_digit())
    {
        return trimmed.to_string();
    }
    let month = month.parse::<u32>().unwrap_or(0);
    let day = day.parse::<u32>().unwrap_or(0);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return trimmed.to_string();
    }
    format!("{}-{:02}-{:02}", year, month, day)
}

fn eval_scalar_args(
    ctx: &EvalContext,
    args: &FnArgs,
    current: &[Value],
) -> Result<Vec<Value>, String> {
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

fn collect_numeric(ctx: &EvalContext, args: &FnArgs) -> Result<(Vec<Value>, bool), String> {
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

fn eval_avg(ctx: &EvalContext, args: &FnArgs) -> Result<Value, String> {
    let (values, _) = collect_numeric(ctx, args)?;
    if values.is_empty() {
        return Ok(Value::Null);
    }
    let sum: f64 = values.iter().map(|v| v.as_f64().unwrap_or(0.0)).sum();
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
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[]).expect("eval scalar")
    }

    fn scalar_result(expr: &str) -> Result<Value, String> {
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[])
    }

    #[test]
    fn scalar_functions() {
        assert_eq!(scalar("LEN('SheetQL')"), Value::Int(7));
        assert_eq!(scalar("LOWER('ABC')"), Value::Text("abc".to_string()));
        assert_eq!(scalar("UPPER('abc')"), Value::Text("ABC".to_string()));
        assert_eq!(scalar("TRIM('  x  ')"), Value::Text("x".to_string()));
        assert_eq!(
            scalar("CONCAT('a', 'b', 'c')"),
            Value::Text("abc".to_string())
        );
        assert_eq!(
            scalar("SUBSTRING('SheetQL', 1, 4)"),
            Value::Text("Shee".to_string())
        );
        assert_eq!(
            scalar("REPLACE('a-b-a', 'a', 'x')"),
            Value::Text("x-b-x".to_string())
        );
        assert_eq!(scalar("ABS(-5)"), Value::Int(5));
        assert_eq!(scalar("ROUND(3.146, 2)"), Value::Float(3.15));
        assert_eq!(scalar("MOD(10, 3)"), Value::Int(1));
        assert_eq!(
            scalar("IFNULL(NULL, 'fallback')"),
            Value::Text("fallback".to_string())
        );
        assert_eq!(scalar("IFNULL(5, 'fallback')"), Value::Int(5));
        assert_eq!(scalar("COALESCE(NULL, NULL, 7)"), Value::Int(7));
    }

    #[test]
    fn startswith_endswith_split_functions() {
        assert_eq!(scalar("STARTSWITH('SheetQL', 'Sheet')"), Value::Bool(true));
        assert_eq!(scalar("STARTSWITH('SheetQL', 'sql')"), Value::Bool(false));
        assert_eq!(scalar("ENDSWITH('SheetQL', 'QL')"), Value::Bool(true));
        assert_eq!(scalar("ENDSWITH('SheetQL', 'She')"), Value::Bool(false));
        assert_eq!(
            scalar("SPLIT('a,b,c', ',', 2)"),
            Value::Text("b".to_string())
        );
        assert_eq!(
            scalar("SPLIT('a,b,c', ',', 1)"),
            Value::Text("a".to_string())
        );
        assert_eq!(scalar("SPLIT('a,b,c', ',', 9)"), Value::Null);
        assert_eq!(scalar("STARTSWITH(NULL, 'x')"), Value::Null);
    }

    #[test]
    fn json_functions_parse_extract_and_test_paths() {
        let json = r#"'{"user":{"name":"Alice","age":30},"items":[{"id":7}],"empty":null}'"#;
        assert_eq!(scalar(&format!("JSON_VALID({json})")), Value::Bool(true));
        assert_eq!(
            scalar(&format!("JSON_VALUE({json}, '$.user.name')")),
            Value::Text("Alice".into())
        );
        assert_eq!(
            scalar(&format!("JSON_VALUE({json}, '$.user.age')")),
            Value::Int(30)
        );
        assert_eq!(
            scalar(&format!("JSON_VALUE({json}, '$.items[0].id')")),
            Value::Int(7)
        );
        assert_eq!(
            scalar(&format!("JSON_EXISTS({json}, '$.empty')")),
            Value::Bool(true)
        );
        assert_eq!(
            scalar(&format!("JSON_EXISTS({json}, '$.missing')")),
            Value::Bool(false)
        );
        assert_eq!(
            scalar(&format!("JSON_QUERY({json}, '$.user')")),
            Value::Json(serde_json::json!({"name": "Alice", "age": 30}))
        );
        assert_eq!(
            scalar(&format!("JSON_PARSE({json})")),
            serde_json::from_str::<serde_json::Value>(
                r#"{"user":{"name":"Alice","age":30},"items":[{"id":7}],"empty":null}"#,
            )
            .map(Value::Json)
            .unwrap()
        );
    }

    #[test]
    fn json_valid_returns_false_for_invalid_input() {
        assert_eq!(scalar("JSON_VALID('{not json}')"), Value::Bool(false));
    }

    #[test]
    fn url_functions_extract_components_and_parameters() {
        assert_eq!(
            scalar(
                "URL_SCHEME('https://user:pass@example.com:8443/a/b?x=hello%20world&x=second#top')"
            ),
            Value::Text("https".into())
        );
        assert_eq!(
            scalar(
                "URL_HOST('https://user:pass@example.com:8443/a/b?x=hello%20world&x=second#top')"
            ),
            Value::Text("example.com".into())
        );
        assert_eq!(
            scalar("URL_PORT('https://example.com:8443/a')"),
            Value::Text("8443".into())
        );
        assert_eq!(
            scalar("URL_PATH('https://example.com/a/b')"),
            Value::Text("/a/b".into())
        );
        assert_eq!(
            scalar("URL_QUERY('https://example.com/a?x=hello%20world')"),
            Value::Text("x=hello%20world".into())
        );
        assert_eq!(
            scalar("URL_FRAGMENT('https://example.com/a#top')"),
            Value::Text("top".into())
        );
        assert_eq!(
            scalar("URL_PARAM('https://example.com/a?x=hello%20world&x=second', 'x')"),
            Value::Text("hello world".into())
        );
        assert_eq!(
            scalar("URL_PARAM('https://example.com/a', 'x')"),
            Value::Null
        );
    }

    #[test]
    fn url_functions_cover_missing_values_nulls_and_errors() {
        assert_eq!(scalar("URL_PORT('https://example.com/path')"), Value::Null);
        assert_eq!(scalar("URL_QUERY('https://example.com/path')"), Value::Null);
        assert_eq!(
            scalar("URL_FRAGMENT('https://example.com/path')"),
            Value::Null
        );
        assert_eq!(scalar("URL_HOST(NULL)"), Value::Null);
        assert!(scalar_result("URL_HOST('not a url')").is_err());
        assert!(scalar_result("URL_PARAM('https://example.com', 1)").is_err());
        assert!(scalar_result("URL_HOST(42)").is_err());
        assert_eq!(
            scalar("URL_PARAM('https://example.com/?empty=&flag', 'empty')"),
            Value::Text(String::new())
        );
        assert_eq!(
            scalar("URL_PARAM('https://example.com/?flag', 'missing')"),
            Value::Null
        );
    }

    #[test]
    fn email_functions_validate_and_split_addresses() {
        assert_eq!(
            scalar("EMAIL_LOCAL('Alice.Example@example.com')"),
            Value::Text("Alice.Example".into())
        );
        assert_eq!(
            scalar("EMAIL_DOMAIN('Alice.Example@example.com')"),
            Value::Text("example.com".into())
        );
        assert_eq!(scalar("EMAIL_VALID('a@example.com')"), Value::Bool(true));
        assert_eq!(scalar("EMAIL_VALID('not-an-email')"), Value::Bool(false));
        assert_eq!(scalar("EMAIL_DOMAIN('not-an-email')"), Value::Null);
    }

    #[test]
    fn email_functions_reject_malformed_addresses_and_handle_nulls() {
        for email in [
            "",
            "missing-at.example.com",
            "@example.com",
            "user@",
            "user@example",
            ".user@example.com",
            "user.@example.com",
            "u..ser@example.com",
            "user@example..com",
            "user name@example.com",
        ] {
            assert_eq!(
                scalar(&format!("EMAIL_VALID('{email}')")),
                Value::Bool(false)
            );
            assert_eq!(scalar(&format!("EMAIL_LOCAL('{email}')")), Value::Null);
            assert_eq!(scalar(&format!("EMAIL_DOMAIN('{email}')")), Value::Null);
        }
        assert_eq!(scalar("EMAIL_VALID(NULL)"), Value::Null);
        assert_eq!(scalar("EMAIL_LOCAL(NULL)"), Value::Null);
        assert_eq!(scalar("EMAIL_DOMAIN(NULL)"), Value::Null);
    }

    #[test]
    fn format_function_supports_sequential_indexed_and_escaped_placeholders() {
        assert_eq!(
            scalar("FORMAT('Hello {}, {}!', 'Alice', 'world')"),
            Value::Text("Hello Alice, world!".into())
        );
        assert_eq!(
            scalar("FORMAT('{1} before {0}', 'first', 'second')"),
            Value::Text("second before first".into())
        );
        assert_eq!(
            scalar("FORMAT('{{id}} = {}', 42)"),
            Value::Text("{id} = 42".into())
        );
    }

    #[test]
    fn format_function_handles_types_null_and_errors() {
        assert_eq!(
            scalar("FORMAT('id={}, ok={}, missing={}', 7, TRUE, NULL)"),
            Value::Text("id=7, ok=true, missing=NULL".into())
        );
        assert_eq!(scalar("FORMAT(NULL, 'ignored')"), Value::Null);
        assert!(scalar_result("FORMAT()").is_err());
        assert!(scalar_result("FORMAT('{0}', 'a', 'b')").is_ok());
        assert!(scalar_result("FORMAT('{1}', 'a')").is_err());
        assert!(scalar_result("FORMAT('{name}', 'a')").is_err());
        assert!(scalar_result("FORMAT('{', 'a')").is_err());
        assert!(scalar_result("FORMAT('}', 'a')").is_err());
        assert_eq!(
            scalar("FORMAT('reuse {0} and {0}', 'x')"),
            Value::Text("reuse x and x".into())
        );
    }

    #[test]
    fn date_parses_padded_and_unpadded() {
        assert_eq!(
            scalar("DATE('2026/5/7')"),
            Value::Date("2026-05-07".to_string())
        );
        assert_eq!(
            scalar("DATE('2026-05-07')"),
            Value::Date("2026-05-07".to_string())
        );
        assert_eq!(
            scalar("DATE('2026/08/14 10:30:00')"),
            Value::Date("2026-08-14".to_string())
        );
        assert_eq!(scalar("DATE('nope')"), Value::Date("nope".to_string()));
    }

    #[test]
    fn contains_aggregate_detection() {
        assert!(contains_aggregate(&parse_expr("COUNT(*) > 1")));
        assert!(contains_aggregate(&parse_expr("SUM(amount) + 1")));
        assert!(contains_aggregate(&parse_expr(
            "CASE WHEN MAX(x) > 0 THEN 1 ELSE 0 END"
        )));
        assert!(!contains_aggregate(&parse_expr("LOWER(name)")));
        assert!(!contains_aggregate(&parse_expr("age + 1")));
    }
}
