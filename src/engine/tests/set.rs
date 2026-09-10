use super::*;
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn set_operations_return_union_intersection_and_left_difference() {
    let mut schema = make_schema();
    let union = run(
        &mut schema,
        "SELECT city FROM people UNION SELECT city FROM people",
    );
    assert_eq!(union.rows.len(), 3);

    let intersection = run(
        &mut schema,
        "SELECT city FROM people INTERSECT SELECT city FROM people WHERE city = 'NY'",
    );
    assert_eq!(intersection.rows, vec![vec![Value::Text("NY".into())]]);

    let difference = run(
        &mut schema,
        "SELECT city FROM people EXCEPT SELECT city FROM people WHERE city = 'NY'",
    );
    assert!(!difference.rows.contains(&vec![Value::Text("NY".into())]));
    assert!(difference.rows.contains(&vec![Value::Text("LA".into())]));

    // Reversing EXCEPT returns the right-only region of the Venn diagram.
    let right_only = run(
        &mut schema,
        "SELECT city FROM people WHERE city = 'NY' EXCEPT SELECT city FROM people WHERE city = 'LA'",
    );
    assert_eq!(right_only.rows, vec![vec![Value::Text("NY".into())]]);
}

#[test]
fn set_operations_deduplicate_complete_rows_and_normalize_numeric_values() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 1 AS value UNION SELECT 1.0 AS value");
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);

    let result = run(
        &mut schema,
        "SELECT NULL AS value UNION SELECT NULL AS value",
    );
    assert_eq!(result.rows, vec![vec![Value::Null]]);

    let result = run(
        &mut schema,
        "SELECT city, id FROM people WHERE id = 1 UNION SELECT city, id FROM people WHERE id = 1",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.columns, vec!["city", "id"]);
}

#[test]
fn set_query_applies_outer_order_limit_and_offset() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city FROM people UNION SELECT city FROM people ORDER BY city DESC LIMIT 2 OFFSET 1",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("NY".into())],
            vec![Value::Text("LA".into())],
        ]
    );
    assert_eq!(result.stats.output_rows, 2);
}

#[test]
fn set_query_applies_branch_limit_before_union() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "(SELECT city FROM people ORDER BY city LIMIT 1) UNION SELECT city FROM people WHERE city = 'NY'",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("LA".into())],
            vec![Value::Text("NY".into())],
        ]
    );
}

#[test]
fn set_query_supports_outer_order_by_ordinal() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city, id FROM people UNION SELECT city, id FROM people ORDER BY 2 DESC LIMIT 2",
    );
    assert_eq!(result.rows[0][1], Value::Int(5));
    assert_eq!(result.rows[1][1], Value::Int(4));
}

#[test]
fn set_operations_validate_columns_and_support_union_all() {
    let mut schema = make_schema();
    let error = run_query(
        &mut schema,
        "SELECT city FROM people UNION SELECT id, name FROM people",
    )
    .unwrap_err();
    assert!(error.contains("same number of columns"), "got: {error}");
    let result = run_query(
        &mut schema,
        "SELECT city FROM people UNION ALL SELECT city FROM people",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 10);

    for operator in ["INTERSECT", "EXCEPT"] {
        let error = run_query(
            &mut schema,
            &format!("SELECT city FROM people {operator} ALL SELECT city FROM people"),
        )
        .unwrap_err();
        assert!(error.contains("ALL"), "got: {error}");
    }
}