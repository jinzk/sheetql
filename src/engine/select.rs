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
    BinaryOperator, Distinct, Expr, GroupByExpr, JoinConstraint, JoinOperator, LimitClause, Offset,
    OrderByKind, Query, Select, SelectItem, SetExpr, TableFactor, TableWithJoins,
};
use sqlparser::ast::{Visit, Visitor};

use crate::database::Schema;
use crate::engine::join::join_key;
use crate::engine::plan::{ExecutionPlan, ExprPlan, QueryPlan};
use crate::engine::rewrite::{AliasPrecedence, ExprRewriter, alias_map, ordinal_literal};
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
use crate::value::values_eq;
use crate::value::values_partial_cmp;

type KeyedRows = Vec<(Vec<Value>, Vec<Value>)>;

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
    let subquery_executor = SchemaSubqueryExecutor { schema };

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
        keyed = execute_aggregate_rows(
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

    let runtime_stats = runtime.stats();
    Ok(crate::engine::QueryResult {
        columns: output_titles,
        rows: final_rows,
        stats: crate::engine::QueryStats {
            input_rows: rows_view.len(),
            correlated_cache_hits: runtime_stats.correlated_cache_hits,
            correlated_cache_misses: runtime_stats.correlated_cache_misses,
            ..Default::default()
        },
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
    collect_subqueries_from_query(query, &mut queries);
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

fn collect_subqueries_from_query(query: &Query, output: &mut Vec<Query>) {
    struct Collector<'a> {
        output: &'a mut Vec<Query>,
    }
    impl Visitor for Collector<'_> {
        type Break = ();
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            match expr {
                Expr::Subquery(query) => self.output.push((**query).clone()),
                Expr::Exists { subquery, .. } | Expr::InSubquery { subquery, .. } => {
                    self.output.push((**subquery).clone())
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }
    let _ = query.visit(&mut Collector { output });
}

struct SchemaSubqueryExecutor<'a> {
    schema: &'a Schema,
}

impl SubqueryExecutor for SchemaSubqueryExecutor<'_> {
    fn execute(&self, query: &Query, scope: &OuterScope<'_>) -> Result<SubqueryResult, Error> {
        let result =
            execute_select_query_with_runtime(self.schema, query, Some(scope), scope.runtime)?;
        Ok(SubqueryResult {
            columns: result.columns,
            rows: result.rows,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_aggregate_rows(
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
    let groups = build_groups(lookup, rows, group_sources, active, now, expr_plan, runtime)?;
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

/// Load every relation in the FROM clause, folding in joins. A single plain
/// table is borrowed straight from the schema (zero-copy); any join or
/// multi-table FROM materializes an owned copy.
/// Materialized FROM-clause state: the flattened column references and the
/// combined row set (borrowed when a single plain table is selected).
type Relations<'a> = (Vec<ColumnRef>, Cow<'a, [Vec<Value>]>);

/// Decide which WHERE conjuncts can be pushed down onto individual relations.
/// Pushdown is only valid when every join is inner/cross, and only for
/// conjuncts that are pure scalar predicates (no subquery, aggregate, or
/// window function) and reference columns of exactly one relation.
fn pushdown_conjuncts(from: &[TableWithJoins], selection: Option<&Expr>) -> Option<Vec<Expr>> {
    let selection = selection?;
    if !from_is_inner_only(from) {
        return None;
    }
    let conjuncts = split_conjuncts(selection);
    if conjuncts.len() < 2 {
        // A single conjunct is trivially applied by the WHERE scan; pushing it
        // down adds no benefit.
        return None;
    }
    let pushable: Vec<Expr> = conjuncts
        .into_iter()
        .filter(|conjunct| conjunct_is_pushable(conjunct))
        .cloned()
        .collect();
    (!pushable.is_empty()).then_some(pushable)
}

fn from_is_inner_only(from: &[TableWithJoins]) -> bool {
    from.iter().all(|item| {
        item.joins.iter().all(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Inner(_) | JoinOperator::CrossJoin(_)
            )
        })
    })
}

/// Split an expression into its top-level AND conjuncts.
fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    collect_conjuncts(expr, &mut out);
    out
}

fn collect_conjuncts<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_conjuncts(left, out);
            collect_conjuncts(right, out);
        }
        other => out.push(other),
    }
}

