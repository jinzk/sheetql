use serde_json::json;
use unicode_width::UnicodeWidthStr;

use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Yaml,
}

pub fn render(format: OutputFormat, columns: &[String], rows: &[Vec<Value>]) -> String {
    match format {
        OutputFormat::Table => render_table(columns, rows),
        OutputFormat::Json => render_json(columns, rows),
        OutputFormat::Csv => render_csv(columns, rows),
        OutputFormat::Yaml => render_yaml(columns, rows),
    }
}

pub(crate) fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Int(number) => json!(number),
        Value::Float(number) => {
            if number.is_finite() {
                json!(number)
            } else {
                serde_json::Value::Null
            }
        }
        Value::Bool(boolean) => json!(boolean),
        Value::Text(text) => json!(text),
        Value::Date(text) | Value::DateTime(text) => json!(text),
        Value::Null => serde_json::Value::Null,
    }
}

fn rows_to_objects(columns: &[String], rows: &[Vec<Value>]) -> Vec<serde_json::Value> {
    // Disambiguate duplicate column names instead of silently overwriting
    // earlier values when building JSON/YAML objects.
    let keys: Vec<String> = {
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        columns
            .iter()
            .map(|column| {
                let count = seen.entry(column.as_str()).or_insert(0);
                *count += 1;
                if *count == 1 {
                    column.clone()
                } else {
                    format!("{column}_{count}")
                }
            })
            .collect()
    };

    let mut objects = vec![];
    for row in rows {
        let mut object = serde_json::Map::new();
        for (index, key) in keys.iter().enumerate() {
            object.insert(
                key.clone(),
                value_to_json(row.get(index).unwrap_or(&Value::Null)),
            );
        }
        objects.push(serde_json::Value::Object(object));
    }
    objects
}

fn render_json(columns: &[String], rows: &[Vec<Value>]) -> String {
    let objects = rows_to_objects(columns, rows);
    serde_json::to_string_pretty(&objects).unwrap_or_else(|_| "[]".to_string())
}

fn render_yaml(columns: &[String], rows: &[Vec<Value>]) -> String {
    let objects = rows_to_objects(columns, rows);
    serde_yaml::to_string(&objects).unwrap_or_else(|_| "[]\n".to_string())
}

fn render_csv(columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut writer = csv::Writer::from_writer(vec![]);
    if writer.write_record(columns).is_err() {
        return String::new();
    }
    for row in rows {
        let fields = (0..columns.len())
            .map(|index| row.get(index).unwrap_or(&Value::Null).to_display_string());
        if writer.write_record(fields).is_err() {
            return String::new();
        }
    }
    let bytes = writer.into_inner().unwrap_or_default();
    String::from_utf8(bytes).unwrap_or_default()
}

fn render_table(columns: &[String], rows: &[Vec<Value>]) -> String {
    if columns.is_empty() {
        return String::new();
    }

    let cell_texts: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| row.get(index).unwrap_or(&Value::Null).to_display_string())
                .collect()
        })
        .collect();

    let mut widths: Vec<usize> = columns.iter().map(|column| column.width()).collect();
    for row in &cell_texts {
        for (index, text) in row.iter().enumerate() {
            let width = text.width();
            if width > widths[index] {
                widths[index] = width;
            }
        }
    }

    let mut output = String::new();
    let header = columns
        .iter()
        .enumerate()
        .map(|(index, column)| pad(column, widths[index] + 2))
        .collect::<Vec<_>>()
        .join("│");

    output.push_str(&format!("╭{}╮\n", top_border(&widths)));
    output.push_str(&format!("│{}│\n", header));
    output.push_str(&format!("╞{}╡\n", header_border(&widths)));

    for (index, row) in cell_texts.iter().enumerate() {
        let line = row
            .iter()
            .enumerate()
            .map(|(column_index, text)| pad(text, widths[column_index] + 2))
            .collect::<Vec<_>>()
            .join("│");
        output.push_str(&format!("│{}│\n", line));
        if index + 1 < cell_texts.len() {
            output.push_str(&format!("├{}┤\n", middle_border(&widths)));
        }
    }

    output.push_str(&format!("╰{}╯\n", bottom_border(&widths)));
    output
}

