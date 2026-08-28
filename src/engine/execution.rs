use crate::database::Schema;
use crate::engine::ExecutionOutput;
use crate::error::Error;
use crate::evaluator::{OuterScope, QueryRuntime};

/// Entry point for executing a plain SELECT. The SELECT module owns AST
/// normalization and row operators; this module owns the execution boundary.
pub(crate) fn execute_select_output<'a>(
    schema: &'a Schema,
    query: &sqlparser::ast::Query,
    outer_scope: Option<&'a OuterScope<'a>>,
    runtime: &'a QueryRuntime,
) -> Result<ExecutionOutput, Error> {
    super::select::execute_select_output_impl(schema, query, outer_scope, runtime)
}
