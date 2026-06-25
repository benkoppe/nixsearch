use std::collections::HashSet;

use percent_encoding::percent_decode;

use super::{ParseResult, RequestParseError};

pub fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

pub(crate) fn strict_query_pairs(raw_query: &str) -> ParseResult<Vec<(String, String)>> {
    if raw_query.is_empty() {
        return Ok(Vec::new());
    }

    raw_query
        .split('&')
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Ok((strict_decode(key, true)?, strict_decode(value, true)?))
        })
        .collect()
}

pub(crate) fn strict_decode(value: &str, plus_as_space: bool) -> ParseResult<String> {
    let mut bytes = value.as_bytes().to_vec();

    if plus_as_space {
        for byte in &mut bytes {
            if *byte == b'+' {
                *byte = b' ';
            }
        }
    }

    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return Err(RequestParseError::new("invalid percent encoding"));
            }
            index += 3;
        } else {
            index += 1;
        }
    }

    percent_decode(&bytes)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| RequestParseError::new("invalid UTF-8 in URL"))
}

pub(super) fn non_empty_string(value: String) -> Option<String> {
    if non_empty(&value).is_some() {
        Some(value)
    } else {
        None
    }
}

pub(super) fn required_value(key: &str, value: String) -> ParseResult<String> {
    if non_empty(&value).is_none() {
        return Err(RequestParseError::new(format!("{key} must not be empty")));
    }
    Ok(value)
}

pub(super) fn mark_seen(seen: &mut HashSet<String>, key: &str) -> ParseResult<()> {
    if !seen.insert(key.to_owned()) {
        return Err(RequestParseError::new(format!("duplicate {key} parameter")));
    }
    Ok(())
}

pub(super) fn parse_bounded_usize(
    value: &str,
    name: &str,
    min: usize,
    max: usize,
) -> ParseResult<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RequestParseError::new(format!("{name} must be an integer")));
    }

    let value = value
        .parse::<usize>()
        .map_err(|_| RequestParseError::new(format!("{name} is out of range")))?;

    if value < min || value > max {
        return Err(RequestParseError::new(format!("{name} is out of range")));
    }

    Ok(value)
}
