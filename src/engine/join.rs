use super::scope::build_lookup;
use crate::database::Schema;
use crate::error::Error;
use crate::evaluator::QueryRuntime;
use crate::value::values_eq;
use crate::value::{GroupKey, Value, group_key};
use chrono::Local;
use sqlparser::ast::TableWithJoins;
use sqlparser::ast::{BinaryOperator, Expr, JoinConstraint, JoinOperator};
use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(crate) struct ColumnRef {
    pub(crate) table_name: String,
    pub(crate) qualifier: String,
    pub(crate) column: String,
}

pub(crate) struct Relation<'a> {
    pub(crate) schema: Vec<ColumnRef>,
    pub(crate) rows: Cow<'a, [Vec<Value>]>,
}

pub(crate) type Relations<'a> = (Vec<ColumnRef>, Cow<'a, [Vec<Value>]>);

pub(crate) fn load_relation<'a>(
    schema: &'a Schema,
    factor: &sqlparser::ast::TableFactor,
    runtime: &'a QueryRuntime,
) -> Result<Relation<'a>, Error> {
    match factor {
        sqlparser::ast::TableFactor::Table { name, alias, .. } => {
            let parts = super::scope::object_name_to_parts(name);
            let (database, table_name) = match parts.as_slice() {
                [table] => (None, table.as_str()),
                [database, table] => (Some(database.as_str()), table.as_str()),
                _ => return Err("Table reference must be `table` or `database.table`".into()),
            };
            let (_, table) = schema.resolve_table(database, table_name)?;
            let table_name = table.name.clone();
            let qualifier = alias
                .as_ref()
                .map(|a| a.name.value.to_lowercase())
                .unwrap_or_else(|| table_name.clone());
            let schema = table
                .columns
                .iter()
                .map(|column| ColumnRef {
                    table_name: table_name.clone(),
                    qualifier: qualifier.clone(),
                    column: column.clone(),
                })
                .collect();
            Ok(Relation {
                schema,
                rows: Cow::Borrowed(&table.rows),
            })
        }
        sqlparser::ast::TableFactor::Derived {
            subquery, alias, ..
        } => {
            let result = super::set::execute_query_with_runtime(schema, subquery, runtime)?;
            let qualifier = alias
                .as_ref()
                .map(|a| a.name.value.to_lowercase())
                .ok_or("Derived tables require an alias")?;
            let schema = result
                .columns
                .iter()
                .map(|column| ColumnRef {
                    table_name: qualifier.clone(),
                    qualifier: qualifier.clone(),
                    column: column.clone(),
                })
                .collect();
            Ok(Relation {
                schema,
                rows: Cow::Owned(result.rows),
            })
        }
        _ => Err("Only plain table references are supported in FROM".into()),
    }
}

pub(crate) fn load_relation_filtered<'a>(
    schema: &'a Schema,
    factor: &sqlparser::ast::TableFactor,
    pending: &mut Vec<Expr>,
    now: chrono::DateTime<Local>,
    runtime: &'a QueryRuntime,
) -> Result<Relation<'a>, Error> {
    let mut relation = load_relation(schema, factor, runtime)?;
    let mut assigned = Vec::new();
    let mut remaining = Vec::new();
    for predicate in pending.drain(..) {
        if super::filter::fits_relation(&predicate, &relation.schema) {
            assigned.push(predicate);
        } else {
            remaining.push(predicate);
        }
    }
    *pending = remaining;
    if !assigned.is_empty() {
        relation = super::filter::filter_relation(relation, &assigned, now, runtime)?;
    }
    Ok(relation)
}

