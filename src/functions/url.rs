use crate::error::Error;
use crate::value::Value;

use super::require_arity;

/// URL and email functions (`URL_*`, `EMAIL_*`).
pub(crate) fn eval(name: &str, values: &[Value]) -> Result<Option<Value>, Error> {
    let value = match name {
        "url_scheme" | "url_host" | "url_port" | "url_path" | "url_query" | "url_fragment" => {
            eval_url_field(name, values)?
        }
        "url_param" => eval_url_param(values)?,
        "email_local" | "email_domain" | "email_valid" => eval_email_function(name, values)?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn eval_url_field(name: &str, values: &[Value]) -> Result<Value, Error> {
    require_arity(name, values, 1)?;
    if values[0].is_null() {
        return Ok(Value::Null);
    }
    let text = values[0]
        .as_text()
        .ok_or_else(|| format!("Function `{name}` expects a text URL, got `{}`", values[0]))?;
    let parsed = url::Url::parse(text).map_err(|error| format!("Invalid URL `{text}`: {error}"))?;
    let value = match name {
        "url_scheme" => Some(parsed.scheme().to_string()),
        "url_host" => parsed.host_str().map(str::to_string),
        "url_port" => parsed.port().map(|port| port.to_string()),
        "url_path" => Some(parsed.path().to_string()),
        "url_query" => parsed.query().map(str::to_string),
        "url_fragment" => parsed.fragment().map(str::to_string),
        _ => unreachable!(),
    };
    Ok(value.map(Value::Text).unwrap_or(Value::Null))
}

fn eval_url_param(values: &[Value]) -> Result<Value, Error> {
    require_arity("url_param", values, 2)?;
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let text = values[0]
        .as_text()
        .ok_or("Function `url_param` expects a text URL")?;
    let name = values[1]
        .as_text()
        .ok_or("Function `url_param` expects a text parameter name")?;
    let parsed = url::Url::parse(text).map_err(|error| format!("Invalid URL `{text}`: {error}"))?;
    Ok(parsed
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| Value::Text(value.into_owned()))
        .unwrap_or(Value::Null))
}

fn eval_email_function(name: &str, values: &[Value]) -> Result<Value, Error> {
    require_arity(name, values, 1)?;
    if values[0].is_null() {
        return Ok(Value::Null);
    }
    let email = values[0]
        .as_text()
        .ok_or_else(|| format!("Function `{name}` expects a text email address"))?;
    let Some((local, domain)) = split_email(email) else {
        return if name == "email_valid" {
            Ok(Value::Bool(false))
        } else {
            Ok(Value::Null)
        };
    };
    match name {
        "email_local" => Ok(Value::Text(local.to_string())),
        "email_domain" => Ok(Value::Text(domain.to_string())),
        "email_valid" => Ok(Value::Bool(true)),
        _ => unreachable!(),
    }
}

fn split_email(email: &str) -> Option<(&str, &str)> {
    let email = email.trim();
    let (local, domain) = email.rsplit_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
        || local.chars().any(|character| {
            character.is_whitespace()
                || matches!(character, '(' | ')' | '<' | '>' | ',' | ';' | ':')
        })
        || !domain.contains('.')
        || domain.chars().any(|character| {
            character.is_whitespace()
                || matches!(character, '(' | ')' | '<' | '>' | ',' | ';' | ':')
        })
    {
        return None;
    }
    Some((local, domain))
}
