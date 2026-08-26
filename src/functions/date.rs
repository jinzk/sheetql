use chrono::{DateTime, Local};

use crate::error::Error;
use crate::value::Value;

use super::require_arity;

/// Date/time functions (`NOW`, `CURRENT_TIMESTAMP`, `DATE`).
pub(crate) fn eval(
    name: &str,
    values: &[Value],
    now: &DateTime<Local>,
) -> Result<Option<Value>, Error> {
    let value = match name {
        "now" | "current_timestamp" => {
            require_arity(name, values, 0)?;
            Value::DateTime(now_string(now))
        }
        "date" => {
            if values.is_empty() {
                return Ok(Some(Value::Date(today_string(now))));
            }
            require_arity(name, values, 1)?;
            Value::Date(extract_date(&values[0].to_display_string()))
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn now_string(now: &DateTime<Local>) -> String {
    now.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today_string(now: &DateTime<Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

fn extract_date(input: &str) -> String {
    let trimmed = input.trim();
    let date_part = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let has_separator = date_part.chars().any(|c| matches!(c, '-' | '/' | '.'));
    if !has_separator {
        return trimmed.to_string();
    }
    let parts: Vec<&str> = date_part.split(['-', '/', '.']).collect();
    if parts.len() != 3 {
        return trimmed.to_string();
    }
    let (year, month, day) = (parts[0], parts[1], parts[2]);
    if year.len() != 4
        || month.is_empty()
        || month.len() > 2
        || day.is_empty()
        || day.len() > 2
        || !year.chars().all(|c| c.is_ascii_digit())
        || !month.chars().all(|c| c.is_ascii_digit())
        || !day.chars().all(|c| c.is_ascii_digit())
    {
        return trimmed.to_string();
    }
    let month = month.parse::<u32>().unwrap_or(0);
    let day = day.parse::<u32>().unwrap_or(0);
    // Reject impossible dates like 2026-02-31 instead of normalizing them
    // into values that later date comparisons cannot parse.
    if chrono::NaiveDate::from_ymd_opt(year.parse::<i32>().unwrap_or(0), month, day).is_none() {
        return trimmed.to_string();
    }
    format!("{}-{:02}-{:02}", year, month, day)
}