/// A conjunct is pushable when it is a pure scalar predicate: it contains no
/// subquery, aggregate call, or window function.
fn conjunct_is_pushable(expr: &Expr) -> bool {
    struct Check {
        ok: bool,
        query_depth: usize,
    }
    impl Visitor for Check {
        type Break = ();
        fn pre_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.query_depth > 0 {
                return ControlFlow::Continue(());
            }
            match expr {
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                    self.ok = false;
                    ControlFlow::Break(())
                }
                Expr::Function(func) if func.over.is_some() => {
                    self.ok = false;
                    ControlFlow::Break(())
                }
                _ => ControlFlow::Continue(()),
            }
        }
    }
    // Reject expressions containing aggregates directly.
    if crate::functions::contains_aggregate(expr) {
        return false;
    }
    let mut check = Check {
        ok: true,
        query_depth: 0,
    };
    let _ = expr.visit(&mut check);
    check.ok
}

/// The global-column names a predicate may reference, mirroring the keys used
/// by `build_lookup` (bare, qualifier-qualified, table-qualified).
fn referenced_column_names(expr: &Expr) -> Vec<String> {
    struct Collector {
        names: Vec<String>,
        query_depth: usize,
    }
    impl Visitor for Collector {
        type Break = ();
        fn pre_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _query: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.query_depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.query_depth > 0 {
                return ControlFlow::Continue(());
            }
            match expr {
                Expr::Identifier(ident) => {
                    self.names.push(ident.value.to_lowercase());
                }
                Expr::CompoundIdentifier(parts) => {
                    let lower: Vec<String> = parts.iter().map(|p| p.value.to_lowercase()).collect();
                    if lower.len() == 2 {
                        self.names.push(format!("{}.{}", lower[0], lower[1]));
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        }
    }
    let mut collector = Collector {
        names: Vec::new(),
        query_depth: 0,
    };
    let _ = expr.visit(&mut collector);
    collector.names
}

/// True if every referenced name resolves to exactly one column of `schema`.
fn conjunct_fits_relation(names: &[String], schema: &[ColumnRef]) -> bool {
    if names.is_empty() {
        return false;
    }
    names.iter().all(|name| {
        let mut matches = schema.iter().enumerate().filter(|(_, reference)| {
            reference.column == *name
                || format!("{}.{}", reference.qualifier, reference.column) == *name
                || format!("{}.{}", reference.table_name, reference.column) == *name
        });
        matches.next().is_some() && matches.next().is_none()
    })
}

fn apply_predicate<'a>(
    relation: Relation<'a>,
    predicates: &[&Expr],
    now: chrono::DateTime<Local>,
    runtime: &QueryRuntime,
) -> Result<Relation<'a>, Error> {
    let lookup = build_lookup(&relation.schema)?;
    let relation_rows = relation.rows.into_owned();
    let mut kept: Vec<Vec<Value>> = Vec::with_capacity(relation_rows.len());
    for row in &relation_rows {
        let ctx = EvalContext::new(&lookup, &relation_rows, &[], now, runtime);
        let all = predicates.iter().try_fold(true, |acc, predicate| {
            Ok::<_, Error>(acc && eval_expr(&ctx, predicate, row)?.truthy())
        })?;
        if all {
            kept.push(row.clone());
        }
    }
    Ok(Relation::<'_> {
        schema: relation.schema,
        rows: Cow::Owned(kept),
    })
}

