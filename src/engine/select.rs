use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;

use chrono::Local;
use sqlparser::ast::{
    Distinct, Expr, GroupByExpr, JoinConstraint, JoinOperator, LimitClause, Offset, OrderByKind,
    Query, Select, SelectItem, SetExpr, TableFactor,
};

use crate::database::Schema;
use crate::evaluator::eval_expr;
use crate::evaluator::EvalContext;
use crate::functions::contains_aggregate;
use crate::value::values_eq;
use crate::value::values_partial_cmp;
use crate::value::Value;

pub(crate) fn execute_query(
    schema: &Schema,
    query: &Query,
) -> Result<crate::engine::QueryResult, String> {
    let now = Local::now();
    let select: &Select = match &*query.body {
        SetExpr::Select(select) => select,
        _ => return Err("Only plain SELECT queries are supported".to_string()),
    };

    let has_from = !select.from.is_empty();

    let mut schema_refs: Vec<ColumnRef> = vec![];
    let mut rows: Vec<Vec<Value>> = vec![];

    for (index, table_with_joins) in select.from.iter().enumerate() {
        let base = load_relation(schema, &table_with_joins.relation)?;
        if index == 0 {
            schema_refs = base.schema;
            rows = base.rows;
        } else {
            schema_refs.extend(base.schema.clone());
            rows = cross_combine(rows, base.rows);
        }

        for join in &table_with_joins.joins {
            let right = load_relation(schema, &join.relation)?;
            let merged = apply_join(&schema_refs, &rows, &right, &join.join_operator)?;
            schema_refs = merged.0;
            rows = merged.1;
        }
    }

    if !has_from {
        rows = vec![vec![]];
    }

    let lookup = build_lookup(&schema_refs)?;

    if let Some(selection) = &select.selection {
        let ctx = EvalContext::new(&lookup, &rows, &[], now);
        let mut filtered: Vec<Vec<Value>> = vec![];
        for row in &rows {
            let value = eval_expr(&ctx, selection, row)?;
            if value.truthy() {
                filtered.push(row.clone());
            }
        }
        rows = filtered;
    }

    let plan = build_projection_plan(&schema_refs, &select.projection)?;

    let group_exprs = group_by_expressions(&select.group_by)?;
    let is_aggregate = !group_exprs.is_empty()
        || select.projection.iter().any(projection_has_aggregate)
        || select.having.is_some();

    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = vec![];
    let output_titles = titles(&plan);

    let mut order_columns: HashMap<String, usize> = HashMap::new();
    for (name, index) in &lookup {
        order_columns.insert(name.clone(), output_titles.len() + index);
    }
    for (index, title) in output_titles.iter().enumerate() {
        order_columns.insert(title.clone(), index);
    }

    if is_aggregate {
        let groups = build_groups(&lookup, &rows, &group_exprs)?;

        for group in &groups {
            let ctx = EvalContext::new(&lookup, &rows, group, now);
            let representative = representative_row(&rows, group);

            if let Some(having) = &select.having {
                let value = eval_expr(&ctx, having, representative)?;
                if !value.truthy() {
                    continue;
                }
            }

            let out = project(&ctx, &plan, representative)?;

            let mut combined = out;
            combined.extend_from_slice(representative);

            let order_ctx = EvalContext::new(&order_columns, &rows, group, now);
            let keys = compute_order_keys(query, &order_ctx, &combined)?;
            let _original = combined.split_off(output_titles.len());
            keyed.push((keys, combined));
        }
    } else {
        let ctx = EvalContext::new(&lookup, &rows, &[], now);
        for row in &rows {
            let out = project(&ctx, &plan, row)?;
            let mut combined = out;
            combined.extend_from_slice(row);

            let order_ctx = EvalContext::new(&order_columns, &rows, &[], now);
            let keys = compute_order_keys(query, &order_ctx, &combined)?;
            let _original = combined.split_off(output_titles.len());
            keyed.push((keys, combined));
        }
    }

    if is_distinct(select)? {
        let mut seen: HashSet<Vec<Value>> = HashSet::new();
        keyed.retain(|(_, out)| seen.insert(out.clone()));
    }

    if let Some(order) = &query.order_by {
        let exprs = match &order.kind {
            OrderByKind::Expressions(exprs) => exprs,
            OrderByKind::All(_) => return Err("ORDER BY ALL is not supported".to_string()),
        };
        keyed.sort_by(|a, b| compare_keys(&a.0, &b.0, exprs));
    }

    let mut final_rows: Vec<Vec<Value>> = keyed.into_iter().map(|(_, out)| out).collect();

    let (limit, offset) = parse_limit(&query.limit_clause)?;
    if let Some(offset) = offset {
        final_rows = final_rows.into_iter().skip(offset).collect();
    }
    if let Some(limit) = limit {
        final_rows.truncate(limit);
    }

    Ok(crate::engine::QueryResult {
        columns: titles(&plan),
        rows: final_rows,
    })
}

