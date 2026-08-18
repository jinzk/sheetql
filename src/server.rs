use std::io::{self, BufRead, Write};
use std::time::Instant;

use serde_json::{Value as JsonValue, json};

use crate::database::Schema;
use crate::engine::{self, QueryResult};
use crate::printer::{self, OutputFormat};

/// Serve a JSONL protocol on stdin/stdout: one request per line, one response
/// per line. Returns on stdin EOF or an `{"op":"exit"}` request.
pub fn run(schema: &mut Schema) -> Result<(), String> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    let mut line = String::new();
    loop {
        line.clear();
        match input.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonValue = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut output,
                    &json!({ "ok": false, "error": format!("Invalid JSON: {error}") }),
                )?;
                continue;
            }
        };

        let response = match handle_request(schema, &request) {
            Outcome::Respond(response) => response,
            Outcome::Exit => {
                write_response(&mut output, &json!({ "ok": true }))?;
                return Ok(());
            }
        };
        write_response(&mut output, &response)?;
    }
    Ok(())
}

enum Outcome {
    Respond(JsonValue),
    Exit,
}

/// Handle a single request.
fn handle_request(schema: &mut Schema, request: &JsonValue) -> Outcome {
    let op = request.get("op").and_then(JsonValue::as_str).unwrap_or("");
    match op {
        "query" => Outcome::Respond(handle_query(schema, request)),
        "list" => Outcome::Respond(handle_list(schema)),
        "export" => Outcome::Respond(handle_export(schema, request)),
        "exit" => Outcome::Exit,
        _ => Outcome::Respond(
            json!({ "ok": false, "error": format!("Unknown op `{op}`, expected one of: query, list, export, exit") }),
        ),
    }
}

fn handle_query(schema: &mut Schema, request: &JsonValue) -> JsonValue {
    let sql = request.get("sql").and_then(JsonValue::as_str).unwrap_or("");
    if sql.is_empty() {
        return json!({ "ok": false, "error": "query requires a non-empty `sql`" });
    }

    let format = request
        .get("format")
        .and_then(JsonValue::as_str)
        .unwrap_or("json");
    let output_format = match format.to_lowercase().as_str() {
        "json" => OutputFormat::Json,
        "csv" => OutputFormat::Csv,
        "yaml" => OutputFormat::Yaml,
        "render" | "table" => OutputFormat::Table,
        _ => {
            return json!({ "ok": false, "error": format!("Unknown format `{format}`, expected one of: json, csv, yaml, table") });
        }
    };

    let db = request.get("db").and_then(JsonValue::as_str);
    let start = Instant::now();
    let result = run_query_with_db(schema, sql, db);
    respond_with_result(result, start.elapsed().as_millis() as u64, |result| {
        let rows: Vec<Vec<JsonValue>> = result
            .rows
            .iter()
            .map(|row| row.iter().map(printer::value_to_json).collect())
            .collect();
        let text = printer::render(output_format, &result.columns, &result.rows);
        json!({
            "ok": true,
            "columns": result.columns,
            "rows": rows,
            "text": text,
        })
    })
}

fn handle_list(schema: &Schema) -> JsonValue {
    let databases: Vec<JsonValue> = schema
        .databases
        .iter()
        .map(|database| {
            let tables: Vec<JsonValue> = database
                .tables
                .iter()
                .map(|table| json!({ "name": table.name, "columns": table.columns }))
                .collect();
            json!({ "name": database.name, "tables": tables })
        })
        .collect();
    json!({
        "ok": true,
        "data": {
            "current": schema.current_database(),
            "databases": databases,
        }
    })
}

fn handle_export(schema: &mut Schema, request: &JsonValue) -> JsonValue {
    let sql = request.get("sql").and_then(JsonValue::as_str).unwrap_or("");
    if sql.is_empty() {
        return json!({ "ok": false, "error": "export requires a non-empty `sql`" });
    }
    let path = request
        .get("path")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    if path.is_empty() {
        return json!({ "ok": false, "error": "export requires a `path`" });
    }
    let overwrite = request
        .get("overwrite")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);

    let db = request.get("db").and_then(JsonValue::as_str);
    let start = Instant::now();
    let result = run_query_with_db(schema, sql, db);
    respond_with_result(result, start.elapsed().as_millis() as u64, |result| {
        let content = printer::render(OutputFormat::Csv, &result.columns, &result.rows);
        match write_export(path, &content, overwrite) {
            Ok(()) => json!({ "ok": true, "path": path }),
            Err(error) => json!({ "ok": false, "error": error }),
        }
    })
}

