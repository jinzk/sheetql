use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Local};
use sqlparser::ast::{
    BinaryOperator, CaseWhen, CeilFloorKind, DataType as SqlDataType, DateTimeField, Expr,
    TrimWhereField, UnaryOperator, Value as SqlValue, ValueWithSpan,
};

use crate::error::Error;
use crate::functions::{eval_function, floor_ceil};
use crate::value::GroupKey;
use crate::value::Value;
use crate::value::group_key;

#[derive(Debug, Clone, Default)]
pub struct AggregateSummary {
    pub count: i64,
    pub distinct_count: i64,
    pub numeric_count: usize,
    pub is_float: bool,
    pub sum_int: Option<i64>,
    pub sum_int_overflow: bool,
    pub sum_float: f64,
    pub distinct_numeric_count: usize,
    pub distinct_is_float: bool,
    pub distinct_sum_int: Option<i64>,
    pub distinct_sum_int_overflow: bool,
    pub distinct_sum_float: f64,
    pub distinct: std::collections::HashSet<GroupKey>,
    pub non_numeric: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ExprId(pub(crate) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TriBool {
    True,
    False,
    Unknown,
}

impl TriBool {
    fn from_value(value: &Value) -> Self {
        if value.is_null() {
            Self::Unknown
        } else if value.truthy() {
            Self::True
        } else {
            Self::False
        }
    }

    fn into_value(self) -> Value {
        match self {
            Self::True => Value::Bool(true),
            Self::False => Value::Bool(false),
            Self::Unknown => Value::Null,
        }
    }

    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}
use crate::value::values_eq;
use crate::value::values_partial_cmp;

pub struct EvalContext<'a> {
    pub columns: &'a HashMap<String, usize>,
    pub all_rows: &'a [Vec<Value>],
    pub group_rows: &'a [usize],
    pub now: DateTime<Local>,
    pub(crate) expr_ids: &'a HashMap<Expr, ExprId>,
    pub(crate) expressions: &'a [Expr],

    /// Values of aggregate arguments already evaluated for this group.
    /// Aggregate functions share a context, so SUM(x), AVG(x), and MAX(x)
    /// do not repeatedly evaluate the same expression for every row.
    pub(crate) group_state: Option<&'a RefCell<GroupState>>,
    pub(crate) subqueries: &'a HashMap<sqlparser::ast::Query, SubqueryResult>,
    pub(crate) runtime: &'a QueryRuntime,
    pub(crate) subquery_executor: Option<&'a dyn SubqueryExecutor>,
    pub(crate) outer_scope: Option<&'a OuterScope<'a>>,
    /// Maps a window function expression's canonical string to the offset of
    /// its computed value in the augmented `current` row. Set for queries that
    /// use window functions.
    pub(crate) window_resolver: Option<&'a HashMap<Expr, usize>>,
}

static EMPTY_COLUMNS: std::sync::OnceLock<HashMap<String, usize>> = std::sync::OnceLock::new();
static EMPTY_ROWS: [Vec<Value>; 0] = [];
static EMPTY_GROUP: [usize; 0] = [];
static EMPTY_EXPR_IDS: std::sync::OnceLock<HashMap<Expr, ExprId>> = std::sync::OnceLock::new();
static EMPTY_SUBQUERIES: std::sync::OnceLock<HashMap<sqlparser::ast::Query, SubqueryResult>> =
    std::sync::OnceLock::new();
static SCALAR_RUNTIME: std::sync::OnceLock<QueryRuntime> = std::sync::OnceLock::new();
pub(crate) type ExprIds = HashMap<Expr, ExprId>;

#[derive(Debug, Default)]
pub(crate) struct GroupState {
    pub(crate) argument_values: HashMap<ExprId, Arc<[Value]>>,
    pub(crate) summaries: HashMap<ExprId, AggregateSummary>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubqueryResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CorrelatedCacheKey {
    pub(crate) subquery: SubqueryId,
    pub(crate) outer_values: Vec<GroupKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CorrelationExpr {
    pub(crate) scope_level: usize,
    pub(crate) outer_column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubqueryId(pub(crate) usize);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RuntimeStats {
    pub(crate) correlated_cache_hits: usize,
    pub(crate) correlated_cache_misses: usize,
}

#[derive(Debug, Default)]
pub(crate) struct QueryRuntime {
    pub(crate) correlated_cache: std::sync::Mutex<HashMap<CorrelatedCacheKey, SubqueryResult>>,
    pub(crate) correlated_cache_hits: std::sync::Mutex<usize>,
    pub(crate) correlated_cache_misses: std::sync::Mutex<usize>,
    pub(crate) subquery_ids: std::sync::Mutex<HashMap<sqlparser::ast::Query, SubqueryId>>,
    pub(crate) correlations: std::sync::Mutex<HashMap<SubqueryId, Arc<[CorrelationExpr]>>>,
}

impl QueryRuntime {
    pub(crate) fn stats(&self) -> RuntimeStats {
        RuntimeStats {
            correlated_cache_hits: *self.correlated_cache_hits.lock().unwrap(),
            correlated_cache_misses: *self.correlated_cache_misses.lock().unwrap(),
        }
    }

    pub(crate) fn subquery_id(&self, query: &sqlparser::ast::Query) -> SubqueryId {
        let mut ids = self.subquery_ids.lock().unwrap();
        if let Some(id) = ids.get(query).copied() {
            return id;
        }
        let id = SubqueryId(ids.len());
        ids.insert(query.clone(), id);
        id
    }

    pub(crate) fn register_correlations(
        &self,
        subquery: SubqueryId,
        correlations: Vec<CorrelationExpr>,
    ) {
        self.correlations
            .lock()
            .unwrap()
            .insert(subquery, correlations.into());
    }
}

pub(crate) const MAX_CORRELATED_SUBQUERY_CACHE_ENTRIES: usize = 100_000;

pub(crate) struct OuterScope<'a> {
    pub(crate) row: &'a [Value],
    pub(crate) columns: &'a HashMap<String, usize>,
    pub(crate) parent: Option<&'a OuterScope<'a>>,
    pub(crate) runtime: &'a QueryRuntime,
}

pub(crate) trait SubqueryExecutor {
    fn execute(
        &self,
        query: &sqlparser::ast::Query,
        scope: &OuterScope<'_>,
    ) -> Result<SubqueryResult, Error>;
}

impl<'a> EvalContext<'a> {
    pub(crate) fn expr_id(&self, expr: &Expr) -> ExprId {
        self.expr_ids
            .get(expr)
            .copied()
            .expect("all aggregate expressions must be registered in the query plan")
    }

    pub(crate) fn planned_expr(&self, id: ExprId) -> &Expr {
        &self.expressions[id.0]
    }

    pub fn new(
        columns: &'a HashMap<String, usize>,
        all_rows: &'a [Vec<Value>],
        group_rows: &'a [usize],
        now: DateTime<Local>,
        runtime: &'a QueryRuntime,
    ) -> Self {
        Self {
            columns,
            all_rows,
            group_rows,
            now,
            expr_ids: EMPTY_EXPR_IDS.get_or_init(HashMap::new),
            expressions: &[],
            group_state: None,
            subqueries: EMPTY_SUBQUERIES.get_or_init(HashMap::new),
            runtime,
            subquery_executor: None,
            outer_scope: None,
            window_resolver: None,
        }
    }

    pub fn with_expr_ids(
        columns: &'a HashMap<String, usize>,
        all_rows: &'a [Vec<Value>],
        group_rows: &'a [usize],
        now: DateTime<Local>,
        expr_ids: &'a ExprIds,
        expressions: &'a [Expr],
        runtime: &'a QueryRuntime,
    ) -> Self {
        let mut context = Self::new(columns, all_rows, group_rows, now, runtime);
        context.expr_ids = expr_ids;
        context.expressions = expressions;
        context
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_group_state(
        columns: &'a HashMap<String, usize>,
        all_rows: &'a [Vec<Value>],
        group_rows: &'a [usize],
        now: DateTime<Local>,
        expr_ids: &'a ExprIds,
        expressions: &'a [Expr],
        group_state: &'a RefCell<GroupState>,
        runtime: &'a QueryRuntime,
    ) -> Self {
        let mut context = Self::with_expr_ids(
            columns,
            all_rows,
            group_rows,
            now,
            expr_ids,
            expressions,
            runtime,
        );
        context.group_state = Some(group_state);
        context
    }

    pub(crate) fn with_subqueries(
        mut self,
        subqueries: &'a HashMap<sqlparser::ast::Query, SubqueryResult>,
    ) -> Self {
        self.subqueries = subqueries;
        self
    }

    pub(crate) fn with_runtime(mut self, runtime: &'a QueryRuntime) -> Self {
        self.runtime = runtime;
        self
    }

    pub(crate) fn with_subquery_executor(
        mut self,
        executor: &'a dyn SubqueryExecutor,
        scope: Option<&'a OuterScope<'a>>,
    ) -> Self {
        self.subquery_executor = Some(executor);
        self.outer_scope = scope;
        self
    }

    pub(crate) fn with_outer_scope(mut self, scope: &'a OuterScope<'a>) -> Self {
        self.outer_scope = Some(scope);
        self
    }

    pub(crate) fn with_window_resolver(mut self, resolver: &'a HashMap<Expr, usize>) -> Self {
        self.window_resolver = Some(resolver);
        self
    }

    /// The value of the window column at the given canonical key, read from the
    /// augmented current row.
    pub(crate) fn window_value(&self, key: &Expr, current: &[Value]) -> Option<Value> {
        let offset = self.window_resolver?.get(key)?;
        current.get(*offset).cloned()
    }

    pub fn scalar() -> Self {
        Self {
            columns: EMPTY_COLUMNS.get_or_init(HashMap::new),
            all_rows: &EMPTY_ROWS,
            group_rows: &EMPTY_GROUP,
            now: Local::now(),
            expr_ids: EMPTY_EXPR_IDS.get_or_init(HashMap::new),
            expressions: &[],
            group_state: None,
            subqueries: EMPTY_SUBQUERIES.get_or_init(HashMap::new),
            runtime: SCALAR_RUNTIME.get_or_init(QueryRuntime::default),
            subquery_executor: None,
            outer_scope: None,
            window_resolver: None,
        }
    }
}

pub fn eval_expr(ctx: &EvalContext, expr: &Expr, current: &[Value]) -> Result<Value, Error> {
    match expr {
        Expr::Value(ValueWithSpan { value, .. }) => eval_sql_value(value),
        Expr::Identifier(ident) => resolve_column(ctx, &ident.value, current),
        Expr::CompoundIdentifier(parts) => {
            let mut name_parts: Vec<String> =
                parts.iter().map(|p| p.value.to_lowercase()).collect();
            if name_parts.len() < 2 {
                return Err("Invalid compound identifier".to_string().into());
            }
            let column = name_parts.pop().unwrap();
            let qualifier = name_parts.join(".");
            resolve_column(ctx, &format!("{}.{}", qualifier, column), current)
        }
        Expr::Nested(inner) => eval_expr(ctx, inner, current),
        Expr::BinaryOp { left, op, right } => {
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                return eval_logic(ctx, left, op, right, current);
            }
            let lhs = eval_expr(ctx, left, current)?;
            let rhs = eval_expr(ctx, right, current)?;
            eval_binary(op, lhs, rhs)
        }
        Expr::UnaryOp { op, expr } => {
            let value = eval_expr(ctx, expr, current)?;
            eval_unary(op, value)
        }
        Expr::IsNull(inner) => Ok(Value::Bool(eval_expr(ctx, inner, current)?.is_null())),
        Expr::IsNotNull(inner) => Ok(Value::Bool(!eval_expr(ctx, inner, current)?.is_null())),
        Expr::IsTrue(inner) => Ok(Value::Bool(eval_expr(ctx, inner, current)?.truthy())),
        Expr::IsNotTrue(inner) => Ok(Value::Bool(!eval_expr(ctx, inner, current)?.truthy())),
        // SQL three-valued logic: NULL is neither TRUE nor FALSE.
        Expr::IsFalse(inner) => {
            let value = eval_expr(ctx, inner, current)?;
            Ok(Value::Bool(!value.is_null() && !value.truthy()))
        }
        Expr::IsNotFalse(inner) => {
            let value = eval_expr(ctx, inner, current)?;
            Ok(Value::Bool(value.is_null() || value.truthy()))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => eval_in_list(ctx, expr, list, *negated, current),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => eval_between(ctx, expr, low, high, *negated, current),
        Expr::Like {
            negated,
            expr,
            pattern,
            escape_char,
            ..
        } => eval_like(ctx, *negated, expr, pattern, escape_char, false, current),
        Expr::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            ..
        } => eval_like(ctx, *negated, expr, pattern, escape_char, true, current),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => eval_case(ctx, operand, conditions, else_result, current),
        Expr::Cast {
            expr, data_type, ..
        } => {
            let value = eval_expr(ctx, expr, current)?;
            cast_value(value, data_type)
        }
        Expr::Function(func) => eval_function(ctx, func, current),
        Expr::Floor { expr, field } => eval_floor_ceil(ctx, "floor", expr, field, current),
        Expr::Ceil { expr, field } => eval_floor_ceil(ctx, "ceil", expr, field, current),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let text = eval_expr(ctx, expr, current)?.to_display_string();
            let from = match substring_from {
                Some(from) => eval_expr(ctx, from, current)?
                    .as_i64()
                    .ok_or("SUBSTRING start must be a number")?,
                None => 1,
            };
            let length = match substring_for {
                Some(length) => Some(
                    eval_expr(ctx, length, current)?
                        .as_i64()
                        .ok_or("SUBSTRING length must be a number")?,
                ),
                None => None,
            };
            Ok(Value::Text(crate::functions::substring(
                &text, from, length,
            )))
        }
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters,
        } => {
            let value = eval_expr(ctx, expr, current)?.to_display_string();
            let side = trim_where.unwrap_or(TrimWhereField::Both);
            let what = match (trim_what, trim_characters) {
                (Some(what), _) => Some(eval_expr(ctx, what, current)?.to_display_string()),
                (None, Some(characters)) => {
                    let mut joined = String::new();
                    for character in characters {
                        joined.push_str(&eval_expr(ctx, character, current)?.to_display_string());
                    }
                    Some(joined)
                }
                (None, None) => None,
            };
            Ok(Value::Text(trim_string(&value, side, what.as_deref())))
        }
        Expr::Subquery(query) => scalar_subquery(ctx, query),
        Expr::Exists { subquery, negated } => {
            let result = subquery_result(ctx, subquery)?;
            let exists = !result.rows.is_empty();
            Ok(Value::Bool(if *negated { !exists } else { exists }))
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let value = eval_expr(ctx, expr, current)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let result = subquery_result(ctx, subquery)?;
            let mut saw_null = false;
            for row in &result.rows {
                let candidate = row.first().ok_or("IN subquery must return one column")?;
                if candidate.is_null() {
                    saw_null = true;
                } else if values_eq(&value, candidate) {
                    return Ok(Value::Bool(!*negated));
                }
            }
            if saw_null {
                Ok(Value::Null)
            } else {
                Ok(Value::Bool(*negated))
            }
        }
        other => Err(format!("Unsupported expression: {}", expr_display(other)).into()),
    }
}

fn scalar_subquery(ctx: &EvalContext, query: &sqlparser::ast::Query) -> Result<Value, Error> {
    let result = subquery_result(ctx, query)?;
    if result.columns.len() != 1 {
        return Err("Scalar subquery must return exactly one column".into());
    }
    match result.rows.as_slice() {
        [] => Ok(Value::Null),
        [row] => Ok(row.first().cloned().unwrap_or(Value::Null)),
        _ => Err("Scalar subquery must return at most one row".into()),
    }
}

fn subquery_result<'a>(
    ctx: &'a EvalContext<'a>,
    query: &sqlparser::ast::Query,
) -> Result<std::borrow::Cow<'a, SubqueryResult>, Error> {
    if let Some(result) = ctx.subqueries.get(query) {
        return Ok(std::borrow::Cow::Borrowed(result));
    }
    let executor = ctx.subquery_executor.ok_or("Subquery was not prepared")?;
    let scope = ctx
        .outer_scope
        .ok_or("Correlated subquery requires an outer row")?;
    let subquery = ctx.runtime.subquery_id(query);
    let correlations = ctx
        .runtime
        .correlations
        .lock()
        .unwrap()
        .get(&subquery)
        .cloned()
        .unwrap_or_default();
    // An empty dependency list means the correlation analyzer could not prove
    // which outer values affect the subquery. Never cache such a result: using
    // one shared entry would be incorrect for different outer rows.
    if correlations.is_empty() {
        return Ok(std::borrow::Cow::Owned(executor.execute(query, scope)?));
    }
    let outer_values = correlations
        .iter()
        .filter_map(|correlation| {
            scope_at(scope, correlation.scope_level)
                .and_then(|scope| scope.row.get(correlation.outer_column))
        })
        .map(group_key)
        .collect();
    let key = CorrelatedCacheKey {
        subquery,
        outer_values,
    };
    if let Some(result) = ctx.runtime.correlated_cache.lock().unwrap().get(&key) {
        *ctx.runtime.correlated_cache_hits.lock().unwrap() += 1;
        return Ok(std::borrow::Cow::Owned(result.clone()));
    }
    *ctx.runtime.correlated_cache_misses.lock().unwrap() += 1;
    let result = executor.execute(query, scope)?;
    let mut cache = ctx.runtime.correlated_cache.lock().unwrap();
    if cache.len() < MAX_CORRELATED_SUBQUERY_CACHE_ENTRIES {
        cache.insert(key, result.clone());
    }
    Ok(std::borrow::Cow::Owned(result))
}

