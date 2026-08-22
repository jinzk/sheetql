use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sheetql_it_{}_{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn sample_csv(dir: &Path) -> PathBuf {
    let path = dir.join("people.csv");
    std::fs::write(&path, "id,name,city\n1,alice,LA\n2,bob,NY\n3,carol,\n").unwrap();
    path
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn start(csv: &Path) -> Server {
        Self::start_with_args(csv, &[])
    }

    fn start_with_args(csv: &Path, extra_args: &[&str]) -> Server {
        let binary = std::env::var_os("CARGO_BIN_EXE_sheetql")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target")
                    .join("debug")
                    .join("sheetql.exe")
            });
        let mut child = Command::new(binary)
            .arg("-S")
            .args(["-f", csv.to_str().unwrap()])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sheetql --server");
        let input = child.stdin.take().expect("child stdin");
        let output = BufReader::new(child.stdout.take().expect("child stdout"));
        Server {
            child,
            input,
            output,
        }
    }

    fn request(&mut self, line: &str) -> serde_json::Value {
        writeln!(self.input, "{line}").expect("write request");
        self.input.flush().expect("flush request");
        let mut response = String::new();
        let read = self.output.read_line(&mut response);
        if read.expect("read response") == 0 {
            let mut stderr = String::new();
            if let Some(mut err) = self.child.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            panic!("server exited before responding; stderr:\n{stderr}");
        }
        serde_json::from_str(&response).expect("valid JSON response")
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_some()
    }

    fn shutdown(mut self) {
        let response = self.request(r#"{ "id": "shutdown", "op": "exit" }"#);
        assert_eq!(
            response,
            serde_json::json!({ "id": "shutdown", "ok": true })
        );
        let status = self.child.wait().expect("wait");
        assert!(status.success());
    }
}

