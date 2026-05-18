use tempfile::tempdir;

use nix_search_core::SearchDocument;
use nix_search_index::{SearchHit, SearchIndex, SearchOptions, SearchScope};
use nix_search_test_support::{
    OPTION_GIT_ENABLE, OPTION_SYSTEMD_BOOT_ENABLE, OPTION_TAILSCALE_ENABLE, PACKAGE_GIT,
    PACKAGE_RIPGREP, REF_SMALL, SOURCE_FIXTURES, canonical_documents, ingest_context_for,
    option_doc_for, package_doc_with_main_program,
};

fn build_index(docs: Vec<SearchDocument>) -> (tempfile::TempDir, SearchIndex) {
    let tempdir = tempdir().unwrap();

    let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
    let mut writer = index.writer().unwrap();

    for doc in &docs {
        writer.add_document(doc).unwrap();
    }

    writer.commit().unwrap();

    let index = SearchIndex::open(tempdir.path()).unwrap();

    (tempdir, index)
}

fn search(index: &SearchIndex, query: &str) -> Vec<SearchHit> {
    index
        .search(SearchOptions {
            query: query.to_owned(),
            limit: 20,
            scopes: Vec::new(),
        })
        .unwrap()
}

fn names(hits: &[SearchHit]) -> Vec<&str> {
    hits.iter().map(|hit| hit.document.name()).collect()
}

fn assert_contains(hits: &[SearchHit], name: &str) {
    assert!(
        hits.iter().any(|hit| hit.document.name() == name),
        "expected hits to contain {name:?}; got {:?}",
        names(hits)
    );
}

fn assert_ranks_before(hits: &[SearchHit], before: &str, after: &str) {
    let before_index = hits
        .iter()
        .position(|hit| hit.document.name() == before)
        .unwrap_or_else(|| panic!("missing expected hit {before:?}; got {:?}", names(hits)));

    let after_index = hits
        .iter()
        .position(|hit| hit.document.name() == after)
        .unwrap_or_else(|| panic!("missing expected hit {after:?}; got {:?}", names(hits)));

    assert!(
        before_index < after_index,
        "expected {before:?} to rank before {after:?}; got {:?}",
        names(hits)
    );
}

#[test]
fn exact_option_name_query_finds_option() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = search(&index, OPTION_GIT_ENABLE);

    assert_contains(&hits, OPTION_GIT_ENABLE);
}

#[test]
fn description_query_finds_matching_option() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = search(&index, "EFI");

    assert_contains(&hits, OPTION_SYSTEMD_BOOT_ENABLE);
}

#[test]
fn group_query_finds_nested_option() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = search(&index, "services.tailscale");

    assert_contains(&hits, OPTION_TAILSCALE_ENABLE);
}

#[test]
fn package_attribute_query_finds_package() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = search(&index, PACKAGE_GIT);

    assert_contains(&hits, PACKAGE_GIT);
}

#[test]
fn package_main_program_query_finds_package() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = search(&index, "rg");

    assert_contains(&hits, PACKAGE_RIPGREP);
}

#[test]
fn exact_name_match_ranks_before_description_only_match() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

    let docs = vec![
        option_doc_for(
            &context,
            OPTION_GIT_ENABLE,
            "Whether to enable Git integration.",
        ),
        option_doc_for(
            &context,
            "services.example.enable",
            "This option mentions programs.git.enable in its description.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, OPTION_GIT_ENABLE);

    assert_ranks_before(&hits, OPTION_GIT_ENABLE, "services.example.enable");
}

#[test]
fn search_limit_is_respected() {
    let (_tempdir, index) = build_index(canonical_documents());

    let hits = index
        .search(SearchOptions {
            query: "enable".to_owned(),
            limit: 2,
            scopes: Vec::new(),
        })
        .unwrap();

    assert_eq!(hits.len(), 2);
}

#[test]
fn multiple_scopes_are_ored_by_source_ref_pair() {
    let stable_context = ingest_context_for("nixos", "stable");
    let unstable_context = ingest_context_for("home-manager", "unstable");

    let docs = vec![
        option_doc_for(&stable_context, OPTION_GIT_ENABLE, "Stable Git option."),
        option_doc_for(
            &unstable_context,
            OPTION_GIT_ENABLE,
            "Home Manager Git option.",
        ),
        option_doc_for(
            &ingest_context_for("nixos", "unstable"),
            OPTION_GIT_ENABLE,
            "Unselected Git option.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = index
        .search(SearchOptions {
            query: OPTION_GIT_ENABLE.to_owned(),
            limit: 20,
            scopes: vec![
                SearchScope {
                    source: "nixos".to_owned(),
                    ref_id: "stable".to_owned(),
                },
                SearchScope {
                    source: "home-manager".to_owned(),
                    ref_id: "unstable".to_owned(),
                },
            ],
        })
        .unwrap();

    let pairs = hits
        .iter()
        .map(|hit| {
            (
                hit.document.common().source.as_str(),
                hit.document.common().ref_id.as_str(),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(pairs.len(), 2);
    assert!(pairs.contains(&("nixos", "stable")));
    assert!(pairs.contains(&("home-manager", "unstable")));
}

#[test]
fn indexed_document_round_trips_from_stored_json() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let original =
        package_doc_with_main_program(&context, "ripgrep", "Line-oriented search tool.", "rg");

    let (_tempdir, index) = build_index(vec![original.clone()]);

    let hits = search(&index, "rg");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].document, original);
}
