use crate::error::Error;
use crate::value::Value;

/// Match `text` against a `LIKE` pattern (`%` any run, `_` one character),
/// optionally case-insensitively and with an escape character.
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

/// Apply a `LIKE` pattern to two evaluated `Value`s, treating a NULL operand
/// as a non-match for the purpose of the boolean result.
pub(crate) fn like_values(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_match_trailing_escape_is_ignored() {
        assert!(like_match("abc", "abc!", false, Some('!')));
        assert!(!like_match("abcd", "abc!", false, Some('!')));
    }

    #[test]
    fn like_match_escape_can_quote_wildcards() {
        // `!%` and `!_` match the literal wildcard characters.
        assert!(like_match("a%b", "a!%b", false, Some('!')));
        assert!(like_match("a_b", "a!_b", false, Some('!')));
        assert!(!like_match("axb", "a!%b", false, Some('!')));
        assert!(!like_match("axb", "a!_b", false, Some('!')));
    }

    #[test]
    fn like_match_double_escape_quotes_the_escape() {
        assert!(like_match("a!b", "a!!b", false, Some('!')));
        assert!(!like_match("axb", "a!!b", false, Some('!')));
    }

    #[test]
    fn like_match_underscore_matches_one_unicode_character() {
        assert!(like_match("aé", "a_", false, None));
        assert!(like_match("日本", "__", false, None));
        assert!(!like_match("日本語", "__", false, None));
    }

    #[test]
    fn like_match_percent_matches_empty_and_wildcards_only() {
        assert!(like_match("", "%", false, None));
        assert!(like_match("ab", "%", false, None));
        assert!(like_match("ab", "a%b", false, None));
        assert!(like_match("anything", "%%%", false, None));
        assert!(like_match("anything", "a%g", false, None));
    }

    #[test]
    fn like_match_case_insensitive_handles_unicode() {
        assert!(like_match("ABC", "abc", true, None));
        assert!(like_match("ÄBC", "äbc", true, None));
        assert!(!like_match("ABC", "abc", false, None));
    }
}