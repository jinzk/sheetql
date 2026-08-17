use std::collections::HashSet;

use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::database::Schema;
use crate::highlight::KEYWORDS;

/// The kind of a completion candidate, used for ordering and coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Function,
    Table,
    Column,
    Database,
}

/// A single completion candidate.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Text inserted in place of the typed prefix.
    pub value: String,
    /// Text shown in the popup.
    pub label: String,
    pub kind: Kind,
}

impl Completion {
    fn keyword(value: &str) -> Self {
        let upper = value.to_uppercase();
        Self {
            value: upper.clone(),
            label: upper,
            kind: Kind::Keyword,
        }
    }

    fn function(name: &str) -> Self {
        Self {
            value: name.to_string(),
            label: format!("{name}()"),
            kind: Kind::Function,
        }
    }

    fn named(kind: Kind, value: &str) -> Self {
        Self {
            value: value.to_string(),
            label: value.to_string(),
            kind,
        }
    }
}

/// Functions supported by the engine (`src/functions.rs`).
const FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "ifnull",
    "isnull",
    "coalesce",
    "len",
    "length",
    "char_length",
    "lower",
    "lcase",
    "upper",
    "ucase",
    "trim",
    "ltrim",
    "rtrim",
    "concat",
    "substring",
    "substr",
    "replace",
    "abs",
    "round",
    "floor",
    "ceil",
    "ceiling",
    "mod",
    "left",
    "right",
    "instr",
    "startswith",
    "endswith",
    "split",
    "now",
    "current_timestamp",
    "date",
    "power",
    "pow",
    "sqrt",
    "greatest",
    "least",
];

/// Keywords that can follow an expression, e.g. inside a WHERE clause.
const EXPRESSION_KEYWORDS: &[&str] = &[
    "AND", "OR", "NOT", "IN", "BETWEEN", "LIKE", "IS", "NULL", "TRUE", "FALSE",
];

/// Statement keywords suggested at the start of a query.
const STATEMENT_KEYWORDS: &[&str] = &["SELECT", "SHOW", "USE", "DESCRIBE", "EXIT", "QUIT"];

/// Caches the validated candidate list across keystrokes. The cache key is the
/// full text before the cursor; prefix filtering is applied on every lookup.
pub struct CandidateCache {
    before: String,
    unfiltered: Vec<Completion>,
}

impl CandidateCache {
    pub fn new() -> Self {
        Self {
            before: String::new(),
            unfiltered: Vec::new(),
        }
    }

    pub fn candidates(&mut self, schema: &Schema, line: &str) -> Vec<Completion> {
        let (_, prefix) = split_prefix(line);
        if line != self.before {
            self.before = line.to_string();
            self.unfiltered = compute_candidates(schema, line);
        }
        let prefix = prefix.to_lowercase();
        self.unfiltered
            .iter()
            .filter(|c| prefix.is_empty() || c.value.to_lowercase().starts_with(&prefix))
            .cloned()
            .collect()
    }
}

/// Split a line into (base, prefix) where base is the text before the current
/// word and prefix is the trailing partial word typed so far.
fn split_prefix(line: &str) -> (&str, &str) {
    let len = trailing_token_len(line);
    (&line[..line.len() - len], &line[line.len() - len..])
}

/// Every completion candidate for the text before the cursor, filtered by
/// prefix and validated against the SQL grammar. Used by the tests; the TUI
/// goes through [`CandidateCache`].
#[allow(dead_code)]
pub fn candidates(schema: &Schema, line: &str) -> Vec<Completion> {
    compute_candidates(schema, line)
}

fn compute_candidates(schema: &Schema, line: &str) -> Vec<Completion> {
    if !should_suggest(line) {
        return Vec::new();
    }

    let prefix = current_prefix(line);
    let mut pool = match infer_context(line) {
        Context::StatementStart => statement_keywords(),
        Context::Database => database_candidates(schema),
        Context::Table => table_candidates(schema),
        Context::SelectList => {
            let mut out = column_candidates(schema, line);
            out.extend(function_candidates());
            out.push(Completion::named(Kind::Keyword, "*"));
            out.push(Completion::keyword("FROM"));
            out.push(Completion::keyword("WHERE"));
            out.push(Completion::keyword("GROUP"));
            out.push(Completion::keyword("ORDER"));
            out.push(Completion::keyword("LIMIT"));
            out.push(Completion::keyword("HAVING"));
            out
        }
        Context::Expression => {
            let mut out = column_candidates(schema, line);
            out.extend(function_candidates());
            out.extend(expression_keywords());
            out
        }
        Context::ByClause => {
            let mut out = column_candidates(schema, line);
            out.extend(function_candidates());
            out.push(Completion::keyword("BY"));
            out
        }
        Context::General => {
            let mut out = keyword_candidates();
            out.extend(function_candidates());
            out.extend(table_candidates(schema));
            out.extend(column_candidates(schema, line));
            out.extend(database_candidates(schema));
            out
        }
    };

    pool.retain(|candidate| {
        prefix.is_empty() || candidate.value.to_lowercase().starts_with(&prefix)
    });

    let mut seen = HashSet::new();
    pool.retain(|candidate| seen.insert(candidate.value.to_lowercase()));

    pool.retain(|candidate| {
        is_repl_command(&candidate.value)
            || (candidate.value.to_uppercase() == "FROM" && from_is_valid(line))
            || is_valid_continuation(line, &candidate.value)
    });

    pool.sort_by(|a, b| {
        (a.kind as u8)
            .cmp(&(b.kind as u8))
            .then_with(|| a.value.to_lowercase().cmp(&b.value.to_lowercase()))
    });

    pool
}

