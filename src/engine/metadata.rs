use crate::database::Schema;
use crate::database::Table;
use crate::error::Error;
use crate::evaluator::like_match;
use crate::value::Value;

pub(crate) fn run_show_databases(
    schema: &Schema,
    like: Option<&str>,
) -> Result<crate::engine::QueryResult, Error> {
    let columns = vec!["Database".to_string()];
    let rows = schema
        .database_names()
        .into_iter()
        .filter(|name| like.is_none_or(|pattern| like_match(name, pattern, false, None)))
        .map(|name| vec![Value::Text(name.to_string())])
        .collect();
    Ok(crate::engine::QueryResult {
        columns,
        rows,
        stats: Default::default(),
    })
}

pub(crate) fn run_use(
    schema: &mut Schema,
    name: &str,
) -> Result<crate::engine::QueryResult, Error> {
    schema.set_current_database(name)?;
    Ok(crate::engine::QueryResult {
        columns: vec!["Status".to_string()],
        rows: vec![vec![Value::Text("Database changed".to_string())]],
        stats: Default::default(),
    })
}

pub(crate) fn run_show_tables(
    schema: &Schema,
    database_name: Option<&str>,
    like: Option<&str>,
) -> Result<crate::engine::QueryResult, Error> {
    let name = match database_name {
        Some(name) => name.to_string(),
        None => schema
            .current_database()
            .map(|name| name.to_string())
            .ok_or_else(|| "No database selected, use `USE <database>` first".to_string())?,
    };
    let database = schema
        .get_database(&name)
        .ok_or_else(|| format!("Unknown database `{name}`"))?;
    let columns = vec!["Tables".to_string()];
    let mut rows: Vec<Vec<Value>> = database
        .table_names()
        .into_iter()
        .filter(|name| like.is_none_or(|pattern| like_match(name, pattern, false, None)))
        .map(|name| vec![Value::Text(name.to_string())])
        .collect();
    if database.name == schema.current_database().unwrap_or_default() {
        rows.extend(schema.temporary.table_names().into_iter().filter(|name| {
            like.is_none_or(|pattern| like_match(name, pattern, false, None))
        }).map(|name| vec![Value::Text(name.to_string())]));
    }
    Ok(crate::engine::QueryResult {
        columns,
        rows,
        stats: Default::default(),
    })
}

pub(crate) fn run_describe_table(
    schema: &Schema,
    reference: &str,
) -> Result<crate::engine::QueryResult, Error> {
    let parts: Vec<&str> = reference.split('.').collect();
    let (database, table_name) = match parts.as_slice() {
        [table_name] => (None, *table_name),
        [database, table_name] => (Some(*database), *table_name),
        _ => {
            return Err("Table reference must be `table` or `database.table`"
                .to_string()
                .into());
        }
    };
    let (_, table) = schema.resolve_table(database, table_name)?;
    describe_table(table)
}

fn describe_table(table: &Table) -> Result<crate::engine::QueryResult, Error> {
    let columns = vec!["Column".to_string(), "Type".to_string()];

    // Scan the rows once for all columns instead of once per column.
    let mut has_int = vec![false; table.columns.len()];
    let mut has_float = vec![false; table.columns.len()];
    let mut has_bool = vec![false; table.columns.len()];
    let mut has_text = vec![false; table.columns.len()];
    for row in &table.rows {
        for index in 0..table.columns.len() {
            match row.get(index) {
                Some(Value::Int(_)) => has_int[index] = true,
                Some(Value::Float(_)) => has_float[index] = true,
                Some(Value::Bool(_)) => has_bool[index] = true,
                Some(Value::Text(_)) => has_text[index] = true,
                Some(Value::Date(_) | Value::DateTime(_)) => has_text[index] = true,
                _ => {}
            }
        }
    }

    let rows: Vec<Vec<Value>> = table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let kind = if has_text[index] {
                "Text"
            } else if has_bool[index] {
                "Boolean"
            } else if has_float[index] {
                "Float"
            } else if has_int[index] {
                "Integer"
            } else {
                "Text"
            };
            vec![Value::Text(column.clone()), Value::Text(kind.to_string())]
        })
        .collect();
    Ok(crate::engine::QueryResult {
        columns,
        rows,
        stats: Default::default(),
    })
}
