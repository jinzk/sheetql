use std::cmp::Ordering;
use std::collections::HashMap;

use sqlparser::ast::{Expr, OrderByKind, Query};

use crate::engine::plan::ExprPlan;
use crate::engine::projection::ProjectionItem;
use crate::engine::rewrite::{AliasPrecedence, ExprRewriter, ordinal_literal};
use crate::engine::select::{OrderSource, OrderTerm, PlannedOrderSource, PlannedOrderTerm};
use crate::error::Error;
use crate::evaluator::{EvalContext, eval_expr};
use crate::value::{Value, values_partial_cmp};

pub(crate) fn order_terms(
    query: &Query,
    plan: &[ProjectionItem],
    aliases: &HashMap<String, &Expr>,
) -> Result<Vec<OrderTerm>, Error> {
    let Some(order) = &query.order_by else {
        return Ok(vec![]);
    };
    let OrderByKind::Expressions(expressions) = &order.kind else {
        return Err("ORDER BY ALL is not supported".into());
    };
    let rewriter = ExprRewriter::with_aliases(aliases, AliasPrecedence::AliasFirst, None);
    expressions
        .iter()
        .map(|order| {
            let source = if let Some(ordinal) = ordinal_literal(&order.expr) {
                if ordinal > plan.len() {
                    return Err(
                        format!("ORDER BY position {ordinal} is not in the select list").into(),
                    );
                }
                OrderSource::Ordinal(ordinal)
            } else {
                OrderSource::Expr(Box::new(rewriter.rewritten(&order.expr)))
            };
            Ok(OrderTerm {
                source,
                ascending: order.options.asc.unwrap_or(true),
            })
        })
        .collect()
}

pub(crate) fn order_keys(
    terms: &[PlannedOrderTerm],
    ctx: &EvalContext,
    out: &[Value],
    source_row: &[Value],
    expr_plan: &ExprPlan,
) -> Result<Vec<Value>, Error> {
    terms
        .iter()
        .map(|term| match &term.source {
            PlannedOrderSource::Ordinal(position) => Ok(out[position - 1].clone()),
            PlannedOrderSource::Expr(id) => eval_expr(ctx, expr_plan.expression(*id), source_row),
        })
        .collect()
}

pub(crate) fn sort_planned_keyed(
    keyed: &mut [(Vec<Value>, Vec<Value>)],
    terms: &[PlannedOrderTerm],
) {
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
