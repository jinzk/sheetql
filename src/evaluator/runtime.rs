use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::value::{GroupKey, Value};
use crate::value::group_key;

/// The materialized result of a subquery, reused for `IN`/`EXISTS`/scalar
/// subqueries and correlated cache hits.
#[derive(Debug, Clone, Default)]
pub(crate) struct SubqueryResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CorrelatedCacheKey {
    pub(crate) subquery: SubqueryId,
    pub(crate) outer_values: Vec<GroupKey>,
}

/// Identifies a single outer-scope column reference inside a correlated
/// subquery, so the cache key can be built from the affecting outer values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CorrelationExpr {
    pub(crate) scope_level: usize,
    pub(crate) outer_column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubqueryId(pub(crate) usize);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RuntimeStats {
    pub(crate) correlated_cache_hits: usize,
    pub(crate) correlated_cache_misses: usize,
}

/// Process-wide mutable state for a single query execution. Correlated
/// subqueries memoize their results per outer-key, and every subquery AST is
/// assigned a stable id so cache keys stay compact.
#[derive(Debug, Default)]
pub(crate) struct QueryRuntime {
    pub(crate) correlated_cache: std::sync::Mutex<HashMap<CorrelatedCacheKey, SubqueryResult>>,
    pub(crate) correlated_cache_hits: std::sync::Mutex<usize>,
    pub(crate) correlated_cache_misses: std::sync::Mutex<usize>,
    pub(crate) subquery_ids: std::sync::Mutex<HashMap<sqlparser::ast::Query, SubqueryId>>,
    pub(crate) correlations: std::sync::Mutex<HashMap<SubqueryId, Arc<[CorrelationExpr]>>>,
}

impl QueryRuntime {
    pub(crate) fn stats(&self) -> RuntimeStats {
        RuntimeStats {
            correlated_cache_hits: *self.correlated_cache_hits.lock().unwrap(),
            correlated_cache_misses: *self.correlated_cache_misses.lock().unwrap(),
        }
    }

    pub(crate) fn subquery_id(&self, query: &sqlparser::ast::Query) -> SubqueryId {
        let mut ids = self.subquery_ids.lock().unwrap();
        if let Some(id) = ids.get(query).copied() {
            return id;
        }
        let id = SubqueryId(ids.len());
        ids.insert(query.clone(), id);
        id
    }

    pub(crate) fn register_correlations(
        &self,
        subquery: SubqueryId,
        correlations: Vec<CorrelationExpr>,
    ) {
        self.correlations
            .lock()
            .unwrap()
            .insert(subquery, correlations.into());
    }
}

pub(crate) const MAX_CORRELATED_SUBQUERY_CACHE_ENTRIES: usize = 100_000;

/// The outer rows visible to a correlated subquery, chained through parents
/// for nested subqueries.
pub(crate) struct OuterScope<'a> {
    pub(crate) row: &'a [Value],
    pub(crate) columns: &'a HashMap<String, usize>,
    pub(crate) parent: Option<&'a OuterScope<'a>>,
    pub(crate) runtime: &'a QueryRuntime,
}

pub(crate) trait SubqueryExecutor {
    fn execute(
        &self,
        query: &sqlparser::ast::Query,
        scope: &OuterScope<'_>,
    ) -> Result<SubqueryResult, Error>;
}

pub(crate) fn scope_at<'a>(scope: &'a OuterScope<'a>, level: usize) -> Option<&'a OuterScope<'a>> {
    let mut current = Some(scope);
    for _ in 0..level {
        current = current?.parent;
    }
    current
}

pub(crate) fn scalar_subquery(
    ctx: &EvalContext,
    query: &sqlparser::ast::Query,
) -> Result<Value, Error> {
    let result = subquery_result(ctx, query)?;
    if result.columns.len() != 1 {
        return Err("Scalar subquery must return exactly one column".into());
    }
    match result.rows.as_slice() {
        [] => Ok(Value::Null),
        [row] => Ok(row.first().cloned().unwrap_or(Value::Null)),
        _ => Err("Scalar subquery must return at most one row".into()),
    }
}