fn pad(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(text.width());
    let left = padding / 2;
    let right = padding - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

fn border(widths: &[usize], fill: char, join: char) -> String {
    widths
        .iter()
        .map(|width| fill.to_string().repeat(width + 2))
        .collect::<Vec<_>>()
        .join(&join.to_string())
}

fn top_border(widths: &[usize]) -> String {
    border(widths, '─', '┬')
}

fn header_border(widths: &[usize]) -> String {
    border(widths, '═', '╪')
}

fn middle_border(widths: &[usize]) -> String {
    border(widths, '─', '┼')
}

fn bottom_border(widths: &[usize]) -> String {
    border(widths, '─', '┴')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rendered line of a table must share the same terminal display
    /// width so the box-drawing borders line up, even with CJK (wide) characters.
    fn line_widths(table: &str) -> Vec<usize> {
        table.lines().map(|line| line.width()).collect()
    }

    #[test]
    fn table_rows_align_with_cjk() {
        let columns = vec!["Tables".to_string(), "列".to_string()];
        let rows = vec![vec![
            Value::Text("商品销售明细".to_string()),
            Value::Text("值".to_string()),
        ]];
        let widths = line_widths(&render_table(&columns, &rows));
        assert!(!widths.is_empty());
        let expected = widths[0];
        for (index, width) in widths.iter().enumerate() {
            assert_eq!(*width, expected, "line {index} has mismatched width");
        }
    }

    #[test]
    fn table_rows_align_with_ascii() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![Value::Int(1), Value::Text("Alice".to_string())],
            vec![Value::Int(2), Value::Text("Bob".to_string())],
        ];
        let widths = line_widths(&render_table(&columns, &rows));
        let expected = widths[0];
        for width in widths {
            assert_eq!(width, expected);
        }
    }

    #[test]
    fn json_renders_typed_values_and_types_as_null() {
        let columns = vec!["n".to_string(), "b".to_string(), "t".to_string()];
        let rows = vec![vec![
            Value::Int(1),
            Value::Bool(true),
            Value::Text("x".to_string()),
        ]];
        let out = render(OutputFormat::Json, &columns, &rows);
        assert!(out.contains("\"n\": 1"), "got: {out}");
        assert!(out.contains("\"b\": true"), "got: {out}");
        assert!(out.contains("\"t\": \"x\""), "got: {out}");
    }

    #[test]
    fn json_maps_non_finite_floats_to_null() {
        let columns = vec!["n".to_string()];
        let rows = vec![vec![Value::Float(f64::NAN)]];
        let out = render(OutputFormat::Json, &columns, &rows);
        assert!(out.contains("\"n\": null"), "got: {out}");
    }

    #[test]
    fn json_and_csv_preserve_date_values_as_strings() {
        let columns = vec!["day".to_string(), "created".to_string()];
        let rows = vec![vec![
            Value::Date("2026-08-14".into()),
            Value::DateTime("2026-08-14 10:30:00".into()),
        ]];
        let json = render(OutputFormat::Json, &columns, &rows);
        assert!(json.contains("2026-08-14"));
        assert!(json.contains("2026-08-14 10:30:00"));
        assert_eq!(
            render(OutputFormat::Csv, &columns, &rows),
            "day,created\n2026-08-14,2026-08-14 10:30:00\n"
        );
    }

    #[test]
    fn json_and_yaml_disambiguate_duplicate_column_names() {
        let columns = vec!["a".to_string(), "a".to_string()];
        let rows = vec![vec![Value::Int(1), Value::Int(2)]];
        let json = render(OutputFormat::Json, &columns, &rows);
        assert!(
            json.contains("\"a\": 1") && json.contains("\"a_2\": 2"),
            "got: {json}"
        );
        let yaml = render(OutputFormat::Yaml, &columns, &rows);
        assert!(
            yaml.contains("a: 1") && yaml.contains("a_2: 2"),
            "got: {yaml}"
        );
    }

    #[test]
    fn csv_renders_header_and_quoted_fields() {
        let columns = vec!["name".to_string(), "note".to_string()];
        let rows = vec![vec![
            Value::Text("Alice, B.".to_string()),
            Value::Text("say \"hi\"".to_string()),
        ]];
        let out = render(OutputFormat::Csv, &columns, &rows);
        assert!(out.starts_with("name,note\n"), "got: {out}");
        assert!(out.contains("\"Alice, B.\""), "got: {out}");
        assert!(out.contains("\"say \"\"hi\"\"\""), "got: {out}");
    }

    #[test]
    fn yaml_renders_rows() {
        let columns = vec!["k".to_string()];
        let rows = vec![vec![Value::Text("v".to_string())]];
        let out = render(OutputFormat::Yaml, &columns, &rows);
        assert!(out.contains("k: v"), "got: {out}");
    }
}
