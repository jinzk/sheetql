use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, OrderByExpr, Visit, Visitor, WindowType};

use crate::error::Error;
use crate::evaluator::eval_expr;
use crate::evaluator::{EvalContext, QueryRuntime};
use crate::functions::AGGREGATE_FUNCTIONS;
use crate::functions::parse_function_args;
use crate::value::Value;
use crate::value::group_key;
use crate::value::values_partial_cmp;

/// The set of distinct window function expressions (`ROW_NUMBER() OVER ...`,
/// `SUM(x) OVER ...`) referenced by a SELECT, each assigned a stable column
/// offset so the computation side and the evaluation side agree.
pub(crate) struct WindowPlan {
    pub(crate) columns: Vec<Expr>,
    pub(crate) offset_by_key: HashMap<Expr, usize>,
}

impl WindowPlan {
    pub(crate) fn new() -> Self {
        Self {
            columns: Vec::new(),
            offset_by_key: HashMap::new(),
        }
    }

    fn register(&mut self, expr: &Expr) {
        if self.offset_by_key.contains_key(expr) {
            return;
        }
        let offset = self.columns.len();
        self.columns.push(expr.clone());
        self.offset_by_key.insert(expr.clone(), offset);
    }
}

/// Result of computing every window expression: for each source row that is
/// part of the active set, the list of window values aligned with
/// `plan.columns`.
pub(crate) struct WindowValues<'a> {
    pub(crate) plan: &'a WindowPlan,
    pub(crate) values: HashMap<usize, Vec<Value>>,
}

/// Walk a projection expression tree, registering every window function call.
/// Does not descend into subqueries (e.g. scalar subqueries).
pub(crate) fn collect_window_exprs(plan: &mut WindowPlan, expr: &Expr) {
    struct Collector<'a> {
        plan: &'a mut WindowPlan,
        query_depth: usize,
    }
    impl Visitor for Collector<'_> {
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
            if let Expr::Function(func) = expr
                && func.over.is_some()
            {
                self.plan.register(expr);
            }
            ControlFlow::Continue(())
        }
    }
    let mut collector = Collector {
        plan,
        query_depth: 0,
    };
    let _ = expr.visit(&mut collector);
}

/// True if an expression tree contains a window function call, ignoring
/// subqueries.
pub(crate) fn contains_window(expr: &Expr) -> bool {
    struct Detector {
        found: bool,
        query_depth: usize,
    }
    impl Visitor for Detector {
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
            if let Expr::Function(func) = expr
                && func.over.is_some()
            {
                self.found = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        }
    }
    let mut detector = Detector {
        found: false,
        query_depth: 0,
    };
    let _ = expr.visit(&mut detector);
    detector.found
}

/// Compute the window value of every registered window expression for every
/// active source row. `values[row_index]` holds `plan.columns.len()` values.
pub(crate) fn compute_window_values<'a>(
    plan: &'a WindowPlan,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    active: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<WindowValues<'a>, Error> {
    let mut calls: Vec<WindowCall> = Vec::with_capacity(plan.columns.len());
    for expr in &plan.columns {
        calls.push(parse_window_call(expr)?);
    }

    let mut values: HashMap<usize, Vec<Value>> = HashMap::with_capacity(active.len());
    for &row_index in active {
        values.insert(row_index, vec![Value::Null; plan.columns.len()]);
    }

    for (call_index, call) in calls.iter().enumerate() {
        let partitions = partition_rows(call, rows, lookup, active, now, runtime)?;
        for partition in &partitions {
            let ordered = order_partition(call, rows, lookup, partition, now, runtime)?;
            let results = evaluate_call(call, rows, lookup, &ordered, now, runtime)?;
            for (&row_index, result) in ordered.iter().zip(results) {
                if let Some(slot) = values.get_mut(&row_index) {
                    slot[call_index] = result;
                }
            }
        }
    }

    Ok(WindowValues { plan, values })
}

struct WindowCall {
    function: String,
    args: Vec<Expr>,
    distinct: bool,
    star: bool,
    partition_by: Vec<Expr>,
    order_by: Vec<OrderByExpr>,
}

impl WindowCall {
    fn is_ranked(&self) -> bool {
        matches!(self.function.as_str(), "row_number" | "rank" | "dense_rank")
    }
}

