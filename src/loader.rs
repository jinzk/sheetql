use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use calamine::{Data as Cell, ExcelDateTime, Reader, Xls, Xlsx};

use crate::database::Database;
use crate::database::Schema;
use crate::database::Table;
use crate::naming::csv_table_name;
use crate::naming::database_name;
use crate::naming::spreadsheet_table_name;
use crate::value::Value;

const SUPPORTED_EXTENSIONS: [&str; 4] = ["xls", "xlsx", "xlsm", "csv"];

pub fn is_supported_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn load_schema(
    files: &[String],
    max_rows: Option<usize>,
    csv_options: &CsvOptions,
) -> Result<Schema, String> {
    let mut schema = Schema::new();
    for file in files {
        let database = load_database_with_options(file, max_rows, csv_options)?;
        schema.add_database(database);
    }
    if schema.database_names().len() == 1 {
        let name = schema.database_names()[0].to_string();
        schema.set_current_database(&name)?;
    }
    Ok(schema)
}

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: u8,
    pub has_header: bool,
    pub null_value: Option<String>,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: b',',
            has_header: true,
            null_value: None,
        }
    }
}

pub fn load_database_with_options(
    path: &str,
    max_rows: Option<usize>,
    csv_options: &CsvOptions,
) -> Result<Database, String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
        .ok_or_else(|| format!("Cannot detect extension of file `{path}`"))?;

    let mut database = Database::named(database_name(path));
    match extension.as_str() {
        "xls" | "xlsx" | "xlsm" => load_spreadsheet(&mut database, path, &extension, max_rows)?,
        "csv" => load_csv(&mut database, path, max_rows, csv_options)?,
        _ => {
            return Err(format!(
                "Unsupported file format `.{extension}`, expected one of: xls, xlsx, xlsm, csv"
            ));
        }
    }
    Ok(database)
}

fn load_spreadsheet(
    database: &mut Database,
    path: &str,
    extension: &str,
    max_rows: Option<usize>,
) -> Result<(), String> {
    let file = File::open(path).map_err(|error| format!("Cannot open file `{path}`: {error}"))?;
    let reader = BufReader::new(file);

    if extension == "xls" {
        let mut workbook =
            Xls::new(reader).map_err(|error| format!("Cannot read file `{path}`: {error}"))?;
        load_workbook(database, path, &mut workbook, max_rows)
    } else {
        let mut workbook =
            Xlsx::new(reader).map_err(|error| format!("Cannot read file `{path}`: {error}"))?;
        load_workbook(database, path, &mut workbook, max_rows)
    }
}

fn load_workbook<R: Reader<BufReader<File>>>(
    database: &mut Database,
    path: &str,
    workbook: &mut R,
    max_rows: Option<usize>,
) -> Result<(), String>
where
    R::Error: std::fmt::Display,
{
    let sheet_names = workbook.sheet_names().to_vec();
    if sheet_names.is_empty() {
        return Err(format!("Workbook `{path}` has no sheets"));
    }

    for sheet_name in sheet_names {
        let range = workbook
            .worksheet_range(&sheet_name)
            .map_err(|error| format!("Cannot read sheet `{sheet_name}` from `{path}`: {error}"))?;

        let mut rows_iter = range.rows();
        let header = match rows_iter.next() {
            Some(header) => header,
            None => continue,
        };

        let columns = build_columns(header.iter().map(|cell| cell.to_string()).collect());
        let mut rows: Vec<Vec<Value>> = vec![];
        for row in rows_iter {
            let mut values: Vec<Value> = Vec::with_capacity(columns.len());
            for index in 0..columns.len() {
                match row.get(index) {
                    Some(cell) => values.push(cell_value(cell)),
                    None => values.push(Value::Null),
                }
            }
            if values.iter().all(Value::is_null) {
                continue;
            }
            rows.push(values);
            enforce_row_limit(path, &sheet_name, rows.len(), max_rows)?;
        }

        let name = spreadsheet_table_name(&sheet_name);
        database.add_table(Table {
            name,
            columns,
            rows,
        });
    }

    Ok(())
}

