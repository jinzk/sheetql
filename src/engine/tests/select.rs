use super::*;
use crate::database::{Database, Table};
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn select_all_returns_all_rows() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT * FROM people");
    assert_eq!(result.columns, vec!["id", "name", "age", "city"]);
    assert_eq!(result.rows.len(), 5);
}

#[test]
fn where_filter_and_order_by_desc() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name FROM people WHERE age >= 30 ORDER BY age DESC",
    );
    let names: Vec<String> = result
        .rows
        .iter()
        .map(|row| row[0].to_display_string())
        .collect();
    assert_eq!(names, vec!["Dan", "Carol", "Alice"]);
}

#[test]
fn url_email_and_format_functions_work_in_sql_filters_and_projection() {
    let mut schema = Schema::new();
    let mut database = Database::named("contacts");
    database.add_table(Table {
        name: "contacts".into(),
        columns: vec!["name".into(), "url".into(), "email".into()],
        rows: vec![
            vec![
                Value::Text("Alice".into()),
                Value::Text("https://example.com?a=1".into()),
                Value::Text("alice@example.com".into()),
            ],
            vec![
                Value::Text("Bob".into()),
                Value::Text("https://other.test?a=2".into()),
                Value::Text("invalid".into()),
            ],
        ],
    });
    schema.add_database(database);
    schema.set_current_database("contacts").unwrap();

    let result = run_query(
        &mut schema,
        "SELECT FORMAT('{} <{}>', name, EMAIL_DOMAIN(email)) AS contact, URL_PARAM(url, 'a') AS campaign FROM contacts WHERE EMAIL_VALID(email) AND URL_HOST(url) = 'example.com'",
    )
    .unwrap();
    assert_eq!(result.columns, vec!["contact", "campaign"]);
    assert_eq!(
        result.rows,
        vec![vec![
            Value::Text("Alice <example.com>".into()),
            Value::Text("1".into())
        ]]
    );
}

#[test]
fn scalar_expressions_in_select_without_from() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 1 + 2 AS three, LOWER('ABC') AS low");
    assert_eq!(result.rows[0][0], Value::Int(3));
    assert_eq!(result.rows[0][1], Value::Text("abc".to_string()));
}

#[test]
fn left_right_instr_string_functions() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT LEFT('Hello', 2) AS l, RIGHT('Hello', 2) AS r, \
         INSTR('Hello', 'll') AS pos, INSTR('Hello', 'zz') AS nf",
    );
    assert_eq!(result.rows[0][0], Value::Text("He".to_string()));
    assert_eq!(result.rows[0][1], Value::Text("lo".to_string()));
    assert_eq!(result.rows[0][2], Value::Int(3));
    assert_eq!(result.rows[0][3], Value::Int(0));
}

#[test]
fn now_and_date_functions() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT NOW() AS n, DATE() AS d, DATE('2026/08/14 10:30:00') AS p",
    );
    let now = result.rows[0][0].to_display_string();
    assert_eq!(now.len(), 19, "got: {now}");
    let today = result.rows[0][1].to_display_string();
    assert_eq!(today.len(), 10, "got: {today}");
    assert_eq!(result.rows[0][2], Value::Date("2026-08-14".to_string()));
}

#[test]
fn where_matches_date_cell_with_slash_and_unpadded_literal() {
    let mut database = Database::named("dates");
    database.add_table(Table {
        name: "events".to_string(),
        columns: vec!["id".to_string(), "day".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Date("2026-05-07".into())],
            vec![Value::Int(2), Value::Date("2026-05-08".into())],
        ],
    });
    let mut schema = Schema::new();
    schema.add_database(database);
    schema.set_current_database("dates").unwrap();

    let result = run(&mut schema, "SELECT id FROM events WHERE day = '2026/5/7'");
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn power_sqrt_math_functions() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT POWER(2, 10) AS p, SQRT(16) AS s");
    assert_eq!(result.rows[0][0], Value::Float(1024.0));
    assert_eq!(result.rows[0][1], Value::Float(4.0));
}

