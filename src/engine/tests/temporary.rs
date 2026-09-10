use super::*;
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn temporary_table_keeps_select_result_for_following_queries() {
    let mut schema = make_schema();
    let created = run(
        &mut schema,
        "CREATE TEMPORARY TABLE adults AS SELECT name, age FROM people WHERE age >= 30",
    );
    assert_eq!(created.rows.len(), 1);

    let result = run(
        &mut schema,
        "SELECT COUNT(*) AS count FROM adults WHERE age < 40",
    );
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
}

#[test]
fn temporary_table_is_visible_to_metadata_and_can_be_replaced() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMP TABLE selected AS SELECT name FROM people WHERE city = 'NY'",
    );
    let tables = run(&mut schema, "SHOW TABLES");
    assert!(tables.rows.contains(&vec![Value::Text("selected".into())]));

    run(
        &mut schema,
        "CREATE TEMPORARY TABLE selected AS SELECT city FROM people WHERE city = 'LA'",
    );
    let result = run(&mut schema, "SELECT * FROM selected");
    assert_eq!(result.columns, vec!["city"]);
    assert_eq!(result.rows.len(), 2);
    assert!(
        result
            .rows
            .iter()
            .all(|row| row == &vec![Value::Text("LA".into())])
    );
}

#[test]
fn temporary_table_rejects_unsupported_forms() {
    let mut schema = make_schema();
    for sql in [
        "CREATE TABLE regular AS SELECT * FROM people",
        "CREATE TEMPORARY TABLE qualified.name AS SELECT * FROM people",
    ] {
        assert!(
            run_query(&mut schema, sql).is_err(),
            "query should fail: {sql}"
        );
    }
}

#[test]
fn temporary_table_can_be_created_and_filled_with_values() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMPORARY TABLE selected (id INT, name TEXT)",
    );
    run(
        &mut schema,
        "INSERT INTO selected VALUES (1, 'Alice'), (2, 'Bob')",
    );
    let result = run(&mut schema, "SELECT id, name FROM selected ORDER BY id");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        result.rows[0],
        vec![Value::Int(1), Value::Text("Alice".into())]
    );
}

#[test]
fn temporary_table_can_be_dropped_and_recreated() {
    let mut schema = make_schema();
    run(&mut schema, "CREATE TEMPORARY TABLE scratch (id INT)");
    run(&mut schema, "INSERT INTO scratch VALUES (1)");
    run(&mut schema, "DROP TEMPORARY TABLE scratch");
    assert!(run_query(&mut schema, "SELECT * FROM scratch").is_err());
    run(&mut schema, "CREATE TEMPORARY TABLE scratch (id INT)");
    assert!(run(&mut schema, "SELECT * FROM scratch").rows.is_empty());
}

#[test]
fn drop_temporary_table_if_exists_is_idempotent() {
    let mut schema = make_schema();
    run(&mut schema, "DROP TEMPORARY TABLE IF EXISTS missing");
    assert!(run_query(&mut schema, "DROP TEMPORARY TABLE missing").is_err());
}

#[test]
fn temporary_table_insert_coerces_declared_types() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMPORARY TABLE typed (id INT, active BOOLEAN)",
    );
    run(&mut schema, "INSERT INTO typed VALUES ('7', 'true')");
    assert_eq!(
        run(&mut schema, "SELECT * FROM typed").rows,
        vec![vec![Value::Int(7), Value::Bool(true)]]
    );
}

#[test]
fn temporary_table_insert_is_atomic_on_conversion_error() {
    let mut schema = make_schema();
    run(&mut schema, "CREATE TEMPORARY TABLE typed (id INT)");
    assert!(run_query(&mut schema, "INSERT INTO typed VALUES (1), ('bad')").is_err());
    assert!(run(&mut schema, "SELECT * FROM typed").rows.is_empty());
}

