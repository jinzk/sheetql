use super::*;
use crate::database::{Database, Table};
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn row_number_and_rank_work_over_partitions() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name, ROW_NUMBER() OVER (PARTITION BY city ORDER BY age) AS rn, \
         RANK() OVER (PARTITION BY city ORDER BY age) AS rk FROM people \
         WHERE city = 'NY' OR city = 'LA' ORDER BY city, rn",
    );
    // NY: Alice(30) rn=1, Dan(40) rn=2
    // LA: Bob(25) rn=1, Eve(28) rn=2
    assert_eq!(result.rows.len(), 4);
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("Bob".into()), Value::Int(1), Value::Int(1)],
            vec![Value::Text("Eve".into()), Value::Int(2), Value::Int(2)],
            vec![Value::Text("Alice".into()), Value::Int(1), Value::Int(1)],
            vec![Value::Text("Dan".into()), Value::Int(2), Value::Int(2)],
        ]
    );
}

#[test]
fn sum_over_partition_is_computed_per_partition() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name, age, SUM(age) OVER (PARTITION BY city) AS city_total \
         FROM people WHERE city IN ('NY', 'LA') ORDER BY name",
    );
    // NY total = 30 + 40 = 70, LA total = 25 + 28 = 53
    assert_eq!(result.rows.len(), 4);
    for row in &result.rows {
        let city_total = row[2].clone();
        match row[0].as_text() {
            Some(name) if name == "Alice" || name == "Dan" => {
                assert_eq!(city_total, Value::Int(70))
            }
            Some(name) if name == "Bob" || name == "Eve" => {
                assert_eq!(city_total, Value::Int(53))
            }
            _ => panic!("unexpected row: {row:?}"),
        }
    }
}

#[test]
fn window_function_rejects_group_by_combination() {
    let mut schema = make_schema();
    assert!(
        run_query(
            &mut schema,
            "SELECT city, COUNT(*) OVER () FROM people GROUP BY city"
        )
        .is_err()
    );
}

#[test]
fn count_star_over_and_order_by_window_reference() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name, COUNT(*) OVER () AS total \
         FROM people WHERE city = 'NY' ORDER BY name",
    );
    // Two NY rows -> total = 2 for every row.
    assert_eq!(result.rows.len(), 2);
    for row in &result.rows {
        assert_eq!(row[1], Value::Int(2));
    }

    // ORDER BY can reference a window function that is not projected.
    let result = run(
        &mut schema,
        "SELECT name FROM people \
         ORDER BY ROW_NUMBER() OVER (PARTITION BY city ORDER BY age)",
    );
    assert_eq!(result.rows.len(), 5);
}

#[test]
fn distinct_window_aggregates_use_distinct_values() {
    let mut database = Database::named("test");
    database.add_table(Table {
        name: "nums".to_string(),
        columns: vec!["n".to_string()],
        rows: vec![
            vec![Value::Int(10)],
            vec![Value::Int(10)],
            vec![Value::Int(20)],
            vec![Value::Int(20)],
            vec![Value::Int(30)],
        ],
    });
    let mut schema = Schema::new();
    schema.add_database(database);
    schema.set_current_database("test").unwrap();

    // Distinct values are {10, 20, 30}: COUNT = 3, SUM = 60, AVG = 20. The
    // non-distinct values would give COUNT = 5, SUM = 90, AVG = 18.
    let result = run(
        &mut schema,
        "SELECT n, COUNT(DISTINCT n) OVER () AS c, SUM(DISTINCT n) OVER () AS s, \
         AVG(DISTINCT n) OVER () AS a FROM nums",
    );
    assert_eq!(result.rows.len(), 5);
    for row in &result.rows {
        assert_eq!(row[1], Value::Int(3));
        assert_eq!(row[2], Value::Int(60));
        assert_eq!(row[3], Value::Float(20.0));
    }

    // The grouped aggregate path agrees with the window path.
    let grouped = run(
        &mut schema,
        "SELECT COUNT(DISTINCT n), SUM(DISTINCT n), AVG(DISTINCT n) FROM nums",
    );
    assert_eq!(grouped.rows[0][0], Value::Int(3));
    assert_eq!(grouped.rows[0][1], Value::Int(60));
    assert_eq!(grouped.rows[0][2], Value::Float(20.0));
}