fn collect_relations<'a>(
    schema: &'a Schema,
    from: &[TableWithJoins],
    now: chrono::DateTime<Local>,
    pushdown: Option<&[Expr]>,
    runtime: &'a QueryRuntime,
) -> Result<Relations<'a>, Error> {
    let mut schema_refs: Vec<ColumnRef> = vec![];
    let mut rows: Cow<'a, [Vec<Value>]> = Cow::Owned(vec![]);

    // Conjuncts not yet assigned to a relation. Each is dropped once it has
    // been pushed onto the relation that owns all of its columns.
    let mut pending: Vec<Expr> = pushdown.map_or_else(Vec::new, |list| list.to_vec());

    for (index, table_with_joins) in from.iter().enumerate() {
        let base = load_relation_filtered(
            schema,
            &table_with_joins.relation,
            &mut pending,
            now,
            runtime,
        )?;

        if table_with_joins.joins.is_empty() {
            if index == 0 {
                schema_refs = base.schema;
                rows = base.rows;
            } else {
                schema_refs.extend(base.schema.clone());
                rows = Cow::Owned(cross_combine(&rows, &base.rows));
            }
            continue;
        }

        // This relation carries joins, so the accumulated rows are
        // materialized and each join is folded in.
        let mut current = rows.into_owned();
        if index == 0 {
            schema_refs = base.schema;
            current = base.rows.into_owned();
        } else {
            schema_refs.extend(base.schema.clone());
            current = cross_combine(&current, &base.rows);
        }

        for join in &table_with_joins.joins {
            let right = load_relation_filtered(schema, &join.relation, &mut pending, now, runtime)?;
            let merged = apply_join(
                &schema_refs,
                &current,
                &right,
                &join.join_operator,
                now,
                runtime,
            )?;
            schema_refs = merged.0;
            current = merged.1;
        }
        rows = Cow::Owned(current);
    }

    Ok((schema_refs, rows))
}

/// Load a relation and, where possible, push candidate WHERE conjuncts onto it.
/// A conjunct is applied to this relation only when every column it references
/// resolves uniquely within this relation's schema.
fn load_relation_filtered<'a>(
    schema: &'a Schema,
    factor: &TableFactor,
    pending: &mut Vec<Expr>,
    now: chrono::DateTime<Local>,
    runtime: &'a QueryRuntime,
) -> Result<Relation<'a>, Error> {
    let mut relation = load_relation(schema, factor, runtime)?;
    if !pending.is_empty() {
        let mut assigned: Vec<Expr> = Vec::new();
        let mut remaining: Vec<Expr> = Vec::with_capacity(pending.len());
        for conjunct in pending.drain(..) {
            let names = referenced_column_names(&conjunct);
            if conjunct_fits_relation(&names, &relation.schema) {
                assigned.push(conjunct);
            } else {
                remaining.push(conjunct);
            }
        }
        *pending = remaining;
        if !assigned.is_empty() {
            let assigned_refs: Vec<&Expr> = assigned.iter().collect();
            relation = apply_predicate(relation, &assigned_refs, now, runtime)?;
        }
    }
    Ok(relation)
}

fn planned_titles(plan: &[PlannedProjectionItem]) -> Vec<String> {
    plan.iter()
        .map(|item| match item {
            PlannedProjectionItem::Column { title, .. }
            | PlannedProjectionItem::Expression { title, .. } => title.clone(),
        })
        .collect()
}

pub(crate) enum PlannedProjectionItem {
    Column { index: usize, title: String },
    Expression { id: ExprId, title: String },
}

pub(crate) enum PlannedGroupSource {
    Expr(ExprId),
    Column(usize),
}

pub(crate) struct PlannedOrderTerm {
    source: PlannedOrderSource,
    ascending: bool,
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

fn order_terms(
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
fn order_keys(
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

fn sort_planned_keyed(keyed: &mut [(Vec<Value>, Vec<Value>)], terms: &[PlannedOrderTerm]) {
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

fn representative_row<'a>(rows: &'a [Vec<Value>], group: &[usize]) -> &'a [Value] {
    match crate::engine::aggregate::representative_index(group) {
        Some(index) => rows[index].as_slice(),
        None => &[],
    }
}

fn build_groups(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    sources: &[PlannedGroupSource],
    active: &[usize],
    now: chrono::DateTime<Local>,
    expr_plan: &ExprPlan,
    runtime: &QueryRuntime,
) -> Result<Vec<Vec<usize>>, Error> {
    if sources.is_empty() {
        return Ok(vec![active.to_vec()]);
    }

    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut index: HashMap<Vec<GroupKey>, usize> = HashMap::new();

    for &row_index in active {
        let row = &rows[row_index];
        let mut key: Vec<GroupKey> = Vec::with_capacity(sources.len());
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
                groups.push(vec![]);
                let new_index = groups.len() - 1;
                index.insert(key, new_index);
                new_index
            }
        };
        groups[group_index].push(row_index);
    }

