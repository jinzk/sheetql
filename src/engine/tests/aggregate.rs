use super::*;
use crate::database::{Database, Table};
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn group_by_having_aggregates() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city, COUNT(*) AS cnt FROM people GROUP BY city HAVING COUNT(*) > 1",
    );
    assert_eq!(result.columns, vec!["city".to_string(), "cnt".to_string()]);
    assert_eq!(result.rows.len(), 2);
    let sum: i64 = result.rows.iter().map(|row| row[1].as_i64().unwrap()).sum();
    assert_eq!(sum, 4);
}

#[test]
fn aggregate_sum_over_column() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT SUM(amount) AS total FROM orders");
    assert_eq!(result.rows[0][0], Value::Float(170.4));
}

#[test]
fn multiple_aggregates_share_argument_summary() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT COUNT(age + 1), SUM(age + 1), AVG(age + 1), MIN(age + 1), MAX(age + 1) FROM people",
    );
    assert_eq!(
        result.rows[0],
        vec![
            Value::Int(5),
            Value::Int(163),
            Value::Float(32.6),
            Value::Int(26),
            Value::Int(41),
        ]
    );
}

#[test]
fn grouped_aggregate_state_isolated_per_group() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city, SUM(age + 1) AS total FROM people GROUP BY city ORDER BY city",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("LA".into()), Value::Int(55)],
            vec![Value::Text("NY".into()), Value::Int(72)],
            vec![Value::Text("SF".into()), Value::Int(36)],
        ]
    );
}

#[test]
fn sum_avg_distinct_ignore_duplicates() {
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

    let result = run(
        &mut schema,
        "SELECT SUM(n) AS s, SUM(DISTINCT n) AS d, AVG(DISTINCT n) AS a FROM nums",
    );
    assert_eq!(result.rows[0][0], Value::Int(90));
    assert_eq!(result.rows[0][1], Value::Int(60));
    assert_eq!(result.rows[0][2], Value::Float(20.0));
}

#[test]
fn order_by_aggregate_without_projection_aggregate() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT city FROM people GROUP BY city ORDER BY COUNT(*) DESC",
    );
    let cities: Vec<String> = result
        .rows
        .iter()
        .map(|row| row[0].to_display_string())
        .collect();
    assert_eq!(cities.len(), 3);
    assert_eq!(cities[0], "NY"); // 2 rows, largest count first
}

#[test]
fn group_by_unifies_int_and_float() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("mixed"));
    schema.set_current_database("mixed").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "mixed")
        .unwrap();
    database.add_table(Table {
        name: "t".to_string(),
        columns: vec!["k".to_string()],
        rows: vec![
            vec![Value::Int(1)],
            vec![Value::Float(1.0)],
            vec![Value::Float(2.5)],
            vec![Value::Int(2)],
        ],
    });
    let result = run(
        &mut schema,
        "SELECT k, COUNT(*) AS cnt FROM t GROUP BY k ORDER BY k",
    );
    assert_eq!(
        result.rows.len(),
        3,
        "Int(1) and Float(1.0) share one group"
    );
    let total: i64 = result.rows.iter().map(|row| row[1].as_i64().unwrap()).sum();
    assert_eq!(total, 4);
    assert_eq!(result.rows[0][1].as_i64(), Some(2));
}

#[test]
fn count_distinct_unifies_int_and_float() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("mixed"));
    schema.set_current_database("mixed").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "mixed")
        .unwrap();
    database.add_table(Table {
        name: "t".to_string(),
        columns: vec!["v".to_string()],
        rows: vec![
            vec![Value::Int(1)],
            vec![Value::Float(1.0)],
            vec![Value::Float(2.0)],
        ],
    });
    let result = run(&mut schema, "SELECT COUNT(DISTINCT v) AS n FROM t");
    assert_eq!(result.rows, vec![vec![Value::Int(2)]]);
}

#[test]
fn abs_of_min_is_an_error_not_a_panic() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("extreme"));
    schema.set_current_database("extreme").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "extreme")
        .unwrap();
    database.add_table(Table {
        name: "t".to_string(),
        columns: vec!["v".to_string()],
        rows: vec![vec![Value::Int(i64::MIN)]],
    });
    let error = run_query(&mut schema, "SELECT ABS(v) FROM t").unwrap_err();
    assert!(error.contains("overflow"), "got: {error}");
}

#[test]
fn sum_overflow_is_an_error_not_a_panic() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("extreme"));
    schema.set_current_database("extreme").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "extreme")
        .unwrap();
    database.add_table(Table {
        name: "t".to_string(),
        columns: vec!["v".to_string()],
        rows: vec![vec![Value::Int(i64::MAX)], vec![Value::Int(1)]],
    });
    let error = run_query(&mut schema, "SELECT SUM(v) FROM t").unwrap_err();
    assert!(error.contains("overflow"), "got: {error}");
}

