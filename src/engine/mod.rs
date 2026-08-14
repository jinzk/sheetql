mod metadata;
mod select;

use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::database::Schema;
use crate::printer::render;
use crate::printer::OutputFormat;
use crate::value::Value;

use crate::engine::metadata::{
    parse_show_clauses, run_describe_table, run_show_databases, run_show_tables, run_use,
};
use crate::engine::select::{execute_query, object_name_to_parts};

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

pub fn run_query(schema: &mut Schema, sql: &str) -> Result<QueryResult, String> {
    let (query_sql, outfile) = strip_into_outfile(sql);
    let trimmed = query_sql.trim();
    let lower = trimmed.to_lowercase().trim_end_matches(';').trim().to_string();

    if lower.starts_with("show databases") {
        let rest = lower.strip_prefix("show databases").unwrap().trim();
        let (_, like) = parse_show_clauses(rest)?;
        return run_show_databases(schema, like.as_deref());
    }
    if lower.starts_with("show schemas") {
        let rest = lower.strip_prefix("show schemas").unwrap().trim();
        let (_, like) = parse_show_clauses(rest)?;
        return run_show_databases(schema, like.as_deref());
    }
    if lower.starts_with("show tables") {
        let rest = lower.strip_prefix("show tables").unwrap().trim();
        let (database, like) = parse_show_clauses(rest)?;
        return run_show_tables(schema, database.as_deref(), like.as_deref());
    }
    if let Some(rest) = lower.strip_prefix("describe ") {
        return run_describe_table(schema, rest.trim());
    }
    if let Some(rest) = lower.strip_prefix("desc ") {
        return run_describe_table(schema, rest.trim());
    }
    if let Some(name) = lower.strip_prefix("use ") {
        let name = name.trim();
        if name.is_empty() {
            return Err("USE requires a database name".to_string());
        }
        return run_use(schema, name);
    }

    let dialect = MySqlDialect {};
    let statements = Parser::parse_sql(&dialect, &query_sql)
        .map_err(|error| format!("SQL parse error: {error}"))?;

    if statements.len() != 1 {
        return Err("Only a single statement per query is supported".to_string());
    }

    let result = match &statements[0] {
        Statement::Query(query) => execute_query(schema, query),
        Statement::ShowColumns { show_options, .. } => {
            if show_options.filter_position.is_some() {
                return Err("SHOW COLUMNS filters (LIKE/WHERE) are not supported".to_string());
            }
            let reference = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .map(|name| object_name_to_parts(name).join("."))
                .ok_or_else(|| "SHOW COLUMNS requires a table name".to_string())?;
            run_describe_table(schema, &reference)
        }
        Statement::ShowSchemas { .. } | Statement::ShowDatabases { .. } => {
            run_show_databases(schema, None)
        }
        other => Err(format!("Unsupported statement: {other}")),
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
        });
    }

    result
}

/// Split a trailing `INTO OUTFILE 'path'` clause (case-insensitive) off a SQL
/// statement. `sqlparser` does not parse MySQL's `INTO OUTFILE` syntax, so we
/// detect and remove it before handing the rest to the parser, and surface the
/// path so the caller can write the result set to that file.
fn strip_into_outfile(sql: &str) -> (String, Option<String>) {
    let upper = sql.to_ascii_uppercase();
    let marker = "INTO OUTFILE";
    let pos = match upper.find(marker) {
        Some(pos) => pos,
        None => return (sql.to_string(), None),
    };

    let tail = &sql[pos + marker.len()..];
    let mut iter = tail.char_indices().peekable();
    while let Some(&(_, c)) = iter.peek() {
        if c.is_whitespace() {
            iter.next();
        } else {
            break;
        }
    }

    let quote = match iter.peek() {
        Some(&(_, '\'')) => '\'',
        Some(&(_, '"')) => '"',
        _ => return (sql.to_string(), None),
    };
    iter.next();

    let mut path = String::new();
    let mut end_byte = 0;
    let mut closed = false;
    for (byte, c) in iter {
        if c == quote {
            end_byte = byte + c.len_utf8();
            closed = true;
            break;
        }
        path.push(c);
        end_byte = byte + c.len_utf8();
    }
    if !closed {
        return (sql.to_string(), None);
    }

    let mut rest = String::with_capacity(pos + tail.len() - end_byte);
    rest.push_str(sql[..pos].trim_end());
    rest.push_str(tail[end_byte..].trim_start());
    (rest.trim().to_string(), Some(path))
}

/// Write a query result to a file as CSV (with header). Refuses to overwrite an
/// existing file to avoid clobbering a data source.
fn write_outfile(path: &str, result: &QueryResult) -> Result<(), String> {
    if std::path::Path::new(path).exists() {
        return Err(format!("Output file `{path}` already exists"));
    }
    let content = render(OutputFormat::Csv, &result.columns, &result.rows);
    std::fs::write(path, content).map_err(|error| format!("Cannot write to `{path}`: {error}"))?;
    Ok(())
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
                vec![Value::Int(1), Value::Text("Alice".into()), Value::Int(30), Value::Text("NY".into())],
                vec![Value::Int(2), Value::Text("Bob".into()), Value::Int(25), Value::Text("LA".into())],
                vec![Value::Int(3), Value::Text("Carol".into()), Value::Int(35), Value::Text("SF".into())],
                vec![Value::Int(4), Value::Text("Dan".into()), Value::Int(40), Value::Text("NY".into())],
                vec![Value::Int(5), Value::Text("Eve".into()), Value::Int(28), Value::Text("LA".into())],
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
        assert_eq!(result.columns, vec!["Column".to_string(), "Type".to_string()]);
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
    fn show_columns_lists_columns() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW COLUMNS FROM people");
        assert_eq!(result.columns, vec!["Column".to_string(), "Type".to_string()]);
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
    fn distinct_limit_offset() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT DISTINCT city FROM people LIMIT 2");
        assert_eq!(result.rows.len(), 2);
        let result = run(&mut schema, "SELECT name FROM people ORDER BY id LIMIT 2 OFFSET 2");
        assert_eq!(result.rows.len(), 2);
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
        assert_eq!(result.rows[0][2], Value::Text("2026-08-14".to_string()));
    }

    #[test]
    fn power_sqrt_math_functions() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT POWER(2, 10) AS p, SQRT(16) AS s");
        assert_eq!(result.rows[0][0], Value::Float(1024.0));
        assert_eq!(result.rows[0][1], Value::Float(4.0));
    }

    #[test]
    fn greatest_least_functions() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT GREATEST(3, 7, 5) AS g, LEAST(3, 7, 5) AS l");
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
    fn into_outfile_writes_csv() {
        let mut schema = make_schema();
        let path = std::env::temp_dir()
            .join(format!("sheetql_outfile_{}.csv", std::process::id()));
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
        let path = std::env::temp_dir()
            .join(format!("sheetql_outfile_existing_{}.csv", std::process::id()));
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
        let path = std::env::temp_dir()
            .join(format!("sheetql_outfile_where_{}.csv", std::process::id()));
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
        let path = std::env::temp_dir()
            .join(format!("sheetql_outfile_group_{}.csv", std::process::id()));
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
        let path = std::env::temp_dir()
            .join(format!("sheetql_outfile_dq_{}.csv", std::process::id()));
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
}
