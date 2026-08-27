mod metadata;
mod output;
pub(crate) mod rewrite;
mod select;
mod temporary;

use sqlparser::ast::{
    ShowStatementFilter, ShowStatementFilterPosition, ShowStatementOptions, Statement,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use std::time::Instant;

use crate::database::Schema;
use crate::error::Error;
use crate::value::Value;

use crate::engine::metadata::{run_describe_table, run_show_databases, run_show_tables, run_use};
use crate::engine::output::{strip_into_outfile, write_outfile};
use crate::engine::select::{execute_query, object_name_to_parts};
use crate::engine::temporary::run_create_table;

/// Split an `INTO OUTFILE 'path'` clause off a query before parsing.
/// Exposed so the server can reject file-writing clauses up front.
pub(crate) fn split_outfile(sql: &str) -> (String, Option<String>) {
    strip_into_outfile(sql)
}

#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    pub elapsed_ms: u128,
    pub input_rows: usize,
    pub output_rows: usize,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
}

pub fn run_query(schema: &mut Schema, sql: &str) -> Result<QueryResult, Error> {
    let started = Instant::now();
    let (query_sql, outfile) = strip_into_outfile(sql);

    let dialect = MySqlDialect {};
    let statements = Parser::parse_sql(&dialect, &query_sql)
        .map_err(|error| format!("SQL parse error: {error}"))?;

    if statements.len() != 1 {
        return Err("Only a single statement per query is supported"
            .to_string()
            .into());
    }

    let result = match &statements[0] {
        Statement::Query(query) => execute_query(schema, query),
        Statement::CreateTable(create) => run_create_table(schema, create),
        Statement::ShowColumns { show_options, .. } => run_show_columns(schema, show_options),
        Statement::ShowDatabases { show_options, .. }
        | Statement::ShowSchemas { show_options, .. } => {
            run_show_databases(schema, show_like_pattern(show_options)?.as_deref())
        }
        Statement::ShowTables { show_options, .. } => {
            let database = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .map(|name| object_name_to_parts(name).join("."));
            run_show_tables(
                schema,
                database.as_deref(),
                show_like_pattern(show_options)?.as_deref(),
            )
        }
        Statement::Use(use_clause) => match use_clause {
            sqlparser::ast::Use::Object(name) | sqlparser::ast::Use::Database(name) => {
                run_use(schema, &object_name_to_parts(name).join("."))
            }
            _ => Err("Unsupported USE statement".to_string().into()),
        },
        Statement::ExplainTable { table_name, .. } => {
            let reference = object_name_to_parts(table_name).join(".");
            run_describe_table(schema, &reference)
        }
        other => Err(format!("Unsupported statement: {other}").into()),
    };

    if let Some(path) = outfile {
        let result = result?;
        write_outfile(&path, &result)?;
        return Ok(QueryResult {
            columns: vec!["Status".to_string()],
            rows: vec![vec![Value::Text(format!(
                "Written {} row(s) to '{}'",
                result.rows.len(),
                path
            ))]],
            stats: QueryStats {
                elapsed_ms: started.elapsed().as_millis(),
                input_rows: result.stats.input_rows,
                output_rows: result.rows.len(),
            },
        });
    }

    finalize_result(result, started)
}

/// Extract the `LIKE 'pattern'` filter from a SHOW statement's options. Only
/// LIKE filters are supported; WHERE/ILIKE filters are rejected.
fn show_like_pattern(show_options: &ShowStatementOptions) -> Result<Option<String>, Error> {
    let Some(position) = &show_options.filter_position else {
        return Ok(None);
    };
    let filter = match position {
        ShowStatementFilterPosition::Infix(filter)
        | ShowStatementFilterPosition::Suffix(filter) => filter,
    };
    match filter {
        ShowStatementFilter::Like(pattern) => Ok(Some(pattern.clone())),
        ShowStatementFilter::NoKeyword(pattern) => Ok(Some(pattern.clone())),
        _ => Err("SHOW ... filters other than LIKE are not supported"
            .to_string()
            .into()),
    }
}