/// Run a query, optionally scoped to `db` for this request only. The process
/// wide `USE` state is restored afterwards.
fn run_query_with_db(
    schema: &mut Schema,
    sql: &str,
    db: Option<&str>,
) -> Result<QueryResult, String> {
    let previous = schema.current_database().map(str::to_string);
    if let Some(db) = db {
        schema.set_current_database(db)?;
    }
    let result = engine::run_query(schema, sql);
    match previous {
        Some(previous) => {
            let _ = schema.set_current_database(&previous);
        }
        None => schema.clear_current_database(),
    }
    result
}

fn respond_with_result(
    result: Result<QueryResult, String>,
    elapsed_ms: u64,
    ok: impl FnOnce(&QueryResult) -> JsonValue,
) -> JsonValue {
    match result {
        Ok(result) => {
            let mut response = ok(&result);
            response["elapsed_ms"] = json!(elapsed_ms);
            response
        }
        Err(error) => json!({ "ok": false, "error": error, "elapsed_ms": elapsed_ms }),
    }
}

fn write_export(path: &str, content: &str, overwrite: bool) -> Result<(), String> {
    if !overwrite && std::path::Path::new(path).exists() {
        return Err(format!("Output file `{path}` already exists"));
    }
    std::fs::write(path, content).map_err(|error| format!("Cannot write file `{path}`: {error}"))
}

