use nixsearch_config::app::AppConfig;

use super::{PageRequest, non_empty, normalized_query};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Named(String),
}

impl SourceFilter {
    pub fn from_request(request: &PageRequest) -> Self {
        if request.query.source == Some(super::LinkOrigin::All) && request.entry.is_some() {
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