fn parse_window_call(expr: &Expr) -> Result<WindowCall, Error> {
    let Expr::Function(func) = &expr else {
        return Err("Window expression is not a function call".into());
    };
    let over = func.over.as_ref().ok_or("Window function missing OVER")?;
    let WindowType::WindowSpec(spec) = over else {
        return Err("Referencing a named window is not supported".into());
    };
    if spec.window_frame.is_some() {
        return Err("Window frames are not supported yet".into());
    }
    let name = func.name.to_string().to_lowercase();
    let args = parse_function_args(&func.args)?;
    let (star, distinct, args_exprs) = match args {
        crate::functions::FnArgs::Star => (true, false, Vec::new()),
        crate::functions::FnArgs::All(list) => {
            let mut exprs = Vec::with_capacity(list.len());
            for arg in list {
                exprs.push(crate::functions::function_arg_expr(arg)?.clone());
            }
            (false, false, exprs)
        }
        crate::functions::FnArgs::Distinct(list) => {
            let mut exprs = Vec::with_capacity(list.len());
            for arg in list {
                exprs.push(crate::functions::function_arg_expr(arg)?.clone());
            }
            (false, true, exprs)
        }
    };
    if args_exprs.len() > 1 {
        return Err("Window functions accept at most one argument".into());
    }
    Ok(WindowCall {
        function: name,
        args: args_exprs,
        distinct,
        star,
        partition_by: spec.partition_by.clone(),
        order_by: spec.order_by.clone(),
    })
}

