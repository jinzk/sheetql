use crate::error::Error;
use crate::value::Value;
use crate::value::values_partial_cmp;

use super::require_arity;

/// Numeric functions (`ABS`, `ROUND`, `CEIL`/`CEILING`, `MOD`, `POWER`,
/// `SQRT`, `GREATEST`, `LEAST`). `FLOOR`/`CEIL` are parsed by sqlparser as
/// `Expr::Floor`/`Expr::Ceil` and evaluated in `crate::evaluator` via
/// [`floor_ceil`]; `CEILING` is a plain function call handled here.
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "abs" => {
            require_arity(name, values, 1)?;
            if let Some(parsed) = values[0].as_i64() {
                Value::Int(
                    parsed
                        .checked_abs()
                        .ok_or_else(|| format!("ABS overflow for `{parsed}`"))?,
                )
            } else if let Some(parsed) = values[0].as_f64() {
                Value::Float(parsed.abs())
            } else {
                return Err(format!("ABS expects a numeric value, got `{}`", values[0]).into());
            }
        }
        "round" => {
            if values.len() != 1 && values.len() != 2 {
                return Err(format!("Function `{name}` expects 1 or 2 arguments").into());
            }
            let number = values[0].as_f64().ok_or("ROUND expects a numeric value")?;
            let decimals = if values.len() == 2 {
                values[1].as_i64().unwrap_or(0) as i32
            } else {
                0
            };
            let factor = 10f64.powi(decimals);
            Value::Float((number * factor).round() / factor)
        }
        "ceil" | "ceiling" => {
            require_arity(name, values, 1)?;
            floor_ceil("ceil", &values[0])?
        }
        "mod" => {
            require_arity(name, values, 2)?;
            Value::Int(
                values[0].as_i64().ok_or("MOD expects numeric values")?
                    % values[1].as_i64().ok_or("MOD expects numeric values")?,
            )
        }
        "power" | "pow" => {
            require_arity(name, values, 2)?;
            let base = values[0].as_f64().ok_or("POWER base must be a number")?;
            let exponent = values[1]
                .as_f64()
                .ok_or("POWER exponent must be a number")?;
            Value::Float(base.powf(exponent))
        }
        "sqrt" => {
            require_arity(name, values, 1)?;
            let number = values[0].as_f64().ok_or("SQRT expects a numeric value")?;
            if number < 0.0 {
                return Err("SQRT expects a non-negative value".to_string().into());
            }
            Value::Float(number.sqrt())
        }
        "greatest" | "least" => {
            if values.is_empty() {
                return Err(format!("Function `{name}` expects at least 1 argument").into());
            }
            if values.iter().any(Value::is_null) {
                return Ok(Some(Value::Null));
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
            best
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Floor or ceil a single numeric value. Shared by the `CEIL`/`CEILING`
/// function calls and the `Expr::Floor`/`Expr::Ceil` AST nodes handled in
/// `crate::evaluator`.
pub(crate) fn floor_ceil(name: &str, value: &Value) -> Result<Value, Error> {
    let number = if name == "floor" {
        value.as_f64().ok_or("FLOOR expects a numeric value")?
    } else {
        value.as_f64().ok_or("CEIL expects a numeric value")?
    };
    Ok(Value::Float(if name == "floor" {
        number.floor()
    } else {
        number.ceil()
    }))
}
