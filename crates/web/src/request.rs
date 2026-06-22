use std::collections::HashSet;
use std::fmt;
use std::ops::Deref;

use axum::http::Uri;
use percent_encoding::percent_decode;

use crate::{DEFAULT_LIMIT, MAX_OFFSET, MAX_PAGE};
use nixsearch_config::app::AppConfig;

type ParseResult<T> = std::result::Result<T, RequestParseError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestParseError(String);

impl RequestParseError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RequestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for RequestParseError {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageState {
    pub q: Option<String>,
    pub page: Option<usize>,
    pub source_filter: SourceFilter,
    pub ref_scope: RefScope,
    pub source_ref: Option<String>,
    pub detail: Option<DetailState>,
}

impl PageState {
    pub fn active_ref_set(&self) -> Option<&str> {
        self.ref_scope.ref_set()
    }

    pub fn set_explicit_ref_set(&mut self, ref_set: String) {
        self.ref_scope = RefScope::RefSet { ref_set };
    }

    pub fn clear_ref_set_context(&mut self) {
        self.ref_scope = RefScope::Ref;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefScope {
    RefSet { ref_set: String },
    Ref,
}

impl RefScope {
    pub fn ref_set(&self) -> Option<&str> {
        match self {
            Self::RefSet { ref_set, .. } => Some(ref_set),
            Self::Ref => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailState {
    pub source: String,
    pub entry: String,
    pub ref_id: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Named(String),
}

impl SourceFilter {
    pub fn from_request(request: &PageRequest) -> Self {
        if request.query.source == Some(LinkOrigin::All) {
            return Self::All;
        }

        match &request.source {
            None => Self::All,
            Some(source) => Self::Named(source.clone()),
        }
    }
}

pub fn page_state(config: &AppConfig, request: &PageRequest) -> PageState {
    let source_filter = SourceFilter::from_request(request);
    let q = normalized_query(&request.query).map(ToOwned::to_owned);
    let raw_ref_set = request.query.ref_set.as_deref().and_then(non_empty);
    let raw_ref = request.query.ref_id.as_deref().and_then(non_empty);

    match &source_filter {
        SourceFilter::All => {
            let ref_scope = normalize_all_ref_set(config, raw_ref_set)
                .map(|ref_set| RefScope::RefSet { ref_set })
                .unwrap_or(RefScope::Ref);
            let detail = detail_state(config, request, raw_ref, ref_scope.ref_set(), None);

            PageState {
                q,
                page: request.query.page,
                source_filter,
                ref_scope,
                source_ref: None,
                detail,
            }
        }
        SourceFilter::Named(source) => {
            let (ref_scope, source_ref) =
                normalize_source_ref_scope(config, source, raw_ref, raw_ref_set);
            let source_ref = source_ref.or_else(|| {
                if raw_ref.is_some() || raw_ref_set.is_some() {
                    return None;
                }

                config
                    .sources
                    .get(source)
                    .and_then(|source| source.default_ref.clone())
            });
            let detail = detail_state(
                config,
                request,
                raw_ref,
                ref_scope.ref_set(),
                source_ref.as_deref(),
            );

            PageState {
                q,
                page: request.query.page,
                source_filter,
                ref_scope,
                source_ref,
                detail,
            }
        }
    }
}

fn detail_state(
    config: &AppConfig,
    request: &PageRequest,
    raw_ref: Option<&str>,
    active_ref_set: Option<&str>,
    source_ref: Option<&str>,
) -> Option<DetailState> {
    request
        .entry
        .as_ref()
        .zip(request.source.as_ref())
        .map(|(entry, source)| DetailState {
            source: source.clone(),
            entry: entry.clone(),
            ref_id: detail_ref(config, source, raw_ref, active_ref_set, source_ref),
        })
}

fn detail_ref(
    config: &AppConfig,
    source: &str,
    raw_ref: Option<&str>,
    active_ref_set: Option<&str>,
    source_ref: Option<&str>,
) -> Option<String> {
    if let Some(refs) =
        active_ref_set.and_then(|ref_set| config.refs_for_ref_set_source(ref_set, source))
    {
        if refs.len() == 1 {
            return match raw_ref {
                Some(ref_id) if !refs.iter().any(|candidate| candidate == ref_id) => None,
                _ => refs.first().cloned(),
            };
        }

        return raw_ref
            .filter(|ref_id| refs.iter().any(|candidate| candidate == ref_id))
            .map(ToOwned::to_owned);
    }

    source_ref
        .map(ToOwned::to_owned)
        .or_else(|| raw_ref.map(ToOwned::to_owned))
}

fn normalize_all_ref_set(config: &AppConfig, ref_set: Option<&str>) -> Option<String> {
    match ref_set {
        Some(ref_set) => Some(ref_set.to_owned()),
        None => config.default_ref_set().map(ToOwned::to_owned),
    }
}

fn normalize_source_ref_scope(
    config: &AppConfig,
    source: &str,
    ref_id: Option<&str>,
    ref_set: Option<&str>,
) -> (RefScope, Option<String>) {
    if let Some(ref_set) = ref_set {
        let source_ref = match config.refs_for_ref_set_source(ref_set, source) {
            Some(refs) if refs.len() == 1 => match ref_id {
                Some(ref_id) if !refs.iter().any(|candidate| candidate == ref_id) => None,
                _ => refs.first().cloned(),
            },
            Some(refs) => ref_id
                .filter(|ref_id| refs.iter().any(|candidate| candidate == ref_id))
                .map(ToOwned::to_owned),
            None => None,
        };

        return (
            RefScope::RefSet {
                ref_set: ref_set.to_owned(),
            },
            source_ref,
        );
    }

    let ref_id = ref_id.map(ToOwned::to_owned);
    (RefScope::Ref, ref_id)
}

pub fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
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

fn non_empty_string(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn required_value(key: &str, value: String) -> ParseResult<String> {
    if non_empty(&value).is_none() {
        return Err(RequestParseError::new(format!("{key} must not be empty")));
    }
    Ok(value)
}

fn mark_seen(seen: &mut HashSet<String>, key: &str) -> ParseResult<()> {
    if !seen.insert(key.to_owned()) {
        return Err(RequestParseError::new(format!("duplicate {key} parameter")));
    }
    Ok(())
}

fn parse_bounded_usize(value: &str, name: &str, min: usize, max: usize) -> ParseResult<usize> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use nixsearch_config::app::RefSetConfig;
    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
    use nixsearch_core::artifact::ArtifactKind;

    fn ref_config(id: &str) -> RefConfig {
        RefConfig {
            id: id.to_owned(),
            producer: ProducerConfig::ExistingFile {
                path: PathBuf::from("unused.json"),
                artifact: ArtifactKind::OptionsJson,
            },
            source_links: None,
        }
    }

    fn handoff_config() -> AppConfig {
        AppConfig {
            sources: [
                (
                    "nixpkgs".to_owned(),
                    SourceConfig {
                        name: None,
                        color: None,
                        kind: SourceKind::Packages,
                        strip_prefixes: Vec::new(),
                        default_ref: Some("nixos-unstable".to_owned()),
                        refs: vec![ref_config("nixos-unstable"), ref_config("nixos-25.11")],
                    },
                ),
                (
                    "hjem".to_owned(),
                    SourceConfig {
                        name: None,
                        color: None,
                        kind: SourceKind::Options,
                        strip_prefixes: Vec::new(),
                        default_ref: Some("main".to_owned()),
                        refs: vec![ref_config("main")],
                    },
                ),
            ]
            .into(),
            ref_sets: [
                (
                    "unstable".to_owned(),
                    RefSetConfig {
                        refs: [
                            ("nixpkgs".to_owned(), vec!["nixos-unstable".to_owned()]),
                            ("hjem".to_owned(), vec!["main".to_owned()]),
                        ]
                        .into(),
                    },
                ),
                (
                    "25.11".to_owned(),
                    RefSetConfig {
                        refs: [
                            ("nixpkgs".to_owned(), vec!["nixos-25.11".to_owned()]),
                            ("hjem".to_owned(), vec!["main".to_owned()]),
                        ]
                        .into(),
                    },
                ),
                (
                    "multi".to_owned(),
                    RefSetConfig {
                        refs: [
                            (
                                "nixpkgs".to_owned(),
                                vec!["nixos-unstable".to_owned(), "nixos-25.11".to_owned()],
                            ),
                            ("hjem".to_owned(), vec!["main".to_owned()]),
                        ]
                        .into(),
                    },
                ),
            ]
            .into(),
            ..AppConfig::default()
        }
    }

    #[test]
    fn parses_root_public_url() {
        let request = page_request_from_public_url("/").unwrap();
        assert_eq!(request.source, None);
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q, None);
    }

    #[test]
    fn public_uri_normalizes_path_query_urls() {
        let uri = public_uri("fixtures?q=git").unwrap();

        assert_eq!(uri.path(), "/fixtures");
        assert_eq!(uri.query(), Some("q=git"));
    }

    #[test]
    fn public_uri_normalizes_query_only_urls() {
        let uri = public_uri("?q=git").unwrap();

        assert_eq!(uri.path(), "/");
        assert_eq!(uri.query(), Some("q=git"));
    }

    #[test]
    fn public_uri_accepts_absolute_urls() {
        let uri = public_uri("https://search.example.com/fixtures?q=git").unwrap();

        assert_eq!(uri.path(), "/fixtures");
        assert_eq!(uri.query(), Some("q=git"));
    }

    #[test]
    fn public_uri_rejects_fragments() {
        let error = public_uri("fixtures?q=git#fragment").unwrap_err();

        assert!(error.contains("fragment"));
    }

    #[test]
    fn parses_source_search_public_url() {
        let request = page_request_from_public_url("/fixtures?q=git&ref=small").unwrap();
        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
    }

    #[test]
    fn parses_absolute_public_url() {
        let request =
            page_request_from_public_url("https://search.example.com/fixtures?q=git&ref=small")
                .unwrap();

        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
    }

    #[test]
    fn parses_entry_public_url() {
        let request =
            page_request_from_public_url("/fixtures/programs.git.enable?q=git&ref=small").unwrap();
        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry.as_deref(), Some("programs.git.enable"));
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
    }

    #[test]
    fn rejects_duplicate_public_query_params() {
        let error = page_request_from_public_url("/?q=&q=git").unwrap_err();

        assert!(error.contains("duplicate q"));
    }

    #[test]
    fn rejects_encoded_duplicate_public_query_params() {
        let error = page_request_from_public_url("/?ref=small&%72ef=other").unwrap_err();

        assert!(error.contains("duplicate ref"));
    }

    #[test]
    fn rejects_unknown_public_query_params() {
        let error = page_request_from_public_url("/?q=git&utm_source=x").unwrap_err();

        assert!(error.contains("unknown query parameter"));
    }

    #[test]
    fn treats_empty_q_as_absent() {
        let request = page_request_from_public_url("/?q=").unwrap();

        assert_eq!(request.query.q, None);
    }

    #[test]
    fn rejects_empty_ref_and_ref_set() {
        assert!(
            page_request_from_public_url("/?ref=")
                .unwrap_err()
                .contains("ref")
        );
        assert!(
            page_request_from_public_url("/?ref_set=")
                .unwrap_err()
                .contains("ref_set")
        );
    }

    #[test]
    fn rejects_public_kind_query_param() {
        for kind in ["", "option", "package", "app", "service", "bogus"] {
            let error = page_request_from_public_url(&format!("/?kind={kind}")).unwrap_err();

            assert!(error.contains("unknown query parameter \"kind\""));
        }
    }

    #[test]
    fn rejects_invalid_source_query_param() {
        let error = page_request_from_public_url("/?source=fixtures").unwrap_err();

        assert!(error.contains("source must be all"));
    }

    #[test]
    fn rejects_page_without_query_and_out_of_bounds_page() {
        assert!(
            page_request_from_public_url("/?page=2")
                .unwrap_err()
                .contains("page requires q")
        );
        assert!(
            page_request_from_public_url("/?q=git&page=0")
                .unwrap_err()
                .contains("page")
        );
        assert!(
            page_request_from_public_url("/?q=git&page=1001")
                .unwrap_err()
                .contains("page")
        );
    }

    #[test]
    fn rejects_empty_path_segments() {
        for url in ["//", "/fixtures/", "/fixtures//entry"] {
            let error = page_request_from_public_url(url).unwrap_err();

            assert!(error.contains("empty segments"));
        }
    }

    #[test]
    fn strictly_decodes_path_and_query() {
        let request =
            page_request_from_public_url("/fix%74ures/programs%2Egit?q=hello+world").unwrap();

        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry.as_deref(), Some("programs.git"));
        assert_eq!(request.query.q.as_deref(), Some("hello world"));
    }

    #[test]
    fn keeps_plus_literal_in_path() {
        let request = page_request_from_public_url("/fixtures/a+b?q=a+b").unwrap();

        assert_eq!(request.entry.as_deref(), Some("a+b"));
        assert_eq!(request.query.q.as_deref(), Some("a b"));
    }

    #[test]
    fn rejects_bad_percent_encoding_and_utf8() {
        assert!(page_request_from_public_url("/fixtures/%zz").is_err());
        assert!(page_request_from_public_url("/?q=%FF").is_err());
    }

    #[test]
    fn parses_source_query_param() {
        let request = page_request_from_public_url("/nixpkgs/git?q=git&source=all").unwrap();
        assert_eq!(request.source.as_deref(), Some("nixpkgs"));
        assert_eq!(request.entry.as_deref(), Some("git"));
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.source, Some(LinkOrigin::All));
    }

    #[test]
    fn parses_ref_set_query_param() {
        let request = page_request_from_public_url("/?q=git&ref_set=25.11").unwrap();

        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_set.as_deref(), Some("25.11"));
    }

