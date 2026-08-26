use sqlparser::ast::{CreateTable, Query};

use crate::database::{Schema, Table};
use crate::engine::QueryResult;
use crate::engine::select::execute_query;
use crate::error::Error;
use crate::value::Value;

pub(crate) fn run_create_table(
    schema: &mut Schema,
    create: &CreateTable,
) -> Result<QueryResult, Error> {
    if !create.temporary {
        return Err("Only CREATE TEMPORARY TABLE ... AS SELECT is supported".into());
    }
    let name = crate::engine::select::object_name_to_parts(&create.name);
    let table_name = match name.as_slice() {
        [name] => name.clone(),
        _ => return Err("Temporary table name must be unqualified".into()),
    };
    if !create.columns.is_empty() || create.query.is_none() {
        return Err("Temporary tables require `AS SELECT ...` and no column definitions".into());
    }
    let query: &Query = create
        .query
        .as_deref()
        .ok_or("Temporary tables require `AS SELECT ...`")?;
    let result = execute_query(schema, query)?;
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