fn load_csv(
    database: &mut Database,
    path: &str,
    max_rows: Option<usize>,
    options: &CsvOptions,
) -> Result<(), String> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(options.delimiter)
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .map_err(|error| format!("Cannot open file `{path}`: {error}"))?;

    let mut header: Option<csv::StringRecord> = None;
    let mut columns: Vec<String> = vec![];
    let mut rows: Vec<Vec<Value>> = vec![];

    for record in reader.records() {
        let record = record.map_err(|error| format!("Cannot parse CSV file `{path}`: {error}"))?;
        match &header {
            None if options.has_header => {
                columns = build_columns(record.iter().map(|value| value.to_string()).collect());
                header = Some(record);
            }
            None => {
                columns = (0..record.len())
                    .map(|index| format!("col{}", index + 1))
                    .collect();
                header = Some(csv::StringRecord::new());
                let values = build_row_values_with_null(
                    record.iter().collect(),
                    columns.len(),
                    options.null_value.as_deref(),
                );
                if !values.iter().all(Value::is_null) {
                    rows.push(values);
                    enforce_row_limit(path, "", rows.len(), max_rows)?;
                }
            }
            Some(_) => {
                if record.len() > columns.len() {
                    let line = record
                        .position()
                        .map(|position| position.line())
                        .unwrap_or(0);
                    return Err(format!(
                        "CSV row {line} has {} fields but the header has {} columns",
                        record.len(),
                        columns.len()
                    ));
                }
                let values = build_row_values_with_null(
                    record.iter().collect(),
                    columns.len(),
                    options.null_value.as_deref(),
                );
                if values.iter().all(Value::is_null) {
                    continue;
                }
                rows.push(values);
                enforce_row_limit(path, "", rows.len(), max_rows)?;
            }
        }
    }

    if header.is_none() {
        return Ok(());
    }

    let name = csv_table_name(path);
    database.add_table(Table {
        name,
        columns,
        rows,
    });

    Ok(())
}

fn enforce_row_limit(
    path: &str,
    sheet: &str,
    row_count: usize,
    max_rows: Option<usize>,
) -> Result<(), String> {
    if let Some(max_rows) = max_rows
        && row_count > max_rows
    {
        let source = if sheet.is_empty() {
            format!("file `{path}`")
        } else {
            format!("sheet `{sheet}` in `{path}`")
        };
        return Err(format!(
            "Input {source} exceeds the maximum of {max_rows} data rows"
        ));
    }
    Ok(())
}

fn build_row_values_with_null(
    cells: Vec<&str>,
    column_count: usize,
    null_value: Option<&str>,
) -> Vec<Value> {
    let mut values: Vec<Value> = Vec::with_capacity(column_count);
    for index in 0..column_count {
        let cell = cells.get(index).copied().unwrap_or("");
        values.push(if null_value == Some(cell) {
            Value::Null
        } else {
            parse_cell(cell)
        });
    }
    values
}

#[cfg(test)]
fn build_row_values(cells: Vec<&str>, column_count: usize) -> Vec<Value> {
    build_row_values_with_null(cells, column_count, None)
}

fn parse_cell(cell: &str) -> Value {
    if cell.is_empty() {
        return Value::Null;
    }

    if has_leading_zero(cell) {
        return Value::Text(cell.to_string());
    }

    if let Ok(value) = cell.parse::<i64>() {
        return Value::Int(value);
    }

    // Keep whole numbers that don't fit in i64 as text instead of silently
    // losing precision when parsed as f64.
    if is_whole_number(cell) {
        return Value::Text(cell.to_string());
    }

    if let Ok(value) = cell.parse::<f64>() {
        if value.is_finite() {
            return Value::Float(value);
        }
        return Value::Text(cell.to_string());
    }

    let lower = cell.to_lowercase();
    if lower == "true" {
        return Value::Bool(true);
    }
    if lower == "false" {
        return Value::Bool(false);
    }

    Value::Text(cell.to_string())
}