fn titles(plan: &[ProjectionItem]) -> Vec<String> {
    plan.iter()
        .map(|item| match item {
            ProjectionItem::Column { title, .. } => title.clone(),
            ProjectionItem::Expression { title, .. } => title.clone(),
        })
        .collect()
}

fn projection_has_aggregate(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(expr) => contains_aggregate(expr),
        SelectItem::ExprWithAlias { expr, .. } => contains_aggregate(expr),
        SelectItem::ExprWithAliases { expr, .. } => contains_aggregate(expr),
        _ => false,
    }
}

fn group_by_expressions(group_by: &GroupByExpr) -> Result<Vec<&Expr>, String> {
    match group_by {
        GroupByExpr::Expressions(exprs, _) => Ok(exprs.iter().collect()),
        GroupByExpr::All(_) => Err("GROUP BY ALL is not supported".to_string()),
    }
}

fn is_distinct(select: &Select) -> Result<bool, String> {
    match &select.distinct {
        Some(Distinct::Distinct) => Ok(true),
        Some(Distinct::On(_)) => Err("DISTINCT ON is not supported".to_string()),
        Some(Distinct::All) | None => Ok(false),
    }
}

fn parse_limit(
    limit_clause: &Option<LimitClause>,
) -> Result<(Option<usize>, Option<usize>), String> {
    match limit_clause {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset {
            limit, offset, ..
        }) => {
            let limit_value = match limit {
                Some(expr) => eval_const_int(expr)?,
                None => None,
            };
            let offset_value = match offset {
                Some(Offset { value, .. }) => eval_const_int(value)?,
                None => None,
            };
            Ok((limit_value, offset_value))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            let offset_value = eval_const_int(offset)?;
            let limit_value = eval_const_int(limit)?;
            Ok((limit_value, offset_value))
        }
    }
}

fn eval_const_int(expr: &Expr) -> Result<Option<usize>, String> {
    let value = eval_expr(&EvalContext::scalar(), expr, &[])?;
    match value {
        Value::Int(number) => Ok(Some(number.max(0) as usize)),
        _ => Ok(None),
    }
}

fn compute_order_keys(
    query: &Query,
    ctx: &EvalContext,
    row: &[Value],
) -> Result<Vec<Value>, String> {
    let mut keys = vec![];
    if let Some(order) = &query.order_by
        && let OrderByKind::Expressions(exprs) = &order.kind
    {
        for order_expr in exprs {
            keys.push(eval_expr(ctx, &order_expr.expr, row)?);
        }
    }
    Ok(keys)
}

fn compare_keys(a: &[Value], b: &[Value], exprs: &[sqlparser::ast::OrderByExpr]) -> Ordering {
    for (index, order_expr) in exprs.iter().enumerate() {
        let mut ordering = values_partial_cmp(&a[index], &b[index]).unwrap_or(Ordering::Equal);
        if !order_expr.options.asc.unwrap_or(true) {
            ordering = ordering.reverse();
        }
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
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
    group_exprs: &[&Expr],
) -> Result<Vec<Vec<usize>>, String> {
    if group_exprs.is_empty() {
        let all: Vec<usize> = (0..rows.len()).collect();
        return Ok(vec![all]);
    }

    let ctx = EvalContext::new(lookup, rows, &[], Local::now());
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut index: HashMap<Vec<Value>, usize> = HashMap::new();

    for (row_index, row) in rows.iter().enumerate() {
        let mut key: Vec<Value> = vec![];
        for expr in group_exprs {
            let value = eval_expr(&ctx, expr, row)?;
            key.push(value);
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

enum ProjectionItem {
    Column {
        index: usize,
        title: String,
    },
    Expression {
        expr: Box<Expr>,
        title: String,
    },
}

fn build_projection_plan(
    schema: &[ColumnRef],
    projection: &[SelectItem],
) -> Result<Vec<ProjectionItem>, String> {
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
                    _ => return Err("Unsupported qualified wildcard".to_string()),
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
                    return Err(format!("Table `{qualifier}` not found"));
                }
            }
            SelectItem::UnnamedExpr(expr) => plan.push(ProjectionItem::Expression {
                expr: Box::new(expr.clone()),
                title: expr_title(expr),
            }),
            SelectItem::ExprWithAlias { expr, alias } => plan.push(ProjectionItem::Expression {
                expr: Box::new(expr.clone()),
                title: alias.to_string(),
            }),
            SelectItem::ExprWithAliases { .. } => {
                return Err("Multiple aliases are not supported".to_string())
            }
        }
    }
    Ok(plan)
}

