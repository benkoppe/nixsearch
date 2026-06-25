use std::collections::HashSet;

use axum::http::Uri;

use crate::{DEFAULT_LIMIT, MAX_OFFSET};

use super::decode::{
    mark_seen, non_empty_string, parse_bounded_usize, required_value, strict_query_pairs,
};
use super::{ParseResult, RequestParseError};

pub(crate) fn state_events_query_from_uri(uri: &Uri) -> ParseResult<StateQuery> {
    let mut url = None;
    let mut previous_url = None;
    let mut generation_id = None;
    let mut seen = HashSet::new();

    for (key, value) in strict_query_pairs(uri.query().unwrap_or(""))? {
        mark_seen(&mut seen, &key)?;

        match key.as_str() {
            "url" => url = Some(endpoint_path_query_uri(&required_value(&key, value)?)?),
            "previous_url" => {
                previous_url = non_empty_string(value)
                    .map(|value| endpoint_path_query_uri(&value))
                    .transpose()?;
            }
            "generation_id" => generation_id = non_empty_string(value),
            "datastar" => {}
            _ => {
                return Err(RequestParseError::new(format!(
                    "unknown query parameter {key:?}"
                )));
            }
        }
    }

    Ok(StateQuery {
        url: url.ok_or_else(|| RequestParseError::new("url is required"))?,
        previous_url,
        generation_id,
    })
}

pub(crate) fn slice_query_from_uri(uri: &Uri) -> ParseResult<SliceQuery> {
    let mut url = None;
    let mut offset = None;
    let mut limit = None;
    let mut generation_id = None;
    let mut seen = HashSet::new();

    for (key, value) in strict_query_pairs(uri.query().unwrap_or(""))? {
        mark_seen(&mut seen, &key)?;

        match key.as_str() {
            "url" => url = Some(endpoint_path_query_uri(&required_value(&key, value)?)?),
            "offset" => {
                offset = Some(parse_bounded_usize(&value, "offset", 0, MAX_OFFSET)?);
            }
            "limit" => {
                let max_limit = DEFAULT_LIMIT
                    .checked_mul(4)
                    .ok_or_else(|| RequestParseError::new("limit bound overflow"))?;
                limit = Some(parse_bounded_usize(&value, "limit", 1, max_limit)?);
            }
            "generation_id" => generation_id = non_empty_string(value),
            "datastar" => {}
            _ => {
                return Err(RequestParseError::new(format!(
                    "unknown query parameter {key:?}"
                )));
            }
        }
    }

    Ok(SliceQuery {
        url: url.ok_or_else(|| RequestParseError::new("url is required"))?,
        offset: offset.ok_or_else(|| RequestParseError::new("offset is required"))?,
        limit,
        generation_id,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateQuery {
    pub url: Uri,
    pub previous_url: Option<Uri>,
    pub generation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SliceQuery {
    pub url: Uri,
    pub offset: usize,
    pub limit: Option<usize>,
    pub generation_id: Option<String>,
}

fn endpoint_path_query_uri(raw_url: &str) -> ParseResult<Uri> {
    if raw_url.contains('#') {
        return Err(RequestParseError::new(
            "endpoint URL must not include a fragment",
        ));
    }
    if !raw_url.starts_with('/') || raw_url.starts_with("//") {
        return Err(RequestParseError::new(
            "endpoint URL must be a path-and-query URL",
        ));
    }

    let uri = raw_url
        .parse::<Uri>()
        .map_err(|error| RequestParseError::new(format!("invalid endpoint URL: {error}")))?;

    if uri.scheme().is_some() || uri.authority().is_some() {
        return Err(RequestParseError::new(
            "endpoint URL must be a path-and-query URL",
        ));
    }

    Ok(uri)
}
