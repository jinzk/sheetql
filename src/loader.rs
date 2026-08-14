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

pub fn load_schema(files: &[String]) -> Result<Schema, String> {
    let mut schema = Schema::new();
    for file in files {
        let database = load_database(file)?;
        schema.add_database(database);
    }
    if schema.database_names().len() == 1 {
        let name = schema.database_names()[0].to_string();
        schema.set_current_database(&name)?;
    }
    Ok(schema)
}

pub fn load_database(path: &str) -> Result<Database, String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
        .ok_or_else(|| format!("Cannot detect extension of file `{path}`"))?;

    let mut database = Database::named(database_name(path));
    match extension.as_str() {
        "xls" | "xlsx" | "xlsm" => load_spreadsheet(&mut database, path, &extension)?,
        "csv" => load_csv(&mut database, path)?,
        _ => {
            return Err(format!(
                "Unsupported file format `.{extension}`, expected one of: xls, xlsx, xlsm, csv"
            ));
        }
    }
    Ok(database)
}

fn load_spreadsheet(database: &mut Database, path: &str, extension: &str) -> Result<(), String> {
    let file = File::open(path).map_err(|error| format!("Cannot open file `{path}`: {error}"))?;
    let reader = BufReader::new(file);

    if extension == "xls" {
        let mut workbook = Xls::new(reader).map_err(|error| format!("Cannot read file `{path}`: {error}"))?;
        load_workbook(database, path, &mut workbook)
    } else {
        let mut workbook = Xlsx::new(reader).map_err(|error| format!("Cannot read file `{path}`: {error}"))?;
        load_workbook(database, path, &mut workbook)
    }
}

fn load_workbook<R: Reader<BufReader<File>>>(
    database: &mut Database,
    path: &str,
    workbook: &mut R,
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

fn load_csv(database: &mut Database, path: &str) -> Result<(), String> {
    let mut reader = csv::ReaderBuilder::new()
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
            None => {
                columns = build_columns(record.iter().map(|value| value.to_string()).collect());
                header = Some(record);
            }
            Some(_) => {
                let values = build_row_values(record.iter().collect(), columns.len());
                if values.iter().all(Value::is_null) {
                    continue;
                }
                rows.push(values);
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

fn build_row_values(cells: Vec<&str>, column_count: usize) -> Vec<Value> {
    let mut values: Vec<Value> = Vec::with_capacity(column_count);
    for index in 0..column_count {
        let cell = cells.get(index).copied().unwrap_or("");
        values.push(parse_cell(cell));
    }
    values
}

fn parse_cell(cell: &str) -> Value {
    if cell.is_empty() {
        return Value::Null;
    }

    if let Ok(value) = cell.parse::<i64>() {
        return Value::Int(value);
    }

    if let Ok(value) = cell.parse::<f64>() {
        return Value::Float(value);
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
        Cell::DateTime(value) => Value::Text(format_excel_datetime(value)),
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
        assert_eq!(parse_cell("0012"), Value::Int(12));
        assert_eq!(parse_cell("a1b2"), Value::Text("a1b2".to_string()));
    }

    #[test]
    fn datetime_cell_becomes_readable_date_not_serial() {
        let serial = calamine::ExcelDateTime::new(46149.0, calamine::ExcelDateTimeType::DateTime, false);
        let cell = calamine::Data::DateTime(serial);
        let expected = match &cell {
            calamine::Data::DateTime(v) => format_excel_datetime(v),
            _ => unreachable!(),
        };
        assert_eq!(cell_value(&cell), Value::Text(expected.clone()));
        assert!(expected.contains('/'), "expected a date like Y/M/D, got {expected}");
        assert!(!expected.contains("46149"), "serial leaked into output: {expected}");
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
        assert_eq!(values, vec![Value::Int(1), Value::Text("Alice".to_string()), Value::Null]);
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