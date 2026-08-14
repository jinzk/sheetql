use serde_json::json;

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

    let mut widths: Vec<usize> = columns.iter().map(|column| column.len()).collect();
    for row in &cell_texts {
        for (index, text) in row.iter().enumerate() {
            if text.len() > widths[index] {
                widths[index] = text.len();
            }
        }
    }

    let mut output = String::new();
    let header = columns
        .iter()
        .enumerate()
        .map(|(index, column)| pad(column, widths[index]))
        .collect::<Vec<_>>()
        .join("│");

    output.push_str(&format!("╭{}╮\n", top_border(&widths)));
    output.push_str(&format!("│{}│\n", header));
    output.push_str(&format!("╞{}╡\n", header_border(&widths)));

    for (index, row) in cell_texts.iter().enumerate() {
        let line = row
            .iter()
            .enumerate()
            .map(|(column_index, text)| pad(text, widths[column_index]))
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
    let padding = width.saturating_sub(text.len());
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