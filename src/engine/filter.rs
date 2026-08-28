use std::borrow::Cow;
use std::ops::ControlFlow;

use chrono::Local;
use sqlparser::ast::{BinaryOperator, Expr, JoinOperator, TableWithJoins, Visit, Visitor};

use crate::engine::join::{ColumnRef, Relation};
use crate::engine::scope::build_lookup;
use crate::error::Error;
use crate::evaluator::{EvalContext, QueryRuntime, eval_expr};

pub(crate) fn pushdown_conjuncts(
    from: &[TableWithJoins],
    selection: Option<&Expr>,
) -> Option<Vec<Expr>> {
    let selection = selection?;
    if !from.iter().all(|item| {
        item.joins.iter().all(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Inner(_) | JoinOperator::CrossJoin(_)
            )
        })
    }) {
        return None;
    }
    let conjuncts = split_conjuncts(selection);
    if conjuncts.len() < 2 {
        return None;
    }
    let pushable = conjuncts
        .into_iter()
        .filter(|expr| is_pushable(expr))
        .cloned()
        .collect::<Vec<_>>();
    (!pushable.is_empty()).then_some(pushable)
}

fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    let mut result = Vec::new();
    fn visit<'a>(expr: &'a Expr, result: &mut Vec<&'a Expr>) {
        match expr {
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                visit(left, result);
                visit(right, result);
            }
            other => result.push(other),
        }
    }
    visit(expr, &mut result);
    result
}

fn is_pushable(expr: &Expr) -> bool {
    if crate::functions::contains_aggregate(expr) {
        return false;
    }
    struct Check {
        ok: bool,
        depth: usize,
    }
    impl Visitor for Check {
        type Break = ();
        fn pre_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.depth == 0
                && matches!(
                    expr,
                    Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
                )
            {
                self.ok = false;
                return ControlFlow::Break(());
            }
            if self.depth == 0 && matches!(expr, Expr::Function(f) if f.over.is_some()) {
                self.ok = false;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        }
    }
    let mut check = Check { ok: true, depth: 0 };
    let _ = expr.visit(&mut check);
    check.ok
}

fn referenced_names(expr: &Expr) -> Vec<String> {
    struct Collector {
        names: Vec<String>,
        depth: usize,
    }
    impl Visitor for Collector {
        type Break = ();
        fn pre_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
            if self.depth == 0 {
                match expr {
                    Expr::Identifier(id) => self.names.push(id.value.to_lowercase()),
                    Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                        self.names.push(format!(
                            "{}.{}",
                            parts[0].value.to_lowercase(),
                            parts[1].value.to_lowercase()
                        ))
                    }
                    _ => {}
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut collector = Collector {
        names: Vec::new(),
        depth: 0,
    };
    let _ = expr.visit(&mut collector);
    collector.names
}

pub(crate) fn filter_relation<'a>(
    relation: Relation<'a>,
    predicates: &[Expr],
    now: chrono::DateTime<Local>,
    runtime: &QueryRuntime,
) -> Result<Relation<'a>, Error> {
    let lookup = build_lookup(&relation.schema)?;
    let rows = relation.rows.into_owned();
    let mut kept = Vec::with_capacity(rows.len());
    for row in &rows {
        let ctx = EvalContext::new(&lookup, &rows, &[], now, runtime);
        if predicates.iter().all(|predicate| {
            eval_expr(&ctx, predicate, row)
                .map(|value| value.truthy())
                .unwrap_or(false)
        }) {
            kept.push(row.clone());
        }
    }
    Ok(Relation {
        schema: relation.schema,
        rows: Cow::Owned(kept),
    })
}

pub(crate) fn fits_relation(expr: &Expr, schema: &[ColumnRef]) -> bool {
    let names = referenced_names(expr);
    !names.is_empty()
        && names.iter().all(|name| {
            schema
                .iter()
                .filter(|column| {
                    column.column == *name
                        || format!("{}.{}", column.qualifier, column.column) == *name
                        || format!("{}.{}", column.table_name, column.column) == *name
                })
                .count()
                == 1
        })
}
