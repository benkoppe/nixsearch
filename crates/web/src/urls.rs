use nixsearch_config::app::AppConfig;
use nixsearch_core::document::SearchDocument;
use nixsearch_index::search::SearchHit;
use nixsearch_service::SitemapCandidate;

use crate::entry::AnnotatedEntryDocument;
#[cfg(test)]
use crate::request::PageRequest;
use crate::request::{LinkOrigin, PageQuery, PageState, SourceFilter, non_empty};

pub fn canonical_home_path() -> String {
    "/".to_owned()
}

pub fn source_path(source: &str) -> String {
    format!("/{}", encode_path(source))
}

pub fn search_url_for(source: Option<&str>, query: &PageQuery) -> String {
    let path = source.map(source_path).unwrap_or_else(|| "/".to_owned());
    let page_str = query.page.filter(|&p| p > 1).map(|p| p.to_string());

    let qs = query_string([
        ("q", query.q.as_deref()),
        ("ref", query.ref_id.as_deref()),
        ("ref_set", query.ref_set.as_deref()),
        ("source", query.source.map(|s| s.as_str())),
        ("page", page_str.as_deref()),
    ]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

pub fn canonical_source_path(config: &AppConfig, source: &str, ref_id: &str) -> String {
    search_url_for(
        Some(source),
        &PageQuery {
            ref_id: ref_id_for_link(config, source, ref_id),
            ..PageQuery::default()
        },
    )
}

pub fn sitemap_candidate_path(candidate: &SitemapCandidate) -> String {
    entry_url_for(&candidate.source, &candidate.name, &PageQuery::default())
}

#[cfg(test)]
fn canonical_entry_path(config: &AppConfig, source: &str, entry: &str, ref_id: &str) -> String {
    entry_url_for(
        source,
        entry,
        &PageQuery {
            ref_id: ref_id_for_link(config, source, ref_id),
            ..PageQuery::default()
        },
    )
}

fn entry_url_for(source: &str, entry: &str, query: &PageQuery) -> String {
    let path = format!("{}/{}", source_path(source), encode_path(entry));
    let page_str = query.page.filter(|&p| p > 1).map(|p| p.to_string());

    let qs = query_string([
        ("q", query.q.as_deref()),
        ("ref", query.ref_id.as_deref()),
        ("ref_set", query.ref_set.as_deref()),
        ("source", query.source.map(|s| s.as_str())),
        ("page", page_str.as_deref()),
    ]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

#[cfg(test)]
pub fn close_url_for(request: &PageRequest) -> String {
    if request.query.source == Some(LinkOrigin::All) {
        return search_url_for(
            None,
            &PageQuery {
                q: request.query.q.clone(),
                ref_set: request.query.ref_set.clone(),
                page: request.query.page,
                ..PageQuery::default()
            },
        );
    }

    search_url_for(
        request.source.as_deref(),
        &PageQuery {
            q: request.query.q.clone(),
            ref_id: request.query.ref_id.clone(),
            page: request.query.page,
            ..PageQuery::default()
        },
    )
}

pub fn search_url_for_state(config: &AppConfig, state: &PageState) -> String {
    match &state.source_filter {
        SourceFilter::All => all_url_from_state(config, state),
        SourceFilter::Named(source) => source_url_from_state(config, state, source),
    }
}

pub fn all_url_from_state(config: &AppConfig, state: &PageState) -> String {
    search_url_for(
        None,
        &PageQuery {
            q: state.q.clone(),
            ref_set: state
                .active_ref_set()
                .and_then(|ref_set| ref_set_for_link(config, ref_set)),
            page: state.page,
            ..PageQuery::default()
        },
    )
}

pub fn all_tab_url_from_state(config: &AppConfig, state: &PageState) -> String {
    let mut state = state.clone();
    state.page = None;
    all_url_from_state(config, &state)
}

pub fn source_url_from_state(config: &AppConfig, state: &PageState, source: &str) -> String {
    let active_ref_set = state.active_ref_set();
    let ref_set_refs =
        active_ref_set.and_then(|ref_set| config.refs_for_ref_set_source(ref_set, source));
    let source_ref = ref_set_refs
        .and_then(|refs| source_ref_for_refs(state, source, refs))
        .or_else(|| match &state.source_filter {
            SourceFilter::Named(current_source) if current_source == source => {
                state.source_ref.clone()
            }
            _ => None,
        })
        .or_else(|| {
            config
                .sources
                .get(source)
                .and_then(|source| source.default_ref.clone())
        });

    let ref_set = active_ref_set.and_then(|ref_set| ref_set_for_link(config, ref_set));
    let source_default_ref = config
        .sources
        .get(source)
        .and_then(|source| source.default_ref.as_deref());
    let ref_id = match ref_set_refs {
        Some(refs)
            if refs.len() == 1
                && (ref_set.is_some() || source_ref.as_deref() == source_default_ref) =>
        {
            None
        }
        Some(_) => source_ref.clone(),
        None => source_ref.clone(),
    };
    let ref_id = match ref_set_refs {
        Some(refs) if refs.len() > 1 => ref_id,
        _ => ref_id
            .as_deref()
            .and_then(|ref_id| ref_id_for_link(config, source, ref_id)),
    };

    search_url_for(
        Some(source),
        &PageQuery {
            q: state.q.clone(),
            ref_id,
            ref_set,
            page: state.page,
            ..PageQuery::default()
        },
    )
}

pub fn source_tab_url_from_state(config: &AppConfig, state: &PageState, source: &str) -> String {
    let mut state = state.clone();
    state.page = None;
    source_url_from_state(config, &state, source)
}

pub fn entry_url_for_hit(
    config: &AppConfig,
    state: &PageState,
    hit: &SearchHit,
    page: Option<usize>,
) -> String {
    entry_url_for_document(config, state, &hit.document, page)
}

pub fn entry_url_for_annotated_document(
    config: &AppConfig,
    state: &PageState,
    entry: &AnnotatedEntryDocument,
    page: Option<usize>,
) -> String {
    entry_url_for_document(config, state, &entry.document, page)
}

pub fn canonical_entry_path_for_document(config: &AppConfig, document: &SearchDocument) -> String {
    let common = document.common();

    entry_url_for(
        &common.source,
        &common.name,
        &PageQuery {
            ref_id: ref_id_for_link(config, &common.source, &common.ref_id),
            ..PageQuery::default()
        },
    )
}

fn entry_url_for_document(
    config: &AppConfig,
    state: &PageState,
    document: &SearchDocument,
    page: Option<usize>,
) -> String {
    let common = document.common();
    let from_scope = matches!(state.source_filter, SourceFilter::All).then_some(LinkOrigin::All);
    let ref_set = state.active_ref_set().filter(|ref_set| {
        config.ref_set_contains_source_ref(ref_set, &common.source, &common.ref_id)
    });
    let ref_set_refs =
        ref_set.and_then(|ref_set| config.refs_for_ref_set_source(ref_set, &common.source));
    let ref_id = match ref_set_refs {
        Some(refs) if refs.len() == 1 => None,
        _ => Some(common.ref_id.as_str()),
    };
    let ref_id = match ref_set_refs {
        Some(refs) if refs.len() > 1 => ref_id.map(ToOwned::to_owned),
        _ => ref_id.and_then(|ref_id| ref_id_for_link(config, &common.source, ref_id)),
    };

    entry_url_for(
        &common.source,
        &common.name,
        &PageQuery {
            q: state.q.clone(),
            ref_id,
            ref_set: ref_set.and_then(|ref_set| ref_set_for_link(config, ref_set)),
            source: from_scope,
            page,
        },
    )
}

fn source_ref_for_refs(state: &PageState, source: &str, refs: &[String]) -> Option<String> {
    match &state.source_filter {
        SourceFilter::Named(current_source) if current_source == source => state
            .source_ref
            .as_ref()
            .filter(|ref_id| refs.iter().any(|candidate| candidate == *ref_id))
            .cloned()
            .or_else(|| refs.first().cloned()),
        _ => refs.first().cloned(),
    }
}

pub fn close_url_for_state(config: &AppConfig, state: &PageState) -> String {
    match &state.source_filter {
        SourceFilter::All => all_url_from_state(config, state),
        SourceFilter::Named(_) => search_url_for_state(config, state),
    }
}

pub fn ref_id_for_link(config: &AppConfig, source: &str, ref_id: &str) -> Option<String> {
    let default_ref = config
        .sources
        .get(source)
        .and_then(|source| source.default_ref.as_deref());

    if default_ref == Some(ref_id) {
        None
    } else {
        Some(ref_id.to_owned())
    }
}

pub fn ref_set_for_link(config: &AppConfig, ref_set: &str) -> Option<String> {
    if config.default_ref_set() == Some(ref_set) {
        None
    } else {
        Some(ref_set.to_owned())
    }
}

fn encode_path(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn query_string<const N: usize>(pairs: [(&str, Option<&str>); N]) -> String {
    pairs
        .into_iter()
        .filter_map(|(key, value)| {
            let value = value.and_then(non_empty)?;
            Some(format!("{}={}", encode_query(key), encode_query(value)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use nixsearch_config::app::{AppConfig, RefSetConfig};
    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::SearchDocument;
    use nixsearch_core::target::RefRole;
    use nixsearch_index::annotation::SearchHitAnnotation;
    use nixsearch_test_support::{
        REF_SMALL, SOURCE_FIXTURES, app_config, ingest_context_for, package_doc_for,
    };

    use crate::request::{LinkOrigin, PageQuery, PageRequest, page_state};

    fn ref_config(id: &str) -> RefConfig {
        RefConfig {
            id: id.to_owned(),
            role: RefRole::Search,
            producer: ProducerConfig::ExistingFile {
                path: PathBuf::from("unused.json"),
                artifact: ArtifactKind::OptionsJson,
            },
            source_links: None,
        }
    }

    fn handoff_config() -> AppConfig {
        AppConfig {
            sources: [(
                "nixpkgs".to_owned(),
                SourceConfig {
                    name: None,
                    color: None,
                    kind: SourceKind::Packages,
                    strip_prefixes: Vec::new(),
                    default_ref: Some("nixos-unstable".to_owned()),
                    refs: vec![ref_config("nixos-unstable"), ref_config("nixos-25.11")],
                },
            )]
            .into(),
            ref_sets: [
                (
                    "unstable".to_owned(),
                    RefSetConfig {
                        refs: [("nixpkgs".to_owned(), vec!["nixos-unstable".to_owned()])].into(),
                    },
                ),
                (
                    "25.11".to_owned(),
                    RefSetConfig {
                        refs: [("nixpkgs".to_owned(), vec!["nixos-25.11".to_owned()])].into(),
                    },
                ),
                (
                    "multi".to_owned(),
                    RefSetConfig {
                        refs: [(
                            "nixpkgs".to_owned(),
                            vec!["nixos-unstable".to_owned(), "nixos-25.11".to_owned()],
                        )]
                        .into(),
                    },
                ),
            ]
            .into(),
            ..AppConfig::default()
        }
    }

    fn hit(document: SearchDocument) -> SearchHit {
        SearchHit {
            annotation: SearchHitAnnotation {
                unique_within_kind: true,
            },
            score: 1.0,
            document,
        }
    }

    #[test]
    fn search_url_for_root_with_query() {
        let url = search_url_for(
            None,
            &PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(url, "/?q=git");
    }

    #[test]
    fn search_url_for_source_with_query_and_ref() {
        let url = search_url_for(
            Some("fixtures"),
            &PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(url, "/fixtures?q=git&ref=small");
    }

    #[test]
    fn search_url_for_root_with_ref_set() {
        let url = search_url_for(
            None,
            &PageQuery {
                q: Some("git".to_owned()),
                ref_set: Some("25.11".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(url, "/?q=git&ref_set=25.11");
    }

    #[test]
    fn entry_url_for_hit_preserves_result_context() {
        let config = app_config("./data/indexes");
        let request = PageRequest {
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_set: Some("unused".to_owned()),
                source: Some(LinkOrigin::All),
                page: Some(2),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let mut state = page_state(&config, &request);
        state.set_explicit_ref_set("unused".to_owned());
        let document = package_doc_for(
            &ingest_context_for(SOURCE_FIXTURES, REF_SMALL),
            "git",
            "Git package.",
        );

        assert_eq!(
            entry_url_for_hit(&config, &state, &hit(document), Some(2)),
            "/fixtures/git?q=git&source=all&page=2"
        );
    }

    #[test]
    fn canonical_entry_path_for_document_uses_clean_path() {
        let config = app_config("./data/indexes");
        let clean_document = package_doc_for(
            &ingest_context_for(SOURCE_FIXTURES, REF_SMALL),
            "ripgrep",
            "Ripgrep package.",
        );

        assert_eq!(
            canonical_entry_path_for_document(&config, &clean_document),
            "/fixtures/ripgrep"
        );
    }

    #[test]
    fn close_url_for_strips_entry_segment() {
        let request = PageRequest {
            source: Some("fixtures".to_owned()),
            entry: Some("programs.git.enable".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                source: None,
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/fixtures?q=git&ref=small");
    }

    #[test]
    fn close_url_for_returns_root_when_no_source() {
        let request = PageRequest {
            source: None,
            entry: None,
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/?q=git");
    }

    #[test]
    fn close_url_for_all_scope_returns_to_root() {
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            entry: Some("rubyPackages.git".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                ref_set: Some("25.11".to_owned()),
                source: Some(LinkOrigin::All),
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/?q=git&ref_set=25.11");
    }

    #[test]
    fn source_url_does_not_serialize_inferred_ref_set() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            source_url_from_state(&config, &state, "nixpkgs"),
            "/nixpkgs?q=git&ref=nixos-25.11"
        );

        assert_eq!(all_url_from_state(&config, &state), "/?q=git");
    }

    #[test]
    fn source_url_prefers_unique_ref_set_over_ref() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                ref_set: Some("25.11".to_owned()),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            source_url_from_state(&config, &state, "nixpkgs"),
            "/nixpkgs?q=git&ref_set=25.11"
        );
    }

    #[test]
    fn source_url_includes_ref_when_ref_set_is_ambiguous() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                ref_set: Some("multi".to_owned()),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            source_url_from_state(&config, &state, "nixpkgs"),
            "/nixpkgs?q=git&ref=nixos-25.11&ref_set=multi"
        );
    }

    #[test]
    fn search_url_for_state_preserves_source_page() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                page: Some(3),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            search_url_for_state(&config, &state),
            "/nixpkgs?q=git&ref=nixos-25.11&page=3"
        );
    }

    #[test]
    fn source_tab_url_resets_page() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                page: Some(3),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            source_tab_url_from_state(&config, &state, "nixpkgs"),
            "/nixpkgs?q=git&ref=nixos-25.11"
        );
    }

    #[test]
    fn all_tab_url_resets_page() {
        let config = handoff_config();
        let request = PageRequest {
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_set: Some("25.11".to_owned()),
                page: Some(3),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let state = page_state(&config, &request);

        assert_eq!(
            all_tab_url_from_state(&config, &state),
            "/?q=git&ref_set=25.11"
        );
    }

    #[test]
    fn close_url_for_state_preserves_source_page() {
        let config = handoff_config();
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            entry: Some("git".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("nixos-25.11".to_owned()),
                page: Some(3),
                ..PageQuery::default()
            },
        };
        let state = page_state(&config, &request);

        assert_eq!(
            close_url_for_state(&config, &state),
            "/nixpkgs?q=git&ref=nixos-25.11&page=3"
        );
    }

    #[test]
    fn canonical_source_path_omits_default_ref() {
        let config = handoff_config();

        assert_eq!(
            canonical_source_path(&config, "nixpkgs", "nixos-unstable"),
            "/nixpkgs"
        );
    }

    #[test]
    fn canonical_source_path_preserves_non_default_ref() {
        let config = handoff_config();

        assert_eq!(
            canonical_source_path(&config, "nixpkgs", "nixos-25.11"),
            "/nixpkgs?ref=nixos-25.11"
        );
    }

    #[test]
    fn canonical_entry_path_omits_default_ref() {
        let config = handoff_config();

        assert_eq!(
            canonical_entry_path(&config, "nixpkgs", "rubyPackages.git", "nixos-unstable"),
            "/nixpkgs/rubyPackages.git"
        );
    }

    #[test]
    fn canonical_entry_path_preserves_non_default_ref() {
        let config = handoff_config();

        assert_eq!(
            canonical_entry_path(&config, "nixpkgs", "rubyPackages.git", "nixos-25.11"),
            "/nixpkgs/rubyPackages.git?ref=nixos-25.11"
        );
    }
}
