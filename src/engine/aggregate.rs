use std::collections::HashMap;

use chrono::{DateTime, Local};

use crate::engine::plan::ExprPlan;
use crate::error::Error;
use crate::evaluator::{EvalContext, QueryRuntime, eval_expr};
use crate::value::{GroupKey, Value, group_key};

use super::ordering::order_keys;
use super::projection::{PlannedProjectionItem, project};
use super::select::PlannedGroupSource;
use super::select::{KeyedRows, PlannedOrderTerm};
use crate::evaluator::{ExprId, GroupState, OuterScope, SubqueryExecutor, SubqueryResult};
use sqlparser::ast::Query;
use std::cell::RefCell;

/// Return the first source row of a group. Empty groups use an empty row so
/// aggregate functions can still produce their SQL NULL/zero result.
pub(crate) fn representative_index(group: &[usize]) -> Option<usize> {
    group.first().copied()
}

pub(crate) fn build_groups(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    sources: &[PlannedGroupSource],
    active: &[usize],
    now: DateTime<Local>,
    expr_plan: &ExprPlan,
    runtime: &QueryRuntime,
) -> Result<Vec<Vec<usize>>, Error> {
    if sources.is_empty() {
        return Ok(vec![active.to_vec()]);
    }
    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let mut groups = Vec::new();
    let mut index: HashMap<Vec<GroupKey>, usize> = HashMap::new();
    for &row_index in active {
        let row = &rows[row_index];
        let mut key = Vec::with_capacity(sources.len());
        for source in sources {
            let value = match source {
                PlannedGroupSource::Expr(id) => eval_expr(&ctx, expr_plan.expression(*id), row)?,
                PlannedGroupSource::Column(column_index) => {
                    row.get(*column_index).cloned().unwrap_or(Value::Null)
                }
            };
            key.push(group_key(&value));
        }
        let group_index = match index.get(&key) {
            Some(existing) => *existing,
            None => {
                groups.push(Vec::new());
                let new_index = groups.len() - 1;
                index.insert(key, new_index);
                new_index
            }
        };
        groups[group_index].push(row_index);
    }
    Ok(groups)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_aggregate_rows(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    group_sources: &[PlannedGroupSource],
    having: Option<ExprId>,
    projection: &[PlannedProjectionItem],
    order_terms: &[PlannedOrderTerm],
    active: &[usize],
    now: DateTime<Local>,
    expr_plan: &ExprPlan,
    subqueries: &HashMap<Query, SubqueryResult>,
    executor: &dyn SubqueryExecutor,
    outer_scope: Option<&OuterScope<'_>>,
    runtime: &QueryRuntime,
) -> Result<KeyedRows, Error> {
    let groups = build_groups(lookup, rows, group_sources, active, now, expr_plan, runtime)?;
    let mut keyed = Vec::with_capacity(groups.len());
    for group in &groups {
        let group_state = RefCell::new(GroupState::default());
        let current_scope = OuterScope {
            row: super::select::representative_row(rows, group),
            columns: lookup,
            parent: outer_scope,
            runtime: outer_scope.map_or(runtime, |scope| scope.runtime),
        };
        let ctx = EvalContext::with_group_state(
            lookup,
            rows,
            group,
            now,
            &expr_plan.ids,
            &expr_plan.expressions,
            &group_state,
            runtime,
        )
        .with_subqueries(subqueries)
        .with_subquery_executor(executor, Some(&current_scope))
        .with_runtime(runtime)
        .with_outer_scope(&current_scope);
        let representative = super::select::representative_row(rows, group);
        if let Some(id) = having
            && !eval_expr(&ctx, expr_plan.expression(id), representative)?.truthy()
        {
            continue;
        }
        let output = project(&ctx, projection, representative, expr_plan)?;
        let keys = order_keys(order_terms, &ctx, &output, representative, expr_plan)?;
        keyed.push((keys, output));
    }
    Ok(keyed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_row_selects_group_head() {
        let _rows = [10, 20, 30];
        assert_eq!(representative_index(&[2, 0]), Some(2));
    }
}