    Ok(groups)
}

#[derive(Debug)]
pub(crate) enum ProjectionItem {
    Column { index: usize, title: String },
    Expression { expr: Box<Expr>, title: String },
}

fn build_projection_plan(
    schema: &[ColumnRef],
    projection: &[SelectItem],
) -> Result<Vec<ProjectionItem>, Error> {
    let mut plan: Vec<ProjectionItem> = vec![];
    for item in projection {
        match item {
            SelectItem::Wildcard(_) => {
                for (index, column) in schema.iter().enumerate() {
                    plan.push(ProjectionItem::Column {
                        index,
                        title: column.column.clone(),
                    });
                }
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let qualifier = match kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        object_name_to_parts(name).join(".")
                    }
                    _ => return Err("Unsupported qualified wildcard".to_string().into()),
                };
                let mut matched = false;
                for (index, column) in schema.iter().enumerate() {
                    if column.qualifier == qualifier || column.table_name == qualifier {
                        plan.push(ProjectionItem::Column {
                            index,
                            title: column.column.clone(),
                        });
                        matched = true;
                    }
                }
                if !matched {
                    return Err(format!("Table `{qualifier}` not found").into());
                }
            }
            // Titles keep the user's spelling; stored expressions are
            // case-normalized so per-row column resolution hits directly.
            SelectItem::UnnamedExpr(expr) => plan.push(ProjectionItem::Expression {
                expr: Box::new(ExprRewriter::lowercase().rewritten(expr)),
                title: expr_title(expr),
            }),
            SelectItem::ExprWithAlias { expr, alias } => plan.push(ProjectionItem::Expression {
                expr: Box::new(ExprRewriter::lowercase().rewritten(expr)),
                title: alias.to_string(),
            }),
            SelectItem::ExprWithAliases { .. } => {
                return Err("Multiple aliases are not supported".to_string().into());
            }
        }
    }
    Ok(plan)
}

fn project(
    ctx: &EvalContext,
    plan: &[PlannedProjectionItem],
    row: &[Value],
    expr_plan: &ExprPlan,
) -> Result<Vec<Value>, Error> {
    let mut out: Vec<Value> = Vec::with_capacity(plan.len());
    for item in plan {
        match item {
            PlannedProjectionItem::Column { index, .. } => {
                out.push(row.get(*index).cloned().unwrap_or(Value::Null));
            }
            PlannedProjectionItem::Expression { id, .. } => {
                out.push(eval_expr(ctx, expr_plan.expression(*id), row)?);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct ColumnRef {
    table_name: String,
    qualifier: String,
    column: String,
}

struct Relation<'a> {
    schema: Vec<ColumnRef>,
    rows: Cow<'a, [Vec<Value>]>,
}

pub(crate) fn object_name_to_parts(name: &sqlparser::ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.to_lowercase())
        .collect()
}

fn expr_title(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|ident| ident.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        other => other.to_string(),
    }
}

fn load_relation<'a>(
    schema: &'a Schema,
    factor: &TableFactor,
    runtime: &'a QueryRuntime,
) -> Result<Relation<'a>, Error> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let parts = object_name_to_parts(name);
            let (database, table_name) = match parts.as_slice() {
                [table_name] => (None, table_name.as_str()),
                [database, table_name] => (Some(database.as_str()), table_name.as_str()),
                _ => {
                    return Err("Table reference must be `table` or `database.table`"
                        .to_string()
                        .into());
                }
            };
            let (_, table) = schema.resolve_table(database, table_name)?;
            let table_name = table.name.clone();
            let qualifier = alias
                .as_ref()
                .map(|alias| alias.name.value.to_lowercase())
                .unwrap_or_else(|| table_name.clone());
            let schema_refs = table
                .columns
                .iter()
                .map(|column| ColumnRef {
                    table_name: table_name.clone(),
                    qualifier: qualifier.clone(),
                    column: column.clone(),
                })
                .collect();
            Ok(Relation {
                schema: schema_refs,
                rows: Cow::Borrowed(&table.rows),
            })
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => {
            let result = crate::engine::set::execute_query_with_runtime(schema, subquery, runtime)?;
            let qualifier = alias
                .as_ref()
                .map(|alias| alias.name.value.to_lowercase())
                .ok_or("Derived tables require an alias")?;
            let schema_refs = result
                .columns
                .iter()
                .map(|column| ColumnRef {
                    table_name: qualifier.clone(),
                    qualifier: qualifier.clone(),
                    column: column.clone(),
                })
                .collect();
            Ok(Relation {
                schema: schema_refs,
                rows: Cow::Owned(result.rows),
            })
        }
        _ => Err("Only plain table references are supported in FROM"
            .to_string()
            .into()),
    }
}