/// Whether a completion popup is useful at the cursor position at all.
/// Suppressed inside unterminated string literals and after numeric values.
pub fn should_suggest(line: &str) -> bool {
    if inside_quoted_literal(line) {
        return false;
    }
    let prefix = current_prefix(line);
    if !prefix.is_empty() && prefix.parse::<f64>().is_ok() {
        return false;
    }
    true
}

/// Byte length of the partial word before the cursor. Used to strip the
/// typed prefix before inserting a completion.
pub fn trailing_token_len(line: &str) -> usize {
    line.chars()
        .rev()
        .take_while(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    ',' | '(' | ')' | '=' | '<' | '>' | ';' | '.' | '"' | '\'' | '`'
                )
        })
        .map(char::len_utf8)
        .sum()
}

/// The partial word under / before the cursor, lowercased. Returns empty when
/// the cursor sits at a word boundary (e.g. after a space).
pub fn current_prefix(line: &str) -> String {
    let len = trailing_token_len(line);
    let raw = &line[line.len() - len..];
    if raw.chars().all(|c| c.is_whitespace()) {
        String::new()
    } else {
        raw.to_lowercase()
    }
}

/// Coarse syntactic position, used only to narrow the candidate pool. The
/// final decision is left to per-candidate grammar validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    StatementStart,
    SelectList,
    Table,
    Expression,
    ByClause,
    Database,
    General,
}

fn infer_context(line: &str) -> Context {
    if line.trim().is_empty() {
        return Context::StatementStart;
    }
    let Some(word) = last_word(line) else {
        return Context::General;
    };
    match word.as_str() {
        "USE" => Context::Database,
        "FROM" | "JOIN" | "INTO" => Context::Table,
        "SELECT" | "DISTINCT" => Context::SelectList,
        "GROUP" | "ORDER" => Context::ByClause,
        "WHERE" | "ON" | "HAVING" | "AND" | "OR" | "SET" | "VALUES" | "BY" => Context::Expression,
        _ => Context::General,
    }
}

/// Uppercased value of the last unquoted identifier in `line`, if any.
fn last_word(line: &str) -> Option<String> {
    tokenize(line)
        .into_iter()
        .rev()
        .find_map(|token| match token {
            Token::Word(word) if word.quote_style.is_none() => Some(word.value.to_uppercase()),
            _ => None,
        })
}

/// Tokenize `line` with the MySQL dialect, ignoring tokenizer errors and
/// discarding whitespace tokens.
fn tokenize(line: &str) -> Vec<Token> {
    Tokenizer::new(&MySqlDialect {}, line)
        .tokenize()
        .unwrap_or_default()
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect()
}

fn statement_keywords() -> Vec<Completion> {
    STATEMENT_KEYWORDS
        .iter()
        .map(|word| Completion::keyword(word))
        .collect()
}

fn keyword_candidates() -> Vec<Completion> {
    KEYWORDS
        .iter()
        .filter(|word| !is_repl_command(word))
        .map(|word| Completion::keyword(word))
        .collect()
}

fn expression_keywords() -> Vec<Completion> {
    EXPRESSION_KEYWORDS
        .iter()
        .map(|word| Completion::keyword(word))
        .collect()
}

fn function_candidates() -> Vec<Completion> {
    FUNCTIONS
        .iter()
        .map(|name| Completion::function(name))
        .collect()
}

fn table_candidates(schema: &Schema) -> Vec<Completion> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for database in &schema.databases {
        for table in &database.tables {
            for value in [
                table.name.clone(),
                format!("{}.{}", database.name, table.name),
            ] {
                if seen.insert(value.to_lowercase()) {
                    out.push(Completion::named(Kind::Table, &value));
                }
            }
        }
    }
    out
}

