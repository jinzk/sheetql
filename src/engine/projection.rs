use sqlparser::ast::{Expr, SelectItem};

use crate::engine::join::ColumnRef;
use crate::engine::plan::ExprPlan;
use crate::engine::rewrite::ExprRewriter;
use crate::error::Error;
use crate::evaluator::{EvalContext, ExprId, eval_expr};
use crate::value::Value;

#[derive(Debug)]
pub(crate) enum ProjectionItem {
    Column { index: usize, title: String },
    Expression { expr: Box<Expr>, title: String },
}

pub(crate) enum PlannedProjectionItem {
    Column { index: usize, title: String },
    Expression { id: ExprId, title: String },
}

pub(crate) fn build_projection_plan(
    schema: &[ColumnRef],
    projection: &[SelectItem],
) -> Result<Vec<ProjectionItem>, Error> {
    let mut plan = Vec::new();
    for item in projection {
        match item {
            SelectItem::Wildcard(_) => {
                plan.extend(schema.iter().enumerate().map(|(index, column)| {
                    ProjectionItem::Column {
                        index,
                        title: column.column.clone(),
                    }
                }))
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                let qualifier = match kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        crate::engine::scope::object_name_to_parts(name).join(".")
                    }
                    _ => return Err("Unsupported qualified wildcard".into()),
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
            SelectItem::UnnamedExpr(expr) => plan.push(ProjectionItem::Expression {
                expr: Box::new(ExprRewriter::lowercase().rewritten(expr)),
                title: expr_title(expr),
            }),
            SelectItem::ExprWithAlias { expr, alias } => plan.push(ProjectionItem::Expression {
                expr: Box::new(ExprRewriter::lowercase().rewritten(expr)),
                title: alias.to_string(),
            }),
            SelectItem::ExprWithAliases { .. } => {
                return Err("Multiple aliases are not supported".into());
            }
        }
    }
    Ok(plan)
}

pub(crate) fn project(
    ctx: &EvalContext,
    plan: &[PlannedProjectionItem],
    row: &[Value],
    expr_plan: &ExprPlan,
) -> Result<Vec<Value>, Error> {
    plan.iter()
        .map(|item| match item {
            PlannedProjectionItem::Column { index, .. } => {
                Ok(row.get(*index).cloned().unwrap_or(Value::Null))
            }
            PlannedProjectionItem::Expression { id, .. } => {
                eval_expr(ctx, expr_plan.expression(*id), row)
            }
        })
        .collect()
}

fn expr_title(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(parts) => parts
            .iter()
            .map(|id| id.value.clone())
            .collect::<Vec<_>>()
            .join("."),
        other => other.to_string(),
    }
}
