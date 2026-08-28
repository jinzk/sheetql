use std::collections::HashMap;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Visit, Visitor};

use crate::evaluator::{ExprId, ExprIds};

use super::select::{
    OrderSource, OrderTerm, PlannedGroupSource, PlannedOrderTerm, PlannedProjectionItem,
    ProjectionItem,
};
use super::window::WindowPlan;

pub(crate) struct QueryPlan {
    pub(crate) expressions: ExprPlan,
    pub(crate) execution: ExecutionPlan,
    pub(crate) projection: Vec<PlannedProjectionItem>,
    pub(crate) groups: Vec<PlannedGroupSource>,
    pub(crate) order: Vec<PlannedOrderTerm>,
    pub(crate) having: Option<ExprId>,
    pub(crate) windows: WindowPlan,
}

pub(crate) struct ExprPlan {
    pub(crate) expressions: Vec<Expr>,
    pub(crate) ids: ExprIds,
}

impl ExprPlan {
    pub(crate) fn id_of(&self, expr: &Expr) -> ExprId {
        self.ids
            .get(expr)
            .copied()
            .expect("expression must be registered before execution")
    }

    pub(crate) fn expression(&self, id: ExprId) -> &Expr {
        &self.expressions[id.0]
    }

    pub(crate) fn build(
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

    pub(crate) fn register_tree(&mut self, root: &Expr) {
        struct Register<'a> {
            plan: &'a mut ExprPlan,
        }
        impl Visitor for Register<'_> {
            type Break = ();
            fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
                if !self.plan.ids.contains_key(expr) {
                    let id = ExprId(self.plan.ids.len());
                    self.plan.ids.insert(expr.clone(), id);
                    self.plan.expressions.push(expr.clone());
                }
                ControlFlow::Continue(())
            }
        }
        let _ = root.visit(&mut Register { plan: self });
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecutionPlan {
    pub(crate) limit: Option<usize>,
    pub(crate) offset: usize,
    pub(crate) aggregate: bool,
    pub(crate) distinct: bool,
    pub(crate) early_stop: bool,
    pub(crate) top_n: bool,
}

impl ExecutionPlan {
    pub(crate) fn new(
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

    pub(crate) fn scan_target(self) -> usize {
        if self.early_stop {
            self.limit.unwrap_or(usize::MAX).saturating_add(self.offset)
        } else {
            usize::MAX
        }
    }
}