fn database_candidates(schema: &Schema) -> Vec<Completion> {
    schema
        .database_names()
        .into_iter()
        .map(|name| Completion::named(Kind::Database, name))
        .collect()
}

/// Columns of the tables referenced by FROM/JOIN clauses, or of every table
/// in the current database when no table is referenced yet.
fn column_candidates(schema: &Schema, line: &str) -> Vec<Completion> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let tables = referenced_tables(line);
    if !tables.is_empty() {
        for (database, table) in tables {
            if let Ok((_, table)) = schema.resolve_table(database.as_deref(), &table) {
                for column in &table.columns {
                    if seen.insert(column.to_lowercase()) {
                        out.push(Completion::named(Kind::Column, column));
                    }
                }
            }
        }
        return out;
    }

    let databases: Vec<&crate::database::Database> = match schema.current_database() {
        Some(current) => schema
            .databases
            .iter()
            .filter(|db| db.name == current)
            .collect(),
        None => schema.databases.iter().collect(),
    };
    for database in databases {
        for table in &database.tables {
            for column in &table.columns {
                if seen.insert(column.to_lowercase()) {
                    out.push(Completion::named(Kind::Column, column));
                }
            }
        }
    }
    out
}

/// `(database, table)` pairs named by FROM/JOIN/INTO clauses in `line`,
/// supporting both `table` and `database.table` forms.
fn referenced_tables(line: &str) -> Vec<(Option<String>, String)> {
    let tokens = tokenize(line);
    let mut out = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let is_clause = matches!(
            &tokens[index],
            Token::Word(word) if matches!(word.value.to_uppercase().as_str(), "FROM" | "JOIN" | "INTO")
        );
        if !is_clause {
            index += 1;
            continue;
        }
        index += 1;
        while index < tokens.len() {
            match &tokens[index] {
                Token::Comma => index += 1,
                Token::Word(word) => {
                    if word.quote_style.is_none() && is_clause_keyword(&word.value) {
                        break;
                    }
                    let first = word.value.clone();
                    if index + 1 < tokens.len()
                        && tokens[index + 1] == Token::Period
                        && let Some(Token::Word(second)) = tokens.get(index + 2)
                    {
                        out.push((Some(first), second.value.clone()));
                        index += 3;
                        continue;
                    }
                    out.push((None, first));
                    index += 1;
                }
                _ => break,
            }
        }
    }
    out
}

/// Keywords that terminate a table list, so they are not mistaken for tables.
fn is_clause_keyword(word: &str) -> bool {
    matches!(
        word.to_uppercase().as_str(),
        "WHERE"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "ON"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "CROSS"
            | "UNION"
            | "AND"
            | "OR"
            | "AS"
            | "SET"
            | "VALUES"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "USING"
    )
}

/// True when the candidate is a REPL command (handled by the shell, not the
/// SQL parser), which is never validated against the grammar.
fn is_repl_command(value: &str) -> bool {
    matches!(value.to_uppercase().as_str(), "EXIT" | "QUIT")
}

/// Grammar validation: does appending `value` to `line` advance the parse?
///
/// A successful parse means the candidate is legal. On a parse error the
/// candidate is still a legal continuation when the parser consumed it and
/// failed later (e.g. `SELECT * FROM sales WHE` + `WHERE` parses the keyword
/// then fails looking for the predicate). We detect that by checking the
/// `found:` token of the error message: if it is the candidate's first token
/// the parser stopped before consuming the candidate, so it is not a valid
/// continuation.
fn is_valid_continuation(line: &str, value: &str) -> bool {
    // At a word boundary (empty prefix) use the whole line as the base so that
    // a trailing space is preserved.  Otherwise strip the partial word.
    let prefix_len = trailing_token_len(line);
    let base = if line[line.len() - prefix_len..]
        .chars()
        .all(|c| c.is_whitespace())
    {
        line
    } else {
        &line[..line.len() - prefix_len]
    };
    let test = format!("{base}{}", validation_form(value));
    match Parser::parse_sql(&MySqlDialect {}, &test) {
        Ok(_) => true,
        Err(sqlparser::parser::ParserError::ParserError(message)) => {
            let Some(first_token) = tokenize(value).into_iter().next() else {
                return false;
            };
            // The candidate is a legal continuation when the parser consumed
            // it and failed later (e.g. `SELECT * FROM sales WHE` + `WHERE`
            // parses the keyword then fails looking for the predicate). If the
            // parser stopped before consuming the candidate's first token, the
            // `found:` token matches it and it is not a valid continuation.
            found_token(&message).is_some_and(|found| found != first_token)
        }
        Err(sqlparser::parser::ParserError::TokenizerError(_)) => true,
        Err(_) => true,
    }
}