fn write_response(output: &mut impl Write, response: &JsonValue) -> Result<(), String> {
    let line = serde_json::to_string(response)
        .map_err(|error| format!("Cannot serialize response: {error}"))?;
    writeln!(output, "{line}").map_err(|error| format!("Cannot write response: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("Cannot flush response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;
    use serde_json::json;

    fn respond(schema: &mut Schema, request: &JsonValue) -> JsonValue {
        match handle_request(schema, request) {
            Outcome::Respond(response) => response,
            Outcome::Exit => panic!("unexpected exit outcome"),
        }
    }

    fn schema() -> Schema {
        let mut schema = Schema::new();
        let mut database = crate::database::Database::named("data_sales_csv");
        database.add_table(crate::database::Table {
            name: "people".to_string(),
            columns: vec!["id".to_string(), "name".to_string(), "city".to_string()],
            rows: vec![
                vec![
                    Value::Int(1),
                    Value::Text("alice".to_string()),
                    Value::Text("LA".to_string()),
                ],
                vec![
                    Value::Int(2),
                    Value::Text("bob".to_string()),
                    Value::Text("NY".to_string()),
                ],
                vec![Value::Int(3), Value::Text("carol".to_string()), Value::Null],
            ],
        });
        schema.add_database(database);
        schema
    }

    #[test]
    fn query_returns_rows_and_text() {
        let mut schema = schema();
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT name, city FROM people ORDER BY id" }),
        );
        assert_eq!(response["ok"], true);
        assert_eq!(response["columns"], json!(["name", "city"]));
        assert_eq!(
            response["rows"],
            json!([["alice", "LA"], ["bob", "NY"], ["carol", null]])
        );
        assert!(response["text"].as_str().is_some());
        assert!(response["elapsed_ms"].as_u64().is_some());
    }

    #[test]
    fn query_error_does_not_panic() {
        let mut schema = schema();
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT nope FROM missing" }),
        );
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().is_some());
    }

    #[test]
    fn query_with_db_is_scoped_to_the_request() {
        let mut schema = schema();
        schema
            .set_current_database("data_sales_csv")
            .expect("database exists");
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT name FROM people ORDER BY id", "db": "data_sales_csv" }),
        );
        assert_eq!(response["ok"], true);
        assert_eq!(schema.current_database(), Some("data_sales_csv"));
    }

    #[test]
    fn query_with_unknown_db_fails() {
        let mut schema = schema();
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT 1", "db": "nope" }),
        );
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("Unknown database")
        );
    }

    #[test]
    fn query_with_unknown_format_fails() {
        let mut schema = schema();
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT 1", "format": "xml" }),
        );
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("Unknown format")
        );
    }

    #[test]
    fn query_without_sql_fails() {
        let mut schema = schema();
        let response = respond(&mut schema, &json!({ "op": "query" }));
        assert_eq!(response["ok"], false);
    }

    #[test]
    fn exit_op_returns_exit_outcome() {
        let mut schema = schema();
        assert!(matches!(
            handle_request(&mut schema, &json!({ "op": "exit" })),
            Outcome::Exit
        ));
    }

    #[test]
    fn missing_op_reports_unknown_op() {
        let mut schema = schema();
        let response = respond(&mut schema, &json!({ "sql": "SELECT 1" }));
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("Unknown op"));
    }

    #[test]
    fn query_scoping_restores_previous_database_even_on_error() {
        let mut schema = schema();
        schema
            .set_current_database("data_sales_csv")
            .expect("database exists");
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT nope FROM missing", "db": "data_sales_csv" }),
        );
        assert_eq!(response["ok"], false);
        assert_eq!(schema.current_database(), Some("data_sales_csv"));
    }

    #[test]
    fn query_scoping_with_no_previous_database_stays_none() {
        let mut schema = schema();
        let response = respond(
            &mut schema,
            &json!({ "op": "query", "sql": "SELECT name FROM people ORDER BY id", "db": "data_sales_csv" }),
        );
        assert_eq!(response["ok"], true);
        assert_eq!(schema.current_database(), None);
    }

    #[test]
    fn list_reports_databases_tables_and_columns() {
        let mut schema = schema();
        let response = respond(&mut schema, &json!({ "op": "list" }));
        assert_eq!(response["ok"], true);
        assert_eq!(response["data"]["current"], serde_json::Value::Null);
        assert_eq!(
            response["data"]["databases"][0]["name"],
            json!("data_sales_csv")
        );
        assert_eq!(
            response["data"]["databases"][0]["tables"][0]["name"],
            json!("people")
        );
        assert_eq!(
            response["data"]["databases"][0]["tables"][0]["columns"],
            json!(["id", "name", "city"])
        );
    }

    #[test]
    fn export_writes_csv_and_respects_overwrite() {
        let mut schema = schema();
        let path = std::env::temp_dir().join(format!("sheetql_export_{}.csv", std::process::id()));
        let path = path.to_string_lossy().into_owned();
        let response = respond(
            &mut schema,
            &json!({ "op": "export", "sql": "SELECT name, city FROM people ORDER BY id", "path": path }),
        );
        assert_eq!(response["ok"], true);
        let content = std::fs::read_to_string(&path).expect("file written");
        assert!(content.starts_with("name,city\n"));
        assert!(content.contains("alice,LA"));
        assert!(content.contains("carol,NULL"));
        assert!(content.contains("bob,NY"));

        let again = respond(
            &mut schema,
            &json!({ "op": "export", "sql": "SELECT 1 AS x", "path": path }),
        );
        assert_eq!(again["ok"], true, "overwrite defaults to true");

        let no_overwrite = respond(
            &mut schema,
            &json!({ "op": "export", "sql": "SELECT 1 AS x", "path": path, "overwrite": false }),
        );
        assert_eq!(no_overwrite["ok"], false);
        assert!(
            no_overwrite["error"]
                .as_str()
                .unwrap()
                .contains("already exists")
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn export_without_sql_or_path_fails() {
        let mut schema = schema();
        assert_eq!(
            respond(&mut schema, &json!({ "op": "export", "path": "x.csv" }))["ok"],
            false
        );
        assert_eq!(
            respond(&mut schema, &json!({ "op": "export", "sql": "SELECT 1" }))["ok"],
            false
        );
    }

    #[test]
    fn unknown_op_returns_error() {
        let mut schema = schema();
        let response = respond(&mut schema, &json!({ "op": "frobnicate" }));
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("Unknown op"));
    }

    #[test]
    fn export_uses_csv_format_even_for_quoted_values() {
        let mut schema = schema();
        let path =
            std::env::temp_dir().join(format!("sheetql_export_quote_{}.csv", std::process::id()));
        let path = path.to_string_lossy().into_owned();
        let response = respond(
            &mut schema,
            &json!({ "op": "export", "sql": "SELECT 'a,b' AS x", "path": path }),
        );
        assert_eq!(response["ok"], true);
        let content = std::fs::read_to_string(&path).expect("file written");
        assert_eq!(content, "x\n\"a,b\"\n");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn response_serializes_as_single_line() {
        let mut schema = schema();
        let response = respond(&mut schema, &json!({ "op": "list" }));
        let line = serde_json::to_string(&response).expect("serializes");
        assert!(!line.contains('\n'));
        assert!(line.starts_with("{\""));
    }
}