#[test]
fn server_supports_query_list_and_export() {
    let dir = temp_dir("basic");
    let csv = sample_csv(&dir);
    let export = dir.join("out.csv");

    let mut server = Server::start(&csv);

    let list = server.request(r#"{ "id": 1, "op": "list" }"#);
    assert_eq!(list["ok"], true);
    assert_eq!(list["id"], 1);
    let db = &list["data"]["databases"][0];
    assert!(db["name"].as_str().unwrap().ends_with("people_csv"));
    assert_eq!(db["tables"][0]["name"], "people");
    assert_eq!(
        db["tables"][0]["columns"],
        serde_json::json!(["id", "name", "city"])
    );

    let query =
        server.request(r#"{ "op": "query", "sql": "SELECT name, city FROM people ORDER BY id" }"#);
    assert_eq!(query["ok"], true);
    assert_eq!(query["columns"], serde_json::json!(["name", "city"]));
    assert_eq!(
        query["rows"],
        serde_json::json!([["alice", "LA"], ["bob", "NY"], ["carol", null]])
    );
    assert_eq!(query["stats"]["output_rows"], 3);
    assert_eq!(query["stats"]["input_rows"], 3);

    let export_req = server.request(
        &serde_json::json!({
            "op": "export",
            "sql": "SELECT name, city FROM people ORDER BY id",
            "path": export.to_str().unwrap(),
        })
        .to_string(),
    );
    assert_eq!(export_req["ok"], true);
    let content = std::fs::read_to_string(&export).expect("export file written");
    assert!(content.starts_with("name,city\n"));
    assert!(content.contains("alice,LA"));
    assert!(content.contains("carol,NULL"));

    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_recovers_from_errors_without_exiting() {
    let dir = temp_dir("errors");
    let csv = sample_csv(&dir);

    let mut server = Server::start(&csv);

    let bad_sql = server.request(r#"{ "op": "query", "sql": "SELECT nope FROM missing" }"#);
    assert_eq!(bad_sql["ok"], false);
    assert_eq!(bad_sql["code"], "query_error");
    assert!(bad_sql["error"].as_str().is_some());

    let bad_json = server.request(r#"this is not json"#);
    assert_eq!(bad_json["ok"], false);
    assert_eq!(bad_json["code"], "invalid_json");
    assert!(bad_json["error"].as_str().unwrap().contains("Invalid JSON"));

    let unknown = server.request(r#"{ "op": "warp" }"#);
    assert_eq!(unknown["ok"], false);
    assert_eq!(unknown["code"], "invalid_request");
    assert!(unknown["error"].as_str().unwrap().contains("Unknown op"));

    let ok_after =
        server.request(r#"{ "op": "query", "sql": "SELECT COUNT(*) AS n FROM people" }"#);
    assert_eq!(ok_after["ok"], true);
    assert_eq!(ok_after["rows"], serde_json::json!([[3]]));

    assert!(!server.exited(), "server must survive errors");
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_rejects_oversized_requests_and_continues() {
    let dir = temp_dir("request_limit");
    let csv = sample_csv(&dir);
    let mut server = Server::start(&csv);
    let oversized = format!(
        "{{\"op\":\"query\",\"sql\":\"{}\"}}",
        "x".repeat(1024 * 1024)
    );
    let response = server.request(&oversized);
    assert_eq!(response["ok"], false);
    assert_eq!(response["code"], "invalid_request");
    assert!(response["error"].as_str().unwrap().contains("byte limit"));

    let ok = server.request(r#"{ "op": "query", "sql": "SELECT COUNT(*) AS n FROM people" }"#);
    assert_eq!(ok["ok"], true);
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_applies_csv_options_before_query_execution() {
    let dir = temp_dir("csv_options");
    let csv = dir.join("values.csv");
    std::fs::write(&csv, "1;NA\n2;ok\n").unwrap();
    let mut server = Server::start_with_args(
        &csv,
        &["--delimiter", ";", "--no-header", "--null-value", "NA"],
    );
    let response = server
        .request(r#"{ "op": "query", "sql": "SELECT col1, col2 FROM values ORDER BY col1" }"#);
    assert_eq!(response["ok"], true);
    assert_eq!(response["rows"], serde_json::json!([[1, null], [2, "ok"]]));
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_returns_date_values_and_query_statistics() {
    let dir = temp_dir("date_stats");
    let csv = sample_csv(&dir);
    let mut server = Server::start(&csv);
    let response = server.request(
        r#"{ "op": "query", "sql": "SELECT DATE('2026/8/14') AS day, NOW() AS created" }"#,
    );
    assert_eq!(response["ok"], true);
    assert_eq!(response["rows"][0][0], "2026-08-14");
    assert!(response["rows"][0][1].as_str().unwrap().contains("2026-"));
    assert_eq!(response["stats"]["input_rows"], 1);
    assert_eq!(response["stats"]["output_rows"], 1);
    assert!(response["stats"]["elapsed_ms"].is_number());
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_rejects_input_over_max_rows_before_serving() {
    let dir = temp_dir("max_rows");
    let csv = sample_csv(&dir);
    let binary = std::env::var_os("CARGO_BIN_EXE_sheetql")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("sheetql.exe")
        });
    let output = Command::new(binary)
        .args(["-S", "-f", csv.to_str().unwrap(), "--max-rows", "2"])
        .stdin(Stdio::null())
        .output()
        .expect("spawn sheetql");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("maximum of 2 data rows"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cli_returns_nonzero_exit_codes_for_argument_and_query_errors() {
    let binary = std::env::var_os("CARGO_BIN_EXE_sheetql")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("sheetql.exe")
        });

    let argument_error = Command::new(&binary)
        .args(["--unknown"])
        .output()
        .expect("spawn sheetql");
    assert_eq!(argument_error.status.code(), Some(2));

    let query_error = Command::new(binary)
        .args([
            "-f",
            "test_data/people.csv",
            "-q",
            "SELECT nope FROM missing",
        ])
        .output()
        .expect("spawn sheetql");
    assert_eq!(query_error.status.code(), Some(4));
}

#[test]
fn server_exits_on_stdin_eof() {
    let dir = temp_dir("eof");
    let csv = sample_csv(&dir);

    let binary = std::env::var_os("CARGO_BIN_EXE_sheetql")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("debug")
                .join("sheetql.exe")
        });
    let mut child = Command::new(binary)
        .arg("-S")
        .args(["-f", csv.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sheetql --server");

    drop(child.stdin.take());
    let status = child.wait().expect("wait");
    assert!(status.success());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn server_skips_empty_lines() {
    let dir = temp_dir("blank");
    let csv = sample_csv(&dir);

    let mut server = Server::start(&csv);
    writeln!(server.input).unwrap();
    server.input.flush().unwrap();
    let query = server.request(r#"{ "op": "query", "sql": "SELECT COUNT(*) AS n FROM people" }"#);
    assert_eq!(query["ok"], true);
    assert_eq!(query["rows"], serde_json::json!([[3]]));
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_to_missing_directory_fails_without_stopping_server() {
    let dir = temp_dir("missing_dir");
    let csv = sample_csv(&dir);
    let missing =
        std::env::temp_dir().join(format!("sheetql_it_no_such_dir_{}", std::process::id()));
    let target = missing.join("out.csv");

    let mut server = Server::start(&csv);
    let response = server.request(
        &serde_json::json!({
            "op": "export",
            "sql": "SELECT name FROM people",
            "path": target.to_str().unwrap(),
        })
        .to_string(),
    );
    assert_eq!(response["ok"], false);
    assert!(response["error"].as_str().is_some());
    assert!(!server.exited(), "server must survive export failures");

    let ok = server.request(r#"{ "op": "query", "sql": "SELECT COUNT(*) AS n FROM people" }"#);
    assert_eq!(ok["ok"], true);
    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn query_db_scoping_is_temporary_at_process_level() {
    let dir = temp_dir("scoping");
    let csv = sample_csv(&dir);

    let mut server = Server::start(&csv);
    let list = server.request(r#"{ "op": "list" }"#);
    let db_name = list["data"]["databases"][0]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let current_before = list["data"]["current"].clone();

    let scoped = server.request(
        &serde_json::json!({
            "op": "query",
            "sql": "SELECT name FROM people ORDER BY id",
            "db": db_name,
        })
        .to_string(),
    );
    assert_eq!(scoped["ok"], true);
    assert_eq!(
        scoped["rows"],
        serde_json::json!([["alice"], ["bob"], ["carol"]])
    );

    let after = server.request(r#"{ "op": "list" }"#);
    assert_eq!(after["data"]["current"], current_before);

    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn export_respects_overwrite_false() {
    let dir = temp_dir("overwrite");
    let csv = sample_csv(&dir);
    let export = dir.join("out.csv");
    std::fs::write(&export, "keep me").unwrap();

    let mut server = Server::start(&csv);
    let blocked = server.request(
        &serde_json::json!({
            "op": "export",
            "sql": "SELECT name FROM people",
            "path": export.to_str().unwrap(),
            "overwrite": false,
        })
        .to_string(),
    );
    assert_eq!(blocked["ok"], false);
    assert!(
        blocked["error"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(std::fs::read_to_string(&export).unwrap(), "keep me");

    let overwritten = server.request(
        &serde_json::json!({
            "op": "export",
            "sql": "SELECT name FROM people",
            "path": export.to_str().unwrap(),
            "overwrite": true,
        })
        .to_string(),
    );
    assert_eq!(overwritten["ok"], true);
    assert!(
        std::fs::read_to_string(&export)
            .unwrap()
            .starts_with("name\n")
    );

    server.shutdown();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn piped_input_uses_non_interactive_mode() {
    let dir = temp_dir("piped");
    let csv = sample_csv(&dir);
    let mut child = Command::new(env!("CARGO_BIN_EXE_sheetql"))
        .args(["-f", csv.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sheetql");
    {
        let mut input = child.stdin.take().expect("child stdin");
        writeln!(input, "SELECT COUNT(*) AS n FROM people").expect("write query");
    }
    let output = child.wait_with_output().expect("wait for piped mode");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("n"), "stdout was: {stdout}");
    assert!(stdout.contains("3"), "stdout was: {stdout}");
    std::fs::remove_dir_all(&dir).ok();
}
