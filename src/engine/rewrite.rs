use std::collections::HashMap;
use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Value as SqlValue, ValueWithSpan, VisitMut, VisitorMut};

use crate::engine::select::ProjectionItem;

/// How an identifier that names both a source column and an output alias is
/// resolved. MySQL prefers the source column in `GROUP BY`/`HAVING` but the
/// output alias in `ORDER BY`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum AliasPrecedence {
    /// Substitute whenever the identifier names an alias (`ORDER BY`).
    AliasFirst,
    /// Substitute only when the identifier matches no source column
    /// (`GROUP BY`, `HAVING`).
    SourceFirst,
}

/// Rewrites expressions before evaluation:
/// - lowercases identifier spellings so per-row column resolution hits the
///   (lowercase) lookup directly instead of allocating on every row;
/// - optionally substitutes select-list aliases with their underlying
///   expression, enabling `HAVING cnt > 1` and `ORDER BY cnt + 1`.
pub(crate) struct ExprRewriter<'a> {
    aliases: Option<&'a HashMap<String, &'a Expr>>,
    precedence: AliasPrecedence,
    known_columns: Option<&'a HashMap<String, usize>>,
}

impl<'a> ExprRewriter<'a> {
    pub(crate) fn lowercase() -> Self {
        Self {
            aliases: None,
            precedence: AliasPrecedence::AliasFirst,
            known_columns: None,
        }
    }

    /// Rewrite with alias substitution at the given precedence. `SourceFirst`
    /// needs the column lookup to decide whether a name is a real column.
    pub(crate) fn with_aliases(
        aliases: &'a HashMap<String, &'a Expr>,
        precedence: AliasPrecedence,
        known_columns: Option<&'a HashMap<String, usize>>,
    ) -> Self {
        Self {
            aliases: Some(aliases),
            precedence,
            known_columns,
        }
    }

    /// Convenience wrapper returning an owned rewritten copy.
    pub(crate) fn rewritten(&self, expr: &Expr) -> Expr {
        let mut owned = expr.clone();
        let mut visitor = ExprRewriter {
            aliases: self.aliases,
            precedence: self.precedence,
            known_columns: self.known_columns,
        };
        let _ = owned.visit(&mut visitor);
        owned
    }

    fn replacement_for(&self, name: &str) -> Option<&'a Expr> {
        let aliases = self.aliases?;
        let lowered = name.to_lowercase();
        if self.precedence == AliasPrecedence::SourceFirst
            && let Some(columns) = self.known_columns
            && columns.contains_key(&lowered)
        {
            return None;
        }
        aliases.get(&lowered).copied()
    }
}

impl<'a> VisitorMut for ExprRewriter<'a> {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        // Alias substitution first: a bare (or fully qualified) identifier
        // that names an output alias is replaced by the aliased expression.
        // The replacement itself keeps being traversed afterwards, so any
        // identifiers inside it are still case-normalized below.
        let replacement = match expr {
            Expr::Identifier(ident) => self.replacement_for(&ident.value),
            Expr::CompoundIdentifier(parts) => {
                let joined = parts
                    .iter()
                    .map(|part| part.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                self.replacement_for(&joined)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            *expr = replacement.clone();
        }

        match expr {
            Expr::Identifier(ident) => ident.value = ident.value.to_lowercase(),
            Expr::CompoundIdentifier(parts) => {
                for part in parts {
                    part.value = part.value.to_lowercase();
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

/// True when `expr` is a bare positive integer literal such as `1`, used by
/// MySQL-style ordinal references (`GROUP BY 1`, `ORDER BY 2 DESC`).
pub(crate) fn ordinal_literal(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Value(ValueWithSpan {
            value: SqlValue::Number(text, _),
            ..
        }) => text.parse::<usize>().ok().filter(|ordinal| *ordinal >= 1),
        _ => None,
    }
}

/// Build the alias map used for substitution: lowercase title -> projection
/// expression. Only expression items qualify; plain column projections keep
/// resolving through the source schema as usual.
pub(crate) fn alias_map(plan: &[ProjectionItem]) -> HashMap<String, &Expr> {
    let mut aliases = HashMap::new();
    for item in plan {
        if let ProjectionItem::Expression { expr, title } = item {
            aliases.insert(title.to_lowercase(), expr.as_ref());
        }
    }
    aliases
}
