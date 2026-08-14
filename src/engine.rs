use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;

use sqlparser::ast::{
    Distinct, Expr, GroupByExpr, JoinConstraint, JoinOperator, LimitClause, Offset, OrderByKind,
    Select, SelectItem, SetExpr, Statement, TableFactor,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::database::Schema;
use crate::database::Table;
use crate::evaluator::contains_aggregate;
use crate::evaluator::eval_expr;
use crate::evaluator::EvalContext;
use crate::value::values_eq;
use crate::value::values_partial_cmp;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

pub fn run_query(schema: &mut Schema, sql: &str) -> Result<QueryResult, String> {
    let trimmed = sql.trim();
    let lower = trimmed.to_lowercase().trim_end_matches(';').trim().to_string();

    if lower.starts_with("show databases") {
        let rest = lower.strip_prefix("show databases").unwrap().trim();
        let (_, like) = parse_show_clauses(rest)?;
        return run_show_databases(schema, like.as_deref());
    }
    if lower.starts_with("show schemas") {
        let rest = lower.strip_prefix("show schemas").unwrap().trim();
        let (_, like) = parse_show_clauses(rest)?;
        return run_show_databases(schema, like.as_deref());
    }
    if lower.starts_with("show tables") {
        let rest = lower.strip_prefix("show tables").unwrap().trim();
        let (database, like) = parse_show_clauses(rest)?;
        return run_show_tables(schema, database.as_deref(), like.as_deref());
    }
    if let Some(rest) = lower.strip_prefix("describe ") {
        return run_describe_table(schema, rest.trim());
    }
    if let Some(rest) = lower.strip_prefix("desc ") {
        return run_describe_table(schema, rest.trim());
    }
    if let Some(name) = lower.strip_prefix("use ") {
        let name = name.trim();
        if name.is_empty() {
            return Err("USE requires a database name".to_string());
        }
        return run_use(schema, name);
    }

    let dialect = MySqlDialect {};
    let statements =
        Parser::parse_sql(&dialect, sql).map_err(|error| format!("SQL parse error: {error}"))?;

    if statements.len() != 1 {
        return Err("Only a single statement per query is supported".to_string());
    }

    match &statements[0] {
        Statement::Query(query) => execute_query(schema, query),
        Statement::ShowColumns { show_options, .. } => {
            if show_options.filter_position.is_some() {
                return Err("SHOW COLUMNS filters (LIKE/WHERE) are not supported".to_string());
            }
            let reference = show_options
                .show_in
                .as_ref()
                .and_then(|show_in| show_in.parent_name.as_ref())
                .map(|name| object_name_to_parts(name).join("."))
                .ok_or_else(|| "SHOW COLUMNS requires a table name".to_string())?;
            run_describe_table(schema, &reference)
        }
        Statement::ShowSchemas { .. } | Statement::ShowDatabases { .. } => {
            run_show_databases(schema, None)
        }
        other => Err(format!("Unsupported statement: {other}")),
    }
}

fn run_show_databases(schema: &Schema, like: Option<&str>) -> Result<QueryResult, String> {
    let columns = vec!["Database".to_string()];
    let rows = schema
        .database_names()
        .into_iter()
        .filter(|name| like.is_none_or(|pattern| like_match(pattern, name)))
        .map(|name| vec![Value::Text(name.to_string())])
        .collect();
    Ok(QueryResult { columns, rows })
}

fn run_use(schema: &mut Schema, name: &str) -> Result<QueryResult, String> {
    schema.set_current_database(name)?;
    Ok(QueryResult {
        columns: vec!["Status".to_string()],
        rows: vec![vec![Value::Text("Database changed".to_string())]],
    })
}

fn run_show_tables(
    schema: &Schema,
    database_name: Option<&str>,
    like: Option<&str>,
) -> Result<QueryResult, String> {
    let name = match database_name {
        Some(name) => name.to_string(),
        None => schema
            .current_database()
            .map(|name| name.to_string())
            .ok_or_else(|| "No database selected, use `USE <database>` first".to_string())?,
    };
    let database = schema
        .get_database(&name)
        .ok_or_else(|| format!("Unknown database `{name}`"))?;
    let columns = vec!["Tables".to_string()];
    let rows = database
        .table_names()
        .into_iter()
        .filter(|name| like.is_none_or(|pattern| like_match(pattern, name)))
        .map(|name| vec![Value::Text(name.to_string())])
        .collect();
    Ok(QueryResult { columns, rows })
}

fn run_describe_table(schema: &Schema, reference: &str) -> Result<QueryResult, String> {
    let parts: Vec<&str> = reference.split('.').collect();
    let (database, table_name) = match parts.as_slice() {
        [table_name] => (None, *table_name),
        [database, table_name] => (Some(*database), *table_name),
        _ => return Err("Table reference must be `table` or `database.table`".to_string()),
    };
    let (_, table) = schema.resolve_table(database, table_name)?;
    describe_table(table)
}

fn describe_table(table: &Table) -> Result<QueryResult, String> {
    let columns = vec!["Column".to_string(), "Type".to_string()];
    let rows: Vec<Vec<Value>> = table
        .columns
        .iter()
        .map(|column| {
            vec![
                Value::Text(column.clone()),
                Value::Text(infer_column_type(table, column).to_string()),
            ]
        })
        .collect();
    Ok(QueryResult { columns, rows })
}

fn infer_column_type(table: &Table, column: &str) -> &'static str {
    let index = match table.column_index(column) {
        Some(index) => index,
        None => return "Text",
    };
    let mut has_int = false;
    let mut has_float = false;
    let mut has_bool = false;
    let mut has_text = false;
    for row in &table.rows {
        match row.get(index) {
            Some(Value::Int(_)) => has_int = true,
            Some(Value::Float(_)) => has_float = true,
            Some(Value::Bool(_)) => has_bool = true,
            Some(Value::Text(_)) => has_text = true,
            _ => {}
        }
    }
    if has_text {
        "Text"
    } else if has_bool {
        "Boolean"
    } else if has_float {
        "Float"
    } else if has_int {
        "Integer"
    } else {
        "Text"
    }
}