fn scope_at<'a>(scope: &'a OuterScope<'a>, level: usize) -> Option<&'a OuterScope<'a>> {
    let mut current = Some(scope);
    for _ in 0..level {
        current = current?.parent;
    }
    current
}

pub(crate) fn eval_predicate(
    ctx: &EvalContext,
    expr: &Expr,
    current: &[Value],
) -> Result<TriBool, Error> {
    Ok(TriBool::from_value(&eval_expr(ctx, expr, current)?))
}

fn expr_display(expr: &Expr) -> String {
    expr.to_string()
}

/// Evaluate the `Expr::Floor`/`Expr::Ceil` AST nodes that sqlparser produces
/// for `FLOOR(...)` and `CEIL(...)`. Only the plain single-argument form is
/// supported; the `TO <field>` and scale forms are rejected.
fn eval_floor_ceil(
    ctx: &EvalContext,
    name: &str,
    expr: &Expr,
    field: &CeilFloorKind,
    current: &[Value],
) -> Result<Value, Error> {
    match field {
        CeilFloorKind::DateTimeField(DateTimeField::NoDateTime) => {
            let value = eval_expr(ctx, expr, current)?;
            floor_ceil(name, &value)
        }
        CeilFloorKind::DateTimeField(_) => {
            Err(format!("{} ... TO is not supported", name.to_uppercase()).into())
        }
        CeilFloorKind::Scale(_) => {
            Err(format!("{} with a scale is not supported", name.to_uppercase()).into())
        }
    }
}

