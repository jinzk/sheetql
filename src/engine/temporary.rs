use sqlparser::ast::{
    CreateTable, Expr, Query, SetExpr, TableObject, Value as SqlValue, ValueWithSpan,
};

use crate::database::{Schema, Table};
use crate::engine::QueryResult;
use crate::engine::select::execute_select_query;
use crate::error::Error;
use crate::value::Value;

pub(crate) fn run_create_table(
    schema: &mut Schema,
    create: &CreateTable,
) -> Result<QueryResult, Error> {
    if !create.temporary {
        return Err("Only temporary tables are supported".into());
    }
    let name = crate::engine::select::object_name_to_parts(&create.name);
    let table_name = match name.as_slice() {
        [name] => name.clone(),
        _ => return Err("Temporary table name must be unqualified".into()),
    };
    if create.query.is_none() {
        if create.columns.is_empty() {
            return Err("Temporary tables require columns or AS SELECT".into());
        }
        let columns = create
            .columns
            .iter()
            .map(|column| column.name.value.to_lowercase())
            .collect();
        schema.add_temporary_table(Table {
            name: table_name.clone(),
            columns,
            rows: Vec::new(),
        });
        return Ok(status(&table_name, "created"));
    }
    if !create.columns.is_empty() {
        return Err("CREATE TEMPORARY TABLE with columns cannot also use AS SELECT".into());
    }
    let query: &Query = create
        .query
        .as_deref()
        .ok_or("Temporary tables require `AS SELECT ...`")?;
    let result = execute_select_query(schema, query)?;
    schema.add_temporary_table(Table {
        name: table_name.clone(),
        columns: result.columns,
        rows: result.rows,
    });
    Ok(QueryResult {
        columns: vec!["Status".to_string()],
        rows: vec![vec![Value::Text(format!(
            "Temporary table `{table_name}` created"
        ))]],
        stats: Default::default(),
    })
}

pub(crate) fn run_insert(
    schema: &mut Schema,
    insert: &sqlparser::ast::Insert,
) -> Result<QueryResult, Error> {
    let TableObject::TableName(name) = &insert.table else {
        return Err("INSERT target must be a table name".into());
    };
    let parts: Vec<String> = name
        .0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.to_lowercase())
        .collect();
    let [table_name] = parts.as_slice() else {
        return Err("INSERT target must be an unqualified temporary table".into());
    };
    let (table_columns, table_exists) = match schema.get_temporary_table(table_name) {
        Some(table) => (table.columns.clone(), true),
        None => (Vec::new(), false),
    };
    if !table_exists {
        return Err(format!("Temporary table `{table_name}` not found").into());
    }
    let Some(source) = &insert.source else {
        return Err("INSERT requires VALUES or SELECT".into());
    };
    match source.body.as_ref() {
        SetExpr::Values(values) => {
            let target_indices = insert_column_indices(&insert.columns, &table_columns)?;
            let mut rows = Vec::with_capacity(values.rows.len());
            for row in &values.rows {
                if row.content.len() != target_indices.len() {
                    return Err("INSERT row column count does not match temporary table".into());
                }
                let values = row
                    .content
                    .iter()
                    .map(eval_literal)
                    .collect::<Result<Vec<_>, _>>()?;
                let mut target_row = vec![Value::Null; table_columns.len()];
                for (value, target_index) in values.into_iter().zip(&target_indices) {
                    target_row[*target_index] = value;
                }
                rows.push(target_row);
            }
            let count = rows.len();
            let table = schema
                .get_temporary_table_mut(table_name)
                .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
            table.rows.extend(rows);
            Ok(QueryResult {
                columns: vec!["Status".into()],
                rows: vec![vec![Value::Text(format!("Inserted {count} row(s)"))]],
                stats: Default::default(),
            })
        }
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::SetOperation { .. } => {
            let result = crate::engine::set::execute_query(schema, source)?;
            let target_indices = insert_column_indices(&insert.columns, &table_columns)?;
            if result.columns.len() != target_indices.len() {
                return Err("INSERT SELECT column count does not match target columns".into());
            }
            let mut rows = Vec::with_capacity(result.rows.len());
            for source_row in result.rows {
                let mut target_row = vec![Value::Null; table_columns.len()];
                for (source_index, target_index) in target_indices.iter().enumerate() {
                    target_row[*target_index] =
                        source_row.get(source_index).cloned().unwrap_or(Value::Null);
                }
                rows.push(target_row);
            }
            let count = rows.len();
            let table = schema
                .get_temporary_table_mut(table_name)
                .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
            table.rows.extend(rows);
            Ok(QueryResult {
                columns: vec!["Status".into()],
                rows: vec![vec![Value::Text(format!("Inserted {count} row(s)"))]],
                stats: Default::default(),
            })
        }
        _ => Err("INSERT requires VALUES or SELECT".into()),
    }
}

fn insert_column_indices(
    columns: &[sqlparser::ast::ObjectName],
    target_columns: &[String],
) -> Result<Vec<usize>, Error> {
    if columns.is_empty() {
        return Ok((0..target_columns.len()).collect());
    }
    let mut indices = Vec::with_capacity(columns.len());
    for column in columns {
        let parts = crate::engine::select::object_name_to_parts(column);
        let [name] = parts.as_slice() else {
            return Err("INSERT column names must be unqualified".into());
        };
        let index = target_columns
            .iter()
            .position(|target| target.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("Temporary table column `{name}` not found"))?;
        if indices.contains(&index) {
            return Err(format!("INSERT column `{name}` is specified more than once").into());
        }
        indices.push(index);
    }
    Ok(indices)
}

fn eval_literal(expr: &Expr) -> Result<Value, Error> {
    match expr {
        Expr::Value(ValueWithSpan { value, .. }) => match value {
            SqlValue::Number(number, _) => {
                if let Ok(value) = number.parse::<i64>() {
                    Ok(Value::Int(value))
                } else {
                    number
                        .parse::<f64>()
                        .map(Value::Float)
                        .map_err(|_| "INSERT numeric value is invalid".into())
                }
            }
            SqlValue::SingleQuotedString(value) | SqlValue::DoubleQuotedString(value) => {
                Ok(Value::Text(value.clone()))
            }
            SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
            SqlValue::Null => Ok(Value::Null),
            _ => Err("INSERT VALUES accepts only scalar literals".into()),
        },
        _ => Err("INSERT VALUES accepts only scalar literals".into()),
    }
}

fn status(table_name: &str, action: &str) -> QueryResult {
    QueryResult {
        columns: vec!["Status".into()],
        rows: vec![vec![Value::Text(format!(
            "Temporary table `{table_name}` {action}"
        ))]],
        stats: Default::default(),
    }
}
