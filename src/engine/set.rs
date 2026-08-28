use std::collections::HashSet;

use sqlparser::ast::{
    Expr, LimitClause, Offset, OrderByKind, SetExpr, SetOperator, SetQuantifier, Value as SqlValue,
    ValueWithSpan,
};

use crate::database::Schema;
use crate::database::Table;
use crate::engine::{ExecutionOutput, QueryResult};
use crate::error::Error;
use crate::evaluator::{EvalContext, QueryRuntime, eval_expr};
use crate::value::{GroupKey, Value, group_key};

pub(crate) type RowKey = Vec<GroupKey>;

pub(crate) fn execute_query(
    schema: &Schema,
    query: &sqlparser::ast::Query,
) -> Result<QueryResult, Error> {
    let runtime = QueryRuntime::default();
    execute_query_with_runtime(schema, query, &runtime)
}

pub(crate) fn execute_query_with_runtime(
    schema: &Schema,
    query: &sqlparser::ast::Query,
    runtime: &QueryRuntime,
) -> Result<QueryResult, Error> {
    if let Some(with) = &query.with {
        let mut scoped_schema = schema.clone();
        if with.recursive {
            return Err("WITH RECURSIVE is not supported".into());
        }
        for cte in &with.cte_tables {
            let mut result = execute_query_with_runtime(&scoped_schema, &cte.query, runtime)?;
            let name = cte.alias.name.value.to_lowercase();
            let columns = if cte.alias.columns.is_empty() {
                result.columns
            } else {
                if cte.alias.columns.len() != result.columns.len() {
                    return Err("CTE column list must match the query result".into());
                }
                cte.alias
                    .columns
                    .iter()
                    .map(|column| column.name.value.to_lowercase())
                    .collect()
            };
            scoped_schema.add_query_table(Table {
                name,
                columns,
                rows: std::mem::take(&mut result.rows),
            });
        }
        let mut query_without_with = query.clone();
        query_without_with.with = None;
        return execute_query_with_runtime(&scoped_schema, &query_without_with, runtime);
    }
    let mut result = match query.body.as_ref() {
        SetExpr::Select(_) => {
            crate::engine::select::execute_select_query_with_runtime(schema, query, None, runtime)
        }
        _ => execute_set_expr(schema, &query.body, runtime),
    }?;
    if !matches!(query.body.as_ref(), SetExpr::Select(_)) {
        apply_query_clauses(&mut result, query)?;
    }
    let runtime_stats = runtime.stats();
    result.stats.correlated_cache_hits = runtime_stats.correlated_cache_hits;
    result.stats.correlated_cache_misses = runtime_stats.correlated_cache_misses;
    Ok(result)
}

