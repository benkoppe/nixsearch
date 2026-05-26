use camino::Utf8PathBuf;
use tempfile::tempdir;

use nixsearch_core::{DocumentKind, SearchDocument};
use nixsearch_index::{
    EntryLookup, EntryLookupResult, SearchHit, SearchIndex, SearchOptions, SearchScope,
};

use nixsearch_test_support::{
    OPTION_GIT_ENABLE, OPTION_SYSTEMD_BOOT_ENABLE, OPTION_TAILSCALE_ENABLE, PACKAGE_GIT,
    PACKAGE_RIPGREP, REF_SMALL, SOURCE_FIXTURES, canonical_documents, ingest_context_for,
    option_doc_for, package_doc_for, package_doc_with_main_program,
};

fn build_index(docs: Vec<SearchDocument>) -> (tempfile::TempDir, SearchIndex) {
    let tempdir = tempdir().unwrap();
    let index_path = Utf8PathBuf::from_path_buf(tempdir.path().to_path_buf())
        .expect("test path must be valid UTF-8");

    let index = SearchIndex::create_or_replace(&index_path).unwrap();
    let mut writer = index.writer().unwrap();

    for doc in &docs {
        writer.add_document(doc).unwrap();
    }

    writer.commit().unwrap();

    let index = SearchIndex::open(&index_path).unwrap();

    (tempdir, index)
}

fn search(index: &SearchIndex, query: &str) -> Vec<SearchHit> {
    index
        .search(SearchOptions {
            query: query.to_owned(),
            limit: 20,
            ..Default::default()
        })
        .unwrap()
        .hits
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

    let result = index
        .search(SearchOptions {
            query: "enable".to_owned(),
            limit: 2,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(result.hits.len(), 2);
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

    let result = index
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
            ..Default::default()
        })
        .unwrap();

    let pairs = result
        .hits
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

#[test]
fn gets_document_by_id() {
    let docs = canonical_documents();
    let expected_id = docs[0].id().to_owned();

    let (_tempdir, index) = build_index(docs);

    let document = index.get_by_id(&expected_id).unwrap().unwrap();

    assert_eq!(document.id(), expected_id);
}

#[test]
fn get_by_id_returns_none_for_missing_id() {
    let (_tempdir, index) = build_index(canonical_documents());

    let document = index.get_by_id("missing/source/ref/option/name").unwrap();

    assert!(document.is_none());
}

#[test]
fn finds_entry_by_source_ref_name() {
    let (_tempdir, index) = build_index(canonical_documents());

    let result = index
        .find_entry(EntryLookup {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            name: OPTION_GIT_ENABLE.to_owned(),
            kind: Some(DocumentKind::Option),
        })
        .unwrap();

    let EntryLookupResult::Found(document) = result else {
        panic!("expected entry to be found");
    };

    assert_eq!(document.name(), OPTION_GIT_ENABLE);
    assert_eq!(document.common().source, SOURCE_FIXTURES);
    assert_eq!(document.common().ref_id, REF_SMALL);
    assert_eq!(document.kind(), &DocumentKind::Option);
}

#[test]
fn find_entry_returns_not_found() {
    let (_tempdir, index) = build_index(canonical_documents());

    let result = index
        .find_entry(EntryLookup {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            name: "missing.entry".to_owned(),
            kind: None,
        })
        .unwrap();

    assert!(matches!(result, EntryLookupResult::NotFound));
}

#[test]
fn find_entry_uses_kind_to_disambiguate() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

    let docs = vec![
        option_doc_for(&context, "git", "Git option."),
        package_doc_for(&context, "git", "Git package."),
    ];

    let (_tempdir, index) = build_index(docs);

    let result = index
        .find_entry(EntryLookup {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            name: "git".to_owned(),
            kind: Some(DocumentKind::Package),
        })
        .unwrap();

    let EntryLookupResult::Found(document) = result else {
        panic!("expected package entry to be found");
    };

    assert_eq!(document.name(), "git");
    assert_eq!(document.kind(), &DocumentKind::Package);
}

#[test]
fn find_entry_returns_ambiguous_without_kind() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

    let docs = vec![
        option_doc_for(&context, "git", "Git option."),
        package_doc_for(&context, "git", "Git package."),
    ];

    let (_tempdir, index) = build_index(docs);

    let result = index
        .find_entry(EntryLookup {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            name: "git".to_owned(),
            kind: None,
        })
        .unwrap();

    let EntryLookupResult::Ambiguous(documents) = result else {
        panic!("expected ambiguous entry lookup");
    };

    let kinds = documents
        .iter()
        .map(|document| document.kind())
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 2);
    assert!(kinds.contains(&&DocumentKind::Option));
    assert!(kinds.contains(&&DocumentKind::Package));
}