pub(crate) fn subquery_result<'a>(
    ctx: &'a EvalContext<'a>,
    query: &sqlparser::ast::Query,
) -> Result<std::borrow::Cow<'a, SubqueryResult>, Error> {
    if let Some(result) = ctx.subqueries.get(query) {
        return Ok(std::borrow::Cow::Borrowed(result));
    }
    let executor = ctx.subquery_executor.ok_or("Subquery was not prepared")?;
    let scope = ctx
        .outer_scope
        .ok_or("Correlated subquery requires an outer row")?;
    let subquery = ctx.runtime.subquery_id(query);
    let correlations = ctx
        .runtime
        .correlations
        .lock()
        .unwrap()
        .get(&subquery)
        .cloned()
        .unwrap_or_default();
    // An empty dependency list means the correlation analyzer could not prove
    // which outer values affect the subquery. Never cache such a result: using
    // one shared entry would be incorrect for different outer rows.
    if correlations.is_empty() {
        return Ok(std::borrow::Cow::Owned(executor.execute(query, scope)?));
    }
    let outer_values = correlations
        .iter()
        .filter_map(|correlation| {
            scope_at(scope, correlation.scope_level)
                .and_then(|scope| scope.row.get(correlation.outer_column))
        })
        .map(group_key)
        .collect();
    let key = CorrelatedCacheKey {
        subquery,
        outer_values,
    };
    if let Some(result) = ctx.runtime.correlated_cache.lock().unwrap().get(&key) {
        *ctx.runtime.correlated_cache_hits.lock().unwrap() += 1;
        return Ok(std::borrow::Cow::Owned(result.clone()));
    }
    *ctx.runtime.correlated_cache_misses.lock().unwrap() += 1;
    let result = executor.execute(query, scope)?;
    let mut cache = ctx.runtime.correlated_cache.lock().unwrap();
    if cache.len() < MAX_CORRELATED_SUBQUERY_CACHE_ENTRIES {
        cache.insert(key, result.clone());
    }
    Ok(std::borrow::Cow::Owned(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    #[test]
    fn runtime_assigns_subquery_ids_by_query_ast() {
        let first = Parser::new(&MySqlDialect {})
            .try_with_sql("SELECT 1")
            .expect("valid query")
            .parse_query()
            .expect("valid query");
        let second = Parser::new(&MySqlDialect {})
            .try_with_sql("SELECT 2")
            .expect("valid query")
            .parse_query()
            .expect("valid query");
        let runtime = QueryRuntime::default();
        assert_ne!(runtime.subquery_id(&first), runtime.subquery_id(&second));
        assert_eq!(runtime.subquery_id(&first), runtime.subquery_id(&first));
    }

    #[test]
    fn runtime_stats_are_read_through_one_snapshot() {
        let runtime = QueryRuntime::default();
        *runtime.correlated_cache_hits.lock().unwrap() = 3;
        *runtime.correlated_cache_misses.lock().unwrap() = 2;
        assert_eq!(
            runtime.stats(),
            RuntimeStats {
                correlated_cache_hits: 3,
                correlated_cache_misses: 2,
            }
        );
    }

    #[test]
    fn correlated_scope_lookup_walks_parent_scopes() {
        let columns = HashMap::from([(String::from("p.id"), 0usize)]);
        let runtime = QueryRuntime::default();
        let outer_row = vec![Value::Int(7)];
        let outer = OuterScope {
            row: &outer_row,
            columns: &columns,
            parent: None,
            runtime: &runtime,
        };
        assert_eq!(
            scope_at(&outer, 0).map(|scope| scope.row[0].clone()),
            Some(Value::Int(7))
        );
        assert!(scope_at(&outer, 1).is_none());
    }
}