use crate::error::Error;
use crate::value::Value;

use super::require_arity;

/// NULL-handling scalar functions (`IFNULL`, `ISNULL`, `COALESCE`).
///
/// `ISNULL(expr)` follows MySQL semantics and reports whether the single
/// argument is `NULL`; `IFNULL(expr1, expr2)` returns the first non-`NULL`
/// argument.
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "ifnull" => {
            require_arity(name, values, 2)?;
            if values[0].is_null() {
                values[1].clone()
            } else {
                values[0].clone()
            }
        }
        "isnull" => {
            require_arity(name, values, 1)?;
            Value::Bool(values[0].is_null())
        }
        "coalesce" => {
            for value in values {
                if !value.is_null() {
                    return Ok(Some(value.clone()));
                }
            }
            Value::Null
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}