fn cross_combine(left_rows: &[Vec<Value>], right_rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut output = vec![];
    for left in left_rows {
        for right in right_rows {
            let mut combined = left.clone();
            combined.extend_from_slice(right);
            output.push(combined);
        }
    }
    output
}

fn apply_join(
    left_schema: &[ColumnRef],
    left_rows: &[Vec<Value>],
    right: &Relation<'_>,
    operator: &JoinOperator,
    now: chrono::DateTime<Local>,
    runtime: &QueryRuntime,
) -> Result<(Vec<ColumnRef>, Vec<Vec<Value>>), Error> {
    let mut schema = left_schema.to_vec();
    schema.extend(right.schema.clone());
    let left_len = left_schema.len();
    let right_len = right.schema.len();

    // Build the column lookup once for the whole join instead of per row pair.
    let lookup = build_lookup(&schema)?;

    let mut output: Vec<Vec<Value>> = vec![];
    let mut matched_left = vec![false; left_rows.len()];
    let mut matched_right = vec![false; right.rows.len()];

    let using_pairs = using_column_pairs(operator, left_schema, &right.schema)?;
    let equi_pairs = using_pairs.or_else(|| on_column_pairs(operator, left_schema, &right.schema));

    if let Some(pairs) = &equi_pairs {
        hash_using_join(
            left_rows,
            &right.rows,
            pairs,
            &mut output,
            &mut matched_left,
            &mut matched_right,
        );
    } else {
        // Nested-loop fallback: reuse one scratch buffer to build candidate
        // combined rows so only genuinely matching pairs get cloned.
        let mut scratch: Vec<Value> = Vec::with_capacity(left_len + right_len);
        for (left_index, left_row) in left_rows.iter().enumerate() {
            for (right_index, right_row) in right.rows.iter().enumerate() {
                scratch.clear();
                scratch.extend_from_slice(left_row);
                scratch.extend_from_slice(right_row);
                if join_keep(operator, &lookup, now, runtime, &scratch)? {
                    matched_left[left_index] = true;
                    matched_right[right_index] = true;
                    output.push(scratch.clone());
                }
            }
        }
    }

    let (is_left, is_right) = match operator {
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => (true, false),
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => (false, true),
        JoinOperator::FullOuter(_) => (true, true),
        _ => (false, false),
    };

    if is_left {
        for (index, left_row) in left_rows.iter().enumerate() {
            if !matched_left[index] {
                let mut combined = left_row.clone();
                combined.extend(std::iter::repeat_n(Value::Null, right_len));
                output.push(combined);
            }
        }
    }

    if is_right {
        for (index, right_row) in right.rows.iter().enumerate() {
            if !matched_right[index] {
                let mut combined: Vec<Value> = std::iter::repeat_n(Value::Null, left_len).collect();
                combined.extend_from_slice(right_row);
                output.push(combined);
            }
        }
    }

    Ok((schema, output))
}

