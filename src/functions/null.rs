use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::value::Value;

use super::FnArgs;
use super::require_arity;
use super::require_arity_len;

/// NULL-handling scalar functions (`IFNULL`, `ISNULL`, `COALESCE`).
///
/// `ISNULL(expr)` follows MySQL semantics and reports whether the single
/// argument is `NULL`; `IFNULL(expr1, expr2)` returns the first non-`NULL`
/// argument.
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "isnull" => {
            require_arity(name, values, 1)?;
            Value::Bool(values[0].is_null())
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Lazily evaluated NULL-coalescing functions. Their arguments are only
/// evaluated until the result is decided, so later arguments never observe
/// errors from earlier ones being computed needlessly
/// (e.g. `COALESCE(1, 1/0)` is 1, not a division-by-zero error).
pub(crate) fn eval_lazy<'a>(
    ctx: &EvalContext,
    name: &str,
    args: &FnArgs<'a>,
    current: &[Value],
) -> Result<Option<Value>, Error> {
    let args = match args {
        // Wildcards are never valid for these functions; report via the eager
        // path so the standard "Wildcard is not allowed here" error surfaces.
        FnArgs::Star => return Ok(None),
        FnArgs::All(args) | FnArgs::Distinct(args) => *args,
    };
    let value = match name {
        "ifnull" => {
            require_arity_len(name, args.len(), 2)?;
            let first = eval_expr(ctx, super::function_arg_expr(&args[0])?, current)?;
            if !first.is_null() {
                first
            } else {
                eval_expr(ctx, super::function_arg_expr(&args[1])?, current)?
            }
        }
        "coalesce" => {
            let mut result = Value::Null;
            for arg in args {
                let value = eval_expr(ctx, super::function_arg_expr(arg)?, current)?;
                if !value.is_null() {
                    result = value;
                    break;
                }
            }
            result
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}
