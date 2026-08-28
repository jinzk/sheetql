use sqlparser::ast::{
    AssignmentTarget, CreateTable, Expr, Query, SetExpr, TableFactor, TableObject,
    Value as SqlValue, ValueWithSpan,
};

use crate::database::{Schema, Table};
use crate::engine::select::execute_select_query;
use crate::engine::{QueryResult, QueryStats, status_result};
use crate::error::Error;
use crate::evaluator::{EvalContext, QueryRuntime, eval_expr};
use crate::value::Value;
use std::collections::HashMap;

fn type_name(data_type: &sqlparser::ast::DataType) -> Result<String, Error> {
    use sqlparser::ast::DataType;
    let name = match data_type {
        DataType::Int(_) | DataType::Integer(_) | DataType::BigInt(_) => "int",
        DataType::Float(_) | DataType::Real | DataType::Double(_) | DataType::DoublePrecision => {
            "float"
        }
        DataType::Boolean | DataType::Bool => "boolean",
        DataType::Date => "date",
        DataType::Text
        | DataType::String(_)
        | DataType::Char(_)
        | DataType::Varchar(_)
        | DataType::Character(_)
        | DataType::CharacterVarying(_) => "text",
        other => return Err(format!("Unsupported temporary table type `{other}`").into()),
    };
    Ok(name.into())
}

fn coerce(value: Value, target: &str) -> Result<Value, Error> {
    if value.is_null() {
        return Ok(value);
    }
    match target {
        "int" => value
            .as_i64()
            .or_else(|| value.as_text().and_then(|text| text.parse().ok()))
            .map(Value::Int)
            .ok_or_else(|| format!("Cannot convert `{value}` to INT").into()),
        "float" => value
            .as_f64()
            .or_else(|| value.as_text().and_then(|text| text.parse().ok()))
            .map(Value::Float)
            .ok_or_else(|| format!("Cannot convert `{value}` to FLOAT").into()),
        "boolean" => value
            .as_bool()
            .or_else(|| {
                value
                    .as_text()
                    .and_then(|text| match text.to_lowercase().as_str() {
                        "true" | "1" => Some(true),
                        "false" | "0" => Some(false),
                        _ => None,
                    })
            })
            .map(Value::Bool)
            .ok_or_else(|| format!("Cannot convert `{value}` to BOOLEAN").into()),
        "date" => match value {
            Value::Date(_) | Value::Text(_) => Ok(value),
            other => Err(format!("Cannot convert `{other}` to DATE").into()),
        },
        "text" => Ok(Value::Text(value.to_display_string())),
        _ => Err(format!("Unknown temporary column type `{target}`").into()),
    }
}

pub(crate) fn run_create_table(
    schema: &mut Schema,
    create: &CreateTable,
) -> Result<QueryResult, Error> {
    if !create.temporary {
        return Err("Only temporary tables are supported".into());
    }
    let name = crate::engine::scope::object_name_to_parts(&create.name);
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
        let types = create
            .columns
            .iter()
            .map(|column| type_name(&column.data_type))
            .collect::<Result<Vec<_>, _>>()?;
        schema.add_temporary_table(Table {
            name: table_name.clone(),
            columns,
            rows: Vec::new(),
        });
        schema.set_temporary_column_types(table_name.clone(), types);
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
    let result_column_count = result.columns.len();
    schema.add_temporary_table(Table {
        name: table_name.clone(),
        columns: result.columns,
        rows: result.rows,
    });
    schema.set_temporary_column_types(table_name.clone(), vec!["text".into(); result_column_count]);
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
    let column_types = schema
        .temporary_column_types(table_name)
        .map(|types| types.to_vec());
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
                    target_row[*target_index] = match &column_types {
                        Some(types) => coerce(value, &types[*target_index])?,
                        None => value,
                    };
                }
                rows.push(target_row);
            }
            let count = rows.len();
            let table = schema
                .get_temporary_table_mut(table_name)
                .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
            table.rows.extend(rows);
            Ok(affected("Inserted", count, count))
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
                    let value = source_row.get(source_index).cloned().unwrap_or(Value::Null);
                    target_row[*target_index] = match &column_types {
                        Some(types) => coerce(value, &types[*target_index])?,
                        None => value,
                    };
                }
                rows.push(target_row);
            }
            let count = rows.len();
            let table = schema
                .get_temporary_table_mut(table_name)
                .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
            table.rows.extend(rows);
            Ok(affected("Inserted", count, count))
        }
        _ => Err("INSERT requires VALUES or SELECT".into()),
    }
}

pub(crate) fn run_drop_table(
    schema: &mut Schema,
    name: &sqlparser::ast::ObjectName,
    if_exists: bool,
) -> Result<QueryResult, Error> {
    let parts = crate::engine::scope::object_name_to_parts(name);
    let [table_name] = parts.as_slice() else {
        return Err("Temporary table name must be unqualified".into());
    };
    if schema.remove_temporary_table(table_name).is_none() && !if_exists {
        return Err(format!("Temporary table `{table_name}` not found").into());
    }
    Ok(status(table_name, "dropped"))
}