/// Evaluate a `LIKE`/`ILIKE` expression. The two operators differ only in
/// case sensitivity, so they share a single implementation.
fn eval_like(
    ctx: &EvalContext,
    negated: bool,
    expr: &Expr,
    pattern: &Expr,
    escape_char: &Option<ValueWithSpan>,
    case_insensitive: bool,
    current: &[Value],
) -> Result<Value, Error> {
    let value = eval_expr(ctx, expr, current)?;
    let pattern = eval_expr(ctx, pattern, current)?;
    if value.is_null() || pattern.is_null() {
        return Ok(Value::Null);
    }
    let escape = match escape_char {
        Some(v) => match eval_sql_value(&v.value) {
            Ok(Value::Text(s)) => s.chars().next(),
            _ => None,
        },
        None => None,
    };
    let matched = like_values(&value, &pattern, case_insensitive, escape)?;
    Ok(Value::Bool(if negated { !matched } else { matched }))
}

fn eval_sql_value(value: &SqlValue) -> Result<Value, Error> {
    match value {
        SqlValue::Number(number, _) => {
            if let Ok(parsed) = number.parse::<i64>() {
                Ok(Value::Int(parsed))
            } else if let Ok(parsed) = number.parse::<f64>() {
                Ok(Value::Float(parsed))
            } else {
                Err(format!("Invalid number literal `{number}`").into())
            }
        }
        SqlValue::SingleQuotedString(s) => Ok(Value::Text(s.clone())),
        SqlValue::DoubleQuotedString(s) => Ok(Value::Text(s.clone())),
        SqlValue::Boolean(value) => Ok(Value::Bool(*value)),
        SqlValue::Null => Ok(Value::Null),
        other => Err(format!("Unsupported literal: {other}").into()),
    }
}

