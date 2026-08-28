use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Query, Visit, Visitor};

use crate::database::Schema;
use crate::error::Error;
use crate::evaluator::{OuterScope, SubqueryExecutor, SubqueryResult};

pub(crate) fn collect_subqueries(query: &Query, output: &mut Vec<Query>) {
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

pub(crate) struct SchemaExecutor<'a> {
    pub(crate) schema: &'a Schema,
}

impl SubqueryExecutor for SchemaExecutor<'_> {
    fn execute(&self, query: &Query, scope: &OuterScope<'_>) -> Result<SubqueryResult, Error> {
        let result = super::select::execute_select_query_with_runtime(
            self.schema,
            query,
            Some(scope),
            scope.runtime,
        )?;
        Ok(SubqueryResult {
            columns: result.columns,
            rows: result.rows,
        })
    }
}
