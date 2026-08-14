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

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Int(number) => json!(number),
        Value::Float(number) => json!(number),
        Value::Bool(boolean) => json!(boolean),
        Value::Text(text) => json!(text),
        Value::Null => serde_json::Value::Null,
    }
}

fn rows_to_objects(columns: &[String], rows: &[Vec<Value>]) -> Vec<serde_json::Value> {
    let mut objects = vec![];
    for row in rows {
        let mut object = serde_json::Map::new();
        for (index, column) in columns.iter().enumerate() {
            object.insert(column.clone(), value_to_json(row.get(index).unwrap_or(&Value::Null)));
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
    let mut output = String::new();
    output.push_str(&columns.iter().map(|column| csv_escape(column)).collect::<Vec<_>>().join(","));
    output.push('\n');
    for row in rows {
        let fields: Vec<String> = (0..columns.len())
            .map(|index| {
                csv_escape(&row.get(index).unwrap_or(&Value::Null).to_display_string())
            })
            .collect();
        output.push_str(&fields.join(","));
        output.push('\n');
    }
    output
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
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
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(right)
    )
}

fn top_border(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┬")
}

fn header_border(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "═".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("╪")
}

fn middle_border(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┼")
}

fn bottom_border(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "─".repeat(width + 2))
        .collect::<Vec<_>>()
        .join("┴")
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
}