fn apply_query_clauses(
    result: &mut QueryResult,
    query: &sqlparser::ast::Query,
) -> Result<(), Error> {
    if let Some(order_by) = &query.order_by {
        let OrderByKind::Expressions(expressions) = &order_by.kind else {
            return Err("ORDER BY ALL is not supported for set results".into());
        };
        let mut terms = Vec::with_capacity(expressions.len());
        for order in expressions {
            let index = match &order.expr {
                Expr::Value(ValueWithSpan {
                    value: SqlValue::Number(number, _),
                    ..
                }) => number.parse::<usize>().ok().and_then(|n| n.checked_sub(1)),
                Expr::Identifier(identifier) => result
                    .columns
                    .iter()
                    .position(|column| column.eq_ignore_ascii_case(&identifier.value)),
                Expr::CompoundIdentifier(parts) if parts.len() == 1 => result
                    .columns
                    .iter()
                    .position(|column| column.eq_ignore_ascii_case(&parts[0].value)),
                _ => None,
            }
            .ok_or_else(|| {
                "Set result ORDER BY supports only output column names or ordinals".to_string()
            })?;
            if index >= result.columns.len() {
                return Err("Set result ORDER BY position is not in the select list".into());
            }
            terms.push((index, order.options.asc.unwrap_or(true)));
        }
        result.rows.sort_by(|left, right| {
            terms
                .iter()
                .find_map(|(index, ascending)| {
                    let mut ordering =
                        crate::value::values_partial_cmp(&left[*index], &right[*index])
                            .unwrap_or(std::cmp::Ordering::Equal);
                    if !ascending {
                        ordering = ordering.reverse();
                    }
                    (ordering != std::cmp::Ordering::Equal).then_some(ordering)
                })
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let (limit, offset) = parse_set_limit(&query.limit_clause)?;
    if offset > 0 {
        result.rows = result.rows.drain(..).skip(offset).collect();
    }
    if let Some(limit) = limit {
        result.rows.truncate(limit);
    }
    result.stats.output_rows = result.rows.len();
    Ok(())
}

fn parse_set_limit(clause: &Option<LimitClause>) -> Result<(Option<usize>, usize), Error> {
    let Some(clause) = clause else {
        return Ok((None, 0));
    };
    match clause {
        LimitClause::LimitOffset { limit, offset, .. } => {
            let limit = limit
                .as_ref()
                .map(|expr| eval_set_int(expr, "LIMIT"))
                .transpose()?;
            let offset = offset
                .as_ref()
                .map(|Offset { value, .. }| eval_set_int(value, "OFFSET"))
                .transpose()?
                .unwrap_or(0);
            Ok((limit, offset))
        }
        LimitClause::OffsetCommaLimit { offset, limit } => Ok((
            Some(eval_set_int(limit, "LIMIT")?),
            eval_set_int(offset, "OFFSET")?,
        )),
    }
}

fn eval_set_int(expr: &Expr, clause: &str) -> Result<usize, Error> {
    match eval_expr(&EvalContext::scalar(), expr, &[])? {
        Value::Int(value) if value >= 0 => Ok(value as usize),
        Value::Int(value) => Err(format!("{clause} must be non-negative, got `{value}`").into()),
        value => Err(format!("{clause} expects an integer constant, got `{value}`").into()),
    }
}

fn execute_set_expr(
    schema: &Schema,
    expr: &SetExpr,
    runtime: &QueryRuntime,
) -> Result<QueryResult, Error> {
    match expr {
        SetExpr::Select(select) => crate::engine::select::execute_select_query_with_runtime(
            schema,
            &sqlparser::ast::Query {
                with: None,
                body: Box::new(SetExpr::Select(select.clone())),
                order_by: None,
                limit_clause: None,
                fetch: None,
                locks: vec![],
                for_clause: None,
                settings: None,
                format_clause: None,
                pipe_operators: vec![],
            },
            None,
            runtime,
        ),
        SetExpr::Query(query) => execute_query_with_runtime(schema, query, runtime),
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            if !matches!(
                set_quantifier,
                SetQuantifier::None | SetQuantifier::Distinct
            ) {
                return Err("ALL set quantifiers are not supported".into());
            }
            let left = execute_set_expr(schema, left, runtime)?;
            let right = execute_set_expr(schema, right, runtime)?;
            combine_set_results(left, right, op)
        }
        _ => Err("Only SELECT set operations are supported".into()),
    }
}

pub(crate) fn row_key(row: &[Value]) -> RowKey {
    row.iter().map(group_key).collect::<RowKey>()
}

pub(crate) fn combine_set_results(
    left: QueryResult,
    right: QueryResult,
    op: &SetOperator,
) -> Result<QueryResult, Error> {
    if left.columns.len() != right.columns.len() {
        return Err(
            "Set operation requires both queries to return the same number of columns".into(),
        );
    }

    let right_keys: HashSet<RowKey> = right.rows.iter().map(|row| row_key(row)).collect();
    let mut rows = match op {
        SetOperator::Union => left
            .rows
            .iter()
            .chain(&right.rows)
            .cloned()
            .collect::<Vec<_>>(),
        SetOperator::Intersect => left
            .rows
            .iter()
            .filter(|row| right_keys.contains(&row_key(row)))
            .cloned()
            .collect::<Vec<_>>(),
        SetOperator::Except => left
            .rows
            .iter()
            .filter(|row| !right_keys.contains(&row_key(row)))
            .cloned()
            .collect::<Vec<_>>(),
        SetOperator::Minus => return Err("MINUS set operation is not supported".into()),
    };

    let mut seen = HashSet::new();
    rows.retain(|row| seen.insert(row_key(row)));
    let output_rows = rows.len();
    let mut result = ExecutionOutput {
        columns: left.columns,
        rows,
    }
    .into_result();
    result.stats.input_rows = left.stats.input_rows + right.stats.input_rows;
    result.stats.output_rows = output_rows;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::QueryStats;

    fn result(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|column| (*column).into()).collect(),
            stats: QueryStats {
                input_rows: rows.len(),
                ..Default::default()
            },
            rows,
        }
    }

    #[test]
    fn set_operations_use_complete_rows_and_deduplicate() {
        let left = result(
            &["id", "name"],
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(1), Value::Text("b".into())],
            ],
        );
        let right = result(
            &["other_id", "other_name"],
            vec![
                vec![Value::Float(1.0), Value::Text("b".into())],
                vec![Value::Int(2), Value::Text("c".into())],
            ],
        );
        let union = combine_set_results(left.clone(), right.clone(), &SetOperator::Union).unwrap();
        assert_eq!(union.rows.len(), 3);
        let intersection =
            combine_set_results(left.clone(), right.clone(), &SetOperator::Intersect).unwrap();
        assert_eq!(
            intersection.rows,
            vec![vec![Value::Int(1), Value::Text("b".into())]]
        );
        let except = combine_set_results(left, right, &SetOperator::Except).unwrap();
        assert_eq!(
            except.rows,
            vec![vec![Value::Int(1), Value::Text("a".into())]]
        );
    }

    #[test]
    fn row_key_treats_null_and_numeric_variants_consistently() {
        assert_eq!(
            row_key(&[Value::Null, Value::Int(1)]),
            row_key(&[Value::Null, Value::Float(1.0)])
        );
    }

    #[test]
    fn set_operation_rejects_different_column_counts_and_all() {
        let left = result(&["id"], vec![vec![Value::Int(1)]]);
        let right = result(
            &["id", "name"],
            vec![vec![Value::Int(1), Value::Text("a".into())]],
        );
        assert!(combine_set_results(left.clone(), right, &SetOperator::Union).is_err());
        assert!(matches!(SetQuantifier::All, SetQuantifier::All));
    }
}
