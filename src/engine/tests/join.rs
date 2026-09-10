use super::*;
use crate::database::{Database, Table};
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn inner_join_matches_rows() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT p.name, o.amount FROM people AS p JOIN orders AS o ON p.id = o.customer_id",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn inner_join_hashes_reversed_and_multiple_equality_keys() {
    let mut database = Database::named("test");
    database.add_table(Table {
        name: "left_table".to_string(),
        columns: vec!["id".to_string(), "kind".to_string(), "value".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Text("a".into()), Value::Int(10)],
            vec![Value::Int(1), Value::Text("b".into()), Value::Int(20)],
            vec![Value::Int(2), Value::Text("a".into()), Value::Int(30)],
        ],
    });
    database.add_table(Table {
        name: "right_table".to_string(),
        columns: vec!["id".to_string(), "kind".to_string(), "label".to_string()],
        rows: vec![
            vec![
                Value::Int(1),
                Value::Text("b".into()),
                Value::Text("B".into()),
            ],
            vec![
                Value::Int(1),
                Value::Text("a".into()),
                Value::Text("A".into()),
            ],
            vec![
                Value::Int(3),
                Value::Text("a".into()),
                Value::Text("C".into()),
            ],
        ],
    });
    let mut schema = Schema::new();
    schema.add_database(database);
    schema.set_current_database("test").unwrap();

    let result = run(
        &mut schema,
        "SELECT l.value, r.label FROM left_table l JOIN right_table r \
         ON r.id = l.id AND l.kind = r.kind ORDER BY l.value",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(10), Value::Text("A".into())],
            vec![Value::Int(20), Value::Text("B".into())],
        ]
    );
}

#[test]
fn left_join_keeps_unmatched_left_rows() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT p.name FROM people p LEFT JOIN orders o ON p.id = o.customer_id",
    );
    // Alice has 2 orders, Bob has 1; Carol, Dan and Eve have none, so 3 matched
    // pairs plus 3 unmatched left rows.
    assert_eq!(result.rows.len(), 6);
}

#[test]
fn right_join_keeps_unmatched_right_rows() {
    let mut database = Database::named("test");
    database.add_table(Table {
        name: "people".to_string(),
        columns: vec!["id".to_string(), "name".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Text("Alice".into())],
            vec![Value::Int(2), Value::Text("Bob".into())],
        ],
    });
    database.add_table(Table {
        name: "orders".to_string(),
        columns: vec!["order_id".to_string(), "customer_id".to_string()],
        rows: vec![
            vec![Value::Int(101), Value::Int(1)],
            vec![Value::Int(102), Value::Int(99)],
        ],
    });
    let mut schema = Schema::new();
    schema.add_database(database);
    schema.set_current_database("test").unwrap();

    let result = run(
        &mut schema,
        "SELECT p.name, o.order_id FROM people p RIGHT JOIN orders o ON p.id = o.customer_id",
    );
    // Alice matches order 101; order 102 has no matching person and is kept with NULL.
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        result.rows[0],
        vec![Value::Text("Alice".into()), Value::Int(101)]
    );
    assert_eq!(result.rows[1], vec![Value::Null, Value::Int(102)]);
}

#[test]
fn using_join_matches_on_shared_column() {
    let mut schema = make_schema();
    let mut regions_db = Database::named("regions_db");
    regions_db.add_table(Table {
        name: "regions".to_string(),
        columns: vec!["id".to_string(), "region".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Text("East".into())],
            vec![Value::Int(3), Value::Text("West".into())],
        ],
    });
    schema.add_database(regions_db);
    let result = run(
        &mut schema,
        "SELECT p.name, r.region FROM people p INNER JOIN regions r USING (id)",
    );
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn using_join_keeps_outer_join_rows_with_hash_lookup() {
    let mut schema = make_schema();
    let mut regions_db = Database::named("regions_db");
    regions_db.add_table(Table {
        name: "regions".to_string(),
        columns: vec!["id".to_string(), "region".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Text("East".into())],
            vec![Value::Int(3), Value::Text("West".into())],
        ],
    });
    schema.add_database(regions_db);
    let result = run(
        &mut schema,
        "SELECT p.name, r.region FROM people p LEFT JOIN regions r USING (id)",
    );
    assert_eq!(result.rows.len(), 5);
    assert_eq!(
        result.rows[0],
        vec![Value::Text("Alice".into()), Value::Text("East".into())]
    );
    assert_eq!(
        result.rows[1],
        vec![Value::Text("Carol".into()), Value::Text("West".into())]
    );
    assert_eq!(result.rows[2][1], Value::Null);
}