fn partition_rows(
    call: &WindowCall,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    active: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<Vec<Vec<usize>>, Error> {
    if call.partition_by.is_empty() {
        return Ok(vec![active.to_vec()]);
    }
    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut index: HashMap<Vec<crate::value::GroupKey>, usize> = HashMap::new();
    for &row_index in active {
        let row = &rows[row_index];
        let mut key: Vec<crate::value::GroupKey> = Vec::with_capacity(call.partition_by.len());
        for expr in &call.partition_by {
            key.push(group_key(&eval_expr(&ctx, expr, row)?));
        }
        let group_index = match index.get(&key) {
            Some(existing) => *existing,
            None => {
                groups.push(Vec::new());
                let new_index = groups.len() - 1;
                index.insert(key, new_index);
                new_index
            }
        };
        groups[group_index].push(row_index);
    }
    Ok(groups)
}

/// Return the partition's rows ordered by the window ORDER BY (stable for
/// equal keys, preserving active order). Without an ORDER BY the rows keep
/// their active order.
fn order_partition(
    call: &WindowCall,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    partition: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<Vec<usize>, Error> {
    if call.order_by.is_empty() {
        return Ok(partition.to_vec());
    }
    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let mut decorated: Vec<(usize, Vec<Value>, usize)> = Vec::with_capacity(partition.len());
    for (position, &row_index) in partition.iter().enumerate() {
        let row = &rows[row_index];
        let mut keys = Vec::with_capacity(call.order_by.len());
        for order_expr in &call.order_by {
            keys.push(eval_expr(&ctx, &order_expr.expr, row)?);
        }
        decorated.push((row_index, keys, position));
    }
    decorated.sort_by(|a, b| compare_keys(&a.1, &b.1, &call.order_by).then_with(|| a.2.cmp(&b.2)));
    Ok(decorated.into_iter().map(|(index, _, _)| index).collect())
}

fn compare_keys(left: &[Value], right: &[Value], terms: &[OrderByExpr]) -> Ordering {
    left.iter()
        .zip(right)
        .zip(terms)
        .map(|((a, b), term)| {
            let mut ordering = values_partial_cmp(a, b).unwrap_or(Ordering::Equal);
            if !term.options.asc.unwrap_or(true) {
                ordering = ordering.reverse();
            }
            ordering
        })
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

/// Compute the window value for every row of an ordered partition, in order.
fn evaluate_call(
    call: &WindowCall,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    ordered: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<Vec<Value>, Error> {
    if call.is_ranked() {
        return rank_values(call, rows, lookup, ordered, now, runtime);
    }
    if AGGREGATE_FUNCTIONS.contains(&call.function.as_str()) {
        let value = evaluate_aggregate(call, rows, lookup, ordered, now, runtime)?;
        return Ok(vec![value; ordered.len()]);
    }
    Err(format!("Unknown window function `{}`", call.function).into())
}

fn rank_values(
    call: &WindowCall,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    ordered: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<Vec<Value>, Error> {
    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let mut results = Vec::with_capacity(ordered.len());
    match call.function.as_str() {
        "row_number" => {
            for (index, _) in ordered.iter().enumerate() {
                results.push(Value::Int(index as i64 + 1));
            }
        }
        "rank" => {
            let mut rank = 1i64;
            let mut previous: Option<Vec<Value>> = None;
            for &row_index in ordered {
                let key = order_keys_for(call, &ctx, &rows[row_index])?;
                if let Some(previous_key) = &previous
                    && compare_keys(previous_key, &key, &call.order_by) != Ordering::Equal
                {
                    rank = results.len() as i64 + 1;
                }
                results.push(Value::Int(rank));
                previous = Some(key);
            }
        }
        "dense_rank" => {
            let mut dense = 1i64;
            let mut previous: Option<Vec<Value>> = None;
            for &row_index in ordered {
                let key = order_keys_for(call, &ctx, &rows[row_index])?;
                if let Some(previous_key) = &previous
                    && compare_keys(previous_key, &key, &call.order_by) != Ordering::Equal
                {
                    dense += 1;
                }
                results.push(Value::Int(dense));
                previous = Some(key);
            }
        }
        _ => unreachable!("validated by is_ranked"),
    }
    Ok(results)
}

fn order_keys_for(
    call: &WindowCall,
    ctx: &EvalContext,
    row: &[Value],
) -> Result<Vec<Value>, Error> {
    let mut keys = Vec::with_capacity(call.order_by.len());
    for order_expr in &call.order_by {
        keys.push(eval_expr(ctx, &order_expr.expr, row)?);
    }
    Ok(keys)
}

fn evaluate_aggregate(
    call: &WindowCall,
    rows: &[Vec<Value>],
    lookup: &HashMap<String, usize>,
    ordered: &[usize],
    now: chrono::DateTime<chrono::Local>,
    runtime: &QueryRuntime,
) -> Result<Value, Error> {
    let ctx = EvalContext::new(lookup, rows, &[], now, runtime);
    let values: Vec<Value> = if call.star {
        ordered
            .iter()
            .filter_map(|&row_index| rows.get(row_index))
            .map(|_| Value::Int(1))
            .collect()
    } else {
        let expr = call
            .args
            .first()
            .ok_or("Aggregate window function expects an argument")?;
        ordered
            .iter()
            .map(|&row_index| eval_expr(&ctx, expr, &rows[row_index]))
            .collect::<Result<_, _>>()?
    };
    aggregate_values(&call.function, &values, call.distinct)
}

fn aggregate_values(name: &str, values: &[Value], distinct: bool) -> Result<Value, Error> {
    let mut count = 0i64;
    let mut numeric_count = 0usize;
    let mut sum_int: Option<i64> = None;
    let mut sum_int_overflow = false;
    let mut sum_float = 0.0;
    let mut is_float = false;
    let mut distinct_set: std::collections::HashSet<crate::value::GroupKey> =
        std::collections::HashSet::new();
    let mut non_numeric: Option<Value> = None;
    for value in values.iter().filter(|value| !value.is_null()) {
        count += 1;
        distinct_set.insert(group_key(value));
        let is_nan = matches!(value, Value::Float(n) if n.is_nan());
        if matches!(value, Value::Int(_) | Value::Float(_)) && !is_nan {
            numeric_count += 1;
            is_float = is_float || matches!(value, Value::Float(_));
            sum_float += value.as_f64().unwrap_or(0.0);
            if let Value::Int(n) = value {
                sum_int = Some(match sum_int.unwrap_or(0).checked_add(*n) {
                    Some(sum) => sum,
                    None => {
                        sum_int_overflow = true;
                        0
                    }
                });
            }
        } else if !is_nan && non_numeric.is_none() {
            non_numeric = Some(value.clone());
        }
    }
    let distinct_count = distinct_set.len() as i64;
    let effective_numeric = if distinct {
        distinct_count as usize
    } else {
        numeric_count
    };
    match name {
        "count" => Ok(Value::Int(if distinct { distinct_count } else { count })),
        "sum" => {
            if let Some(value) = non_numeric {
                return Err(format!("Aggregate expects numeric values, got `{value}`").into());
            }
            if effective_numeric == 0 {
                return Ok(Value::Null);
            }
            if is_float {
                Ok(Value::Float(sum_float))
            } else if sum_int_overflow {
                Err("Integer overflow in SUM".into())
            } else {
                Ok(Value::Int(sum_int.unwrap_or(0)))
            }
        }
        "avg" => {
            if let Some(value) = non_numeric {
                return Err(format!("Aggregate expects numeric values, got `{value}`").into());
            }
            if effective_numeric == 0 {
                return Ok(Value::Null);
            }
            Ok(Value::Float(sum_float / effective_numeric as f64))
        }
        "min" | "max" => {
            let mut best: Option<Value> = None;
            for value in values.iter().filter(|value| !value.is_null()) {
                best = match best {
                    None => Some(value.clone()),
                    Some(current) => {
                        let ordering = values_partial_cmp(value, &current);
                        match ordering {
                            Some(Ordering::Less) if name == "min" => Some(value.clone()),
                            Some(Ordering::Greater) if name == "max" => Some(value.clone()),
                            _ => Some(current),
                        }
                    }
                };
            }
            Ok(best.unwrap_or(Value::Null))
        }
        _ => Err(format!("Unknown window aggregate `{name}`").into()),
    }
}

#[cfg(test)]
mod tests {
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
    fn window_plan_uses_ast_expression_identity() {
        let first = expr("ROW_NUMBER() OVER (ORDER BY age)");
        let second = expr("ROW_NUMBER() OVER (ORDER BY age)");
        let mut plan = WindowPlan::new();
        plan.register(&first);
        plan.register(&second);
        assert_eq!(plan.columns.len(), 1);
        assert_eq!(plan.offset_by_key[&first], 0);
    }
}
