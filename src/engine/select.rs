use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::Arc;

use chrono::Local;
use sqlparser::ast::{
    Distinct, Expr, GroupByExpr, LimitClause, Offset, OrderByKind, Query, Select, SelectItem,
    SetExpr,
};
use sqlparser::ast::{Visit, Visitor};

use crate::database::Schema;
use crate::engine::filter::pushdown_conjuncts;
use crate::engine::join::{ColumnRef, collect_relations};
use crate::engine::ordering::{order_keys, order_terms, sort_planned_keyed};
use crate::engine::plan::{ExecutionPlan, ExprPlan, QueryPlan};
pub(crate) use crate::engine::projection::{
    PlannedProjectionItem, ProjectionItem, build_projection_plan, project,
};
use crate::engine::rewrite::{AliasPrecedence, ExprRewriter, alias_map, ordinal_literal};
use crate::engine::scope::build_lookup;
use crate::engine::window::{
    WindowPlan, WindowValues, collect_window_exprs, compute_window_values, contains_window,
};
use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::evaluator::{
    ExprId, GroupState, OuterScope, QueryRuntime, SubqueryExecutor, SubqueryResult,
};
use crate::functions::contains_aggregate;
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;
use crate::value::values_partial_cmp;

pub(crate) type KeyedRows = Vec<(Vec<Value>, Vec<Value>)>;

pub(crate) fn execute_select_query(
    schema: &Schema,
    query: &Query,
) -> Result<crate::engine::QueryResult, Error> {
    let runtime = QueryRuntime::default();
    execute_select_query_with_runtime(schema, query, None, &runtime)
}

pub(crate) fn execute_select_query_with_runtime<'a>(
    schema: &'a Schema,
    query: &Query,
    outer_scope: Option<&'a OuterScope<'a>>,
    runtime: &'a QueryRuntime,
) -> Result<crate::engine::QueryResult, Error> {
    Ok(
        crate::engine::execution::execute_select_output(schema, query, outer_scope, runtime)?
            .into_result(),
    )
}

