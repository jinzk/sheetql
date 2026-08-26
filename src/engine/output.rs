use crate::engine::QueryResult;
use crate::error::Error;
use crate::printer::{OutputFormat, render};

pub(crate) fn strip_into_outfile(sql: &str) -> (String, Option<String>) {
    let marker = "INTO OUTFILE";
    let Some(pos) = find_outfile_pos(sql, marker) else {
        return (sql.to_string(), None);
    };

    let tail = &sql[pos + marker.len()..];
    let mut iter = tail.char_indices().peekable();
    while let Some(&(_, c)) = iter.peek() {
        if c.is_whitespace() {
            iter.next();
        } else {
            break;
        }
    }

    let quote = match iter.peek() {
        Some(&(_, '\'')) => '\'',
        Some(&(_, '"')) => '"',
        _ => return (sql.to_string(), None),
    };
    iter.next();

    let mut path = String::new();
    let mut end_byte = 0;
    let mut closed = false;
    while let Some((byte, c)) = iter.next() {
        if c == quote {
            if let Some(&(_, next)) = iter.peek()
                && next == quote
            {
                path.push(quote);
                iter.next();
                continue;
            }
            end_byte = byte + c.len_utf8();
            closed = true;
            break;
        }
        path.push(c);
    }
    if !closed {
        return (sql.to_string(), None);
    }

    let mut rest = String::with_capacity(pos + tail.len() - end_byte);
    rest.push_str(sql[..pos].trim_end());
    rest.push_str(tail[end_byte..].trim_start());
    (rest.trim().to_string(), Some(path))
}

fn find_outfile_pos(sql: &str, marker: &str) -> Option<usize> {
    let upper = sql.to_ascii_uppercase();
    let mut iter = sql.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        match c {
            '\'' | '"' | '`' => {
                let quote = c;
                while let Some((_, next)) = iter.next() {
                    if next == quote {
                        if let Some(&(_, peeked)) = iter.peek()
                            && peeked == quote
                        {
                            iter.next();
                            continue;
                        }
                        break;
                    }
                }
            }
            '-' if iter.peek().is_some_and(|&(_, next)| next == '-') => {
                for (_, ch) in iter.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            '/' if iter
                .peek()
                .is_some_and(|&(_, next)| next == '*' || next == '/') =>
            {
                // Skip both line and block comments; text inside them must
                // never trigger the OUTFILE scan.
                if iter.peek().is_some_and(|&(_, next)| next == '/') {
                    iter.next();
                    for (_, ch) in iter.by_ref() {
                        if ch == '\n' {
                            break;
                        }
                    }
                } else {
                    iter.next();
                    while let Some((_, ch)) = iter.next() {
                        if ch == '*' && iter.peek().is_some_and(|&(_, next)| next == '/') {
                            iter.next();
                            break;
                        }
                    }
                }
            }
            _ => {
                if upper[i..].starts_with(marker) {
                    let prev = sql[..i].chars().next_back();
                    if !prev.is_some_and(|p| p.is_alphanumeric() || p == '_') {
                        return Some(i);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn write_outfile(path: &str, result: &QueryResult) -> Result<(), Error> {
    if std::path::Path::new(path).exists() {
        return Err(format!("Output file `{path}` already exists").into());
    }
    let content = render(OutputFormat::Csv, &result.columns, &result.rows);
    std::fs::write(path, content).map_err(|error| format!("Cannot write to `{path}`: {error}"))?;
    Ok(())
}
