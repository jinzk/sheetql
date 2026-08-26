use crate::error::Error;
use crate::value::Value;

use super::require_arity;

/// Shared `SUBSTRING` implementation with MySQL semantics: positions are
/// 1-based, a start of 0 yields an empty string, and negative starts count
/// back from the end of the text. Used both by the plain function call and by
/// the `Expr::Substring` AST node handled in `crate::evaluator`.
pub(crate) fn substring(text: &str, from: i64, length: Option<i64>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let char_count = chars.len() as i64;
    let start_index = if from >= 1 {
        // Positions beyond the text simply produce an empty result.
        (from - 1).min(char_count) as usize
    } else {
        // from == 0 or negative: count back from the end.
        char_count.saturating_add(from).max(0) as usize
    };
    let end_index = match length {
        Some(count) => start_index.saturating_add(count.max(0) as usize),
        None => chars.len(),
    };
    chars
        .get(start_index..end_index.min(chars.len()))
        .unwrap_or(&[])
        .iter()
        .collect()
}

/// String and formatting functions. Returns `Ok(None)` when `name` does not
/// belong to this module so the caller can try the next category.
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "len" | "length" | "char_length" => {
            require_arity(name, values, 1)?;
            Value::Int(values[0].to_display_string().chars().count() as i64)
        }
        "lower" | "lcase" => {
            require_arity(name, values, 1)?;
            Value::Text(values[0].to_display_string().to_lowercase())
        }
        "upper" | "ucase" => {
            require_arity(name, values, 1)?;
            Value::Text(values[0].to_display_string().to_uppercase())
        }
        "trim" => {
            require_arity(name, values, 1)?;
            Value::Text(values[0].to_display_string().trim().to_string())
        }
        "ltrim" => {
            require_arity(name, values, 1)?;
            Value::Text(values[0].to_display_string().trim_start().to_string())
        }
        "rtrim" => {
            require_arity(name, values, 1)?;
            Value::Text(values[0].to_display_string().trim_end().to_string())
        }
        "concat" => {
            if values.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
            }
            Value::Text(
                values
                    .iter()
                    .map(Value::to_display_string)
                    .collect::<Vec<_>>()
                    .join(""),
            )
        }
        "substring" | "substr" => {
            if values.len() != 2 && values.len() != 3 {
                return Err(format!("Function `{name}` expects 2 or 3 arguments").into());
            }
            let start = values[1]
                .as_i64()
                .ok_or("SUBSTRING start must be a number")?;
            let length = if values.len() == 3 {
                Some(
                    values[2]
                        .as_i64()
                        .ok_or("SUBSTRING length must be a number")?,
                )
            } else {
                None
            };
            Value::Text(substring(&values[0].to_display_string(), start, length))
        }
        "replace" => {
            require_arity(name, values, 3)?;
            Value::Text(values[0].to_display_string().replace(
                &values[1].to_display_string(),
                &values[2].to_display_string(),
            ))
        }
        "left" => {
            require_arity(name, values, 2)?;
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let count = values[1]
                .as_i64()
                .ok_or("LEFT length must be a number")?
                .max(0) as usize;
            Value::Text(text.iter().take(count).collect())
        }
        "right" => {
            require_arity(name, values, 2)?;
            let text: Vec<char> = values[0].to_display_string().chars().collect();
            let count = values[1]
                .as_i64()
                .ok_or("RIGHT length must be a number")?
                .max(0) as usize;
            let start = text.len().saturating_sub(count);
            Value::Text(text.iter().skip(start).collect())
        }
        "instr" => {
            require_arity(name, values, 2)?;
            let haystack = values[0].to_display_string();
            let needle = values[1].to_display_string();
            let position = haystack
                .find(&needle)
                .map(|index| haystack[..index].chars().count() as i64 + 1)
                .unwrap_or(0);
            Value::Int(position)
        }
        "startswith" => {
            require_arity(name, values, 2)?;
            if values.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
            }
            Value::Bool(
                values[0]
                    .to_display_string()
                    .starts_with(&values[1].to_display_string()),
            )
        }
        "endswith" => {
            require_arity(name, values, 2)?;
            if values.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
            }
            Value::Bool(
                values[0]
                    .to_display_string()
                    .ends_with(&values[1].to_display_string()),
            )
        }
        "split" => {
            require_arity(name, values, 3)?;
            if values.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
            }
            let text = values[0].to_display_string();
            let sep = values[1].to_display_string();
            let index = values[2].as_i64().ok_or("SPLIT index must be a number")?;
            if index < 1 {
                return Ok(Some(Value::Null));
            }
            let parts: Vec<&str> = if sep.is_empty() {
                vec![text.as_str()]
            } else {
                text.split(&sep).collect()
            };
            match parts.get((index - 1) as usize) {
                Some(part) => Value::Text(part.to_string()),
                None => return Ok(Some(Value::Null)),
            }
        }
        "format" => eval_format(values)?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn eval_format(values: &[Value]) -> Result<Value, Error> {
    if values.is_empty() {
        return Err("Function `format` expects at least 1 argument"
            .to_string()
            .into());
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
                return Err("FORMAT contains an unterminated placeholder"
                    .to_string()
                    .into());
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
                return Err("FORMAT contains an unmatched `}`".to_string().into());
            }
        } else {
            output.push(character);
        }
    }
    Ok(Value::Text(output))
}