fn hash_using_join(
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
    pairs: &[(usize, usize)],
    output: &mut Vec<Vec<Value>>,
    matched_left: &mut [bool],
    matched_right: &mut [bool],
) {
    // Build the hash index on the smaller input, while emitting matches in
    // logical left-row order so changing the build side does not change the
    // observable order of existing queries.
    if left_rows.len() <= right_rows.len() {
        let mut index: HashMap<Vec<GroupKey>, Vec<usize>> = HashMap::new();
        for (left_index, row) in left_rows.iter().enumerate() {
            if pairs
                .iter()
                .any(|&(left_column, _)| row[left_column].is_null())
            {
                continue;
            }
            index
                .entry(join_key(
                    row,
                    pairs.iter().map(|&(left_column, _)| left_column),
                ))
                .or_default()
                .push(left_index);
        }
        let mut matches: Vec<Vec<usize>> = vec![Vec::new(); left_rows.len()];
        for (right_index, row) in right_rows.iter().enumerate() {
            if pairs
                .iter()
                .any(|&(_, right_column)| row[right_column].is_null())
            {
                continue;
            }
            let key = join_key(row, pairs.iter().map(|&(_, right_column)| right_column));
            if let Some(left_indices) = index.get(&key) {
                for &left_index in left_indices {
                    if pairs.iter().all(|&(left_column, right_column)| {
                        values_eq(&left_rows[left_index][left_column], &row[right_column])
                    }) {
                        matched_left[left_index] = true;
                        matched_right[right_index] = true;
                        matches[left_index].push(right_index);
                    }
                }
            }
        }
        for (left_index, right_indices) in matches.into_iter().enumerate() {
            for right_index in right_indices {
                let mut row = left_rows[left_index].clone();
                row.extend_from_slice(&right_rows[right_index]);
                output.push(row);
            }
        }
        return;
    }

    let mut index: HashMap<Vec<GroupKey>, Vec<usize>> = HashMap::new();
    for (right_index, row) in right_rows.iter().enumerate() {
        // SQL equi-joins never match NULL keys, so they are excluded from the
        // build side entirely (they would otherwise bucket together and
        // compare equal under `values_eq`).
        if pairs
            .iter()
            .any(|&(_, right_column)| row[right_column].is_null())
        {
            continue;
        }
        let key = join_key(row, pairs.iter().map(|&(_, right_column)| right_column));
        index.entry(key).or_default().push(right_index);
    }

    for (left_index, left_row) in left_rows.iter().enumerate() {
        if pairs
            .iter()
            .any(|&(left_column, _)| left_row[left_column].is_null())
        {
            continue;
        }
        let key = join_key(left_row, pairs.iter().map(|&(left_column, _)| left_column));
        if let Some(right_indices) = index.get(&key) {
            for &right_index in right_indices {
                if !pairs.iter().all(|&(left_column, right_column)| {
                    values_eq(
                        &left_row[left_column],
                        &right_rows[right_index][right_column],
                    )
                }) {
                    continue;
                }
                matched_left[left_index] = true;
                matched_right[right_index] = true;
                let mut row = left_row.clone();
                row.extend_from_slice(&right_rows[right_index]);
                output.push(row);
            }
        }
    }
}

