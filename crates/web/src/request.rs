use axum::http::Uri;
use serde::Deserialize;

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::DocumentKind;

#[derive(Debug, Clone, Default)]
pub struct PageRequest {
    pub source: Option<String>,
    pub entry: Option<String>,
    pub query: PageQuery,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageQuery {
    pub q: Option<String>,

    #[serde(rename = "ref")]
    pub ref_id: Option<String>,

    pub ref_set: Option<String>,

    pub kind: Option<String>,

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
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkOrigin {
    All,
}

impl LinkOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
        }
    }

    pub fn from_query_param(s: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
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
            kind: request
                .query
                .kind
                .as_deref()
                .and_then(non_empty)
                .map(ToOwned::to_owned),
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

pub fn decode_path_value(value: &str) -> Option<String> {
    urlencoding::decode(value)
        .ok()
        .map(|value| value.into_owned())
}

pub(crate) fn public_uri(raw_url: &str) -> std::result::Result<Uri, String> {
    let raw_url = raw_url
        .split_once('#')
        .map_or(raw_url, |(before_fragment, _)| before_fragment);
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
        .map_err(|error| format!("invalid public URL: {error}"))
}

#[cfg(test)]
pub fn page_request_from_public_url(raw_url: &str) -> std::result::Result<PageRequest, String> {
    let uri = public_uri(raw_url)?;
    page_request_from_public_uri(&uri)
}

pub(crate) fn page_request_from_public_uri(uri: &Uri) -> std::result::Result<PageRequest, String> {
    let raw_path = uri.path();
    let raw_query = uri.query().unwrap_or("");

    let path_parts = raw_path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let source = path_parts
        .first()
        .map(|value| decode_path_value(value).unwrap_or_else(|| (*value).to_owned()));

    let entry = if path_parts.len() >= 2 {
        let raw_entry = path_parts[1..].join("/");
        Some(decode_path_value(&raw_entry).unwrap_or(raw_entry))
    } else {
        None
    };

    let mut q = None;
    let mut ref_id = None;
    let mut ref_set = None;
    let mut kind = None;
    let mut source_param = None;
    let mut page = None;

    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "q" => q = Some(value.into_owned()),
            "ref" => ref_id = Some(value.into_owned()),
            "ref_set" => ref_set = Some(value.into_owned()),
            "kind" => kind = Some(value.into_owned()),
            "source" => source_param = LinkOrigin::from_query_param(&value),
            "page" => page = value.parse::<usize>().ok(),
            _ => {}
        }
    }

    Ok(PageRequest {
        source,
        entry,
        query: PageQuery {
            q,
            ref_id,
            ref_set,
            kind,
            source: source_param,
            page,
        },
    })
}

pub fn parse_document_kind(
    value: Option<&str>,
) -> std::result::Result<Option<DocumentKind>, String> {
    match value.and_then(non_empty) {
        None => Ok(None),
        Some("option") => Ok(Some(DocumentKind::Option)),
        Some("package") => Ok(Some(DocumentKind::Package)),
        Some("app") => Ok(Some(DocumentKind::App)),
        Some("service") => Ok(Some(DocumentKind::Service)),
        Some(other) => Err(format!("unknown entry kind {other:?}")),
    }
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
        let uri = public_uri("fixtures?q=git#ignored").unwrap();

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
        let uri = public_uri("https://search.example.com/fixtures?q=git#ignored").unwrap();

        assert_eq!(uri.path(), "/fixtures");
        assert_eq!(uri.query(), Some("q=git"));
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
        let request = page_request_from_public_url(
            "https://search.example.com/fixtures?q=git&ref=small#ignored",
        )
        .unwrap();

        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
    }

    #[test]
    fn parses_entry_public_url() {
        let request = page_request_from_public_url(
            "/fixtures/programs.git.enable?q=git&ref=small&kind=option",
        )
        .unwrap();
        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry.as_deref(), Some("programs.git.enable"));
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
        assert_eq!(request.query.kind.as_deref(), Some("option"));
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