fn resolve_column(ctx: &EvalContext, name: &str, current: &[Value]) -> Result<Value, Error> {
    if let Some(index) = ctx.columns.get(name) {
        if *index == usize::MAX {
            return Err(
                format!("Column `{name}` is ambiguous; qualify it with a table name").into(),
            );
        }
        return Ok(current.get(*index).cloned().unwrap_or(Value::Null));
    }
    let lowered = name.to_lowercase();
    match ctx.columns.get(&lowered) {
        Some(index) if *index == usize::MAX => {
            Err(format!("Column `{lowered}` is ambiguous; qualify it with a table name").into())
        }
        Some(index) => Ok(current.get(*index).cloned().unwrap_or(Value::Null)),
        None => {
            let mut scope = ctx.outer_scope;
            while let Some(current_scope) = scope {
                if let Some(index) = current_scope.columns.get(&lowered) {
                    return Ok(current_scope
                        .row
                        .get(*index)
                        .cloned()
                        .unwrap_or(Value::Null));
                }
                scope = current_scope.parent;
            }
            Err(format!("Column `{lowered}` not found").into())
        }
    }
}

fn eval_binary(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, Error> {
    match op {
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => eval_arithmetic(op, lhs, rhs),
        BinaryOperator::StringConcat => {
            if lhs.is_null() || rhs.is_null() {
                return Ok(Value::Null);
            }
            Ok(Value::Text(format!(
                "{}{}",
                lhs.to_display_string(),
                rhs.to_display_string()
            )))
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => eval_compare(op, lhs, rhs),
        other => Err(format!("Unsupported operator: {other}").into()),
    }
}

fn eval_arithmetic(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, Error> {
    if lhs.is_null() || rhs.is_null() {
        return Ok(Value::Null);
    }

    match op {
        BinaryOperator::Divide => {
            let b = rhs
                .as_f64()
                .ok_or_else(|| format!("Cannot divide by `{}`", rhs))?;
            if b == 0.0 {
                return Err("Division by zero".to_string().into());
            }
            let a = lhs
                .as_f64()
                .ok_or_else(|| format!("Cannot divide `{}`", lhs))?;
            Ok(Value::Float(a / b))
        }
        BinaryOperator::Modulo => {
            let b = rhs
                .as_i64()
                .ok_or_else(|| format!("Cannot modulo by `{}`", rhs))?;
            if b == 0 {
                return Err("Modulo by zero".to_string().into());
            }
            let a = lhs
                .as_i64()
                .ok_or_else(|| format!("Cannot modulo `{}`", lhs))?;
            let value = a
                .checked_rem(b)
                .ok_or_else(|| format!("Integer overflow in `%` for `{a}` and `{b}`"))?;
            Ok(Value::Int(value))
        }
        _ => {
            if matches!(lhs, Value::Int(_)) && matches!(rhs, Value::Int(_)) {
                let a = lhs.as_i64().expect("int value");
                let b = rhs.as_i64().expect("int value");
                let value = match op {
                    BinaryOperator::Plus => a.checked_add(b),
                    BinaryOperator::Minus => a.checked_sub(b),
                    BinaryOperator::Multiply => a.checked_mul(b),
                    _ => unreachable!(),
                }
                .ok_or_else(|| format!("Integer overflow in `{op}` for `{a}` and `{b}`"))?;
                Ok(Value::Int(value))
            } else {
                let a = lhs
                    .as_f64()
                    .ok_or_else(|| format!("Cannot apply arithmetic to `{}`", lhs))?;
                let b = rhs
                    .as_f64()
                    .ok_or_else(|| format!("Cannot apply arithmetic to `{}`", rhs))?;
                let value = match op {
                    BinaryOperator::Plus => a + b,
                    BinaryOperator::Minus => a - b,
                    BinaryOperator::Multiply => a * b,
                    _ => unreachable!(),
                };
                Ok(Value::Float(value))
            }
        }
    }
}

fn eval_compare(op: &BinaryOperator, lhs: Value, rhs: Value) -> Result<Value, Error> {
    if lhs.is_null() || rhs.is_null() {
        return Ok(Value::Null);
    }

    match op {
        BinaryOperator::Eq => Ok(Value::Bool(values_eq(&lhs, &rhs))),
        BinaryOperator::NotEq => Ok(Value::Bool(!values_eq(&lhs, &rhs))),
        BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq => {
            let ordering = values_partial_cmp(&lhs, &rhs)
                .ok_or_else(|| format!("Cannot compare `{}` and `{}`", lhs, rhs))?;
            use std::cmp::Ordering;
            let result = match op {
                BinaryOperator::Lt => ordering == Ordering::Less,
                BinaryOperator::LtEq => ordering != Ordering::Greater,
                BinaryOperator::Gt => ordering == Ordering::Greater,
                BinaryOperator::GtEq => ordering != Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        _ => unreachable!(),
    }
}

/// Evaluate `AND`/`OR` with short-circuit semantics: when the left operand
/// already determines the result the right side is not evaluated, matching
/// SQL behavior (e.g. `FALSE AND 1/0 = 1` is FALSE, not an error).
fn eval_logic(
    ctx: &EvalContext,
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    current: &[Value],
) -> Result<Value, Error> {
    let a = eval_predicate(ctx, left, current)?;
    let short_circuited = match op {
        BinaryOperator::And => a == TriBool::False,
        BinaryOperator::Or => a == TriBool::True,
        _ => unreachable!(),
    };
    if short_circuited {
        return Ok(Value::Bool(matches!(op, BinaryOperator::Or)));
    }
    let b = eval_predicate(ctx, right, current)?;
    Ok(match op {
        BinaryOperator::And => a.and(b),
        BinaryOperator::Or => a.or(b),
        _ => unreachable!(),
    }
    .into_value())
}

fn eval_unary(op: &UnaryOperator, value: Value) -> Result<Value, Error> {
    match op {
        UnaryOperator::Plus => Ok(value),
        UnaryOperator::Minus => {
            if value.is_null() {
                return Ok(Value::Null);
            }
            match value {
                Value::Int(parsed) => parsed
                    .checked_neg()
                    .map(Value::Int)
                    .ok_or_else(|| format!("Integer overflow in unary `-` for `{parsed}`").into()),
                Value::Float(parsed) => Ok(Value::Float(-parsed)),
                other => Err(format!("Cannot negate `{other}`").into()),
            }
        }
        UnaryOperator::Not => Ok(TriBool::from_value(&value).not().into_value()),
        other => Err(format!("Unsupported unary operator: {other}").into()),
    }
}

fn eval_in_list(
    ctx: &EvalContext,
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    current: &[Value],
) -> Result<Value, Error> {
    let value = eval_expr(ctx, expr, current)?;
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut found = false;
    let mut saw_null = false;
    for item in list {
        let item_value = eval_expr(ctx, item, current)?;
        if item_value.is_null() {
            saw_null = true;
        } else if values_eq(&value, &item_value) {
            found = true;
            break;
        }
    }
    if found {
        Ok(Value::Bool(!negated))
    } else if saw_null {
        Ok(Value::Null)
    } else {
        Ok(Value::Bool(negated))
    }
}

fn eval_between(
    ctx: &EvalContext,
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    current: &[Value],
) -> Result<Value, Error> {
    let value = eval_expr(ctx, expr, current)?;
    let low = eval_expr(ctx, low, current)?;
    let high = eval_expr(ctx, high, current)?;

    let lower = compare_predicate(&value, &low, |ordering| {
        ordering != std::cmp::Ordering::Less
    });
    let upper = compare_predicate(&value, &high, |ordering| {
        ordering != std::cmp::Ordering::Greater
    });
    let result = lower.and(upper);
    Ok(if negated {
        result.not().into_value()
    } else {
        result.into_value()
    })
}

fn compare_predicate(
    left: &Value,
    right: &Value,
    predicate: impl FnOnce(std::cmp::Ordering) -> bool,
) -> TriBool {
    if left.is_null() || right.is_null() {
        return TriBool::Unknown;
    }
    match values_partial_cmp(left, right) {
        Some(ordering) => TriBool::from_value(&Value::Bool(predicate(ordering))),
        None => TriBool::Unknown,
    }
}

fn eval_case(
    ctx: &EvalContext,
    operand: &Option<Box<Expr>>,
    conditions: &[CaseWhen],
    else_result: &Option<Box<Expr>>,
    current: &[Value],
) -> Result<Value, Error> {
    let operand_value = match operand {
        Some(operand) => Some(eval_expr(ctx, operand, current)?),
        None => None,
    };

    for case_when in conditions {
        let matched = match &operand_value {
            Some(expected) => {
                let actual = eval_expr(ctx, &case_when.condition, current)?;
                values_eq(expected, &actual)
            }
            None => eval_expr(ctx, &case_when.condition, current)?.truthy(),
        };
        if matched {
            return eval_expr(ctx, &case_when.result, current);
        }
    }

    match else_result {
        Some(result) => eval_expr(ctx, result, current),
        None => Ok(Value::Null),
    }
}

fn trim_string(value: &str, side: TrimWhereField, what: Option<&str>) -> String {
    match what {
        None => match side {
            TrimWhereField::Both => value.trim().to_string(),
            TrimWhereField::Leading => value.trim_start().to_string(),
            TrimWhereField::Trailing => value.trim_end().to_string(),
        },
        Some(characters) => {
            let trimmed = if matches!(side, TrimWhereField::Leading | TrimWhereField::Both) {
                value.trim_start_matches(|c| characters.contains(c))
            } else {
                value
            };
            let trimmed = if matches!(side, TrimWhereField::Trailing | TrimWhereField::Both) {
                trimmed.trim_end_matches(|c| characters.contains(c))
            } else {
                trimmed
            };
            trimmed.to_string()
        }
    }
}

fn like_values(
    value: &Value,
    pattern: &Value,
    case_insensitive: bool,
    escape: Option<char>,
) -> Result<bool, Error> {
    if value.is_null() || pattern.is_null() {
        return Ok(false);
    }
    let text = value.to_display_string();
    let pattern_text = pattern.to_display_string();
    Ok(like_match(&text, &pattern_text, case_insensitive, escape))
}

pub fn like_match(text: &str, pattern: &str, case_insensitive: bool, escape: Option<char>) -> bool {
    let text: Vec<char> = if case_insensitive {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let pattern_norm: String = if case_insensitive {
        pattern.to_lowercase()
    } else {
        pattern.to_string()
    };

    enum Token {
        Percent,
        Underscore,
        Literal(char),
    }

    let mut tokens: Vec<Token> = Vec::new();
    let mut chars = pattern_norm.chars().peekable();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            if let Some(next) = chars.next() {
                tokens.push(Token::Literal(next));
            }
            // A trailing escape character with nothing after it is ignored.
        } else if c == '%' {
            tokens.push(Token::Percent);
        } else if c == '_' {
            tokens.push(Token::Underscore);
        } else {
            tokens.push(Token::Literal(c));
        }
    }

    let n = tokens.len();

    // Fast path: a pattern with no wildcards is a plain equality check
    // (LIKE is anchored on both ends).
    if n > 0
        && tokens
            .iter()
            .all(|token| matches!(token, Token::Literal(_)))
    {
        return text.iter().eq(tokens.iter().map(|token| match token {
            Token::Literal(c) => c,
            _ => unreachable!("checked above"),
        }));
    }

    // Rolling two-row DP: O(n) memory instead of O(text_len * n).
    let mut prev = vec![false; n + 1];
    let mut curr = vec![false; n + 1];
    prev[0] = true;
    for j in 0..n {
        if matches!(tokens[j], Token::Percent) {
            prev[j + 1] = prev[j];
        }
    }

    for &text_char in &text {
        curr[0] = false;
        for j in 0..n {
            match tokens[j] {
                Token::Percent => curr[j + 1] = curr[j] || prev[j + 1],
                Token::Underscore => curr[j + 1] = prev[j],
                Token::Literal(ch) => curr[j + 1] = prev[j] && text_char == ch,
            }
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

fn cast_value(value: Value, data_type: &SqlDataType) -> Result<Value, Error> {
    match data_type {
        SqlDataType::Int(_)
        | SqlDataType::Integer(_)
        | SqlDataType::SmallInt(_)
        | SqlDataType::SmallIntUnsigned(_)
        | SqlDataType::Int2Unsigned(_)
        | SqlDataType::TinyInt(_)
        | SqlDataType::TinyIntUnsigned(_)
        | SqlDataType::UTinyInt
        | SqlDataType::USmallInt
        | SqlDataType::BigInt(_)
        | SqlDataType::BigIntUnsigned(_)
        | SqlDataType::Int8Unsigned(_)
        | SqlDataType::Int4Unsigned(_)
        | SqlDataType::IntegerUnsigned(_)
        | SqlDataType::IntUnsigned(_)
        | SqlDataType::MediumIntUnsigned(_)
        | SqlDataType::Unsigned
        | SqlDataType::UnsignedInteger => {
            if let Some(parsed) = value.as_i64() {
                return Ok(Value::Int(parsed));
            }
            if let Some(text) = value.as_text()
                && let Ok(parsed) = text.parse::<i64>()
            {
                return Ok(Value::Int(parsed));
            }
            Err(format!("Cannot cast `{}` to integer", value).into())
        }
        SqlDataType::Float(_)
        | SqlDataType::Real
        | SqlDataType::Double(_)
        | SqlDataType::DoublePrecision
        | SqlDataType::FloatUnsigned(_)
        | SqlDataType::RealUnsigned
        | SqlDataType::DoubleUnsigned(_)
        | SqlDataType::DoublePrecisionUnsigned => {
            if let Some(parsed) = value.as_f64() {
                return Ok(Value::Float(parsed));
            }
            if let Some(text) = value.as_text()
                && let Ok(parsed) = text.parse::<f64>()
            {
                return Ok(Value::Float(parsed));
            }
            Err(format!("Cannot cast `{}` to float", value).into())
        }
        SqlDataType::Boolean | SqlDataType::Bool => {
            if let Some(parsed) = value.as_bool() {
                return Ok(Value::Bool(parsed));
            }
            if let Some(text) = value.as_text() {
                let lower = text.to_lowercase();
                if lower == "true" || lower == "1" {
                    return Ok(Value::Bool(true));
                }
                if lower == "false" || lower == "0" {
                    return Ok(Value::Bool(false));
                }
            }
            Err(format!("Cannot cast `{}` to boolean", value).into())
        }
        SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Char(_)
        | SqlDataType::Varchar(_)
        | SqlDataType::Character(_)
        | SqlDataType::CharacterVarying(_)
        | SqlDataType::TinyText
        | SqlDataType::MediumText
        | SqlDataType::LongText => Ok(Value::Text(value.to_display_string())),
        _ => Err(format!("Unsupported cast target type `{}`", data_type).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    fn parse_expr(sql: &str) -> Expr {
        Parser::new(&MySqlDialect {})
            .try_with_sql(sql)
            .expect("parse sql")
            .parse_expr()
            .expect("parse expr")
    }

    fn scalar(expr: &str) -> Value {
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[]).expect("eval scalar")
    }

    fn scalar_result(expr: &str) -> Result<Value, Error> {
        eval_expr(&EvalContext::scalar(), &parse_expr(expr), &[])
    }

    #[test]
    fn runtime_assigns_subquery_ids_by_query_ast() {
        let first = Parser::new(&MySqlDialect {})
            .try_with_sql("SELECT 1")
            .expect("valid query")
            .parse_query()
            .expect("valid query");
        let second = Parser::new(&MySqlDialect {})
            .try_with_sql("SELECT 2")
            .expect("valid query")
            .parse_query()
            .expect("valid query");
        let runtime = QueryRuntime::default();
        assert_ne!(runtime.subquery_id(&first), runtime.subquery_id(&second));
        assert_eq!(runtime.subquery_id(&first), runtime.subquery_id(&first));
    }

    #[test]
    fn runtime_stats_are_read_through_one_snapshot() {
        let runtime = QueryRuntime::default();
        *runtime.correlated_cache_hits.lock().unwrap() = 3;
        *runtime.correlated_cache_misses.lock().unwrap() = 2;
        assert_eq!(
            runtime.stats(),
            RuntimeStats {
                correlated_cache_hits: 3,
                correlated_cache_misses: 2,
            }
        );
    }

    #[test]
    fn arithmetic() {
        assert_eq!(scalar("1 + 2"), Value::Int(3));
        assert_eq!(scalar("10 - 4"), Value::Int(6));
        assert_eq!(scalar("2 * 3"), Value::Int(6));
        assert_eq!(scalar("10 / 4"), Value::Float(2.5));
        assert_eq!(scalar("7 % 3"), Value::Int(1));
        assert_eq!(scalar("2 * 3.5"), Value::Float(7.0));
    }

    #[test]
    fn arithmetic_null_and_zero() {
        assert_eq!(scalar("1 + NULL"), Value::Null);
        assert!(scalar_result("1 / 0").is_err());
        assert!(scalar_result("1 % 0").is_err());
    }

    #[test]
    fn modulo_overflow_is_an_error_not_a_panic() {
        // i64::MIN % -1 traps in debug builds; it must surface as an error.
        let error = scalar_result("-9223372036854775808 % -1").unwrap_err();
        assert!(error.contains("overflow"), "got: {error}");
    }

    #[test]
    fn is_false_semantics_follow_three_valued_logic() {
        assert_eq!(
            eval_expr(&EvalContext::scalar(), &parse_expr("NULL IS FALSE"), &[]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_expr(
                &EvalContext::scalar(),
                &parse_expr("NULL IS NOT FALSE"),
                &[]
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_expr(&EvalContext::scalar(), &parse_expr("FALSE IS FALSE"), &[]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_expr(&EvalContext::scalar(), &parse_expr("TRUE IS FALSE"), &[]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_expr(&EvalContext::scalar(), &parse_expr("NULL IS TRUE"), &[]).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn like_without_wildcards_is_an_exact_match() {
        assert!(!like_match("Bobby", "Bob", false, None));
        assert!(like_match("Bob", "Bob", false, None));
        assert!(!like_match("abc", "ab", false, None));
        assert!(!like_match("ab", "abc", false, None));
        assert!(like_match("", "", false, None));
        // Wildcard patterns are unaffected.
        assert!(like_match("Bobby", "Bob%", false, None));
        assert!(like_match("abc", "%ab%", false, None));
    }

    #[test]
    fn substring_negative_start_counts_from_the_end() {
        assert_eq!(
            scalar("SUBSTRING('SheetQL', -2)"),
            Value::Text("QL".to_string())
        );
        assert_eq!(
            scalar("SUBSTRING('SheetQL', -3, 2)"),
            Value::Text("tQ".to_string())
        );
        // Position 0 yields an empty string (MySQL behavior).
        assert_eq!(
            scalar("SUBSTRING('SheetQL', 0)"),
            Value::Text(String::new())
        );
        assert_eq!(
            scalar("SUBSTRING('SheetQL', 3)"),
            Value::Text("eetQL".to_string())
        );
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(scalar("1 < 2"), Value::Bool(true));
        assert_eq!(scalar("1 = 1"), Value::Bool(true));
        assert_eq!(scalar("1 != 2"), Value::Bool(true));
        assert_eq!(scalar("2 <= 2"), Value::Bool(true));
        assert_eq!(scalar("3 > 4"), Value::Bool(false));
        assert_eq!(scalar("TRUE AND FALSE"), Value::Bool(false));
        assert_eq!(scalar("TRUE OR FALSE"), Value::Bool(true));
        assert_eq!(scalar("NOT FALSE"), Value::Bool(true));
        assert_eq!(scalar("1 = NULL"), Value::Null);
    }

    #[test]
    fn three_valued_logic_truth_tables() {
        assert_eq!(scalar("NULL AND TRUE"), Value::Null);
        assert_eq!(scalar("NULL AND FALSE"), Value::Bool(false));
        assert_eq!(scalar("NULL OR TRUE"), Value::Bool(true));
        assert_eq!(scalar("NULL OR FALSE"), Value::Null);
        assert_eq!(scalar("NOT NULL"), Value::Null);
    }

    #[test]
    fn correlated_scope_lookup_walks_parent_scopes() {
        let columns = HashMap::from([(String::from("p.id"), 0usize)]);
        let runtime = QueryRuntime::default();
        let outer_row = vec![Value::Int(7)];
        let outer = OuterScope {
            row: &outer_row,
            columns: &columns,
            parent: None,
            runtime: &runtime,
        };
        assert_eq!(
            scope_at(&outer, 0).map(|scope| scope.row[0].clone()),
            Some(Value::Int(7))
        );
        assert!(scope_at(&outer, 1).is_none());
    }

    #[test]
    fn string_operators() {
        assert_eq!(scalar("'SheetQL' LIKE '%QL'"), Value::Bool(true));
        assert_eq!(scalar("'SheetQL' LIKE '%missing%'"), Value::Bool(false));
        assert_eq!(scalar("'abc' ILIKE 'ABC'"), Value::Bool(true));
        assert_eq!(scalar("'a' IN ('a', 'b')"), Value::Bool(true));
        assert_eq!(scalar("'c' IN ('a', 'b')"), Value::Bool(false));
        assert_eq!(scalar("5 BETWEEN 1 AND 10"), Value::Bool(true));
        assert_eq!(scalar("15 BETWEEN 1 AND 10"), Value::Bool(false));
    }

    #[test]
    fn in_list_null_semantics() {
        // Matching value wins even when NULL is in the list.
        assert_eq!(scalar("1 IN (1, NULL)"), Value::Bool(true));
        // Unmatched with NULL in the list is NULL, not FALSE.
        assert_eq!(scalar("3 IN (1, NULL)"), Value::Null);
        // NOT IN with NULL in the list is NULL when unmatched.
        assert_eq!(scalar("3 NOT IN (1, NULL)"), Value::Null);
        // NOT IN still returns FALSE for a matching value.
        assert_eq!(scalar("1 NOT IN (1, NULL)"), Value::Bool(false));
    }

    #[test]
    fn between_null_semantics() {
        // SQL three-valued logic propagates UNKNOWN from a NULL operand.
        assert_eq!(scalar("5 BETWEEN 1 AND NULL"), Value::Null);
        assert_eq!(scalar("NULL BETWEEN 1 AND 5"), Value::Null);
    }

    #[test]
    fn case_expression() {
        assert_eq!(
            scalar("CASE WHEN 2 > 1 THEN 'yes' ELSE 'no' END"),
            Value::Text("yes".to_string())
        );
        assert_eq!(
            scalar("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END"),
            Value::Text("two".to_string())
        );
        assert_eq!(scalar("CASE WHEN FALSE THEN 1 ELSE 2 END"), Value::Int(2));
    }

    #[test]
    fn cast_function() {
        assert_eq!(scalar("CAST('42' AS INTEGER)"), Value::Int(42));
        assert_eq!(scalar("CAST(3.5 AS TEXT)"), Value::Text("3.5".to_string()));
        assert_eq!(scalar("CAST('true' AS BOOLEAN)"), Value::Bool(true));
    }

    #[test]
    fn unary_operators() {
        assert_eq!(scalar("-5"), Value::Int(-5));
        assert_eq!(scalar("+5"), Value::Int(5));
        assert_eq!(scalar("-3.7"), Value::Float(-3.7));
        assert_eq!(scalar("NOT TRUE"), Value::Bool(false));
    }

    #[test]
    fn unknown_function_errors() {
        assert!(scalar_result("NOPE(1)").is_err());
    }
}