fn on_column_pairs(
    operator: &JoinOperator,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Option<Vec<(usize, usize)>> {
    let JoinConstraint::On(expr) = join_constraint(operator)? else {
        return None;
    };
    let mut pairs = Vec::new();
    collect_equi_pairs(expr, left_schema, right_schema, &mut pairs)?;
    (!pairs.is_empty()).then_some(pairs)
}

fn collect_equi_pairs(
    expr: &Expr,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
    pairs: &mut Vec<(usize, usize)>,
) -> Option<()> {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_equi_pairs(left, left_schema, right_schema, pairs)?;
            collect_equi_pairs(right, left_schema, right_schema, pairs)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let left_column = resolve_join_column(left, left_schema, right_schema)?;
            let right_column = resolve_join_column(right, left_schema, right_schema)?;
            match (left_column, right_column) {
                (JoinColumn::Left(left_index), JoinColumn::Right(right_index)) => {
                    pairs.push((left_index, right_index));
                    Some(())
                }
                (JoinColumn::Right(right_index), JoinColumn::Left(left_index)) => {
                    pairs.push((left_index, right_index));
                    Some(())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

enum JoinColumn {
    Left(usize),
    Right(usize),
}

fn resolve_join_column(
    expr: &Expr,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Option<JoinColumn> {
    let parts = match expr {
        Expr::Identifier(identifier) => vec![identifier.value.to_lowercase()],
        Expr::CompoundIdentifier(parts) => {
            parts.iter().map(|part| part.value.to_lowercase()).collect()
        }
        _ => return None,
    };
    let (qualifier, column) = match parts.as_slice() {
        [column] => (None, column.as_str()),
        [qualifier, column] => (Some(qualifier.as_str()), column.as_str()),
        _ => return None,
    };

    let find = |schema: &[ColumnRef]| {
        let mut matches = schema.iter().enumerate().filter(|(_, reference)| {
            reference.column == column
                && qualifier.is_none_or(|qualifier| {
                    reference.qualifier == qualifier || reference.table_name == qualifier
                })
        });
        let (index, _) = matches.next()?;
        matches.next().is_none().then_some(index)
    };

    match (find(left_schema), find(right_schema)) {
        (Some(index), None) => Some(JoinColumn::Left(index)),
        (None, Some(index)) => Some(JoinColumn::Right(index)),
        _ => None,
    }
}

fn join_constraint(operator: &JoinOperator) -> Option<&JoinConstraint> {
    match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::Left(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::Right(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::CrossJoin(constraint) => Some(constraint),
        _ => None,
    }
}

fn using_column_pairs(
    operator: &JoinOperator,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Result<Option<Vec<(usize, usize)>>, Error> {
    let columns = match operator {
        JoinOperator::Join(JoinConstraint::Using(cols))
        | JoinOperator::Inner(JoinConstraint::Using(cols))
        | JoinOperator::Left(JoinConstraint::Using(cols))
        | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
        | JoinOperator::Right(JoinConstraint::Using(cols))
        | JoinOperator::RightOuter(JoinConstraint::Using(cols))
        | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => cols,
        _ => return Ok(None),
    };

    let mut pairs = vec![];
    for column in columns {
        let name = column
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|ident| ident.value.to_lowercase())
            .unwrap_or_default();
        let left_index = left_schema
            .iter()
            .position(|column_ref| column_ref.column == name)
            .ok_or_else(|| format!("USING column `{name}` not found in left table"))?;
        let right_index = right_schema
            .iter()
            .position(|column_ref| column_ref.column == name)
            .ok_or_else(|| format!("USING column `{name}` not found in right table"))?;
        pairs.push((left_index, right_index));
    }
    Ok(Some(pairs))
}

fn join_keep(
    operator: &JoinOperator,
    lookup: &HashMap<String, usize>,
    now: chrono::DateTime<Local>,
    runtime: &QueryRuntime,
    combined: &[Value],
) -> Result<bool, Error> {
    let constraint = match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::Left(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::Right(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::CrossJoin(constraint) => constraint,
        _ => return Err("Unsupported join type".to_string().into()),
    };

    match constraint {
        JoinConstraint::On(expr) => {
            let ctx = EvalContext::new(lookup, &[], &[], now, runtime);
            let value = eval_expr(&ctx, expr, combined)?;
            Ok(value.truthy())
        }
        JoinConstraint::Using(_) => Err("This join type cannot be combined with a USING clause"
            .to_string()
            .into()),
        JoinConstraint::None => Ok(true),
        JoinConstraint::Natural => Err("NATURAL JOIN is not supported".to_string().into()),
    }
}

fn build_lookup(schema: &[ColumnRef]) -> Result<HashMap<String, usize>, Error> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();

    for (index, column) in schema.iter().enumerate() {
        for name in [
            column.column.clone(),
            format!("{}.{}", column.qualifier, column.column),
            format!("{}.{}", column.table_name, column.column),
        ] {
            let indices = map.entry(name).or_default();
            if !indices.contains(&index) {
                indices.push(index);
            }
        }
    }

    let mut lookup: HashMap<String, usize> = HashMap::new();
    for (name, indices) in &map {
        lookup.insert(
            name.clone(),
            if indices.len() == 1 {
                indices[0]
            } else {
                usize::MAX
            },
        );
    }
    Ok(lookup)
}
