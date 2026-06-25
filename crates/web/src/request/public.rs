use std::collections::HashSet;

use axum::http::Uri;

use crate::MAX_PAGE;

use super::decode::{
    mark_seen, non_empty_string, parse_bounded_usize, required_value, strict_decode,
    strict_query_pairs,
};
use super::{ParseResult, RequestParseError, non_empty};

#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    pub source: Option<String>,
    pub entry: Option<String>,
    pub query: PageQuery,
}

impl PageRequest {
    pub fn has_search_return_context(&self) -> bool {
        normalized_query(&self.query).is_some()
            || self.query.page.is_some()
            || self.query.source == Some(LinkOrigin::All)
    }

    pub fn is_direct_entry(&self) -> bool {
        self.entry.is_some() && !self.has_search_return_context()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PageQuery {
    pub q: Option<String>,

    pub ref_id: Option<String>,

    pub ref_set: Option<String>,

    pub source: Option<LinkOrigin>,

    pub page: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOrigin {
    All,
}

impl LinkOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
        }
    }
}

pub fn normalized_query(query: &PageQuery) -> Option<&str> {
    query.q.as_deref().and_then(non_empty)
}

pub(crate) fn public_uri(raw_url: &str) -> ParseResult<Uri> {
    if raw_url.contains('#') {
        return Err(RequestParseError::new(
            "public URL must not include a fragment",
        ));
    }

    let raw_url = if raw_url.is_empty() { "/" } else { raw_url };

    let normalized;
    let raw_url = if raw_url.starts_with('/') || raw_url.contains("://") {
        raw_url
    } else {
        normalized = format!("/{raw_url}");
        &normalized
    };

    raw_url
        .parse::<Uri>()
        .map_err(|error| RequestParseError::new(format!("invalid public URL: {error}")))
}

#[cfg(test)]
pub fn page_request_from_public_url(raw_url: &str) -> ParseResult<PageRequest> {
    let uri = public_uri(raw_url)?;
    page_request_from_public_uri(&uri)
}

pub(crate) fn page_request_from_public_uri(uri: &Uri) -> ParseResult<PageRequest> {
    let raw_path = uri.path();
    let raw_query = uri.query().unwrap_or("");

    if raw_path != "/" && (raw_path.contains("//") || raw_path.ends_with('/')) {
        return Err(RequestParseError::new(
            "path must not contain empty segments",
        ));
    }

    let path_parts = raw_path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let source = path_parts
        .first()
        .map(|value| strict_decode(value, false))
        .transpose()?;

    let entry = if path_parts.len() >= 2 {
        let raw_entry = path_parts[1..].join("/");
        Some(strict_decode(&raw_entry, false)?)
    } else {
        None
    };

    let mut q = None;
    let mut ref_id = None;
    let mut ref_set = None;
    let mut source_param = None;
    let mut page = None;
    let mut seen = HashSet::new();

    for (key, value) in strict_query_pairs(raw_query)? {
        mark_seen(&mut seen, &key)?;

        match key.as_str() {
            "q" => q = non_empty_string(value),
            "ref" => ref_id = Some(required_value(&key, value)?),
            "ref_set" => ref_set = Some(required_value(&key, value)?),
            "source" => {
                if value != LinkOrigin::All.as_str() {
                    return Err(RequestParseError::new("source must be all"));
                }
                source_param = Some(LinkOrigin::All);
            }
            "page" => {
                page = Some(parse_bounded_usize(&value, "page", 1, MAX_PAGE)?);
            }
            _ => {
                return Err(RequestParseError::new(format!(
                    "unknown query parameter {key:?}"
                )));
            }
        }
    }

    if page.is_some() && q.as_deref().and_then(non_empty).is_none() {
        return Err(RequestParseError::new("page requires q"));
    }

    Ok(PageRequest {
        source,
        entry,
        query: PageQuery {
            q,
            ref_id,
            ref_set,
            source: source_param,
            page,
        },
    })
}