#[test]
fn cross_join_with_comma_from() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT p.name, o.order_id FROM people p, orders o WHERE p.id = 1",
    );
    assert_eq!(result.rows.len(), 3);
}

#[test]
fn equi_join_never_matches_null_keys() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("nulls"));
    schema.set_current_database("nulls").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "nulls")
        .unwrap();
    database.add_table(Table {
        name: "left_table".to_string(),
        columns: vec!["key".to_string(), "label".to_string()],
        rows: vec![
            vec![Value::Null, Value::Text("left-null".into())],
            vec![Value::Int(1), Value::Text("left-one".into())],
        ],
    });
    database.add_table(Table {
        name: "right_table".to_string(),
        columns: vec!["key".to_string(), "label".to_string()],
        rows: vec![
            vec![Value::Null, Value::Text("right-null".into())],
            vec![Value::Int(1), Value::Text("right-one".into())],
        ],
    });

    // The hash path must not pair the two NULL keys together.
    let error_free = run(
        &mut schema,
        "SELECT l.label, r.label FROM left_table l JOIN right_table r ON l.key = r.key",
    );
    assert_eq!(error_free.rows.len(), 1);
    assert_eq!(
        error_free.rows[0],
        vec![
            Value::Text("left-one".into()),
            Value::Text("right-one".into())
        ]
    );

    // With an outer join the unmatched NULL-keyed left row survives with
    // a NULL right side (anti-join pattern), but is never paired with the
    // NULL-keyed right row.
    let outer = run(
        &mut schema,
        "SELECT l.label, r.label FROM left_table l LEFT JOIN right_table r ON l.key = r.key \
         ORDER BY l.label",
    );
    assert_eq!(outer.rows.len(), 2);
    let null_row = outer
        .rows
        .iter()
        .find(|row| row[0] == Value::Text("left-null".into()))
        .expect("unmatched left row should survive the outer join");
    assert_eq!(null_row[1], Value::Null);

    // The nested-loop path agrees with the hash path.
    let result = run(
        &mut schema,
        "SELECT l.label FROM left_table l JOIN right_table r \
         ON l.key = r.key AND l.label < 'z'",
    );
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn where_predicate_is_pushed_onto_join_sides() {
    let mut schema = make_schema();
    // WHERE filters on the right side (orders.amount) and a join condition
    // keep behavior identical whether or not the predicate is pushed.
    let pushed = run(
        &mut schema,
        "SELECT p.name, o.amount FROM people p \
         INNER JOIN orders o ON p.id = o.customer_id \
         WHERE o.amount > 60.0 AND p.city = 'NY' ORDER BY p.name",
    );
    // orders with amount > 60: only order 103 (customer 1, 99.9), customer
    // 1 = Alice (NY).
    assert_eq!(
        pushed.rows,
        vec![vec![Value::Text("Alice".into()), Value::Float(99.9),]]
    );
}

#[test]
fn where_predicate_pushdown_never_changes_outer_join_results() {
    let mut schema = make_schema();
    // A LEFT JOIN must still produce the unmatched preserved row even
    // though a WHERE predicate mentions only the left side.
    let result = run(
        &mut schema,
        "SELECT p.name, o.amount FROM people p \
         LEFT JOIN orders o ON p.id = o.customer_id \
         WHERE p.city = 'SF' ORDER BY p.name",
    );
    // SF is Carol; she has no order -> null amount preserved once.
    assert_eq!(
        result.rows,
        vec![vec![Value::Text("Carol".into()), Value::Null]]
    );
}

#[test]
fn non_equality_join_uses_predicate_fallback() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT p.name, o.amount FROM people p \
         JOIN orders o ON p.id < o.customer_id ORDER BY p.name, o.amount",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::Text("Alice".into()), Value::Float(20.0)],]
    );
}