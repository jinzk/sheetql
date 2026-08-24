mod aggregate;
mod date;
mod json;
mod math;
mod null;
mod string;
mod url;

use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments};

use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::value::Value;

pub use aggregate::{AGGREGATE_FUNCTIONS, contains_aggregate};

pub(crate) use math::floor_ceil;

/// The arguments of a function call, preserving the distinction between
/// `COUNT(*)`, plain arguments and `DISTINCT` arguments.
pub enum FnArgs {
    Star,
    All(Vec<Expr>),
    Distinct(Vec<Expr>),
}

pub fn parse_function_args(args: &FunctionArguments) -> Result<FnArgs, Error> {
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
                    _ => return Err("Unsupported function argument".to_string().into()),
                }
            }
            match list.duplicate_treatment {
                Some(sqlparser::ast::DuplicateTreatment::Distinct) => Ok(FnArgs::Distinct(exprs)),
                _ => Ok(FnArgs::All(exprs)),
            }
        }
        FunctionArguments::Subquery(_) => Err("Subquery function arguments are not supported"
            .to_string()
            .into()),
    }
}

pub fn eval_function(
    ctx: &EvalContext,
    func: &Function,
    current: &[Value],
) -> Result<Value, Error> {
    let name = func.name.to_string().to_lowercase();
    if func.over.is_some() {
        return Err(format!("Window function `{name}` is not supported yet").into());
    }
    let args = parse_function_args(&func.args)?;

    if AGGREGATE_FUNCTIONS.contains(&name.as_str()) {
        return aggregate::eval(ctx, &name, &args);
    }

    let values = eval_scalar_args(ctx, &args, current)?;

    if let Some(value) = null::eval(&name, &values)? {
        return Ok(value);
    }
    if let Some(value) = string::eval(&name, &values)? {
        return Ok(value);
    }
    if let Some(value) = math::eval(&name, &values)? {
        return Ok(value);
    }
    if let Some(value) = date::eval(&name, &values, &ctx.now)? {
        return Ok(value);
    }
    if let Some(value) = json::eval(&name, &values)? {
        return Ok(value);
    }
    if let Some(value) = url::eval(&name, &values)? {
        return Ok(value);
    }

    Err(format!("Unknown function `{name}`").into())
}

/// Evaluate every argument expression to a value. Wildcards are rejected, so
/// this is only used for scalar (non-aggregate) functions.
fn eval_scalar_args(
    ctx: &EvalContext,
    args: &FnArgs,
    current: &[Value],
) -> Result<Vec<Value>, Error> {
    let exprs = match args {
        FnArgs::Star => return Err("Wildcard is not allowed here".to_string().into()),
        FnArgs::All(exprs) | FnArgs::Distinct(exprs) => exprs,
    };
    let mut values = vec![];
    for expr in exprs {
        values.push(eval_expr(ctx, expr, current)?);
    }
    Ok(values)
}

pub(crate) fn require_arity(name: &str, values: &[Value], expected: usize) -> Result<(), Error> {
    if values.len() != expected {
        return Err(format!(
            "Function `{name}` expects {expected} argument(s), got {}",
            values.len()
        )
        .into());
    }
    Ok(())
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

    fn scalar_result(expr: &str) -> Result<Value, Error> {
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
    fn date_without_arguments_returns_today() {
        let Value::Date(text) = scalar("DATE()") else {
            panic!("expected a date value");
        };
        assert_eq!(text.len(), 10, "got: {text}");
    }

    #[test]
    fn math_function_details() {
        assert_eq!(scalar("FLOOR(3.7)"), Value::Float(3.0));
        assert_eq!(scalar("FLOOR(-3.7)"), Value::Float(-4.0));
        assert_eq!(scalar("CEIL(3.2)"), Value::Float(4.0));
        assert_eq!(scalar("CEILING(3.2)"), Value::Float(4.0));
        assert_eq!(scalar("CEIL(-3.2)"), Value::Float(-3.0));
        assert!(scalar_result("FLOOR(3.7, 2)").is_err());
        assert!(scalar_result("FLOOR(3.7 TO HOUR)").is_err());
        assert!(scalar_result("FLOOR('x')").is_err());
        assert_eq!(scalar("POWER(2, 3)"), Value::Float(8.0));
        assert_eq!(scalar("POW(2, 3)"), Value::Float(8.0));
        assert_eq!(scalar("SQRT(9)"), Value::Float(3.0));
        assert!(scalar_result("SQRT(-1)").is_err());
        assert_eq!(scalar("GREATEST(1, 9, 4)"), Value::Int(9));
        assert_eq!(scalar("LEAST(1, 9, 4)"), Value::Int(1));
        assert_eq!(scalar("GREATEST(1, NULL, 4)"), Value::Null);
        assert_eq!(scalar("ROUND(2.5)"), Value::Float(3.0));
        assert_eq!(scalar("ABS(-5)"), Value::Int(5));
        assert_eq!(scalar("MOD(10, 3)"), Value::Int(1));
    }

    #[test]
    fn string_function_details() {
        assert_eq!(scalar("LEFT('Hello', 2)"), Value::Text("He".into()));
        assert_eq!(scalar("RIGHT('Hello', 2)"), Value::Text("lo".into()));
        assert_eq!(scalar("INSTR('Hello', 'll')"), Value::Int(3));
        assert_eq!(scalar("INSTR('Hello', 'zz')"), Value::Int(0));
        assert_eq!(scalar("CONCAT('a', NULL, 'c')"), Value::Null);
        assert_eq!(
            scalar("SUBSTRING('SheetQL', 1)"),
            Value::Text("SheetQL".into())
        );
        assert!(scalar_result("SUBSTRING('x', 1, 2, 3)").is_err());
        assert_eq!(scalar("LENGTH('日本語')"), Value::Int(3));
        assert_eq!(scalar("TRIM('  x  ')"), Value::Text("x".into()));
        assert_eq!(scalar("LTRIM('  x')"), Value::Text("x".into()));
        assert_eq!(scalar("RTRIM('x  ')"), Value::Text("x".into()));
    }

    #[test]
    fn null_functions_semantics() {
        assert_eq!(scalar("IFNULL(NULL, 3)"), Value::Int(3));
        assert_eq!(scalar("IFNULL(5, 0)"), Value::Int(5));
        assert_eq!(scalar("ISNULL(NULL)"), Value::Bool(true));
        assert_eq!(scalar("ISNULL(5)"), Value::Bool(false));
        assert!(scalar_result("ISNULL(1, 2)").is_err());
        assert_eq!(scalar("COALESCE(NULL, 1)"), Value::Int(1));
        assert_eq!(scalar("COALESCE(NULL, NULL)"), Value::Null);
        assert_eq!(scalar("COALESCE(NULL, 'x', 2)"), Value::Text("x".into()));
        assert!(scalar_result("IFNULL(1, 2, 3)").is_err());
    }

    #[test]
    fn json_scalar_and_non_scalar_paths() {
        let json = r#"'{"a":{"b":1},"arr":[10,20]}'"#;
        assert_eq!(
            scalar(&format!("JSON_QUERY({json}, '$.arr')")),
            Value::Json(serde_json::json!([10, 20]))
        );
        assert!(scalar_result(&format!("JSON_VALUE({json}, '$.a')")).is_err());
        assert_eq!(
            scalar(&format!("JSON_VALUE({json}, '$.a.b')")),
            Value::Int(1)
        );
        assert_eq!(
            scalar(&format!("JSON_VALUE({json}, '$.arr[1]')")),
            Value::Int(20)
        );
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