    #[test]
    fn parses_state_events_outer_query_strictly() {
        let uri = Uri::from_static("/-/state/events?url=%2Ffixtures&previous_url=");
        let query = state_events_query_from_uri(&uri).unwrap();

        assert_eq!(query.url.path(), "/fixtures");
        assert_eq!(query.url.query(), None);
        assert_eq!(query.previous_url, None);
        assert_eq!(query.generation_id, None);
    }

    #[test]
    fn parses_state_events_outer_query_with_datastar_metadata() {
        let uri = Uri::from_static(
            "/-/state/events?url=%2F%3Fq%3Dhello&previous_url=%2F&generation_id=sha256%3Aabc&datastar=%7B%7D",
        );
        let query = state_events_query_from_uri(&uri).unwrap();

        assert_eq!(query.url.path(), "/");
        assert_eq!(query.url.query(), Some("q=hello"));
        assert_eq!(query.previous_url.as_ref().map(Uri::path), Some("/"));
        assert_eq!(query.generation_id.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn rejects_bad_state_events_outer_query() {
        for uri in [
            "/-/state/events",
            "/-/state/events?url=",
            "/-/state/events?url=%2F&url=%2Ffixtures",
            "/-/state/events?url=%zz",
            "/-/state/events?url=%2F&extra=1",
            "/-/state/events?url=%2F&generation_id=one&generation_id=two",
            "/-/state/events?url=%3Fq%3Dgit",
            "/-/state/events?url=fixtures%3Fq%3Dgit",
            "/-/state/events?url=https%3A%2F%2Fsearch.example.com%2Ffixtures",
            "/-/state/events?url=%2F%3Fq%3Dgit&previous_url=https%3A%2F%2Fsearch.example.com%2F",
        ] {
            let uri: Uri = uri.parse().unwrap();

            assert!(state_events_query_from_uri(&uri).is_err());
        }
    }

    #[test]
    fn parses_slice_outer_query_strictly() {
        let uri = Uri::from_static(
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=50&limit=25&generation_id=sha256%3Aabc",
        );
        let query = slice_query_from_uri(&uri).unwrap();

        assert_eq!(query.url.path(), "/");
        assert_eq!(query.url.query(), Some("q=git"));
        assert_eq!(query.offset, 50);
        assert_eq!(query.limit, Some(25));
        assert_eq!(query.generation_id.as_deref(), Some("sha256:abc"));
    }

    #[test]
    fn parses_slice_outer_query_with_datastar_metadata() {
        let uri = Uri::from_static("/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&datastar=%7B%7D");
        let query = slice_query_from_uri(&uri).unwrap();

        assert_eq!(query.url.path(), "/");
        assert_eq!(query.url.query(), Some("q=git"));
        assert_eq!(query.offset, 0);
        assert_eq!(query.limit, None);
        assert_eq!(query.generation_id, None);
    }

    #[test]
    fn rejects_bad_slice_outer_query() {
        for uri in [
            "/-/results/slice?url=%2F%3Fq%3Dgit",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=-1",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=50000",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&limit=0",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&limit=201",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&extra=1",
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&generation_id=one&generation_id=two",
            "/-/results/slice?url=%3Fq%3Dgit&offset=0",
            "/-/results/slice?url=fixtures%3Fq%3Dgit&offset=0",
            "/-/results/slice?url=https%3A%2F%2Fsearch.example.com%2Ffixtures&offset=0",
        ] {
            let uri: Uri = uri.parse().unwrap();

            assert!(slice_query_from_uri(&uri).is_err(), "{uri}");
        }
    }

    #[test]
    fn source_filter_treats_source_all_as_all_sources() {
        let request =
            page_request_from_public_url("/nixos/programs.git.config?q=programs.git&source=all")
                .unwrap();

        assert_eq!(SourceFilter::from_request(&request), SourceFilter::All);
    }

    #[test]
    fn page_state_uses_ref_set_to_select_source_ref() {
        let config = handoff_config();
        let request = page_request_from_public_url("/nixpkgs?q=git&ref_set=25.11").unwrap();
        let state = page_state(&config, &request);

        assert_eq!(
            state.source_filter,
            SourceFilter::Named("nixpkgs".to_owned())
        );
        assert_eq!(state.active_ref_set(), Some("25.11"));
        assert_eq!(state.source_ref.as_deref(), Some("nixos-25.11"));
    }

    #[test]
    fn page_state_preserves_overlapping_ref_set_context() {
        let config = handoff_config();
        let request = page_request_from_public_url("/hjem?q=git&ref_set=25.11").unwrap();
        let state = page_state(&config, &request);

        assert_eq!(state.active_ref_set(), Some("25.11"));
        assert_eq!(state.source_ref.as_deref(), Some("main"));
    }

    #[test]
    fn page_state_falls_back_to_default_ref_set_for_ambiguous_ref() {
        let config = handoff_config();
        let request = page_request_from_public_url("/hjem?q=git&ref=main").unwrap();
        let state = page_state(&config, &request);

        assert_eq!(state.active_ref_set(), None);
        assert_eq!(state.source_ref.as_deref(), Some("main"));
    }

    #[test]
    fn page_state_detail_stale_ref_set_ref_stays_unselected() {
        let config = handoff_config();
        let request =
            page_request_from_public_url("/nixpkgs/git?ref=nixos-unstable&ref_set=25.11").unwrap();
        let state = page_state(&config, &request);
        let detail = state.detail.as_ref().unwrap();

        assert_eq!(state.source_ref, None);
        assert_eq!(detail.ref_id, None);
    }

    #[test]
    fn page_state_detail_invalid_ref_set_ref_stays_unselected() {
        let config = handoff_config();
        let request = page_request_from_public_url("/nixpkgs/git?ref=bogus&ref_set=multi").unwrap();
        let state = page_state(&config, &request);
        let detail = state.detail.as_ref().unwrap();

        assert_eq!(state.source_ref, None);
        assert_eq!(detail.ref_id, None);
    }

    #[test]
    fn page_state_ambiguous_ref_set_without_ref_stays_unselected() {
        let config = handoff_config();
        let request = page_request_from_public_url("/nixpkgs/git?ref_set=multi").unwrap();
        let state = page_state(&config, &request);
        let detail = state.detail.as_ref().unwrap();

        assert_eq!(state.source_ref, None);
        assert_eq!(detail.ref_id, None);
    }

    #[test]
    fn page_state_unknown_ref_set_stays_unselected() {
        let config = handoff_config();
        let request = page_request_from_public_url("/nixpkgs/git?ref_set=missing").unwrap();
        let state = page_state(&config, &request);
        let detail = state.detail.as_ref().unwrap();

        assert_eq!(state.active_ref_set(), Some("missing"));
        assert_eq!(state.source_ref, None);
        assert_eq!(detail.ref_id, None);
    }
}