#[test]
fn floor_ceil_and_ceiling_round_numerically() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT FLOOR(age / 4.0) AS f, CEIL(age / 4.0) AS c, CEILING(age / 4.0) AS c2 FROM people WHERE id = 1",
    );
    // age 30: 30 / 4 = 7.5 -> floor 7, ceil 8
    assert_eq!(result.rows[0][0], Value::Float(7.0));
    assert_eq!(result.rows[0][1], Value::Float(8.0));
    assert_eq!(result.rows[0][2], Value::Float(8.0));

    let err = run_query(&mut schema, "SELECT FLOOR(3.7 TO HOUR)").unwrap_err();
    assert!(err.contains("TO is not supported"), "got: {err}");
}

#[test]
fn isnull_reports_null_in_where_clause() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT name FROM people WHERE ISNULL(age)");
    assert_eq!(result.rows.len(), 0);
    let result = run(
        &mut schema,
        "SELECT ISNULL(age) AS n FROM people WHERE id = 1",
    );
    assert_eq!(result.rows[0][0], Value::Bool(false));
}

#[test]
fn greatest_least_functions() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT GREATEST(3, 7, 5) AS g, LEAST(3, 7, 5) AS l",
    );
    assert_eq!(result.rows[0][0], Value::Int(7));
    assert_eq!(result.rows[0][1], Value::Int(3));
}

#[test]
fn case_in_projection() {
    let mut schema = make_schema();
    let result = run(
        &mut schema,
        "SELECT name, CASE WHEN age >= 35 THEN 'senior' ELSE 'junior' END AS tier FROM people",
    );
    assert_eq!(result.rows[2][1], Value::Text("senior".into()));
    assert_eq!(result.rows[0][1], Value::Text("junior".into()));
}

#[test]
fn unknown_table_returns_error() {
    let mut schema = make_schema();
    assert!(run_query(&mut schema, "SELECT * FROM missing").is_err());
}

#[test]
fn malformed_sql_returns_error() {
    let mut schema = make_schema();
    assert!(run_query(&mut schema, "SELECT FROM").is_err());
}

#[test]
fn query_result_contains_execution_statistics() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT * FROM people LIMIT 2");
    assert_eq!(result.stats.input_rows, 5);
    assert_eq!(result.stats.output_rows, 2);
}

#[test]
fn logical_and_short_circuits() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 1 WHERE FALSE AND 1/0 = 1");
    assert_eq!(result.rows, Vec::<Vec<Value>>::new());
    let result = run(&mut schema, "SELECT 1 WHERE TRUE OR 1/0 = 1");
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn in_list_with_null_is_null_when_unmatched() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT 3 IN (1, NULL) AS r");
    assert_eq!(result.rows, vec![vec![Value::Null]]);
    let result = run(&mut schema, "SELECT 1 IN (1, NULL) AS r");
    assert_eq!(result.rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn coalesce_short_circuits_argument_evaluation() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SELECT COALESCE(NULL, 7, 1/0) AS v");
    assert_eq!(result.rows[0][0], Value::Int(7));
    let result = run(&mut schema, "SELECT IFNULL(5, 1/0) AS v");
    assert_eq!(result.rows[0][0], Value::Int(5));
}

#[test]
fn distinct_on_is_rejected() {
    let mut schema = make_schema();
    let err = run_query(&mut schema, "SELECT DISTINCT ON (city) city FROM people").unwrap_err();
    assert!(err.contains("DISTINCT ON"), "got: {err}");
}

#[test]
fn group_by_all_is_rejected() {
    let mut schema = make_schema();
    let err = run_query(&mut schema, "SELECT name FROM people GROUP BY ALL").unwrap_err();
    assert!(err.contains("GROUP BY ALL"), "got: {err}");
}

#[test]
fn order_by_sorts_nulls_first_ascending_and_last_descending() {
    let mut database = Database::named("test");
    database.add_table(Table {
        name: "t".to_string(),
        columns: vec!["id".to_string(), "v".to_string()],
        rows: vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Int(5)],
            vec![Value::Int(3), Value::Int(1)],
            vec![Value::Int(4), Value::Null],
        ],
    });
    let mut schema = Schema::new();
    schema.add_database(database);
    schema.set_current_database("test").unwrap();

    let result = run(&mut schema, "SELECT id FROM t ORDER BY v ASC");
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(4)],
            vec![Value::Int(3)],
            vec![Value::Int(2)],
        ]
    );

    let result = run(&mut schema, "SELECT id FROM t ORDER BY v DESC");
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(1)],
            vec![Value::Int(4)],
        ]
    );
}