pub(crate) fn execute_select_output_impl<'a>(
    schema: &'a Schema,
    query: &Query,
    outer_scope: Option<&'a OuterScope<'a>>,
    runtime: &'a QueryRuntime,
) -> Result<crate::engine::ExecutionOutput, Error> {
    let now = Local::now();
    let select: &Select = match &*query.body {
        SetExpr::Select(select) => select,
        _ => return Err("Only plain SELECT queries are supported".to_string().into()),
    };

    // WHERE only normalizes identifier case; aliases are not visible there.
    let selection = select
        .selection
        .as_ref()
        .map(|selection| ExprRewriter::lowercase().rewritten(selection));

    let pushdown = pushdown_conjuncts(&select.from, selection.as_ref());

    let (schema_refs, mut rows): (Vec<ColumnRef>, Cow<'a, [Vec<Value>]>) =
        collect_relations(schema, &select.from, now, pushdown.as_deref(), runtime)?;
    if select.from.is_empty() {
        rows = Cow::Owned(vec![vec![]]);
    }
    let rows_view: &[Vec<Value>] = &rows;
    let lookup = build_lookup(&schema_refs)?;
    let mut scope_lookup = lookup.clone();
    if let Some(scope) = outer_scope {
        for (name, index) in scope.columns {
            scope_lookup.entry(name.clone()).or_insert(*index);
        }
    }
    let subqueries = prepare_subqueries(schema, query, &scope_lookup, runtime)?;
    let subquery_executor = crate::engine::subquery::SchemaExecutor { schema };

    // The projection plan holds case-normalized expressions; titles keep the
    // original spelling for display.
    let plan = build_projection_plan(&schema_refs, &select.projection)?;
    let aliases = alias_map(&plan);

    // HAVING and GROUP BY accept output aliases (`HAVING cnt > 1`) and
    // ordinals (`GROUP BY 1`), matching MySQL behavior. Source columns take
    // precedence over same-named aliases in these clauses.
    let having = select.having.as_ref().map(|having| {
        ExprRewriter::with_aliases(&aliases, AliasPrecedence::SourceFirst, Some(&lookup))
            .rewritten(having)
    });
    let group_exprs = group_by_expressions(&select.group_by, &aliases, &lookup)?;
    let group_sources: Vec<GroupSource> = group_exprs
        .iter()
        .map(|expr| resolve_group_source(expr, &plan))
        .collect::<Result<_, _>>()?;

    let order_terms = order_terms(query, &plan, &aliases)?;

    // Window functions (ROW_NUMBER/aggregate OVER) are computed over the full
    // active row set before projection. Collect every distinct window
    // expression referenced by the projection or ORDER BY so each can be
    // computed once and read per row.
    let mut window_plan = WindowPlan::new();
    for item in &plan {
        if let ProjectionItem::Expression { expr, .. } = item
            && contains_window(expr)
        {
            collect_window_exprs(&mut window_plan, expr);
        }
    }
    for term in &order_terms {
        if let OrderSource::Expr(expr) = &term.source
            && contains_window(expr)
        {
            collect_window_exprs(&mut window_plan, expr);
        }
    }
    let has_window = !window_plan.columns.is_empty();
    let (limit, offset) = parse_limit(&query.limit_clause)?;
    let is_aggregate = !group_exprs.is_empty()
        || select.projection.iter().any(projection_has_aggregate)
        || select.having.is_some()
        || order_by_has_aggregate(query);
    let is_distinct = is_distinct(select)?;
    let execution_plan = ExecutionPlan::new(
        limit,
        offset,
        is_aggregate,
        is_distinct,
        !order_terms.is_empty(),
    );
    let scan_target = execution_plan.scan_target();

    if has_window && execution_plan.aggregate {
        return Err(
            "Window functions cannot be combined with GROUP BY aggregation"
                .to_string()
                .into(),
        );
    }

    let expr_plan = ExprPlan::build(
        selection.as_ref(),
        having.as_ref(),
        &group_exprs,
        &plan,
        &order_terms,
    );
    let planned_projection = plan
        .iter()
        .map(|item| match item {
            ProjectionItem::Column { index, title } => PlannedProjectionItem::Column {
                index: *index,
                title: title.clone(),
            },
            ProjectionItem::Expression { expr, title } => PlannedProjectionItem::Expression {
                id: expr_plan.id_of(expr),
                title: title.clone(),
            },
        })
        .collect::<Vec<_>>();
    let planned_groups = group_sources
        .iter()
        .map(|source| match source {
            GroupSource::Expr(expr) => PlannedGroupSource::Expr(expr_plan.id_of(expr)),
            GroupSource::Column(index) => PlannedGroupSource::Column(*index),
        })
        .collect::<Vec<_>>();
    let planned_order = order_terms
        .iter()
        .map(|term| PlannedOrderTerm {
            source: match &term.source {
                OrderSource::Ordinal(position) => PlannedOrderSource::Ordinal(*position),
                OrderSource::Expr(expr) => PlannedOrderSource::Expr(expr_plan.id_of(expr)),
            },
            ascending: term.ascending,
        })
        .collect::<Vec<_>>();
    let planned_having = having.as_ref().map(|expr| expr_plan.id_of(expr));
    let output_titles = planned_titles(&planned_projection);
    let query_plan = QueryPlan {
        expressions: expr_plan,
        execution: execution_plan,
        projection: planned_projection,
        groups: planned_groups,
        order: planned_order,
        having: planned_having,
        windows: window_plan,
    };
    let expr_plan = &query_plan.expressions;
    let planned_projection = &query_plan.projection;
    let planned_groups = &query_plan.groups;
    let planned_order = &query_plan.order;
    let execution_plan = query_plan.execution;
    let window_plan = &query_plan.windows;
    // WHERE keeps a set of row indices instead of copying the rows themselves.
    let active = collect_active_rows(
        selection.as_ref(),
        &lookup,
        rows_view,
        scan_target,
        now,
        &subqueries,
        &subquery_executor,
        outer_scope,
        runtime,
    )?;

    let use_top_n = execution_plan.top_n;

    let window_values = if has_window {
        Some(compute_window_values(
            window_plan,
            rows_view,
            &lookup,
            &active,
            now,
            runtime,
        )?)
    } else {
        None
    };

    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = vec![];
    let mut top_n = use_top_n.then(|| {
        TopN::new(
            limit.unwrap().saturating_add(offset.unwrap_or(0)),
            planned_order,
        )
    });

    if execution_plan.aggregate {
        keyed = crate::engine::aggregate::execute_aggregate_rows(
            &lookup,
            rows_view,
            planned_groups,
            query_plan.having,
            planned_projection,
            planned_order,
            &active,
            now,
            expr_plan,
            &subqueries,
            &subquery_executor,
            outer_scope,
            runtime,
        )?;
    } else {
        keyed = execute_regular_rows(
            &lookup,
            rows_view,
            planned_projection,
            planned_order,
            &active,
            &execution_plan,
            top_n.as_mut(),
            now,
            expr_plan,
            &subqueries,
            &subquery_executor,
            outer_scope,
            runtime,
            window_values.as_ref(),
        )?;
    }

    let top_n_used = top_n.is_some();
    if let Some(top_n) = top_n {
        keyed = top_n.into_sorted_vec();
    }

    if execution_plan.distinct {
        let mut seen: HashSet<Vec<GroupKey>> = HashSet::new();
        keyed.retain(|(_, out)| seen.insert(out.iter().map(group_key).collect()));
    }

    if !top_n_used {
        sort_planned_keyed(&mut keyed, planned_order);
    }

    let mut final_rows: Vec<Vec<Value>> = keyed.into_iter().map(|(_, out)| out).collect();

    if !execution_plan.early_stop {
        if execution_plan.offset > 0 {
            final_rows = final_rows.into_iter().skip(execution_plan.offset).collect();
        }
        if let Some(limit) = execution_plan.limit {
            final_rows.truncate(limit);
        }
    }

    Ok(crate::engine::ExecutionOutput {
        columns: output_titles,
        rows: final_rows,
        input_rows: rows_view.len(),
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_active_rows(
    selection: Option<&Expr>,
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    scan_target: usize,
    now: chrono::DateTime<Local>,
    subqueries: &HashMap<Query, SubqueryResult>,
    executor: &dyn SubqueryExecutor,
    outer_scope: Option<&OuterScope<'_>>,
    runtime: &QueryRuntime,
) -> Result<Vec<usize>, Error> {
    if scan_target == 0 {
        return Ok(Vec::new());
    }
    if let Some(selection) = selection {
        let mut active = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let current_scope = OuterScope {
                row,
                columns: lookup,
                parent: outer_scope,
                runtime,
            };
            let ctx = EvalContext::new(lookup, rows, &[], now, runtime)
                .with_subqueries(subqueries)
                .with_subquery_executor(executor, Some(&current_scope))
                .with_runtime(runtime)
                .with_outer_scope(&current_scope);
            if eval_expr(&ctx, selection, row)?.truthy() {
                active.push(index);
                if active.len() >= scan_target {
                    break;
                }
            }
        }
        Ok(active)
    } else {
        Ok((0..scan_target.min(rows.len())).collect())
    }
}

fn prepare_subqueries(
    schema: &Schema,
    query: &Query,
    outer_lookup: &HashMap<String, usize>,
    runtime: &QueryRuntime,
) -> Result<HashMap<Query, SubqueryResult>, Error> {
    let mut queries = Vec::new();
    crate::engine::subquery::collect_subqueries(query, &mut queries);
    let mut results = HashMap::with_capacity(queries.len());
    for subquery in queries {
        let id = runtime.subquery_id(&subquery);
        let correlations = correlated_columns(&subquery, outer_lookup);
        runtime.register_correlations(id, correlations);
        let result = match crate::engine::set::execute_query(schema, &subquery) {
            Ok(result) => result,
            Err(error) if error.to_string().contains("not found") => continue,
            Err(error) => return Err(error),
        };
        results.insert(
            subquery.clone(),
            SubqueryResult {
                columns: result.columns,
                rows: result.rows,
            },
        );
    }
    Ok(results)
}

fn correlated_columns(
    query: &Query,
    outer_lookup: &HashMap<String, usize>,
) -> Vec<crate::evaluator::CorrelationExpr> {
    struct Collector<'a> {
        lookup: &'a HashMap<String, usize>,
        columns: Vec<crate::evaluator::CorrelationExpr>,
    }
    impl Visitor for Collector<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            let Expr::CompoundIdentifier(parts) = expr else {
                return ControlFlow::Continue(());
            };
            if parts.len() < 2 {
                return ControlFlow::Continue(());
            }
            let name = parts
                .iter()
                .map(|part| part.value.to_lowercase())
                .collect::<Vec<_>>()
                .join(".");
            if let Some(index) = self.lookup.get(&name) {
                let correlation = crate::evaluator::CorrelationExpr {
                    scope_level: 0,
                    outer_column: *index,
                };
                if !self.columns.contains(&correlation) {
                    self.columns.push(correlation);
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut collector = Collector {
        lookup: outer_lookup,
        columns: Vec::new(),
    };
    let _ = query.visit(&mut collector);
    collector.columns
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn execute_aggregate_rows_legacy(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    group_sources: &[PlannedGroupSource],
    having: Option<ExprId>,
    projection: &[PlannedProjectionItem],
    order_terms: &[PlannedOrderTerm],
    active: &[usize],
    now: chrono::DateTime<Local>,
    expr_plan: &ExprPlan,
    subqueries: &HashMap<Query, SubqueryResult>,
    executor: &dyn SubqueryExecutor,
    outer_scope: Option<&OuterScope<'_>>,
    runtime: &QueryRuntime,
) -> Result<KeyedRows, Error> {
    let groups = crate::engine::aggregate::build_groups(
        lookup,
        rows,
        group_sources,
        active,
        now,
        expr_plan,
        runtime,
    )?;
    let mut keyed = Vec::with_capacity(groups.len());
    for group in &groups {
        let group_state = RefCell::new(GroupState::default());
        let current_scope = OuterScope {
            row: representative_row(rows, group),
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
        let representative = representative_row(rows, group);
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

#[allow(clippy::too_many_arguments)]
fn execute_regular_rows(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    projection: &[PlannedProjectionItem],
    order_terms: &[PlannedOrderTerm],
    active: &[usize],
    execution_plan: &ExecutionPlan,
    top_n: Option<&mut TopN>,
    now: chrono::DateTime<Local>,
    expr_plan: &ExprPlan,
    subqueries: &HashMap<Query, SubqueryResult>,
    executor: &dyn SubqueryExecutor,
    outer_scope: Option<&OuterScope<'_>>,
    runtime: &QueryRuntime,
    window_values: Option<&WindowValues<'_>>,
) -> Result<KeyedRows, Error> {
    let start = if execution_plan.early_stop {
        execution_plan.offset.min(active.len())
    } else {
        0
    };
    let end = if execution_plan.early_stop {
        start
            .saturating_add(execution_plan.limit.unwrap_or(usize::MAX))
            .min(active.len())
    } else {
        active.len()
    };
    let mut keyed = Vec::new();
    let mut top_n = top_n;

    // With window functions, every source row is augmented with its precomputed
    // window column values, and the resolver tells `eval_function` where each
    // window value lives in the augmented row. All augmented rows are built up
    // front so references remain valid for the whole loop.
    let base_offset = rows.first().map_or(0, |row| row.len());
    let window_resolver = window_values.map(|values| {
        values
            .plan
            .offset_by_key
            .iter()
            .map(|(key, offset)| (key.clone(), offset + base_offset))
            .collect::<HashMap<_, _>>()
    });
    let slice = active[start..end].to_vec();
    let augmented_rows = window_values.map(|values| {
        slice
            .iter()
            .map(|&row_index| {
                let mut augmented = rows[row_index].to_vec();
                if let Some(extra) = values.values.get(&row_index) {
                    augmented.extend_from_slice(extra);
                }
                augmented
            })
            .collect::<Vec<_>>()
    });

    for (position, &row_index) in slice.iter().enumerate() {
        let eval_row: &[Value] = match &augmented_rows {
            Some(augmented) => augmented[position].as_slice(),
            None => &rows[row_index],
        };
        let current_scope = OuterScope {
            row: eval_row,
            columns: lookup,
            parent: outer_scope,
            runtime: outer_scope.map_or(runtime, |scope| scope.runtime),
        };
        let mut ctx = EvalContext::with_expr_ids(
            lookup,
            rows,
            &[],
            now,
            &expr_plan.ids,
            &expr_plan.expressions,
            runtime,
        )
        .with_subqueries(subqueries)
        .with_subquery_executor(executor, Some(&current_scope))
        .with_runtime(runtime)
        .with_outer_scope(&current_scope);
        if let Some(resolver) = &window_resolver {
            ctx = ctx.with_window_resolver(resolver);
        }
        let output = project(&ctx, projection, eval_row, expr_plan)?;
        let keys = order_keys(order_terms, &ctx, &output, eval_row, expr_plan)?;
        if let Some(heap) = top_n.as_deref_mut() {
            heap.push(keys, output);
        } else {
            keyed.push((keys, output));
        }
    }
    Ok(keyed)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod plan_tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    fn expr(sql: &str) -> Expr {
        Parser::new(&MySqlDialect {})
            .try_with_sql(sql)
            .expect("valid expression")
            .parse_expr()
            .expect("valid expression")
    }

    #[test]
    fn expression_plan_reuses_canonical_expression_ids() {
        let first = ExprRewriter::lowercase().rewritten(&expr("age + 1"));
        let second = ExprRewriter::lowercase().rewritten(&expr("AGE + 1"));
        let mut plan = ExprPlan {
            expressions: Vec::new(),
            ids: HashMap::new(),
        };
        plan.register_tree(&first);
        plan.register_tree(&second);
        assert_eq!(plan.ids.len(), 3);
        assert_eq!(plan.expressions.len(), plan.ids.len());
        assert_eq!(plan.ids[&first], plan.ids[&second]);
    }

    #[test]
    fn expression_plan_ids_are_dense_and_index_owned_expressions() {
        let root = ExprRewriter::lowercase().rewritten(&expr("age + 1"));
        let mut plan = ExprPlan {
            expressions: Vec::new(),
            ids: HashMap::new(),
        };
        plan.register_tree(&root);
        let mut ids: Vec<usize> = plan.ids.values().map(|id| id.0).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..plan.expressions.len()).collect::<Vec<_>>());
        for id in plan.ids.values() {
            assert!(!plan.expression(*id).to_string().is_empty());
        }
    }
}

/// Load a relation and, where possible, push candidate WHERE conjuncts onto it.
/// A conjunct is applied to this relation only when every column it references
/// resolves uniquely within this relation's schema.
fn planned_titles(plan: &[PlannedProjectionItem]) -> Vec<String> {
    plan.iter()
        .map(|item| match item {
            PlannedProjectionItem::Column { title, .. }
            | PlannedProjectionItem::Expression { title, .. } => title.clone(),
        })
        .collect()
}

pub(crate) enum PlannedGroupSource {
    Expr(ExprId),
    Column(usize),
}

pub(crate) struct PlannedOrderTerm {
    pub(crate) source: PlannedOrderSource,
    pub(crate) ascending: bool,
}

pub(crate) enum PlannedOrderSource {
    Ordinal(usize),
    Expr(ExprId),
}

fn projection_has_aggregate(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(expr) => contains_aggregate(expr),
        SelectItem::ExprWithAlias { expr, .. } => contains_aggregate(expr),
        SelectItem::ExprWithAliases { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

fn order_by_has_aggregate(query: &Query) -> bool {
    query.order_by.as_ref().is_some_and(|order| {
        if let OrderByKind::Expressions(exprs) = &order.kind {
            exprs
                .iter()
                .any(|order_expr| contains_aggregate(&order_expr.expr))
        } else {
            false
        }
    })
}

/// Parse GROUP BY into rewritten (alias-aware) expressions.
fn group_by_expressions(
    group_by: &GroupByExpr,
    aliases: &HashMap<String, &Expr>,
    lookup: &HashMap<String, usize>,
) -> Result<Vec<Expr>, Error> {
    match group_by {
        GroupByExpr::Expressions(exprs, _) => Ok(exprs
            .iter()
            .map(|expr| {
                ExprRewriter::with_aliases(aliases, AliasPrecedence::SourceFirst, Some(lookup))
                    .rewritten(expr)
            })
            .collect()),
        GroupByExpr::All(_) => Err("GROUP BY ALL is not supported".to_string().into()),
    }
}

/// A single GROUP BY term resolved against the projection plan: either an
/// expression over the source row, or a projected output column referenced by
/// its 1-based position (`GROUP BY 2`).
pub(crate) enum GroupSource<'g> {
    Expr(&'g Expr),
    Column(usize),
}

fn resolve_group_source<'g>(
    expr: &'g Expr,
    plan: &'g [ProjectionItem],
) -> Result<GroupSource<'g>, Error> {
    if let Some(ordinal) = ordinal_literal(expr) {
        let item = plan
            .get(ordinal - 1)
            .ok_or_else(|| format!("GROUP BY position {ordinal} is not in the select list"))?;
        return Ok(match item {
            ProjectionItem::Expression { expr, .. } => GroupSource::Expr(expr.as_ref()),
            ProjectionItem::Column { index, .. } => GroupSource::Column(*index),
        });
    }
    Ok(GroupSource::Expr(expr))
}

/// One ORDER BY term, fully resolved before execution: either an output
/// position or an alias-substituted expression, plus its sort direction.
pub(crate) struct OrderTerm {
    pub(crate) source: OrderSource,
    pub(crate) ascending: bool,
}

pub(crate) enum OrderSource {
    Ordinal(usize),
    Expr(Box<Expr>),
}

#[allow(dead_code)]
fn order_terms_legacy(
    query: &Query,
    plan: &[ProjectionItem],
    aliases: &HashMap<String, &Expr>,
) -> Result<Vec<OrderTerm>, Error> {
    let Some(order) = &query.order_by else {
        return Ok(vec![]);
    };
    let OrderByKind::Expressions(exprs) = &order.kind else {
        return Err("ORDER BY ALL is not supported".to_string().into());
    };
    // Aliases take precedence in ORDER BY (MySQL behavior).
    let rewriter = ExprRewriter::with_aliases(aliases, AliasPrecedence::AliasFirst, None);
    let mut terms = Vec::with_capacity(exprs.len());
    for order_expr in exprs {
        let source = if let Some(ordinal) = ordinal_literal(&order_expr.expr) {
            if ordinal > plan.len() {
                return Err(
                    format!("ORDER BY position {ordinal} is not in the select list").into(),
                );
            }
            OrderSource::Ordinal(ordinal)
        } else {
            OrderSource::Expr(Box::new(rewriter.rewritten(&order_expr.expr)))
        };
        terms.push(OrderTerm {
            source,
            ascending: order_expr.options.asc.unwrap_or(true),
        });
    }
    Ok(terms)
}

/// Compute the sort keys for one output row. Ordinals read straight from the
/// projected values; expressions evaluate against the source row through the
/// given context (which carries `group_rows` in aggregate queries).
#[allow(dead_code)]
fn order_keys_legacy(
    terms: &[PlannedOrderTerm],
    ctx: &EvalContext,
    out: &[Value],
    source_row: &[Value],
    expr_plan: &ExprPlan,
) -> Result<Vec<Value>, Error> {
    let mut keys = Vec::with_capacity(terms.len());
    for term in terms {
        match &term.source {
            PlannedOrderSource::Ordinal(position) => {
                keys.push(out[position - 1].clone());
            }
            PlannedOrderSource::Expr(id) => {
                keys.push(eval_expr(ctx, expr_plan.expression(*id), source_row)?);
            }
        }
    }
    Ok(keys)
}

#[allow(dead_code)]
fn sort_planned_keyed_legacy(keyed: &mut [(Vec<Value>, Vec<Value>)], terms: &[PlannedOrderTerm]) {
    keyed.sort_by(|a, b| compare_planned_keys(&a.0, &b.0, terms));
}

fn compare_planned_keys(left: &[Value], right: &[Value], terms: &[PlannedOrderTerm]) -> Ordering {
    left.iter()
        .zip(right)
        .zip(terms)
        .map(|((a, b), term)| {
            let mut ordering = values_partial_cmp(a, b).unwrap_or(Ordering::Equal);
            if !term.ascending {
                ordering = ordering.reverse();
            }
            ordering
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

struct TopN {
    heap: BinaryHeap<TopNEntry>,
    capacity: usize,
    ascending: Arc<[bool]>,
}

struct TopNEntry {
    keys: Vec<Value>,
    output: Vec<Value>,
    ascending: Arc<[bool]>,
}

impl TopN {
    fn new(capacity: usize, terms: &[PlannedOrderTerm]) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
            ascending: terms.iter().map(|term| term.ascending).collect(),
        }
    }

    fn push(&mut self, keys: Vec<Value>, output: Vec<Value>) {
        if self.capacity == 0 {
            return;
        }
        let entry = TopNEntry {
            keys,
            output,
            ascending: Arc::clone(&self.ascending),
        };
        if self.heap.len() < self.capacity {
            self.heap.push(entry);
        } else if let Some(worst) = self.heap.peek()
            && compare_order_keys(&entry.keys, &worst.keys, &entry.ascending) == Ordering::Less
        {
            self.heap.pop();
            self.heap.push(entry);
        }
    }

    fn into_sorted_vec(self) -> Vec<(Vec<Value>, Vec<Value>)> {
        let ascending = self.ascending;
        let mut entries = self.heap.into_vec();
        entries.sort_by(|left, right| compare_order_keys(&left.keys, &right.keys, &ascending));
        entries
            .into_iter()
            .map(|entry| (entry.keys, entry.output))
            .collect()
    }
}

impl Ord for TopNEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap keeps the worst candidate at the root under query order.
        compare_order_keys(&self.keys, &other.keys, &self.ascending)
    }
}

impl PartialOrd for TopNEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for TopNEntry {
    fn eq(&self, other: &Self) -> bool {
        self.keys
            .iter()
            .zip(&other.keys)
            .all(|(a, b)| values_partial_cmp(a, b) == Some(Ordering::Equal))
    }
}

impl Eq for TopNEntry {}

fn compare_order_keys(left: &[Value], right: &[Value], ascending: &[bool]) -> Ordering {
    left.iter()
        .zip(right)
        .zip(ascending)
        .map(|((a, b), ascending)| {
            let mut ordering = values_partial_cmp(a, b).unwrap_or(Ordering::Equal);
            if !ascending {
                ordering = ordering.reverse();
            }
            ordering
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn is_distinct(select: &Select) -> Result<bool, Error> {
    match &select.distinct {
        Some(Distinct::Distinct) => Ok(true),
        Some(Distinct::On(_)) => Err("DISTINCT ON is not supported".to_string().into()),
        Some(Distinct::All) | None => Ok(false),
    }
}

fn parse_limit(
    limit_clause: &Option<LimitClause>,
) -> Result<(Option<usize>, Option<usize>), Error> {
    match limit_clause {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset { limit, offset, .. }) => {
            let limit_value = match limit {
                Some(expr) => Some(eval_const_int(expr, "LIMIT")?),
                None => None,
            };
            let offset_value = match offset {
                Some(Offset { value, .. }) => Some(eval_const_int(value, "OFFSET")?),
                None => None,
            };
            Ok((limit_value, offset_value))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            let offset_value = eval_const_int(offset, "OFFSET")?;
            let limit_value = eval_const_int(limit, "LIMIT")?;
            Ok((Some(limit_value), Some(offset_value)))
        }
    }
}

/// Evaluate a LIMIT/OFFSET bound. Anything other than a non-negative integer
/// constant is an error rather than being silently ignored or clamped.
fn eval_const_int(expr: &Expr, clause: &str) -> Result<usize, Error> {
    let value = eval_expr(&EvalContext::scalar(), expr, &[])?;
    match value {
        Value::Int(number) if number >= 0 => Ok(number as usize),
        Value::Int(number) => Err(format!("{clause} must be non-negative, got `{number}`").into()),
        other => Err(format!("{clause} expects an integer constant, got `{other}`").into()),
    }
}

pub(crate) fn representative_row<'a>(rows: &'a [Vec<Value>], group: &[usize]) -> &'a [Value] {
    match crate::engine::aggregate::representative_index(group) {
        Some(index) => rows[index].as_slice(),
        None => &[],
    }
}