pub(crate) fn run_update(
    schema: &mut Schema,
    update: &sqlparser::ast::Update,
) -> Result<QueryResult, Error> {
    if !update.table.joins.is_empty() {
        return Err("JOIN UPDATE is not supported for temporary tables".into());
    }
    let TableFactor::Table { name, alias, .. } = &update.table.relation else {
        return Err("UPDATE target must be a table name".into());
    };
    if alias.is_some() {
        return Err("UPDATE table aliases are not supported".into());
    }
    let parts = crate::engine::scope::object_name_to_parts(name);
    let [table_name] = parts.as_slice() else {
        return Err("Temporary table name must be unqualified".into());
    };
    let original = schema
        .get_temporary_table(table_name)
        .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
    let columns = original.columns.clone();
    let original_rows = original.rows.clone();
    let input_count = original_rows.len();
    let types = schema
        .temporary_column_types(table_name)
        .map(|types| types.to_vec());
    let mut lookup = HashMap::new();
    for (index, column) in columns.iter().enumerate() {
        lookup.insert(column.clone(), index);
    }
    let runtime = QueryRuntime::default();
    let mut assignment_indices = Vec::new();
    for assignment in &update.assignments {
        let AssignmentTarget::ColumnName(column) = &assignment.target else {
            return Err("Tuple UPDATE assignments are not supported".into());
        };
        let parts = crate::engine::scope::object_name_to_parts(column);
        let [column_name] = parts.as_slice() else {
            return Err("UPDATE column names must be unqualified".into());
        };
        let index = columns
            .iter()
            .position(|candidate| candidate.eq_ignore_ascii_case(column_name))
            .ok_or_else(|| format!("Temporary table column `{column_name}` not found"))?;
        if assignment_indices
            .iter()
            .any(|(existing, _): &(usize, &Expr)| *existing == index)
        {
            return Err(format!(
                "Temporary table column `{column_name}` is assigned more than once"
            )
            .into());
        }
        assignment_indices.push((index, &assignment.value));
    }
    let mut updated_rows = Vec::with_capacity(original_rows.len());
    let mut changed = 0;
    for row in &original_rows {
        let ctx = EvalContext::new(&lookup, &original_rows, &[], chrono::Local::now(), &runtime);
        let matches = update
            .selection
            .as_ref()
            .map(|predicate| eval_expr(&ctx, predicate, row).map(|value| value.truthy()))
            .transpose()?
            .unwrap_or(true);
        let mut new_row = row.clone();
        if matches {
            for (index, expression) in &assignment_indices {
                let value = eval_expr(&ctx, expression, row)?;
                new_row[*index] = match &types {
                    Some(types) => coerce(value, &types[*index])?,
                    None => value,
                };
            }
            changed += 1;
        }
        updated_rows.push(new_row);
    }
    let table = schema
        .get_temporary_table_mut(table_name)
        .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
    table.rows = updated_rows;
    Ok(affected("Updated", changed, input_count))
}

pub(crate) fn run_delete(
    schema: &mut Schema,
    delete: &sqlparser::ast::Delete,
) -> Result<QueryResult, Error> {
    if delete.using.is_some() || delete.tables.len() > 1 {
        return Err("Multi-table DELETE is not supported for temporary tables".into());
    }
    let from = match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(tables)
        | sqlparser::ast::FromTable::WithoutKeyword(tables) => tables,
    };
    let [table] = from.as_slice() else {
        return Err("DELETE target must be one table".into());
    };
    if !table.joins.is_empty() {
        return Err("JOIN DELETE is not supported for temporary tables".into());
    }
    let TableFactor::Table { name, alias, .. } = &table.relation else {
        return Err("DELETE target must be a table name".into());
    };
    if alias.is_some() {
        return Err("DELETE table aliases are not supported".into());
    }
    let parts = crate::engine::scope::object_name_to_parts(name);
    let [table_name] = parts.as_slice() else {
        return Err("Temporary table name must be unqualified".into());
    };
    let original = schema
        .get_temporary_table(table_name)
        .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
    let columns = original.columns.clone();
    let original_rows = original.rows.clone();
    let input_count = original_rows.len();
    let mut lookup = HashMap::new();
    for (index, column) in columns.iter().enumerate() {
        lookup.insert(column.clone(), index);
    }
    let runtime = QueryRuntime::default();
    let mut kept = Vec::with_capacity(original_rows.len());
    let mut deleted = 0;
    for row in &original_rows {
        let ctx = EvalContext::new(&lookup, &original_rows, &[], chrono::Local::now(), &runtime);
        let matches = delete
            .selection
            .as_ref()
            .map(|predicate| eval_expr(&ctx, predicate, row).map(|value| value.truthy()))
            .transpose()?
            .unwrap_or(true);
        if matches {
            deleted += 1;
        } else {
            kept.push(row.clone());
        }
    }
    let table = schema
        .get_temporary_table_mut(table_name)
        .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
    table.rows = kept;
    Ok(affected("Deleted", deleted, input_count))
}

pub(crate) fn run_truncate(
    schema: &mut Schema,
    truncate: &sqlparser::ast::Truncate,
) -> Result<QueryResult, Error> {
    if truncate.table_names.len() != 1 || truncate.partitions.is_some() {
        return Err("TRUNCATE supports exactly one temporary table".into());
    }
    let parts = crate::engine::scope::object_name_to_parts(&truncate.table_names[0].name);
    let [table_name] = parts.as_slice() else {
        return Err("Temporary table name must be unqualified".into());
    };
    let table = schema
        .get_temporary_table_mut(table_name)
        .ok_or_else(|| format!("Temporary table `{table_name}` not found"))?;
    let count = table.rows.len();
    table.rows.clear();
    Ok(affected("Truncated", count, count))
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
        let parts = crate::engine::scope::object_name_to_parts(column);
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
    status_result(
        format!("Temporary table `{table_name}` {action}"),
        QueryStats::default(),
    )
}

fn affected(action: &str, count: usize, input_rows: usize) -> QueryResult {
    status_result(
        format!("{action} {count} row(s)"),
        QueryStats {
            input_rows,
            affected_rows: count,
            ..Default::default()
        },
    )
}
