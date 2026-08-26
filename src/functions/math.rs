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
            let decimals = match values.get(1) {
                Some(value) => value
                    .as_i64()
                    .ok_or_else(|| format!("ROUND decimals must be a number, got `{value}`"))?,
                None => 0,
            };
            // Extreme decimal counts make the power factor overflow/underflow
            // into inf/0 and produce NaN; clamp to a sane range instead.
            let decimals = i32::try_from(decimals).unwrap_or(if decimals > 0 { 30 } else { -30 });
            let decimals = decimals.clamp(-30, 30);
            let factor = 10f64.powi(decimals);
            let rounded = (number * factor).round() / factor;
            Value::Float(if rounded.is_finite() { rounded } else { number })
        }
        "ceil" | "ceiling" => {
            require_arity(name, values, 1)?;
            floor_ceil("ceil", &values[0])?
        }
        "mod" => {
            require_arity(name, values, 2)?;
            let a = values[0]
                .as_i64()
                .ok_or_else(|| format!("MOD expects numeric values, got `{}`", values[0]))?;
            let b = values[1]
                .as_i64()
                .ok_or_else(|| format!("MOD expects numeric values, got `{}`", values[1]))?;
            if b == 0 {
                return Err("Modulo by zero".to_string().into());
            }
            let result = a
                .checked_rem(b)
                .ok_or_else(|| format!("Integer overflow in MOD for `{a}` and `{b}`"))?;
            Value::Int(result)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, values: &[Value]) -> Result<Option<Value>, crate::error::Error> {
        eval(name, values)
    }

    #[test]
    fn mod_rejects_zero_and_overflow() {
        assert!(call("mod", &[Value::Int(10), Value::Int(0)]).is_err());
        let error = call("mod", &[Value::Int(i64::MIN), Value::Int(-1)]).unwrap_err();
        assert!(error.contains("overflow"), "got: {error}");
        assert_eq!(
            call("mod", &[Value::Int(10), Value::Int(3)]).unwrap(),
            Some(Value::Int(1))
        );
    }

    #[test]
    fn round_requires_numeric_decimals_and_stays_finite() {
        assert!(call("round", &[Value::Float(1.5), Value::Text("x".into())]).is_err());
        // Extreme decimal counts must not produce NaN.
        let huge = call("round", &[Value::Float(2.5), Value::Int(9_000_000_000)]).unwrap();
        assert_eq!(huge, Some(Value::Float(2.5)));
    }
}