#[test]
fn temporary_table_update_is_atomic_and_uses_current_row_values() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMPORARY TABLE work (id INT, label TEXT)",
    );
    run(&mut schema, "INSERT INTO work VALUES (1, 'a'), (2, 'b')");
    run(
        &mut schema,
        "UPDATE work SET id = id + 10, label = 'updated' WHERE id = 1",
    );
    assert_eq!(
        run(&mut schema, "SELECT * FROM work ORDER BY id").rows,
        vec![
            vec![Value::Int(2), Value::Text("b".into())],
            vec![Value::Int(11), Value::Text("updated".into())],
        ]
    );
    assert!(run_query(&mut schema, "UPDATE work SET id = 'bad'").is_err());
    assert_eq!(
        run(&mut schema, "SELECT id FROM work ORDER BY id").rows[0][0],
        Value::Int(2)
    );
}

#[test]
fn temporary_table_delete_filters_rows_and_can_delete_all() {
    let mut schema = make_schema();
    run(&mut schema, "CREATE TEMPORARY TABLE work (id INT)");
    run(&mut schema, "INSERT INTO work VALUES (1), (2), (3)");
    run(&mut schema, "DELETE FROM work WHERE id = 2");
    assert_eq!(
        run(&mut schema, "SELECT id FROM work ORDER BY id")
            .rows
            .len(),
        2
    );
    run(&mut schema, "DELETE FROM work");
    assert!(run(&mut schema, "SELECT * FROM work").rows.is_empty());
}

#[test]
fn temporary_table_truncate_keeps_schema_and_allows_reinsert() {
    let mut schema = make_schema();
    run(&mut schema, "CREATE TEMPORARY TABLE work (id INT)");
    run(&mut schema, "INSERT INTO work VALUES (1), (2)");
    run(&mut schema, "TRUNCATE TABLE work");
    assert!(run(&mut schema, "SELECT * FROM work").rows.is_empty());
    run(&mut schema, "INSERT INTO work VALUES (3)");
    assert_eq!(
        run(&mut schema, "SELECT id FROM work").rows[0][0],
        Value::Int(3)
    );
}

#[test]
fn temporary_table_supports_complete_analysis_workflow() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMPORARY TABLE work (id INT, label TEXT)",
    );
    run(&mut schema, "INSERT INTO work VALUES (1, 'a'), (2, 'b')");
    run(&mut schema, "UPDATE work SET label = 'x' WHERE id = 1");
    run(&mut schema, "DELETE FROM work WHERE id = 2");
    assert_eq!(
        run(&mut schema, "SELECT * FROM work").rows,
        vec![vec![Value::Int(1), Value::Text("x".into())]]
    );
    run(&mut schema, "TRUNCATE TABLE work");
    run(&mut schema, "DROP TEMPORARY TABLE work");
    assert!(run_query(&mut schema, "SELECT * FROM work").is_err());
}

#[test]
fn temporary_dml_reports_input_and_affected_row_counts() {
    let mut schema = make_schema();
    let created = run(&mut schema, "CREATE TEMPORARY TABLE work (id INT)");
    assert_eq!(created.stats.input_rows, 0);
    let inserted = run(&mut schema, "INSERT INTO work VALUES (1), (2)");
    assert_eq!(inserted.stats.input_rows, 2);
    assert_eq!(inserted.stats.affected_rows, 2);
    let updated = run(&mut schema, "UPDATE work SET id = id + 1 WHERE id = 1");
    assert_eq!(updated.stats.input_rows, 2);
    assert_eq!(updated.stats.affected_rows, 1);
    let deleted = run(&mut schema, "DELETE FROM work WHERE id = 2");
    assert_eq!(deleted.stats.input_rows, 2);
    assert_eq!(deleted.stats.affected_rows, 2);
}

#[test]
fn temporary_table_supports_insert_select_and_target_columns() {
    let mut schema = make_schema();
    run(
        &mut schema,
        "CREATE TEMPORARY TABLE copied (id INT, name TEXT, note TEXT)",
    );
    run(
        &mut schema,
        "INSERT INTO copied (name, id) SELECT name, id FROM people WHERE id <= 2",
    );
    let result = run(&mut schema, "SELECT id, name, note FROM copied ORDER BY id");
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(1), Value::Text("Alice".into()), Value::Null],
            vec![Value::Int(2), Value::Text("Bob".into()), Value::Null],
        ]
    );
}