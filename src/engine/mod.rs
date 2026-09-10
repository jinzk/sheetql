mod aggregate;
mod execution;
mod filter;
mod join;
mod metadata;
mod ordering;
mod output;
mod plan;
mod projection;
pub(crate) mod rewrite;
mod scope;
mod select;
mod set;
mod subquery;
mod temporary;
mod window;

#[cfg(test)]
mod tests;

use sqlparser::ast::{
    ObjectType, ShowStatementFilter, ShowStatementFilterPosition, ShowStatementOptions, Statement,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use std::time::Instant;

use crate::database::Schema;
use crate::error::Error;
use crate::value::Value;

use crate::engine::metadata::{run_describe_table, run_show_databases, run_show_tables, run_use};
use crate::engine::output::{strip_into_outfile, write_outfile};
use crate::engine::scope::object_name_to_parts;
use crate::engine::set::execute_query;
use crate::engine::temporary::{
    run_create_table, run_delete, run_drop_table, run_insert, run_truncate, run_update,
};

/// Split an `INTO OUTFILE 'path'` clause off a query before parsing.
/// Exposed so the server can reject file-writing clauses up front.
pub(crate) fn split_outfile(sql: &str) -> (String, Option<String>) {
    strip_into_outfile(sql)
}

#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    pub elapsed_ms: u128,
    pub input_rows: usize,
    pub output_rows: usize,
    pub affected_rows: usize,
    pub correlated_cache_hits: usize,
    pub correlated_cache_misses: usize,
}

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
}

pub(crate) fn status_result(message: String, stats: QueryStats) -> QueryResult {
    QueryResult {
        columns: vec!["Status".into()],
        rows: vec![vec![Value::Text(message)]],
        stats,
    }
}

/// Internal query data before top-level execution statistics are attached.
#[derive(Debug, Clone)]
pub(crate) struct ExecutionOutput {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) input_rows: usize,
}

impl ExecutionOutput {
    pub(crate) fn into_result(self) -> QueryResult {
        QueryResult {
            columns: self.columns,
            rows: self.rows,
            stats: QueryStats {
                input_rows: self.input_rows,
                ..Default::default()
            },
        }
    }
}

pub fn run_query(schema: &mut Schema, sql: &str) -> Result<QueryResult, Error> {
    let started = Instant::now();
    let (query_sql, outfile) = strip_into_outfile(sql);

    let dialect = MySqlDialect {};
    let statements = Parser::parse_sql(&dialect, &query_sql)
        .map_err(|error| format!("SQL parse error: {error}"))?;

    if statements.len() != 1 {
        return Err("Only a single statement per query is supported"
            .to_string()
            .into());
    }

    let result = match &statements[0] {
        Statement::Query(query) => execute_query(schema, query),
        Statement::CreateTable(create) => run_create_table(schema, create),
        Statement::Insert(insert) => run_insert(schema, insert),
        Statement::Update(update) => run_update(schema, update),
        Statement::Delete(delete) => run_delete(schema, delete),
        Statement::Truncate(truncate) => run_truncate(schema, truncate),
        Statement::Drop {
            object_type: ObjectType::Table,
            temporary: true,
            if_exists,
            names,
            ..
        } if names.len() == 1 => run_drop_table(schema, &names[0], *if_exists),
        Statement::ShowColumns { show_options, .. } => run_show_columns(schema, show_options),
        Statement::ShowDatabases { show_options, .. }
        | Statement::ShowSchemas { show_options, .. } => {
            run_show_databases(schema, show_like_pattern(show_options)?.as_deref())
        }
        Statement::ShowTables { show_options, .. } => {
            let database = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .map(|name| object_name_to_parts(name).join("."));
            run_show_tables(
                schema,
                database.as_deref(),
                show_like_pattern(show_options)?.as_deref(),
            )
        }
        Statement::Use(use_clause) => match use_clause {
            sqlparser::ast::Use::Object(name) | sqlparser::ast::Use::Database(name) => {
                run_use(schema, &object_name_to_parts(name).join("."))
            }
            _ => Err("Unsupported USE statement".to_string().into()),
        },
        Statement::ExplainTable { table_name, .. } => {
            let reference = object_name_to_parts(table_name).join(".");
            run_describe_table(schema, &reference)
        }
        other => Err(format!("Unsupported statement: {other}").into()),
    };

    if let Some(path) = outfile {
        let result = result?;
        write_outfile(&path, &result)?;
        return Ok(QueryResult {
            columns: vec!["Status".to_string()],
            rows: vec![vec![Value::Text(format!(
                "Written {} row(s) to '{}'",
                result.rows.len(),
                path
            ))]],
            stats: QueryStats {
                elapsed_ms: started.elapsed().as_millis(),
                input_rows: result.stats.input_rows,
                output_rows: result.rows.len(),
                affected_rows: result.stats.affected_rows,
                correlated_cache_hits: result.stats.correlated_cache_hits,
                correlated_cache_misses: result.stats.correlated_cache_misses,
            },
        });
    }

    finalize_result(result, started)
}

/// Extract the `LIKE 'pattern'` filter from a SHOW statement's options. Only
/// LIKE filters are supported; WHERE/ILIKE filters are rejected.
fn show_like_pattern(show_options: &ShowStatementOptions) -> Result<Option<String>, Error> {
    let Some(position) = &show_options.filter_position else {
        return Ok(None);
    };
    let filter = match position {
        ShowStatementFilterPosition::Infix(filter)
        | ShowStatementFilterPosition::Suffix(filter) => filter,
    };
    match filter {
        ShowStatementFilter::Like(pattern) => Ok(Some(pattern.clone())),
        ShowStatementFilter::NoKeyword(pattern) => Ok(Some(pattern.clone())),
        _ => Err("SHOW ... filters other than LIKE are not supported"
            .to_string()
            .into()),
    }
}

/// Run `SHOW COLUMNS FROM <table>`, resolving the table name from the
/// statement's `IN`/`FROM` clause.
fn run_show_columns(
    schema: &Schema,
    show_options: &ShowStatementOptions,
) -> Result<QueryResult, Error> {
    if show_options.filter_position.is_some() {
        return Err("SHOW COLUMNS filters (LIKE/WHERE) are not supported"
            .to_string()
            .into());
    }
    let reference = show_options
        .show_in
        .as_ref()
        .and_then(|show_in| show_in.parent_name.as_ref())
        .map(|name| object_name_to_parts(name).join("."))
        .ok_or_else(|| "SHOW COLUMNS requires a table name".to_string())?;
    run_describe_table(schema, &reference)
}

fn finalize_result(
    result: Result<QueryResult, Error>,
    started: Instant,
) -> Result<QueryResult, Error> {
    result.map(|mut result| {
        result.stats.elapsed_ms = started.elapsed().as_millis();
        result.stats.output_rows = result.rows.len();
        result
    })
}

