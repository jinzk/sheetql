use super::*;
use crate::engine::strip_into_outfile;
use crate::engine::tests::make_schema;

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
fn strip_into_outfile_ignores_block_comments() {
    let (rest, path) = strip_into_outfile("SELECT 1 /* INTO OUTFILE 'acc.csv' */ FROM people");
    assert_eq!(rest, "SELECT 1 /* INTO OUTFILE 'acc.csv' */ FROM people");
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