fn is_whole_number(cell: &str) -> bool {
    let digits = cell.strip_prefix('-').unwrap_or(cell);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn has_leading_zero(cell: &str) -> bool {
    let digits = cell.strip_prefix('-').unwrap_or(cell);
    digits.len() > 1 && digits.starts_with('0') && digits.chars().all(|c| c.is_ascii_digit())
}

fn build_columns(headers: Vec<String>) -> Vec<String> {
    let mut columns: Vec<String> = vec![];
    let mut used: HashSet<String> = HashSet::new();

    for (index, header) in headers.iter().enumerate() {
        let mut name = crate::naming::sanitize(header);
        if name.is_empty() {
            name = format!("col{}", index + 1);
        }

        let base_name = name.clone();
        let mut counter = 1;
        while used.contains(&name) {
            name = format!("{}_{}", base_name, counter);
            counter += 1;
        }

        used.insert(name.clone());
        columns.push(name);
    }

    columns
}

fn cell_value(cell: &Cell) -> Value {
    match cell {
        Cell::Empty | Cell::Error(_) => Value::Null,
        Cell::Int(value) => Value::Int(*value),
        Cell::Float(value) => Value::Float(*value),
        Cell::String(value) => Value::Text(value.clone()),
        Cell::Bool(value) => Value::Bool(*value),
        Cell::DateTime(value) => {
            let formatted = format_excel_datetime(value);
            if formatted.len() > 10 {
                Value::DateTime(formatted)
            } else {
                Value::Date(formatted)
            }
        }
        Cell::DateTimeIso(_) | Cell::DurationIso(_) => Value::Text(cell.to_string()),
    }
}

fn format_excel_datetime(value: &ExcelDateTime) -> String {
    let (year, month, day, hour, minute, second, milli) = value.to_ymd_hms_milli();
    if hour == 0 && minute == 0 && second == 0 && milli == 0 {
        format!("{year}/{month}/{day}")
    } else {
        format!("{year}/{month}/{day} {hour}:{minute}:{second}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cell_infers_types() {
        assert_eq!(parse_cell("42"), Value::Int(42));
        assert_eq!(parse_cell("-7"), Value::Int(-7));
        assert_eq!(parse_cell("2.5"), Value::Float(2.5));
        assert_eq!(parse_cell("true"), Value::Bool(true));
        assert_eq!(parse_cell("FALSE"), Value::Bool(false));
        assert_eq!(parse_cell(""), Value::Null);
        assert_eq!(parse_cell("Alice"), Value::Text("Alice".to_string()));
        assert_eq!(parse_cell("0012"), Value::Text("0012".to_string()));
        assert_eq!(parse_cell("007"), Value::Text("007".to_string()));
        assert_eq!(parse_cell("0"), Value::Int(0));
        assert_eq!(parse_cell("-007"), Value::Text("-007".to_string()));
        assert_eq!(parse_cell("a1b2"), Value::Text("a1b2".to_string()));
    }

    #[test]
    fn parse_cell_keeps_overflowing_whole_numbers_as_text() {
        // Larger than i64::MAX: must not silently lose precision as f64.
        assert_eq!(
            parse_cell("123456789012345678901234567890"),
            Value::Text("123456789012345678901234567890".to_string())
        );
        assert_eq!(
            parse_cell("-123456789012345678901234567890"),
            Value::Text("-123456789012345678901234567890".to_string())
        );
    }

    #[test]
    fn parse_cell_rejects_non_finite_floats() {
        assert_eq!(parse_cell("NaN"), Value::Text("NaN".to_string()));
        assert_eq!(parse_cell("inf"), Value::Text("inf".to_string()));
        assert_eq!(parse_cell("-inf"), Value::Text("-inf".to_string()));
    }

    #[test]
    fn load_csv_rejects_rows_with_more_fields_than_headers() {
        let path = std::env::temp_dir().join(format!("sheetql_bad_row_{}.csv", std::process::id()));
        std::fs::write(&path, "a,b\n1,2,3\n").unwrap();
        let mut database = crate::database::Database::new();
        let result = load_csv(
            &mut database,
            &path.to_string_lossy(),
            None,
            &CsvOptions {
                delimiter: b',',
                has_header: true,
                null_value: None,
            },
        );
        let error = result.expect_err("extra fields should be rejected");
        assert!(error.contains("3 fields"), "got: {error}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_csv_pads_short_rows_with_null() {
        let path =
            std::env::temp_dir().join(format!("sheetql_short_row_{}.csv", std::process::id()));
        std::fs::write(&path, "a,b\n1\n").unwrap();
        let mut database = crate::database::Database::new();
        load_csv(
            &mut database,
            &path.to_string_lossy(),
            None,
            &CsvOptions {
                delimiter: b',',
                has_header: true,
                null_value: None,
            },
        )
        .unwrap();
        let table = database.tables.first().unwrap();
        assert_eq!(table.rows[0], vec![Value::Int(1), Value::Null]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_csv_rejects_rows_over_configured_limit() {
        let path =
            std::env::temp_dir().join(format!("sheetql_max_rows_{}.csv", std::process::id()));
        std::fs::write(&path, "a\n1\n2\n").unwrap();
        let mut database = crate::database::Database::new();
        let error = load_csv(
            &mut database,
            &path.to_string_lossy(),
            Some(1),
            &CsvOptions {
                delimiter: b',',
                has_header: true,
                null_value: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("maximum of 1 data rows"), "got: {error}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_csv_supports_custom_delimiter_null_value_and_no_header() {
        let path =
            std::env::temp_dir().join(format!("sheetql_csv_options_{}.csv", std::process::id()));
        std::fs::write(&path, "1;NA\n2;ok\n").unwrap();
        let mut database = crate::database::Database::new();
        load_csv(
            &mut database,
            &path.to_string_lossy(),
            None,
            &CsvOptions {
                delimiter: b';',
                has_header: false,
                null_value: Some("NA".to_string()),
            },
        )
        .unwrap();
        let table = database.tables.first().unwrap();
        assert_eq!(table.columns, vec!["col1", "col2"]);
        assert_eq!(table.rows[0], vec![Value::Int(1), Value::Null]);
        assert_eq!(table.rows[1], vec![Value::Int(2), Value::Text("ok".into())]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn datetime_cell_becomes_readable_date_not_serial() {
        let serial =
            calamine::ExcelDateTime::new(46149.0, calamine::ExcelDateTimeType::DateTime, false);
        let cell = calamine::Data::DateTime(serial);
        let expected = match &cell {
            calamine::Data::DateTime(v) => format_excel_datetime(v),
            _ => unreachable!(),
        };
        assert_eq!(cell_value(&cell), Value::Date(expected.clone()));
        assert!(
            expected.contains('/'),
            "expected a date like Y/M/D, got {expected}"
        );
        assert!(
            !expected.contains("46149"),
            "serial leaked into output: {expected}"
        );
    }

    #[test]
    fn build_columns_sanitizes_dedups_and_fills_empty() {
        let columns = build_columns(vec![
            "Name".to_string(),
            "".to_string(),
            "Name".to_string(),
            "Na-me".to_string(),
        ]);
        assert_eq!(columns, vec!["name", "col2", "name_1", "na_me"]);
    }

    #[test]
    fn build_row_values_pads_to_column_count() {
        let values = build_row_values(vec!["1", "Alice"], 3);
        assert_eq!(
            values,
            vec![Value::Int(1), Value::Text("Alice".to_string()), Value::Null]
        );
    }

    #[test]
    fn is_supported_file_accepts_known_extensions() {
        assert!(is_supported_file("a.csv"));
        assert!(is_supported_file("a.xls"));
        assert!(is_supported_file("a.xlsx"));
        assert!(is_supported_file("a.xlsm"));
        assert!(is_supported_file("a.CSV"));
        assert!(!is_supported_file("a.txt"));
        assert!(!is_supported_file("no_extension"));
    }
}
