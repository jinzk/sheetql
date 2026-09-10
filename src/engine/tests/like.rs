use crate::engine::tests::{make_schema, run};
use crate::evaluator::like_match;
use crate::value::Value;

#[test]
fn like_match_supports_wildcards() {
    assert!(like_match("abc", "a%", false, None));
    assert!(like_match("abc", "%c", false, None));
    assert!(like_match("abc", "a_c", false, None));
    assert!(!like_match("ab", "a_c", false, None));
    assert!(!like_match("xyz", "a%", false, None));
    assert!(like_match("anything", "%", false, None));
    assert!(like_match("", "", false, None));
}

#[test]
fn like_escape_is_supported() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 'a%b' LIKE 'a!%b' ESCAPE '!'");
    assert_eq!(result.rows[0][0], Value::Bool(true));
    let result = run(&mut schema, "SELECT 'axb' LIKE 'a!%b' ESCAPE '!'");
    assert_eq!(result.rows[0][0], Value::Bool(false));
    let result = run(&mut schema, "SELECT 'a_b' LIKE 'a!_b' ESCAPE '!'");
    assert_eq!(result.rows[0][0], Value::Bool(true));
    let result = run(&mut schema, "SELECT '100%' LIKE '%!%' ESCAPE '!'");
    assert_eq!(result.rows[0][0], Value::Bool(true));
}

#[test]
fn like_with_null_operand_returns_null() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT NULL LIKE '%x'");
    assert_eq!(result.rows[0][0], Value::Null);
    let result = run(&mut schema, "SELECT 'x' LIKE NULL");
    assert_eq!(result.rows[0][0], Value::Null);
    let result = run(&mut schema, "SELECT NULL ILIKE '%x'");
    assert_eq!(result.rows[0][0], Value::Null);
}