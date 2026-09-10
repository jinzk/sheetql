use super::*;
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn distinct_limit_offset() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT DISTINCT city FROM people LIMIT 2");
    assert_eq!(result.rows.len(), 2);
    let result = run(
        &mut schema,
        "SELECT name FROM people ORDER BY id LIMIT 2 OFFSET 2",
    );
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn simple_limit_skips_later_projection_errors() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT CASE WHEN id = 1 THEN 1 ELSE 1 / 0 END FROM people LIMIT 1",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn limit_zero_skips_filter_evaluation() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 1 FROM people WHERE 1 / 0 = 1 LIMIT 0");
    assert!(result.rows.is_empty());
}

#[test]
fn order_by_limit_keeps_smallest_rows() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name, age FROM people ORDER BY age LIMIT 2",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("Bob".into()), Value::Int(25)],
            vec![Value::Text("Eve".into()), Value::Int(28)],
        ]
    );
}

#[test]
fn order_by_limit_preserves_mixed_direction_and_offset() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city, age FROM people ORDER BY city ASC, age DESC LIMIT 3 OFFSET 1",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("LA".into()), Value::Int(25)],
            vec![Value::Text("NY".into()), Value::Int(40)],
            vec![Value::Text("NY".into()), Value::Int(30)],
        ]
    );
}

#[test]
fn limit_and_offset_reject_invalid_values() {
    let mut schema = make_schema();
    for sql in [
        "SELECT name FROM people LIMIT 'x'",
        "SELECT name FROM people LIMIT 1.5",
        "SELECT name FROM people LIMIT -1",
        "SELECT name FROM people LIMIT 1 OFFSET -2",
    ] {
        let error = run_query(&mut schema, sql).unwrap_err();
        assert!(
            error.contains("LIMIT") || error.contains("OFFSET"),
            "query `{sql}` should fail with a LIMIT/OFFSET error, got: {error}"
        );
    }
}