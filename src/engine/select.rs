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
use crate::engine::rewrite::{AliasPrecedence, ExprRewriter, alias_map, ordinal_literal};
use crate::error::Error;
use crate::evaluator::EvalContext;
use crate::evaluator::eval_expr;
use crate::evaluator::{ExprId, ExprIds, GroupState};
use crate::functions::contains_aggregate;
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;
use crate::value::values_eq;
use crate::value::values_partial_cmp;

type KeyedRows = Vec<(Vec<Value>, Vec<Value>)>;

pub(crate) fn execute_query<'a>(
    schema: &'a Schema,
    query: &Query,
) -> Result<crate::engine::QueryResult, Error> {
    let now = Local::now();
    let select: &Select = match &*query.body {
        SetExpr::Select(select) => select,
        _ => return Err("Only plain SELECT queries are supported".to_string().into()),
    };

    let (schema_refs, mut rows): (Vec<ColumnRef>, Cow<'a, [Vec<Value>]>) =
        collect_relations(schema, &select.from, now)?;
    if select.from.is_empty() {
        rows = Cow::Owned(vec![vec![]]);
    }
    let rows_view: &[Vec<Value>] = &rows;

    let lookup = build_lookup(&schema_refs)?;

    // WHERE only normalizes identifier case; aliases are not visible there.
    let selection = select
        .selection
        .as_ref()
        .map(|selection| ExprRewriter::lowercase().rewritten(selection));

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
    let output_titles = planned_titles(&planned_projection);
    // WHERE keeps a set of row indices instead of copying the rows themselves.
    let active = collect_active_rows(selection.as_ref(), &lookup, rows_view, scan_target, now)?;

    let use_top_n = execution_plan.top_n;

    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = vec![];
    let mut top_n = use_top_n.then(|| {
        TopN::new(
            limit.unwrap().saturating_add(offset.unwrap_or(0)),
            &planned_order,
        )
    });

    if execution_plan.aggregate {
        keyed = execute_aggregate_rows(
            &lookup,
            rows_view,
            &planned_groups,
            having.as_ref(),
            &planned_projection,
            &planned_order,
            &active,
            now,
            &expr_plan,
        )?;
    } else {
        keyed = execute_regular_rows(
            &lookup,
            rows_view,
            &planned_projection,
            &planned_order,
            &active,
            &execution_plan,
            top_n.as_mut(),
            now,
            &expr_plan,
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
        sort_planned_keyed(&mut keyed, &planned_order);
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

    Ok(crate::engine::QueryResult {
        columns: output_titles,
        rows: final_rows,
        stats: crate::engine::QueryStats {
            input_rows: rows_view.len(),
            ..Default::default()
        },
    })
}

fn collect_active_rows(
    selection: Option<&Expr>,
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    scan_target: usize,
    now: chrono::DateTime<Local>,
) -> Result<Vec<usize>, Error> {
    if scan_target == 0 {
        return Ok(Vec::new());
    }
    if let Some(selection) = selection {
        let ctx = EvalContext::new(lookup, rows, &[], now);
        let mut active = Vec::new();
        for (index, row) in rows.iter().enumerate() {
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

#[allow(clippy::too_many_arguments)]
fn execute_aggregate_rows(
    lookup: &HashMap<String, usize>,
    rows: &[Vec<Value>],
    group_sources: &[PlannedGroupSource],
    having: Option<&Expr>,
    projection: &[PlannedProjectionItem],
    order_terms: &[PlannedOrderTerm],
    active: &[usize],
    now: chrono::DateTime<Local>,
    expr_plan: &ExprPlan,
) -> Result<KeyedRows, Error> {
    let groups = build_groups(lookup, rows, group_sources, active, now, expr_plan)?;
    let mut keyed = Vec::with_capacity(groups.len());
    for group in &groups {
        let group_state = RefCell::new(GroupState::default());
        let ctx = EvalContext::with_group_state(
            lookup,
            rows,
            group,
            now,
            &expr_plan.ids,
            &expr_plan.expressions,
            &group_state,
        );
        let representative = representative_row(rows, group);
        if let Some(expr) = having {
            let id = expr_plan.id_of(expr);
            if !eval_expr(&ctx, expr_plan.expression(id), representative)?.truthy() {
                continue;
            }
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
) -> Result<KeyedRows, Error> {
    let ctx = EvalContext::with_expr_ids(
        lookup,
        rows,
        &[],
        now,
        &expr_plan.ids,
        &expr_plan.expressions,
    );
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
    for &row_index in &active[start..end] {
        let row = &rows[row_index];
        let output = project(&ctx, projection, row, expr_plan)?;
        let keys = order_keys(order_terms, &ctx, &output, row, expr_plan)?;
        if let Some(heap) = top_n.as_deref_mut() {
            heap.push(keys, output);
        } else {
            keyed.push((keys, output));
        }
    }
    Ok(keyed)
}

#[derive(Debug, Clone, Copy)]
struct ExecutionPlan {
    limit: Option<usize>,
    offset: usize,
    aggregate: bool,
    distinct: bool,
    early_stop: bool,
    top_n: bool,
}

struct ExprPlan {
    expressions: Vec<Expr>,
    ids: ExprIds,
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
        assert_eq!(plan.ids[&first.to_string()], plan.ids[&second.to_string()]);
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

impl ExprPlan {
    fn id_of(&self, expr: &Expr) -> ExprId {
        self.ids
            .get(&expr.to_string())
            .copied()
            .expect("expression must be registered before execution")
    }

    fn expression(&self, id: ExprId) -> &Expr {
        &self.expressions[id.0]
    }

    fn build(
        selection: Option<&Expr>,
        having: Option<&Expr>,
        groups: &[Expr],
        projection: &[ProjectionItem],
        order: &[OrderTerm],
    ) -> Self {
        let mut plan = Self {
            expressions: Vec::new(),
            ids: HashMap::new(),
        };
        let mut register = |expr: &Expr| plan.register_tree(expr);
        selection.into_iter().for_each(&mut register);
        having.into_iter().for_each(&mut register);
        groups.iter().for_each(&mut register);
        for item in projection {
            if let ProjectionItem::Expression { expr, .. } = item {
                register(expr);
            }
        }
        for term in order {
            if let OrderSource::Expr(expr) = &term.source {
                register(expr);
            }
        }
        plan
    }

    fn register_tree(&mut self, root: &Expr) {
        struct Register<'a> {
            plan: &'a mut ExprPlan,
        }
        impl Visitor for Register<'_> {
            type Break = ();
            fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
                let key = expr.to_string();
                if !self.plan.ids.contains_key(&key) {
                    let id = ExprId(self.plan.ids.len());
                    self.plan.ids.insert(key, id);
                    self.plan.expressions.push(expr.clone());
                }
                ControlFlow::Continue(())
            }
        }
        let _ = root.visit(&mut Register { plan: self });
    }
}

impl ExecutionPlan {
    fn new(
        limit: Option<usize>,
        offset: Option<usize>,
        aggregate: bool,
        distinct: bool,
        has_order: bool,
    ) -> Self {
        let offset = offset.unwrap_or(0);
        let early_stop = !aggregate && !has_order && !distinct;
        let top_n = !aggregate && has_order && !distinct && limit.is_some();
        Self {
            limit,
            offset,
            aggregate,
            distinct,
            early_stop,
            top_n,
        }
    }

    fn scan_target(self) -> usize {
        if self.early_stop {
            self.limit.unwrap_or(usize::MAX).saturating_add(self.offset)
        } else {
            usize::MAX
        }
    }
}

/// Load every relation in the FROM clause, folding in joins. A single plain
/// table is borrowed straight from the schema (zero-copy); any join or
/// multi-table FROM materializes an owned copy.
/// Materialized FROM-clause state: the flattened column references and the
/// combined row set (borrowed when a single plain table is selected).
type Relations<'a> = (Vec<ColumnRef>, Cow<'a, [Vec<Value>]>);

fn collect_relations<'a>(
    schema: &'a Schema,
    from: &[TableWithJoins],
    now: chrono::DateTime<Local>,
) -> Result<Relations<'a>, Error> {
    let mut schema_refs: Vec<ColumnRef> = vec![];
    let mut rows: Cow<'a, [Vec<Value>]> = Cow::Owned(vec![]);

    for (index, table_with_joins) in from.iter().enumerate() {
        let base = load_relation(schema, &table_with_joins.relation)?;

        if table_with_joins.joins.is_empty() {
            if index == 0 {
                schema_refs = base.schema;
                rows = Cow::Borrowed(base.rows);
            } else {
                schema_refs.extend(base.schema.clone());
                rows = Cow::Owned(cross_combine(&rows, base.rows));
            }
            continue;
        }

        // This relation carries joins, so the accumulated rows are
        // materialized and each join is folded in.
        let mut current = rows.into_owned();
        if index == 0 {
            schema_refs = base.schema;
            current = base.rows.to_vec();
        } else {
            schema_refs.extend(base.schema.clone());
            current = cross_combine(&current, base.rows);
        }

        for join in &table_with_joins.joins {
            let right = load_relation(schema, &join.relation)?;
            let merged = apply_join(&schema_refs, &current, &right, &join.join_operator, now)?;
            schema_refs = merged.0;
            current = merged.1;
        }
        rows = Cow::Owned(current);
    }

    Ok((schema_refs, rows))
}

fn planned_titles(plan: &[PlannedProjectionItem]) -> Vec<String> {
    plan.iter()
        .map(|item| match item {
            PlannedProjectionItem::Column { title, .. }
            | PlannedProjectionItem::Expression { title, .. } => title.clone(),
        })
        .collect()
}

enum PlannedProjectionItem {
    Column { index: usize, title: String },
    Expression { id: ExprId, title: String },
}

enum PlannedGroupSource {
    Expr(ExprId),
    Column(usize),
}

struct PlannedOrderTerm {
    source: PlannedOrderSource,
    ascending: bool,
}

enum PlannedOrderSource {
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
enum GroupSource<'g> {
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
struct OrderTerm {
    source: OrderSource,
    ascending: bool,
}

enum OrderSource {
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
    match group.first() {
        Some(index) => rows[*index].as_slice(),
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
) -> Result<Vec<Vec<usize>>, Error> {
    if sources.is_empty() {
        return Ok(vec![active.to_vec()]);
    }

    let ctx = EvalContext::new(lookup, rows, &[], now);
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
    rows: &'a [Vec<Value>],
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

fn load_relation<'a>(schema: &'a Schema, factor: &TableFactor) -> Result<Relation<'a>, Error> {
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
                rows: &table.rows,
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
            right.rows,
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
                if join_keep(operator, &lookup, now, &scratch)? {
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
        let key = pairs
            .iter()
            .map(|&(_, right_column)| group_key(&row[right_column]))
            .collect();
        index.entry(key).or_default().push(right_index);
    }

    for (left_index, left_row) in left_rows.iter().enumerate() {
        if pairs
            .iter()
            .any(|&(left_column, _)| left_row[left_column].is_null())
        {
            continue;
        }
        let key = pairs
            .iter()
            .map(|&(left_column, _)| group_key(&left_row[left_column]))
            .collect::<Vec<_>>();
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
            let ctx = EvalContext::new(lookup, &[], &[], now);
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