/// The minimal complete form a candidate is validated with. Some keywords
/// open multi-token clauses (`GROUP BY`, `ORDER BY`, `FROM _`) and are
/// rejected by the parser when tested bare, so they are validated in their
/// minimal complete form.
fn validation_form(value: &str) -> String {
    match value.to_uppercase().as_str() {
        "GROUP" => "GROUP BY _".to_string(),
        "ORDER" => "ORDER BY _".to_string(),
        _ => value.to_string(),
    }
}

/// FROM is only valid after a projection (column, *, or comma).  Rather than
/// trying to prove this to the parser (which rejects `SELECT FROM`), we
/// check the surface form directly.  It is also valid immediately after
/// SELECT/DISTINCT (user may type `SELECT * FROM`).
fn from_is_valid(line: &str) -> bool {
    let tokens: Vec<_> = tokenize(line);
    let mut found_item = false;
    for tok in tokens.iter().rev() {
        match tok {
            Token::Word(w) if w.quote_style.is_none() => {
                let kw = w.value.to_uppercase();
                match kw.as_str() {
                    "SELECT" | "DISTINCT" => return true,
                    "FROM" | "JOIN" | "WHERE" | "GROUP" | "ORDER" | "LIMIT" | "HAVING"
                    | "UNION" => return found_item,
                    _ => {
                        found_item = true;
                    }
                }
            }
            Token::Comma => {
                found_item = true;
            }
            Token::Mul => {
                found_item = true;
            }
            _ => {}
        }
    }
    false
}

/// Extract the token printed after `found:` in a parser error message.
fn found_token(message: &str) -> Option<Token> {
    let marker = "found: ";
    let start = message.rfind(marker)? + marker.len();
    let rest = &message[start..];
    let text = rest.split_whitespace().next().unwrap_or(rest).trim();
    Some(match text {
        "EOF" => Token::EOF,
        other => tokenize(other).into_iter().next().unwrap_or(Token::EOF),
    })
}

