use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// SQL keywords highlighted in the interactive input and history cells.
pub const KEYWORDS: &[&str] = &[
    "add", "all", "alter", "and", "as", "asc", "avg", "between", "by", "case",
    "cast", "column", "count", "create", "delete", "desc", "describe", "distinct",
    "drop", "else", "end", "from", "full", "group", "having", "in", "index",
    "inner", "insert", "into", "is", "join", "left", "like", "limit", "max",
    "min", "not", "null", "on", "or", "order", "outer", "replace", "right",
    "select", "set", "show", "sum", "table", "then", "true", "false", "union",
    "unique", "update", "use", "using", "values", "view", "when", "where", "with",
];

/// Split `sql` into styled spans, coloring keywords, string literals, numbers
/// and line comments. Returns one `Line` per input line.
pub fn highlight(sql: &str) -> Vec<Line<'static>> {
    sql.lines().map(highlight_line).collect()
}

/// Highlight a single line of SQL text.
fn highlight_line(line: &str) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Line comment: everything until end of line.
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(
                rest,
                Style::default().fg(Color::DarkGray),
            ));
            break;
        }

        // String or quoted identifier literal.
        if matches!(c, '\'' | '"' | '`') {
            let (text, next) = scan_string(&chars, i);
            spans.push(Span::styled(text, Style::default().fg(Color::Green)));
            i = next;
            continue;
        }

        // Number literal (integers, decimals, and leading-dot forms).
        if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()))
        {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == '.') {
                j += 1;
            }
            let text: String = chars[i..j].iter().collect();
            spans.push(Span::styled(text, Style::default().fg(Color::Yellow)));
            i = j;
            continue;
        }

        // Identifier or keyword.
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let span = if is_keyword(&word) {
                Span::styled(word, Style::default().fg(Color::Cyan))
            } else {
                Span::raw(word)
            };
            spans.push(span);
            i = j;
            continue;
        }

        // Any other single character (operators, punctuation, whitespace).
        spans.push(Span::raw(c.to_string()));
        i += 1;
    }
    Line::from(spans)
}

/// Case-insensitive keyword check on a bare word.
fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|keyword| keyword.eq_ignore_ascii_case(word))
}

/// Scan a quoted literal starting at `start`. Handles doubled quotes as
/// escapes (`''`, `""`, ``` `` ```) and returns the literal text together
/// with the index just past its closing quote.
fn scan_string(chars: &[char], start: usize) -> (String, usize) {
    let quote = chars[start];
    let mut j = start + 1;
    while j < chars.len() {
        if chars[j] == quote {
            if chars.get(j + 1) == Some(&quote) {
                j += 2;
                continue;
            }
            j += 1;
            break;
        }
        j += 1;
    }
    let text: String = chars[start..j].iter().collect();
    (text, j)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    /// Return the (text, foreground color) pairs of a highlighted line.
    fn spans(line: &Line) -> Vec<(String, Option<Color>)> {
        line.spans
            .iter()
            .map(|span: &Span| {
                let text = span.content.to_string();
                let color = span.style.fg;
                (text, color)
            })
            .collect()
    }

    /// Look up the foreground color of the span whose text equals `text`.
    fn color_of(spans: &[(String, Option<Color>)], text: &str) -> Option<Color> {
        spans
            .iter()
            .find(|(span_text, _)| span_text == text)
            .and_then(|(_, color)| *color)
    }

    #[test]
    fn keywords_are_cyan() {
        let lines = highlight("SELECT name FROM people");
        let colored = spans(&lines[0]);
        assert_eq!(color_of(&colored, "SELECT"), Some(Color::Cyan));
        assert_eq!(color_of(&colored, "FROM"), Some(Color::Cyan));
        assert_eq!(color_of(&colored, "name"), None);
        assert_eq!(color_of(&colored, "people"), None);
    }

    #[test]
    fn keywords_are_case_insensitive() {
        let lines = highlight("select name from people");
        let colored = spans(&lines[0]);
        assert_eq!(color_of(&colored, "select"), Some(Color::Cyan));
        assert_eq!(color_of(&colored, "from"), Some(Color::Cyan));
    }

    #[test]
    fn string_content_is_not_highlighted_as_keyword() {
        let lines = highlight("SELECT 'select from' FROM people");
        let colored = spans(&lines[0]);
        assert_eq!(color_of(&colored, "'select from'"), Some(Color::Green));
        assert_eq!(color_of(&colored, "FROM"), Some(Color::Cyan));
    }

    #[test]
    fn doubled_quote_is_an_escape() {
        let lines = highlight("SELECT 'it''s'");
        let colored = spans(&lines[0]);
        assert_eq!(color_of(&colored, "'it''s'"), Some(Color::Green));
    }

    #[test]
    fn numbers_are_yellow() {
        let lines = highlight("SELECT 3.14, .5, 100");
        let colored = spans(&lines[0]);
        assert!(color_of(&colored, "3.14") == Some(Color::Yellow));
        assert!(color_of(&colored, ".5") == Some(Color::Yellow));
        assert!(color_of(&colored, "100") == Some(Color::Yellow));
    }

    #[test]
    fn comment_rest_of_line_is_gray() {
        let lines = highlight("SELECT * FROM people -- all rows");
        let colored = spans(&lines[0]);
        let last = colored.last().unwrap();
        assert_eq!(last.0, "-- all rows");
        assert_eq!(last.1, Some(Color::DarkGray));
    }

    #[test]
    fn multi_line_input_keeps_line_count() {
        let lines = highlight("SELECT 1\nFROM people");
        assert_eq!(lines.len(), 2);
    }
}