fn project(
    ctx: &EvalContext,
    plan: &[ProjectionItem],
    row: &[Value],
) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::with_capacity(plan.len());
    for item in plan {
        match item {
            ProjectionItem::Column { index, .. } => {
                out.push(row.get(*index).cloned().unwrap_or(Value::Null));
            }
            ProjectionItem::Expression { expr, .. } => {
                out.push(eval_expr(ctx, expr, row)?);
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

struct Relation {
    schema: Vec<ColumnRef>,
    rows: Vec<Vec<Value>>,
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

fn load_relation(schema: &Schema, factor: &TableFactor) -> Result<Relation, String> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let parts = object_name_to_parts(name);
            let (database, table_name) = match parts.as_slice() {
                [table_name] => (None, table_name.as_str()),
                [database, table_name] => (Some(database.as_str()), table_name.as_str()),
                _ => {
                    return Err(
                        "Table reference must be `table` or `database.table`".to_string()
                    )
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
                rows: table.rows.clone(),
            })
        }
        _ => Err("Only plain table references are supported in FROM".to_string()),
    }
}

fn cross_combine(
    left_rows: Vec<Vec<Value>>,
    right_rows: Vec<Vec<Value>>,
) -> Vec<Vec<Value>> {
    let mut output = vec![];
    for left in &left_rows {
        for right in &right_rows {
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
    right: &Relation,
    operator: &JoinOperator,
) -> Result<(Vec<ColumnRef>, Vec<Vec<Value>>), String> {
    let mut schema = left_schema.to_vec();
    schema.extend(right.schema.clone());
    let left_len = left_schema.len();
    let right_len = right.schema.len();

    let mut output: Vec<Vec<Value>> = vec![];
    let mut matched_left = vec![false; left_rows.len()];
    let mut matched_right = vec![false; right.rows.len()];

    let using_pairs = using_column_pairs(operator, left_schema, &right.schema)?;

    for (left_index, left_row) in left_rows.iter().enumerate() {
        for (right_index, right_row) in right.rows.iter().enumerate() {
            let keep = match &using_pairs {
                Some(pairs) => pairs
                    .iter()
                    .all(|&(left_col, right_col)| values_eq(&left_row[left_col], &right_row[right_col])),
                None => {
                    let mut combined = left_row.clone();
                    combined.extend_from_slice(right_row);
                    join_keep(operator, left_schema, &right.schema, &combined)?
                }
            };
            if keep {
                let mut combined = left_row.clone();
                combined.extend_from_slice(right_row);
                matched_left[left_index] = true;
                matched_right[right_index] = true;
                output.push(combined);
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

fn using_column_pairs(
    operator: &JoinOperator,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Result<Option<Vec<(usize, usize)>>, String> {
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
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
    combined: &[Value],
) -> Result<bool, String> {
    let constraint = match operator {
        JoinOperator::Join(constraint)
        | JoinOperator::Inner(constraint)
        | JoinOperator::Left(constraint)
        | JoinOperator::LeftOuter(constraint)
        | JoinOperator::Right(constraint)
        | JoinOperator::RightOuter(constraint)
        | JoinOperator::FullOuter(constraint)
        | JoinOperator::CrossJoin(constraint) => constraint,
        _ => return Err("Unsupported join type".to_string()),
    };

    match constraint {
        JoinConstraint::On(expr) => {
            let lookup = build_lookup_on_fly(left_schema, right_schema)?;
            let ctx = EvalContext::new(&lookup, &[], &[], Local::now());
            let value = eval_expr(&ctx, expr, combined)?;
            Ok(value.truthy())
        }
        JoinConstraint::Using(columns) => {
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
                if !values_eq(&combined[left_index], &combined[left_schema.len() + right_index]) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        JoinConstraint::None => Ok(true),
        JoinConstraint::Natural => Err("NATURAL JOIN is not supported".to_string()),
    }
}

fn build_lookup_on_fly(
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Result<HashMap<String, usize>, String> {
    let mut schema = left_schema.to_vec();
    schema.extend(right_schema.iter().cloned());
    build_lookup(&schema)
}

fn build_lookup(schema: &[ColumnRef]) -> Result<HashMap<String, usize>, String> {
    let mut map: HashMap<String, Vec<usize>> = HashMap::new();

    for (index, column) in schema.iter().enumerate() {
        map.entry(column.column.clone()).or_default().push(index);
        map.entry(format!("{}.{}", column.qualifier, column.column))
            .or_default()
            .push(index);
        map.entry(format!("{}.{}", column.table_name, column.column))
            .or_default()
            .push(index);
    }

    let mut lookup: HashMap<String, usize> = HashMap::new();
    for (name, indices) in &map {
        if indices.len() == 1 {
            lookup.insert(name.clone(), indices[0]);
        }
    }

    if lookup.is_empty() {
        return Ok(lookup);
    }

    Ok(lookup)
}