fn execute_query(schema: &Schema, query: &sqlparser::ast::Query) -> Result<QueryResult, String> {
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
        let ctx = EvalContext::new(&lookup, &rows, &[]);
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

    if is_aggregate {
        let groups = build_groups(&lookup, &rows, &group_exprs)?;

        for group in &groups {
            let ctx = EvalContext::new(&lookup, &rows, group);
            let representative = representative_row(&rows, group);

            if let Some(having) = &select.having {
                let value = eval_expr(&ctx, having, representative)?;
                if !value.truthy() {
                    continue;
                }
            }

            let out = project_group(&ctx, &plan, representative)?;

            let mut order_columns: HashMap<String, usize> = HashMap::new();
            for (index, title) in output_titles.iter().enumerate() {
                order_columns.insert(title.clone(), index);
            }
            let order_ctx = EvalContext::new(&order_columns, &rows, group);
            let keys = compute_order_keys(query, &order_ctx, &out)?;
            keyed.push((keys, out));
        }
    } else {
        let ctx = EvalContext::new(&lookup, &rows, &[]);
        for row in &rows {
            let out = project_row(&ctx, &plan, row)?;
            let mut combined = out.clone();
            combined.extend_from_slice(row);

            let mut order_columns: HashMap<String, usize> = HashMap::new();
            for (name, index) in &lookup {
                order_columns.insert(name.clone(), output_titles.len() + index);
            }
            for (index, title) in output_titles.iter().enumerate() {
                order_columns.insert(title.clone(), index);
            }
            let order_ctx = EvalContext::new(&order_columns, &rows, &[]);
            let keys = compute_order_keys(query, &order_ctx, &combined)?;
            keyed.push((keys, out));
        }
    }

    if is_distinct(select)? {
        let mut seen: HashSet<String> = HashSet::new();
        keyed.retain(|(_, out)| seen.insert(format!("{:?}", out)));
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

    Ok(QueryResult {
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

fn parse_limit(limit_clause: &Option<LimitClause>) -> Result<(Option<usize>, Option<usize>), String> {
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
    query: &sqlparser::ast::Query,
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

fn compare_keys(
    a: &[Value],
    b: &[Value],
    exprs: &[sqlparser::ast::OrderByExpr],
) -> Ordering {
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

    let ctx = EvalContext::new(lookup, rows, &[]);
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();

    for (row_index, row) in rows.iter().enumerate() {
        let mut key: Vec<String> = vec![];
        for expr in group_exprs {
            let value = eval_expr(&ctx, expr, row)?;
            key.push(format!("{:?}", value));
        }
        let key = key.join("|");
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

fn project_row(ctx: &EvalContext, plan: &[ProjectionItem], row: &[Value]) -> Result<Vec<Value>, String> {
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

fn project_group(
    ctx: &EvalContext,
    plan: &[ProjectionItem],
    representative: &[Value],
) -> Result<Vec<Value>, String> {
    let mut out: Vec<Value> = Vec::with_capacity(plan.len());
    for item in plan {
        match item {
            ProjectionItem::Column { index, .. } => {
                out.push(representative.get(*index).cloned().unwrap_or(Value::Null));
            }
            ProjectionItem::Expression { expr, .. } => {
                out.push(eval_expr(ctx, expr, representative)?);
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

fn object_name_to_parts(name: &sqlparser::ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.to_lowercase())
        .collect()
}

/// Split the tail of a `SHOW ... [FROM <database>] [LIKE 'pattern']` clause
/// into an optional database name and an optional LIKE pattern.
fn parse_show_clauses(rest: &str) -> Result<(Option<String>, Option<String>), String> {
    let mut database = None;
    let mut pattern = None;
    let mut expecting_name = false;
    let mut tokens = rest.split_whitespace().peekable();

    while let Some(token) = tokens.next() {
        let keyword = token.to_ascii_lowercase();
        match keyword.as_str() {
            "from" | "in" => {
                expecting_name = true;
            }
            "like" => {
                let value = tokens
                    .next()
                    .ok_or_else(|| "SHOW ... LIKE requires a pattern".to_string())?;
                pattern = Some(unquote_pattern(value));
                if expecting_name {
                    return Err("SHOW ... FROM requires a database name".to_string());
                }
            }
            other if expecting_name => {
                database = Some(other.to_string());
                expecting_name = false;
            }
            other => return Err(format!("Invalid SHOW syntax near `{other}`")),
        }
    }

    if expecting_name {
        return Err("SHOW ... FROM requires a database name".to_string());
    }
    Ok((database, pattern))
}

fn unquote_pattern(token: &str) -> String {
    let bytes = token.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            return token[1..token.len() - 1].to_string();
        }
    }
    token.to_string()
}

/// Match a value against a `LIKE` pattern supporting `%` (any sequence) and
/// `_` (single character).
fn like_match(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();

    fn helper(p: &[char], v: &[char]) -> bool {
        if p.is_empty() {
            return v.is_empty();
        }
        match p[0] {
            '%' => helper(&p[1..], v) || (!v.is_empty() && helper(p, &v[1..])),
            '_' => !v.is_empty() && helper(&p[1..], &v[1..]),
            c => !v.is_empty() && v[0] == c && helper(&p[1..], &v[1..]),
        }
    }

    helper(&p, &v)
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

    for (left_index, left_row) in left_rows.iter().enumerate() {
        for (right_index, right_row) in right.rows.iter().enumerate() {
            let mut combined = left_row.clone();
            combined.extend_from_slice(right_row);

            let keep = join_keep(operator, left_schema, &right.schema, &combined)?;
            if keep {
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
                let mut combined: Vec<Value> =
                    std::iter::repeat_n(Value::Null, left_len).collect();
                combined.extend_from_slice(right_row);
                output.push(combined);
            }
        }
    }

    Ok((schema, output))
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
            let ctx = EvalContext::new(&lookup, &[], &[]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn make_database() -> Database {
        let mut database = Database::named("test");
        database.add_table(Table {
            name: "people".to_string(),
            columns: vec![
                "id".to_string(),
                "name".to_string(),
                "age".to_string(),
                "city".to_string(),
            ],
            rows: vec![
                vec![Value::Int(1), Value::Text("Alice".into()), Value::Int(30), Value::Text("NY".into())],
                vec![Value::Int(2), Value::Text("Bob".into()), Value::Int(25), Value::Text("LA".into())],
                vec![Value::Int(3), Value::Text("Carol".into()), Value::Int(35), Value::Text("SF".into())],
                vec![Value::Int(4), Value::Text("Dan".into()), Value::Int(40), Value::Text("NY".into())],
                vec![Value::Int(5), Value::Text("Eve".into()), Value::Int(28), Value::Text("LA".into())],
            ],
        });
        database.add_table(Table {
            name: "orders".to_string(),
            columns: vec![
                "order_id".to_string(),
                "customer_id".to_string(),
                "amount".to_string(),
            ],
            rows: vec![
                vec![Value::Int(101), Value::Int(1), Value::Float(50.5)],
                vec![Value::Int(102), Value::Int(2), Value::Float(20.0)],
                vec![Value::Int(103), Value::Int(1), Value::Float(99.9)],
            ],
        });
        database
    }

    fn make_schema() -> Schema {
        let mut schema = Schema::new();
        schema.add_database(make_database());
        schema.set_current_database("test").unwrap();
        schema
    }

    fn run(schema: &mut Schema, sql: &str) -> QueryResult {
        run_query(schema, sql).unwrap_or_else(|error| panic!("query `{sql}` failed: {error}"))
    }

    #[test]
    fn show_databases_lists_all_databases() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW DATABASES");
        assert_eq!(result.columns, vec!["Database".to_string()]);
        assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
    }

    #[test]
    fn use_switches_current_database() {
        let mut schema = make_schema();
        let mut other = Database::named("other");
        other.add_table(Table {
            name: "extra".to_string(),
            columns: vec!["value".to_string()],
            rows: vec![vec![Value::Int(7)]],
        });
        schema.add_database(other);

        let result = run(&mut schema, "USE other");
        assert_eq!(
            result.rows,
            vec![vec![Value::Text("Database changed".into())]]
        );
        assert_eq!(schema.current_database(), Some("other"));

        let result = run(&mut schema, "SELECT value FROM extra");
        assert_eq!(result.rows, vec![vec![Value::Int(7)]]);
        assert!(run_query(&mut schema, "USE nope").is_err());
    }

    #[test]
    fn qualified_table_reference_ignores_current_database() {
        let mut schema = make_schema();
        let mut other = Database::named("other");
        other.add_table(Table {
            name: "extra".to_string(),
            columns: vec!["value".to_string()],
            rows: vec![vec![Value::Int(7)]],
        });
        schema.add_database(other);

        let result = run(&mut schema, "SELECT value FROM other.extra");
        assert_eq!(result.rows, vec![vec![Value::Int(7)]]);
    }

    #[test]
    fn unqualified_table_reference_reports_ambiguity() {
        let mut schema = make_schema();
        let mut other = Database::named("other");
        other.add_table(Table {
            name: "people".to_string(),
            columns: vec!["id".to_string()],
            rows: vec![],
        });
        schema.add_database(other);
        schema.add_database(Database::named("empty"));

        run(&mut schema, "USE empty");
        let err = run_query(&mut schema, "SELECT * FROM people").unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
        let ok = run(&mut schema, "SELECT * FROM other.people");
        assert_eq!(ok.rows.len(), 0);
    }

    #[test]
    fn show_tables_from_lists_specific_database() {
        let mut schema = make_schema();
        let mut other = Database::named("other");
        other.add_table(Table {
            name: "extra".to_string(),
            columns: vec!["value".to_string()],
            rows: vec![],
        });
        schema.add_database(other);

        let result = run(&mut schema, "SHOW TABLES FROM other");
        assert_eq!(result.rows, vec![vec![Value::Text("extra".into())]]);
        assert!(run_query(&mut schema, "SHOW TABLES FROM nope").is_err());
    }

    #[test]
    fn show_tables_like_filters_table_names() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW TABLES LIKE 'peop%'");
        assert_eq!(result.rows, vec![vec![Value::Text("people".into())]]);
        let result = run(&mut schema, "SHOW TABLES LIKE '%der%'");
        assert_eq!(result.rows, vec![vec![Value::Text("orders".into())]]);
        let result = run(&mut schema, "SHOW TABLES LIKE 'z%'");
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn show_schemas_is_database_alias() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW SCHEMAS");
        assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
    }

    #[test]
    fn show_databases_like_filters_database_names() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW DATABASES LIKE 'te%'");
        assert_eq!(result.rows, vec![vec![Value::Text("test".into())]]);
        let result = run(&mut schema, "SHOW DATABASES LIKE 'x%'");
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn distinct_on_is_rejected() {
        let mut schema = make_schema();
        let err = run_query(&mut schema, "SELECT DISTINCT ON (city) city FROM people").unwrap_err();
        assert!(err.contains("DISTINCT ON"), "got: {err}");
    }

    #[test]
    fn like_match_supports_wildcards() {
        assert!(like_match("a%", "abc"));
        assert!(like_match("%c", "abc"));
        assert!(like_match("a_c", "abc"));
        assert!(!like_match("a_c", "ab"));
        assert!(!like_match("a%", "xyz"));
        assert!(like_match("%", "anything"));
        assert!(like_match("", ""));
    }

    #[test]
    fn show_tables_lists_registered_tables() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW TABLES");
        assert_eq!(result.columns, vec!["Tables".to_string()]);
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn describe_reports_columns_and_types() {
        let mut schema = make_schema();
        let result = run(&mut schema, "DESCRIBE people");
        assert_eq!(result.columns, vec!["Column".to_string(), "Type".to_string()]);
        assert_eq!(
            result.rows[0],
            vec![Value::Text("id".into()), Value::Text("Integer".into())]
        );
        assert_eq!(
            result.rows[1],
            vec![Value::Text("name".into()), Value::Text("Text".into())]
        );
    }

    #[test]
    fn show_columns_lists_columns() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SHOW COLUMNS FROM people");
        assert_eq!(result.columns, vec!["Column".to_string(), "Type".to_string()]);
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0][0], Value::Text("id".into()));
        assert_eq!(result.rows[0][1], Value::Text("Integer".into()));
    }

    #[test]
    fn show_columns_supports_backticks_and_unicode_names() {
        let mut schema = make_schema();
        let mut sales = Database::named("sales_db");
        sales.add_table(Table {
            name: "商品销售明细".to_string(),
            columns: vec!["商品".to_string(), "金额".to_string()],
            rows: vec![vec![Value::Text("A".into()), Value::Int(10)]],
        });
        schema.add_database(sales);
        schema.set_current_database("sales_db").unwrap();

        let result = run(&mut schema, "SHOW COLUMNS FROM `商品销售明细`");
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Text("商品".into()));

        let result = run(&mut schema, "SELECT 商品 FROM 商品销售明细");
        assert_eq!(result.rows, vec![vec![Value::Text("A".into())]]);
    }

    #[test]
    fn select_all_returns_all_rows() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT * FROM people");
        assert_eq!(result.columns, vec!["id", "name", "age", "city"]);
        assert_eq!(result.rows.len(), 5);
    }

    #[test]
    fn where_filter_and_order_by_desc() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT name FROM people WHERE age >= 30 ORDER BY age DESC",
        );
        let names: Vec<String> = result
            .rows
            .iter()
            .map(|row| row[0].to_display_string())
            .collect();
        assert_eq!(names, vec!["Dan", "Carol", "Alice"]);
    }

    #[test]
    fn group_by_having_aggregates() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT city, COUNT(*) AS cnt FROM people GROUP BY city HAVING COUNT(*) > 1",
        );
        assert_eq!(result.columns, vec!["city".to_string(), "cnt".to_string()]);
        assert_eq!(result.rows.len(), 2);
        let sum: i64 = result.rows.iter().map(|row| row[1].as_i64().unwrap()).sum();
        assert_eq!(sum, 4);
    }

    #[test]
    fn inner_join_matches_rows() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT p.name, o.amount FROM people AS p JOIN orders AS o ON p.id = o.customer_id",
        );
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn left_join_keeps_unmatched_left_rows() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT p.name FROM people p LEFT JOIN orders o ON p.id = o.customer_id",
        );
        // Alice has 2 orders, Bob has 1; Carol, Dan and Eve have none, so 3 matched
        // pairs plus 3 unmatched left rows.
        assert_eq!(result.rows.len(), 6);
    }

    #[test]
    fn right_join_keeps_unmatched_right_rows() {
        let mut database = Database::named("test");
        database.add_table(Table {
            name: "people".to_string(),
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![
                vec![Value::Int(1), Value::Text("Alice".into())],
                vec![Value::Int(2), Value::Text("Bob".into())],
            ],
        });
        database.add_table(Table {
            name: "orders".to_string(),
            columns: vec!["order_id".to_string(), "customer_id".to_string()],
            rows: vec![
                vec![Value::Int(101), Value::Int(1)],
                vec![Value::Int(102), Value::Int(99)],
            ],
        });
        let mut schema = Schema::new();
        schema.add_database(database);
        schema.set_current_database("test").unwrap();

        let result = run(
            &mut schema,
            "SELECT p.name, o.order_id FROM people p RIGHT JOIN orders o ON p.id = o.customer_id",
        );
        // Alice matches order 101; order 102 has no matching person and is kept with NULL.
        assert_eq!(result.rows.len(), 2);
        assert_eq!(
            result.rows[0],
            vec![Value::Text("Alice".into()), Value::Int(101)]
        );
        assert_eq!(result.rows[1], vec![Value::Null, Value::Int(102)]);
    }

    #[test]
    fn using_join_matches_on_shared_column() {
        let mut schema = make_schema();
        let mut regions_db = Database::named("regions_db");
        regions_db.add_table(Table {
            name: "regions".to_string(),
            columns: vec!["id".to_string(), "region".to_string()],
            rows: vec![
                vec![Value::Int(1), Value::Text("East".into())],
                vec![Value::Int(3), Value::Text("West".into())],
            ],
        });
        schema.add_database(regions_db);
        let result = run(
            &mut schema,
            "SELECT p.name, r.region FROM people p INNER JOIN regions r USING (id)",
        );
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn cross_join_with_comma_from() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT p.name, o.order_id FROM people p, orders o WHERE p.id = 1",
        );
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn aggregate_sum_over_column() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT SUM(amount) AS total FROM orders");
        assert_eq!(result.rows[0][0], Value::Float(170.4));
    }

    #[test]
    fn distinct_limit_offset() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT DISTINCT city FROM people LIMIT 2");
        assert_eq!(result.rows.len(), 2);
        let result = run(&mut schema, "SELECT name FROM people ORDER BY id LIMIT 2 OFFSET 2");
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn scalar_expressions_in_select_without_from() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT 1 + 2 AS three, LOWER('ABC') AS low");
        assert_eq!(result.rows[0][0], Value::Int(3));
        assert_eq!(result.rows[0][1], Value::Text("abc".to_string()));
    }

    #[test]
    fn left_right_instr_string_functions() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT LEFT('Hello', 2) AS l, RIGHT('Hello', 2) AS r, \
             INSTR('Hello', 'll') AS pos, INSTR('Hello', 'zz') AS nf",
        );
        assert_eq!(result.rows[0][0], Value::Text("He".to_string()));
        assert_eq!(result.rows[0][1], Value::Text("lo".to_string()));
        assert_eq!(result.rows[0][2], Value::Int(3));
        assert_eq!(result.rows[0][3], Value::Int(0));
    }

    #[test]
    fn now_and_date_functions() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT NOW() AS n, DATE() AS d, DATE('2026/08/14 10:30:00') AS p",
        );
        let now = result.rows[0][0].to_display_string();
        assert_eq!(now.len(), 19, "got: {now}");
        let today = result.rows[0][1].to_display_string();
        assert_eq!(today.len(), 10, "got: {today}");
        assert_eq!(result.rows[0][2], Value::Text("2026-08-14".to_string()));
    }

    #[test]
    fn power_sqrt_math_functions() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT POWER(2, 10) AS p, SQRT(16) AS s");
        assert_eq!(result.rows[0][0], Value::Float(1024.0));
        assert_eq!(result.rows[0][1], Value::Float(4.0));
    }

    #[test]
    fn greatest_least_functions() {
        let mut schema = make_schema();
        let result = run(&mut schema, "SELECT GREATEST(3, 7, 5) AS g, LEAST(3, 7, 5) AS l");
        assert_eq!(result.rows[0][0], Value::Int(7));
        assert_eq!(result.rows[0][1], Value::Int(3));
    }

    #[test]
    fn case_in_projection() {
        let mut schema = make_schema();
        let result = run(
            &mut schema,
            "SELECT name, CASE WHEN age >= 35 THEN 'senior' ELSE 'junior' END AS tier FROM people",
        );
        assert_eq!(result.rows[2][1], Value::Text("senior".into()));
        assert_eq!(result.rows[0][1], Value::Text("junior".into()));
    }

    #[test]
    fn unknown_table_returns_error() {
        let mut schema = make_schema();
        assert!(run_query(&mut schema, "SELECT * FROM missing").is_err());
    }

    #[test]
    fn malformed_sql_returns_error() {
        let mut schema = make_schema();
        assert!(run_query(&mut schema, "SELECT FROM").is_err());
    }
}