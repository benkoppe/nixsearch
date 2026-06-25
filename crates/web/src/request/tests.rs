use super::*;
use std::path::PathBuf;

use axum::http::Uri;
use nixsearch_config::app::{AppConfig, RefSetConfig};
use nixsearch_config::producer::ProducerConfig;
use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
use nixsearch_core::artifact::ArtifactKind;

fn ref_config(id: &str) -> RefConfig {
    RefConfig {
        id: id.to_owned(),
        artifact_only: false,
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
    let request = page_request_from_public_url("/fix%74ures/programs%2Egit?q=hello+world").unwrap();

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
