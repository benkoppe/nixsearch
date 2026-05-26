use anyhow::{Context, Result};
use serde::Deserialize;

use nixsearch_config::AppConfig;
use nixsearch_core::DocumentKind;

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

    pub kind: Option<String>,

    pub source: Option<LinkOrigin>,

    pub page: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultsContext {
    q: Option<String>,
    source: Option<String>,
    ref_id: Option<String>,
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

pub fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

pub fn normalized_query(query: &PageQuery) -> Option<&str> {
    query.q.as_deref().and_then(non_empty)
}

pub fn results_context(request: &PageRequest) -> ResultsContext {
    let source_all = request.query.source == Some(LinkOrigin::All);

    ResultsContext {
        q: normalized_query(&request.query).map(ToOwned::to_owned),
        source: if source_all {
            None
        } else {
            request
                .source
                .as_deref()
                .and_then(non_empty)
                .map(ToOwned::to_owned)
        },
        ref_id: if source_all {
            None
        } else {
            request
                .query
                .ref_id
                .as_deref()
                .and_then(non_empty)
                .map(ToOwned::to_owned)
        },
    }
}

pub fn decode_path_value(value: &str) -> Option<String> {
    urlencoding::decode(value)
        .ok()
        .map(|value| value.into_owned())
}

pub fn page_request_from_public_url(raw_url: &str) -> std::result::Result<PageRequest, String> {
    let (raw_path, raw_query) = raw_url
        .split_once('?')
        .map_or((raw_url, ""), |(path, query)| (path, query));

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
    let mut kind = None;
    let mut source_param = None;
    let mut page = None;

    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "q" => q = Some(value.into_owned()),
            "ref" => ref_id = Some(value.into_owned()),
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
            kind,
            source: source_param,
            page,
        },
    })
}

pub fn resolve_entry_ref(
    config: &AppConfig,
    source_id: &str,
    ref_id: Option<&str>,
) -> Result<String> {
    if let Some(ref_id) = ref_id.and_then(non_empty) {
        return Ok(ref_id.to_owned());
    }

    let source = config
        .sources
        .get(source_id)
        .with_context(|| format!("unknown source {source_id:?}"))?;

    source
        .default_ref
        .clone()
        .with_context(|| format!("source {source_id:?} has no default ref"))
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

    #[test]
    fn parses_root_public_url() {
        let request = page_request_from_public_url("/").unwrap();
        assert_eq!(request.source, None);
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q, None);
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
    fn results_context_ignores_entry_path_and_kind() {
        let search = page_request_from_public_url("/fixtures?q=git&ref=small").unwrap();
        let modal = page_request_from_public_url(
            "/fixtures/programs.git.enable?q=git&ref=small&kind=option",
        )
        .unwrap();

        assert_eq!(results_context(&search), results_context(&modal));
    }

    #[test]
    fn results_context_treats_all_scope_modal_as_root_search() {
        let search = page_request_from_public_url("/?q=git").unwrap();
        let modal = page_request_from_public_url(
            "/nixpkgs/rubyPackages.git?q=git&source=all&ref=unstable&kind=package",
        )
        .unwrap();

        assert_eq!(results_context(&search), results_context(&modal));
    }

    #[test]
    fn results_context_changes_when_query_changes() {
        let git = page_request_from_public_url("/fixtures?q=git&ref=small").unwrap();
        let firefox = page_request_from_public_url("/fixtures?q=firefox&ref=small").unwrap();

        assert_ne!(results_context(&git), results_context(&firefox));
    }

    #[test]
    fn results_context_changes_when_source_changes() {
        let nixpkgs = page_request_from_public_url("/nixpkgs?q=git").unwrap();
        let nixos = page_request_from_public_url("/nixos?q=git").unwrap();

        assert_ne!(results_context(&nixpkgs), results_context(&nixos));
    }

    #[test]
    fn results_context_changes_when_scoped_ref_changes() {
        let stable = page_request_from_public_url("/fixtures?q=git&ref=stable").unwrap();
        let unstable = page_request_from_public_url("/fixtures?q=git&ref=unstable").unwrap();

        assert_ne!(results_context(&stable), results_context(&unstable));
    }

    #[test]
    fn source_filter_treats_source_all_as_all_sources() {
        let request =
            page_request_from_public_url("/nixos/programs.git.config?q=programs.git&source=all")
                .unwrap();

        assert_eq!(SourceFilter::from_request(&request), SourceFilter::All);
    }
}
