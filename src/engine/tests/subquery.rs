use super::*;
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn scalar_subquery_returns_value_or_null() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT (SELECT MAX(age) FROM people) AS max_age",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(40)]]);

    let result = run(
        &mut schema,
        "SELECT (SELECT age FROM people WHERE id = 999) AS missing",
    );
    assert_eq!(result.rows, vec![vec![Value::Null]]);
}

#[test]
fn in_and_exists_subqueries_work_with_null_semantics() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT id FROM people WHERE id IN (SELECT customer_id FROM orders) ORDER BY id",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    let result = run(
        &mut schema,
        "SELECT 1 WHERE EXISTS (SELECT 1 FROM people WHERE FALSE)",
    );
    assert!(result.rows.is_empty());

    let result = run(
        &mut schema,
        "SELECT 3 IN (SELECT NULL) AS unknown_membership",
    );
    assert_eq!(result.rows, vec![vec![Value::Null]]);
}

#[test]
fn scalar_subquery_rejects_multiple_rows_and_columns() {
    let mut schema = make_schema();
    assert!(run_query(&mut schema, "SELECT (SELECT age FROM people)").is_err());
    assert!(run_query(&mut schema, "SELECT (SELECT id, age FROM people LIMIT 1)").is_err());
}

#[test]
fn cte_is_materialized_in_query_scope() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "WITH adults AS (SELECT name, age FROM people WHERE age >= 30) SELECT name FROM adults ORDER BY name",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("Alice".into())],
            vec![Value::Text("Carol".into())],
            vec![Value::Text("Dan".into())],
        ]
    );
    assert!(schema.get_temporary_table("adults").is_none());
}

#[test]
fn ctes_can_reference_previous_ctes_and_rename_columns() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "WITH adults (person, years) AS (SELECT name, age FROM people WHERE age >= 35), seniors AS (SELECT person FROM adults) SELECT person FROM seniors",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("Carol".into())],
            vec![Value::Text("Dan".into())]
        ]
    );
}

#[test]
fn derived_table_can_be_used_as_a_relation() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name FROM (SELECT name FROM people WHERE age >= 35) AS adults ORDER BY name",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("Carol".into())],
            vec![Value::Text("Dan".into())],
        ]
    );
}

#[test]
fn correlated_exists_subquery_uses_outer_row_scope() {
    let mut schema = make_schema();
    let result = run_query(
        &mut schema,
        "SELECT p.name FROM people p WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = p.id)",
    )
    .unwrap();
    assert!(
        result.rows
            == vec![
                vec![Value::Text("Alice".into())],
                vec![Value::Text("Bob".into())],
            ],
        "got: {:?}",
        result.rows
    );
}

#[test]
fn correlated_scalar_and_in_subqueries_use_outer_row_scope() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT p.name, (SELECT MAX(amount) FROM orders o WHERE o.customer_id = p.id) AS max_amount FROM people p ORDER BY p.id",
    );
    assert_eq!(
        result.rows[0],
        vec![Value::Text("Alice".into()), Value::Float(99.9)]
    );
    assert_eq!(
        result.rows[1],
        vec![Value::Text("Bob".into()), Value::Float(20.0)]
    );
    assert_eq!(result.rows[2][1], Value::Null);
    assert!(result.stats.correlated_cache_misses >= 3);

    let result = run(
        &mut schema,
        "SELECT p.id FROM people p WHERE p.id IN (SELECT o.customer_id FROM orders o WHERE o.customer_id = p.id) ORDER BY p.id",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    let result = run(
        &mut schema,
        "SELECT p.id, (SELECT MAX(amount) FROM orders o WHERE o.customer_id = p.id) FROM people p JOIN orders x ON x.customer_id = p.id ORDER BY p.id",
    );
    assert_eq!(result.rows.len(), 3);
    assert!(result.stats.correlated_cache_hits >= 1);
}