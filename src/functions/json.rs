use serde_json::Value as JsonValue;

use crate::error::Error;
use crate::value::Value;

use super::require_arity;

/// JSON functions (`JSON_VALID`, `JSON_VALUE`, `JSON_PARSE`, `JSON_QUERY`,
/// `JSON_EXISTS`) plus the shared JSON path helpers.
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "json_valid" => {
            require_arity(name, values, 1)?;
            Value::Bool(parse_json_input(&values[0]).is_ok())
        }
        "json_value" => {
            require_arity(name, values, 2)?;
            let json = parse_json_input(&values[0])?;
            let selected = json_path(&json, &values[1])?.cloned();
            match selected {
                None | Some(JsonValue::Null) => Value::Null,
                Some(JsonValue::String(value)) => Value::Text(value),
                Some(JsonValue::Bool(value)) => Value::Bool(value),
                Some(JsonValue::Number(value)) => json_number_to_value(value)?,
                Some(JsonValue::Object(_)) | Some(JsonValue::Array(_)) => {
                    return Err("JSON_VALUE path must return a scalar".to_string().into());
                }
            }
        }
        "json_parse" => {
            require_arity(name, values, 1)?;
            Value::Json(parse_json_input(&values[0])?)
        }
        "json_query" => {
            require_arity(name, values, 2)?;
            let json = parse_json_input(&values[0])?;
            match json_path(&json, &values[1])? {
                Some(JsonValue::Object(value)) => Value::Json(JsonValue::Object(value.clone())),
                Some(JsonValue::Array(value)) => Value::Json(JsonValue::Array(value.clone())),
                _ => Value::Null,
            }
        }
        "json_exists" => {
            require_arity(name, values, 2)?;
            let json = parse_json_input(&values[0])?;
            Value::Bool(json_path(&json, &values[1])?.is_some())
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn parse_json_input(value: &Value) -> Result<JsonValue, Error> {
    match value {
        Value::Json(value) => Ok(value.clone()),
        Value::Text(text) => {
            serde_json::from_str(text).map_err(|error| format!("Invalid JSON: {error}").into())
        }
        Value::Null => Err("JSON value cannot be NULL".to_string().into()),
        _ => Err(format!("JSON function expects text or JSON, got `{value}`").into()),
    }
}

fn json_number_to_value(value: serde_json::Number) -> Result<Value, Error> {
    if let Some(value) = value.as_i64() {
        Ok(Value::Int(value))
    } else if let Some(value) = value.as_f64() {
        if value.is_finite() {
            Ok(Value::Float(value))
        } else {
            Err("JSON number is not finite".to_string().into())
        }
    } else {
        Err("JSON integer is outside the supported numeric range"
            .to_string()
            .into())
    }
}

fn json_path<'a>(value: &'a JsonValue, path: &Value) -> Result<Option<&'a JsonValue>, Error> {
    let path = match path {
        Value::Text(path) => path,
        _ => return Err("JSON path must be text".to_string().into()),
    };
    if path == "$" {
        return Ok(Some(value));
    }
    if !path.starts_with('$') {
        return Err(format!("Invalid JSON path `{path}`").into());
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
                    return Err(format!("Invalid JSON path `{path}`").into());
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
                    return Err(format!("Invalid JSON path `{path}`").into());
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
            _ => return Err(format!("Invalid JSON path `{path}`").into()),
        }
    }
    Ok(Some(current))
}