#[test]
fn group_by_and_order_by_accept_ordinals() {
    let mut schema = make_schema();
    // GROUP BY 1 groups by the first output column (city).
    let result = run(
        &mut schema,
        "SELECT city, COUNT(*) AS cnt FROM people GROUP BY 1 ORDER BY 1",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Text("LA".into()), Value::Int(2)],
            vec![Value::Text("NY".into()), Value::Int(2)],
            vec![Value::Text("SF".into()), Value::Int(1)],
        ]
    );

    // ORDER BY 2 sorts by the second output column.
    let result = run(
        &mut schema,
        "SELECT city, id FROM people ORDER BY 2 DESC LIMIT 1",
    );
    assert_eq!(result.rows[0][1], Value::Int(5));

    // Out-of-range ordinals are rejected instead of silently ignored.
    let error = run_query(&mut schema, "SELECT city FROM people ORDER BY 9").unwrap_err();
    assert!(error.contains("not in the select list"), "got: {error}");
    let error = run_query(&mut schema, "SELECT city FROM people GROUP BY 4").unwrap_err();
    assert!(error.contains("not in the select list"), "got: {error}");
}

#[test]
fn having_and_order_by_support_column_aliases() {
    let mut schema = make_schema();

    // HAVING can reference the alias of an aggregate.
    let result = run(
        &mut schema,
        "SELECT city, COUNT(*) AS cnt FROM people GROUP BY city HAVING cnt > 1",
    );
    assert_eq!(result.rows.len(), 2);

    // ORDER BY can use aliases inside larger expressions. SUM(age) by
    // city: NY=70, LA=53, SF=35.
    let result = run(
        &mut schema,
        "SELECT city, SUM(age) AS total FROM people GROUP BY city ORDER BY total - 1000 DESC",
    );
    assert_eq!(result.rows[0][0], Value::Text("NY".into()));
    assert_eq!(result.rows[2][0], Value::Text("SF".into()));

    // Non-aggregate queries work too.
    let result = run(
        &mut schema,
        "SELECT name, age + 1 AS next_age FROM people ORDER BY next_age DESC",
    );
    assert_eq!(result.rows[0][0], Value::Text("Dan".into()));
}

#[test]
fn group_by_prefers_source_columns_over_same_named_aliases() {
    let mut schema = make_schema();
    schema.add_database(crate::database::Database::named("case_db"));
    schema.set_current_database("case_db").unwrap();
    let database = schema
        .databases
        .iter_mut()
        .find(|db| db.name == "case_db")
        .unwrap();
    database.add_table(Table {
        name: "mixed".to_string(),
        columns: vec!["k".to_string()],
        rows: vec![vec![Value::Text("a".into())], vec![Value::Text("A".into())]],
    });

    // MySQL semantics: GROUP BY resolves `k` to the source column first,
    // so "a" and "A" form two distinct groups even though the alias would
    // collapse them.
    let result = run(&mut schema, "SELECT UPPER(k) AS k FROM mixed GROUP BY k");
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn aggregates_reject_extra_arguments() {
    let mut schema = make_schema();
    let error = run_query(&mut schema, "SELECT SUM(age, id) FROM people").unwrap_err();
    assert!(error.contains("1 argument(s), got 2"), "got: {error}");
    let error = run_query(&mut schema, "SELECT COUNT(age, id) FROM people").unwrap_err();
    assert!(error.contains("1 argument(s), got 2"), "got: {error}");
}

#[test]
fn infinity_flows_through_numeric_aggregates() {
    let mut schema = make_schema();
    // 1e308 * 1e308 overflows f64 to +inf; float SUM/AVG carry it through.
    let result = run(
        &mut schema,
        "SELECT SUM(1e308 * 1e308) AS s, AVG(1e308 * 1e308) AS a FROM people",
    );
    assert!(matches!(result.rows[0][0], Value::Float(n) if n.is_infinite()));
    assert!(matches!(result.rows[0][1], Value::Float(n) if n.is_infinite()));
}

#[test]
fn nan_values_are_excluded_from_numeric_aggregates_but_counted() {
    let mut schema = make_schema();
    // inf - inf yields NaN. NaN contributes to COUNT but is skipped by the
    // numeric sums, so SUM/AVG of an all-NaN group return NULL.
    let result = run(
        &mut schema,
        "SELECT COUNT(1e308 * 1e308 - 1e308 * 1e308) AS c, \
         SUM(1e308 * 1e308 - 1e308 * 1e308) AS s, \
         AVG(1e308 * 1e308 - 1e308 * 1e308) AS a FROM people",
    );
    assert_eq!(result.rows[0][0], Value::Int(5));
    assert_eq!(result.rows[0][1], Value::Null);
    assert_eq!(result.rows[0][2], Value::Null);
}