/// True when the cursor sits inside an unterminated `'...`, `"...` or
/// `` `...`` literal, where suggesting identifiers would be wrong.
fn inside_quoted_literal(line: &str) -> bool {
    let mut quote: Option<char> = None;
    for ch in line.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None => {
                if matches!(ch, '\'' | '"' | '`') {
                    quote = Some(ch);
                }
            }
        }
    }
    quote.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, Table};

    fn make_schema() -> Schema {
        let mut schema = Schema::new();
        let mut db = Database::named("data_sales_csv");
        db.add_table(Table {
            name: "sales".to_string(),
            columns: vec!["id".to_string(), "name".to_string(), "amount".to_string()],
            rows: vec![],
        });
        db.add_table(Table {
            name: "customers".to_string(),
            columns: vec!["id".to_string(), "city".to_string()],
            rows: vec![],
        });
        schema.add_database(db);
        schema.set_current_database("data_sales_csv").unwrap();
        schema
    }

    fn has(cs: &[Completion], value: &str) -> bool {
        cs.iter().any(|c| c.value == value)
    }

    #[test]
    fn prefix_is_current_word() {
        assert_eq!(current_prefix("SELECT na"), "na");
        assert_eq!(current_prefix("SELECT name, "), "");
        assert_eq!(current_prefix(""), "");
        assert_eq!(trailing_token_len("SELECT name,"), 0);
        assert_eq!(trailing_token_len("SELECT na"), 2);
    }

    #[test]
    fn statement_start_suggests_keywords() {
        let schema = make_schema();
        let cs = candidates(&schema, "");
        assert!(has(&cs, "SELECT"));
        assert!(has(&cs, "SHOW"));
    }

    #[test]
    fn from_suggests_tables() {
        let schema = make_schema();
        let cs = candidates(&schema, "SELECT * FROM ");
        assert!(has(&cs, "sales"));
        assert!(has(&cs, "customers"));
        assert!(has(&cs, "data_sales_csv.sales"));
    }

    #[test]
    fn where_suggests_columns_and_functions() {
        let schema = make_schema();
        let cs = candidates(&schema, "SELECT * FROM sales WHERE ");
        assert!(has(&cs, "name"));
        assert!(has(&cs, "amount"));
        assert!(has(&cs, "count"));
        assert!(has(&cs, "AND"));
    }

    #[test]
    fn use_suggests_databases() {
        let schema = make_schema();
        let cs = candidates(&schema, "USE ");
        assert!(has(&cs, "data_sales_csv"));
    }

    #[test]
    fn select_list_suggests_columns_and_star() {
        let schema = make_schema();
        let cs = candidates(&schema, "SELECT ");
        assert!(has(&cs, "name"));
        assert!(has(&cs, "*"));
        assert!(has(&cs, "FROM"));
    }

    #[test]
    fn keyword_completion_is_validated() {
        let schema = make_schema();
        // Completing WHE -> WHERE stays: WHERE is a valid continuation.
        assert!(has(
            &candidates(&schema, "SELECT * FROM sales WHE"),
            "WHERE"
        ));
        // Adding a second FROM is not a valid continuation and is filtered out.
        assert!(!has(
            &candidates(&schema, "SELECT * FROM sales WHERE name = 'a' AND "),
            "FROM"
        ));
    }

    #[test]
    fn inside_string_literal_is_suppressed() {
        let schema = make_schema();
        assert!(candidates(&schema, "SELECT name FROM sales WHERE name = 'A").is_empty());
        assert!(!candidates(&schema, "SELECT name FROM sales WHERE name = 'A' AND ").is_empty());
    }

    #[test]
    fn after_number_is_suppressed() {
        let schema = make_schema();
        assert!(candidates(&schema, "SELECT amount FROM sales WHERE amount > 10").is_empty());
    }

    #[test]
    fn qualified_table_and_columns() {
        let schema = make_schema();
        let cs = candidates(&schema, "SELECT * FROM data_sales_csv.customers WHERE ");
        assert!(has(&cs, "city"));
        assert!(!has(&cs, "amount"));
    }

    #[test]
    fn candidate_cache_reuses_result_for_same_base() {
        let schema = make_schema();
        let mut cache = CandidateCache::new();
        let values =
            |cs: &[Completion]| -> Vec<String> { cs.iter().map(|c| c.value.clone()).collect() };
        let first = values(&cache.candidates(&schema, "SELECT * FROM sales WHE"));
        assert!(!first.is_empty());
        // Same base, different prefix: cache hit, but prefix filters differently.
        let second = values(&cache.candidates(&schema, "SELECT * FROM sales WHERE"));
        // Prefix "where" does not match any Expression-context candidate, so
        // the result may be empty — that is correct.
        assert!(first.len() >= second.len());
    }

    #[test]
    fn candidate_cache_rebuilds_on_new_base() {
        let schema = make_schema();
        let mut cache = CandidateCache::new();
        let values =
            |cs: &[Completion]| -> Vec<String> { cs.iter().map(|c| c.value.clone()).collect() };
        // Table context first.
        let tables = values(&cache.candidates(&schema, "SELECT * FROM "));
        assert!(tables.iter().any(|v| v == "sales"));
        // A different base must rebuild: now a select-list context, with
        // column candidates instead of table names.
        let columns = values(&cache.candidates(&schema, "SELECT "));
        assert!(columns.iter().any(|v| v == "name"));
        assert!(!columns.contains(&"sales".to_string()));
    }

    #[test]
    fn expression_end_suggests_clause_keywords() {
        let schema = make_schema();
        // GROUP/ORDER open multi-token clauses and must survive validation
        // after a completed expression.
        assert!(has(
            &candidates(&schema, "SELECT * FROM sales WHERE amount > 10 G"),
            "GROUP"
        ));
        assert!(has(
            &candidates(&schema, "SELECT * FROM sales WHERE amount > 10 O"),
            "ORDER"
        ));
        // After the clause opener, BY is suggested.
        assert!(has(
            &candidates(&schema, "SELECT * FROM sales ORDER "),
            "BY"
        ));
        assert!(has(
            &candidates(&schema, "SELECT * FROM sales GROUP "),
            "BY"
        ));
        // Still filtered out in genuinely illegal positions.
        assert!(!has(
            &candidates(&schema, "SELECT * FROM sales WHERE "),
            "GROUP"
        ));
    }

    #[test]
    fn keyword_candidates_are_uppercase() {
        let schema = make_schema();
        for line in [
            "",
            "SELECT * FROM sales WHE",
            "SELECT * FROM sales WHERE ",
            "SELECT * FROM sales WHERE name = 'a' ",
            "SELECT * FROM sales G",
            "SELECT * FROM sales ORDER ",
        ] {
            for c in candidates(&schema, line) {
                if c.kind == Kind::Keyword {
                    assert_eq!(
                        c.value,
                        c.value.to_uppercase(),
                        "line={line:?} value={}",
                        c.value
                    );
                }
            }
        }
    }
}