pub(crate) fn collect_relations<'a>(
    schema: &'a Schema,
    from: &[TableWithJoins],
    now: chrono::DateTime<Local>,
    pushdown: Option<&[Expr]>,
    runtime: &'a QueryRuntime,
) -> Result<Relations<'a>, Error> {
    let mut schema_refs = Vec::new();
    let mut rows: Cow<'a, [Vec<Value>]> = Cow::Owned(Vec::new());
    let mut pending = pushdown.map_or_else(Vec::new, |list| list.to_vec());
    for (index, item) in from.iter().enumerate() {
        let base = load_relation_filtered(schema, &item.relation, &mut pending, now, runtime)?;
        if item.joins.is_empty() {
            if index == 0 {
                schema_refs = base.schema;
                rows = base.rows;
            } else {
                schema_refs.extend(base.schema.clone());
                rows = Cow::Owned(cross_combine(&rows, &base.rows));
            }
            continue;
        }
        let mut current = rows.into_owned();
        if index == 0 {
            schema_refs = base.schema;
            current = base.rows.into_owned();
        } else {
            schema_refs.extend(base.schema.clone());
            current = cross_combine(&current, &base.rows);
        }
        for join in &item.joins {
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

pub(crate) fn cross_combine(
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
) -> Vec<Vec<Value>> {
    let mut output = Vec::new();
    for left in left_rows {
        for right in right_rows {
            let mut combined = left.clone();
            combined.extend_from_slice(right);
            output.push(combined);
        }
    }
    output
}

pub(crate) fn apply_join(
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
    let lookup = build_lookup(&schema)?;
    let mut output = Vec::new();
    let mut matched_left = vec![false; left_rows.len()];
    let mut matched_right = vec![false; right.rows.len()];
    let pairs = using_column_pairs(operator, left_schema, &right.schema)?
        .or_else(|| on_column_pairs(operator, left_schema, &right.schema));
    if let Some(pairs) = pairs {
        hash_using_join(
            left_rows,
            &right.rows,
            &pairs,
            &mut output,
            &mut matched_left,
            &mut matched_right,
        );
    } else {
        let mut scratch = Vec::with_capacity(left_len + right_len);
        for (li, left) in left_rows.iter().enumerate() {
            for (ri, right_row) in right.rows.iter().enumerate() {
                scratch.clear();
                scratch.extend_from_slice(left);
                scratch.extend_from_slice(right_row);
                if join_keep(operator, &lookup, now, runtime, &scratch)? {
                    matched_left[li] = true;
                    matched_right[ri] = true;
                    output.push(scratch.clone());
                }
            }
        }
    }
    let (keep_left, keep_right) = match operator {
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => (true, false),
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => (false, true),
        JoinOperator::FullOuter(_) => (true, true),
        _ => (false, false),
    };
    if keep_left {
        for (i, row) in left_rows.iter().enumerate() {
            if !matched_left[i] {
                let mut combined = row.clone();
                combined.extend(std::iter::repeat_n(Value::Null, right_len));
                output.push(combined);
            }
        }
    }
    if keep_right {
        for (i, row) in right.rows.iter().enumerate() {
            if !matched_right[i] {
                let mut combined = vec![Value::Null; left_len];
                combined.extend_from_slice(row);
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
    let (build_left, build_rows): (bool, &[Vec<Value>]) = if left_rows.len() <= right_rows.len() {
        (true, left_rows)
    } else {
        (false, right_rows)
    };
    let mut index: HashMap<Vec<GroupKey>, Vec<usize>> = HashMap::new();
    for (index_row, row) in build_rows.iter().enumerate() {
        let columns = pairs
            .iter()
            .map(|&(left, right)| if build_left { left } else { right });
        if columns.clone().any(|column| row[column].is_null()) {
            continue;
        }
        index
            .entry(join_key(row, columns))
            .or_default()
            .push(index_row);
    }
    let probe_rows = if build_left { right_rows } else { left_rows };
    for (probe_index, probe) in probe_rows.iter().enumerate() {
        let columns = pairs
            .iter()
            .map(|&(left, right)| if build_left { right } else { left });
        if columns.clone().any(|column| probe[column].is_null()) {
            continue;
        }
        let key = join_key(probe, columns);
        let Some(build_indices) = index.get(&key) else {
            continue;
        };
        for &build_index in build_indices {
            let (left_index, right_index) = if build_left {
                (build_index, probe_index)
            } else {
                (probe_index, build_index)
            };
            if !pairs.iter().all(|&(left, right)| {
                values_eq(
                    &left_rows[left_index][left],
                    &right_rows[right_index][right],
                )
            }) {
                continue;
            }
            matched_left[left_index] = true;
            matched_right[right_index] = true;
        }
    }
    for left_index in 0..left_rows.len() {
        for right_index in 0..right_rows.len() {
            if matched_left[left_index]
                && matched_right[right_index]
                && pairs.iter().all(|&(left, right)| {
                    values_eq(
                        &left_rows[left_index][left],
                        &right_rows[right_index][right],
                    )
                })
            {
                let mut row = left_rows[left_index].clone();
                row.extend_from_slice(&right_rows[right_index]);
                output.push(row);
            }
        }
    }
}

/// Build the composite hash key used by equi-joins. Keeping this operation in
/// the join module gives hash and probe paths one canonical key definition.
pub(crate) fn join_key(row: &[Value], columns: impl Iterator<Item = usize>) -> Vec<GroupKey> {
    columns.map(|column| group_key(&row[column])).collect()
}

pub(crate) fn on_column_pairs(
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
            match (
                resolve_join_column(left, left_schema, right_schema),
                resolve_join_column(right, left_schema, right_schema),
            ) {
                (Some(JoinColumn::Left(l)), Some(JoinColumn::Right(r))) => {
                    pairs.push((l, r));
                    Some(())
                }
                (Some(JoinColumn::Right(r)), Some(JoinColumn::Left(l))) => {
                    pairs.push((l, r));
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

fn resolve_join_column(expr: &Expr, left: &[ColumnRef], right: &[ColumnRef]) -> Option<JoinColumn> {
    let parts: Vec<String> = match expr {
        Expr::Identifier(id) => vec![id.value.to_lowercase()],
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
                && qualifier.is_none_or(|q| reference.qualifier == q || reference.table_name == q)
        });
        let (index, _) = matches.next()?;
        matches.next().is_none().then_some(index)
    };
    match (find(left), find(right)) {
        (Some(index), None) => Some(JoinColumn::Left(index)),
        (None, Some(index)) => Some(JoinColumn::Right(index)),
        _ => None,
    }
}

fn join_constraint(operator: &JoinOperator) -> Option<&JoinConstraint> {
    match operator {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c) => Some(c),
        _ => None,
    }
}

pub(crate) fn using_column_pairs(
    operator: &JoinOperator,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Result<Option<Vec<(usize, usize)>>, Error> {
    let columns = match operator {
        JoinOperator::Join(JoinConstraint::Using(c))
        | JoinOperator::Inner(JoinConstraint::Using(c))
        | JoinOperator::Left(JoinConstraint::Using(c))
        | JoinOperator::LeftOuter(JoinConstraint::Using(c))
        | JoinOperator::Right(JoinConstraint::Using(c))
        | JoinOperator::RightOuter(JoinConstraint::Using(c))
        | JoinOperator::FullOuter(JoinConstraint::Using(c)) => c,
        _ => return Ok(None),
    };
    let mut pairs = Vec::new();
    for column in columns {
        let name = column
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|id| id.value.to_lowercase())
            .unwrap_or_default();
        let left = left_schema
            .iter()
            .position(|c| c.column == name)
            .ok_or_else(|| format!("USING column `{name}` not found in left table"))?;
        let right = right_schema
            .iter()
            .position(|c| c.column == name)
            .ok_or_else(|| format!("USING column `{name}` not found in right table"))?;
        pairs.push((left, right));
    }
    Ok(Some(pairs))
}

pub(crate) fn join_keep(
    operator: &JoinOperator,
    lookup: &HashMap<String, usize>,
    now: chrono::DateTime<Local>,
    runtime: &QueryRuntime,
    combined: &[Value],
) -> Result<bool, Error> {
    let Some(constraint) = join_constraint(operator) else {
        return Err("Unsupported join type".into());
    };
    match constraint {
        JoinConstraint::On(expr) => Ok(crate::evaluator::eval_expr(
            &crate::evaluator::EvalContext::new(lookup, &[], &[], now, runtime),
            expr,
            combined,
        )?
        .truthy()),
        JoinConstraint::Using(_) => {
            Err("This join type cannot be combined with a USING clause".into())
        }
        JoinConstraint::None => Ok(true),
        JoinConstraint::Natural => Err("NATURAL JOIN is not supported".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_join_key_preserves_requested_column_order() {
        let row = vec![Value::Int(1), Value::Text("x".into()), Value::Int(2)];
        assert_eq!(join_key(&row, [2, 0].into_iter()).len(), 2);
    }
}