/// Run `SHOW COLUMNS FROM <table>`, resolving the table name from the
/// statement's `IN`/`FROM` clause.
fn run_show_columns(
    schema: &Schema,
    show_options: &ShowStatementOptions,
) -> Result<QueryResult, Error> {
    if show_options.filter_position.is_some() {
        return Err("SHOW COLUMNS filters (LIKE/WHERE) are not supported"
            .to_string()
            .into());
    }
    let reference = show_options
        .show_in
        .as_ref()
        .and_then(|show_in| show_in.parent_name.as_ref())
        .map(|name| object_name_to_parts(name).join("."))
        .ok_or_else(|| "SHOW COLUMNS requires a table name".to_string())?;
    run_describe_table(schema, &reference)
}

fn finalize_result(
    result: Result<QueryResult, Error>,
    started: Instant,
) -> Result<QueryResult, Error> {
    result.map(|mut result| {
        result.stats.elapsed_ms = started.elapsed().as_millis();
        result.stats.output_rows = result.rows.len();
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::database::Table;
    use crate::evaluator::like_match;

    fn make_database() -> Database {
        let mut database = Database::named("test");
        database.add_table(Table {
            name: "people".to_string(),
            columns: vec![
                "id".to_string(),
                "name".to_string(),
                "age".to_string(),
                "city".to_string(),
            ],
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("Alice".into()),
                    Value::Int(30),
                    Value::Text("NY".into()),
                ],
                vec![
                    Value::Int(2),
                    Value::Text("Bob".into()),
                    Value::Int(25),
                    Value::Text("LA".into()),
                ],
                vec![
                    Value::Int(3),
                    Value::Text("Carol".into()),
                    Value::Int(35),
                    Value::Text("SF".into()),
                ],
                vec![
                    Value::Int(4),
                    Value::Text("Dan".into()),
                    Value::Int(40),
                    Value::Text("NY".into()),
                ],
                vec![
                    Value::Int(5),
                    Value::Text("Eve".into()),
                    Value::Int(28),
                    Value::Text("LA".into()),
                ],
            ],
        });
        database.add_table(Table {
            name: "orders".to_string(),
            columns: vec![
                "order_id".to_string(),
                "customer_id".to_string(),
                "amount".to_string(),
            ],
            rows: vec![
                vec![Value::Int(101), Value::Int(1), Value::Float(50.5)],
                vec![Value::Int(102), Value::Int(2), Value::Float(20.0)],
                vec![Value::Int(103), Value::Int(1), Value::Float(99.9)],
            ],
        });
        database
    }

    fn make_schema() -> Schema {
        let mut schema = Schema::new();
        schema.add_database(make_database());
        schema.set_current_database("test").unwrap();
        schema
    }

    fn run(schema: &mut Schema, sql: &str) -> QueryResult {
        run_query(schema, sql).unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
    }

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
            "CREATE TEMPORARY TABLE empty (name TEXT)",
            "CREATE TEMPORARY TABLE qualified.name AS SELECT * FROM people",
        ] {
            assert!(
                run_query(&mut schema, sql).is_err(),
                "query should fail: {sql}"
            );
        }
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
    fn distinct_on_is_rejected() {
        let mut schema = make_schema();
        let err = run_query(&mut schema, "SELECT DISTINCT ON (city) city FROM people").unwrap_err();
        assert!(err.contains("DISTINCT ON"), "got: {err}");
    }

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
    fn strip_into_outfile_removes_clause() {
        let (rest, path) = strip_into_outfile("SELECT * FROM people INTO OUTFILE 'out.csv'");
        assert_eq!(rest, "SELECT * FROM people");
        assert_eq!(path.as_deref(), Some("out.csv"));
    }

    #[test]
    fn strip_into_outfile_is_case_insensitive_and_drops_semicolon() {
        let (rest, path) = strip_into_outfile("select 1 into outfile \"a.csv\";");
        assert_eq!(rest, "select 1;");
        assert_eq!(path.as_deref(), Some("a.csv"));
    }

    #[test]
    fn strip_into_outfile_is_none_when_absent() {
        let (rest, path) = strip_into_outfile("SELECT * FROM people");
        assert_eq!(rest, "SELECT * FROM people");
        assert!(path.is_none());
    }

    #[test]
    fn strip_into_outfile_ignores_string_literals() {
        let (rest, path) = strip_into_outfile("SELECT 'INTO OUTFILE' AS x");
        assert_eq!(rest, "SELECT 'INTO OUTFILE' AS x");
        assert!(path.is_none());
    }

    #[test]
    fn strip_into_outfile_ignores_comment_text() {
        let (rest, path) = strip_into_outfile("SELECT 1 -- INTO OUTFILE 'nope.csv'\nFROM people");
        assert_eq!(rest, "SELECT 1 -- INTO OUTFILE 'nope.csv'\nFROM people");
        assert!(path.is_none());
    }

    #[test]
    fn into_outfile_writes_csv() {
        let mut schema = make_schema();
        let path = std::env::temp_dir().join(format!("sheetql_outfile_{}.csv", std::process::id()));
        let target = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let sql = format!(
            "SELECT name, city FROM people ORDER BY id LIMIT 2 INTO OUTFILE '{}'",
            target
        );
        let result = run_query(&mut schema, &sql).unwrap();
        assert_eq!(result.columns, vec!["Status".to_string()]);
        assert!(path.exists(), "output file should be created");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("name,city"), "got: {content}");
        assert!(content.contains("Alice"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn into_outfile_rejects_existing_file() {
        let mut schema = make_schema();
        let path = std::env::temp_dir().join(format!(
            "sheetql_outfile_existing_{}.csv",
            std::process::id()
        ));
        std::fs::write(&path, "stub").unwrap();
        let target = path.to_string_lossy().to_string();
        let sql = format!("SELECT name FROM people INTO OUTFILE '{}'", target);
        let error = run_query(&mut schema, &sql).unwrap_err();
        assert!(error.contains("already exists"), "got: {error}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn into_outfile_with_where_filters_rows() {
        let mut schema = make_schema();
        let path =
            std::env::temp_dir().join(format!("sheetql_outfile_where_{}.csv", std::process::id()));
        let target = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let sql = format!(
            "SELECT id, name FROM people WHERE city = 'LA' ORDER BY id INTO OUTFILE '{}'",
            target
        );
        let result = run_query(&mut schema, &sql).unwrap();
        assert_eq!(result.columns, vec!["Status".to_string()]);
        let content = std::fs::read_to_string(&path).unwrap();
        let data_lines: Vec<&str> = content.lines().skip(1).collect();
        assert_eq!(data_lines.len(), 2, "got: {content}");
        assert!(content.contains("Bob"));
        assert!(content.contains("Eve"));
        assert!(!content.contains("Alice"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn into_outfile_with_aggregation() {
        let mut schema = make_schema();
        let path =
            std::env::temp_dir().join(format!("sheetql_outfile_group_{}.csv", std::process::id()));
        let target = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let sql = format!(
            "SELECT city, COUNT(*) AS cnt FROM people GROUP BY city ORDER BY city INTO OUTFILE '{}'",
            target
        );
        let result = run_query(&mut schema, &sql).unwrap();
        assert_eq!(result.columns, vec!["Status".to_string()]);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.starts_with("city,cnt"), "got: {content}");
        let data_lines: Vec<&str> = content.lines().skip(1).collect();
        assert_eq!(data_lines.len(), 3, "got: {content}");
        assert!(content.contains("NY,2"));
        assert!(content.contains("LA,2"));
        assert!(content.contains("SF,1"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn into_outfile_double_quoted_path() {
        let mut schema = make_schema();
        let path =
            std::env::temp_dir().join(format!("sheetql_outfile_dq_{}.csv", std::process::id()));
        let target = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let sql = format!(
            "SELECT name FROM people ORDER BY id LIMIT 1 INTO OUTFILE \"{}\"",
            target
        );
        let _ = run_query(&mut schema, &sql).unwrap();
        assert!(path.exists(), "output file should be created");
        let _ = std::fs::remove_file(&path);
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

    #[test]
    fn strip_into_outfile_ignores_block_comments() {
        let (rest, path) = strip_into_outfile("SELECT 1 /* INTO OUTFILE 'acc.csv' */ FROM people");
        assert_eq!(rest, "SELECT 1 /* INTO OUTFILE 'acc.csv' */ FROM people");
        assert!(path.is_none());
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
}
