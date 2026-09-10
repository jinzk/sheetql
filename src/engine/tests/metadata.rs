use super::*;
use crate::database::{Database, Table};
use crate::engine::tests::{make_schema, run};
use crate::value::Value;

#[test]
fn show_databases_lists_all_databases() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW DATABASES");
    assert_eq!(result.columns, vec!["Database".to_string()]);
    assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
}

#[test]
fn use_switches_current_database() {
    let mut schema = make_schema();
    let mut other = Database::named("other");
    other.add_table(Table {
        name: "extra".to_string(),
        columns: vec!["value".to_string()],
        rows: vec![vec![Value::Int(7)]],
    });
    schema.add_database(other);

    let result = run(&mut schema, "USE other");
    assert_eq!(
        result.rows,
        vec![vec![Value::Text("Database changed".into())]]
    );
    assert_eq!(schema.current_database(), Some("other"));

    let result = run(&mut schema, "SELECT value FROM extra");
    assert_eq!(result.rows, vec![vec![Value::Int(7)]]);
    assert!(run_query(&mut schema, "USE nope").is_err());
}

#[test]
fn qualified_table_reference_ignores_current_database() {
    let mut schema = make_schema();
    let mut other = Database::named("other");
    other.add_table(Table {
        name: "extra".to_string(),
        columns: vec!["value".to_string()],
        rows: vec![vec![Value::Int(7)]],
    });
    schema.add_database(other);

    let result = run(&mut schema, "SELECT value FROM other.extra");
    assert_eq!(result.rows, vec![vec![Value::Int(7)]]);
}

#[test]
fn unqualified_table_reference_reports_ambiguity() {
    let mut schema = make_schema();
    let mut other = Database::named("other");
    other.add_table(Table {
        name: "people".to_string(),
        columns: vec!["id".to_string()],
        rows: vec![],
    });
    schema.add_database(other);
    schema.add_database(Database::named("empty"));

    run(&mut schema, "USE empty");
    let err = run_query(&mut schema, "SELECT * FROM people").unwrap_err();
    assert!(err.contains("ambiguous"), "got: {err}");
    let ok = run(&mut schema, "SELECT * FROM other.people");
    assert_eq!(ok.rows.len(), 0);
}

#[test]
fn show_tables_from_lists_specific_database() {
    let mut schema = make_schema();
    let mut other = Database::named("other");
    other.add_table(Table {
        name: "extra".to_string(),
        columns: vec!["value".to_string()],
        rows: vec![],
    });
    schema.add_database(other);

    let result = run(&mut schema, "SHOW TABLES FROM other");
    assert_eq!(result.rows, vec![vec![Value::Text("extra".into())]]);
    assert!(run_query(&mut schema, "SHOW TABLES FROM nope").is_err());
}

#[test]
fn show_tables_like_filters_table_names() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW TABLES LIKE 'peop%'");
    assert_eq!(result.rows, vec![vec![Value::Text("people".into())]]);
    let result = run(&mut schema, "SHOW TABLES LIKE '%der%'");
    assert_eq!(result.rows, vec![vec![Value::Text("orders".into())]]);
    let result = run(&mut schema, "SHOW TABLES LIKE 'z%'");
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn show_schemas_is_database_alias() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW SCHEMAS");
    assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
}

#[test]
fn show_databases_like_filters_database_names() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW DATABASES LIKE 'te%'");
    assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
    let result = run(&mut schema, "SHOW DATABASES LIKE 'x%'");
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn show_tables_lists_registered_tables() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW TABLES");
    assert_eq!(result.columns, vec!["Tables".to_string()]);
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn describe_reports_columns_and_types() {
    let mut schema = make_schema();
    let result = run(&mut schema, "DESCRIBE people");
    assert_eq!(
        result.columns,
        vec!["Column".to_string(), "Type".to_string()]
    );
    assert_eq!(
        result.rows[0],
        vec![Value::Text("id".into()), Value::Text("Integer".into())]
    );
    assert_eq!(
        result.rows[1],
        vec![Value::Text("name".into()), Value::Text("Text".into())]
    );
}

#[test]
fn desc_abbreviation_describes_table() {
    let mut schema = make_schema();
    let result = run(&mut schema, "DESC people");
    assert_eq!(
        result.columns,
        vec!["Column".to_string(), "Type".to_string()]
    );
    assert_eq!(result.rows.len(), 4);
    assert_eq!(result.rows[0][0], Value::Text("id".into()));
}

#[test]
fn show_columns_rejects_filters() {
    let mut schema = make_schema();
    let err = run_query(&mut schema, "SHOW COLUMNS FROM people LIKE 'i%'").unwrap_err();
    assert!(err.contains("filters"), "got: {err}");
}

#[test]
fn show_tables_rejects_where_filters() {
    let mut schema = make_schema();
    let err = run_query(&mut schema, "SHOW TABLES WHERE 1").unwrap_err();
    assert!(err.contains("LIKE"), "got: {err}");
}

#[test]
fn use_with_backticks_switches_database() {
    let mut schema = make_schema();
    let mut other = Database::named("other");
    other.add_table(Table {
        name: "extra".to_string(),
        columns: vec!["value".to_string()],
        rows: vec![vec![Value::Int(7)]],
    });
    schema.add_database(other);

    let result = run(&mut schema, "USE `other`");
    assert_eq!(
        result.rows,
        vec![vec![Value::Text("Database changed".into())]]
    );
    assert_eq!(schema.current_database(), Some("other"));
}

#[test]
fn show_columns_lists_columns() {
    let mut schema = make_schema();
    let result = run(&mut schema, "SHOW COLUMNS FROM people");
    assert_eq!(
        result.columns,
        vec!["Column".to_string(), "Type".to_string()]
    );
    assert_eq!(result.rows.len(), 4);
    assert_eq!(result.rows[0][0], Value::Text("id".into()));
    assert_eq!(result.rows[0][1], Value::Text("Integer".into()));
}

#[test]
fn show_columns_supports_backticks_and_unicode_names() {
    let mut schema = make_schema();
    let mut sales = Database::named("sales_db");
    sales.add_table(Table {
        name: "商品销售明细".to_string(),
        columns: vec!["商品".to_string(), "金额".to_string()],
        rows: vec![vec![Value::Text("A".into()), Value::Int(10)]],
    });
    schema.add_database(sales);
    schema.set_current_database("sales_db").unwrap();

    let result = run(&mut schema, "SHOW COLUMNS FROM `商品销售明细`");
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], Value::Text("商品".into()));

    let result = run(&mut schema, "SELECT 商品 FROM 商品销售明细");
    assert_eq!(result.rows, vec![vec![Value::Text("A".into())]]);
}