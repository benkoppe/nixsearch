use camino::Utf8PathBuf;
use tempfile::tempdir;

use nixsearch_core::{DocumentKind, SearchDocument};
use nixsearch_index::{
    EntryLookup, EntryLookupResult, SearchHit, SearchIndex, SearchOptions, SearchScope,
};

use nixsearch_test_support::{
    OPTION_GIT_ENABLE, OPTION_SYSTEMD_BOOT_ENABLE, OPTION_TAILSCALE_ENABLE, PACKAGE_GIT,
    PACKAGE_RIPGREP, REF_SMALL, SOURCE_FIXTURES, SOURCE_NIXOS, SOURCE_NIXPKGS, canonical_documents,
    ingest_context_for, option_doc_for, package_doc_for, package_doc_with_main_program,
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

fn source_names(hits: &[SearchHit]) -> Vec<(&str, &str)> {
    hits.iter()
        .map(|hit| (hit.document.common().source.as_str(), hit.document.name()))
        .collect()
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

fn assert_source_name_ranks_before(
    hits: &[SearchHit],
    before_source: &str,
    before_name: &str,
    after_source: &str,
    after_name: &str,
) {
    let pairs = source_names(hits);
    let before_index = pairs
        .iter()
        .position(|pair| *pair == (before_source, before_name))
        .unwrap_or_else(|| {
            panic!("missing expected hit ({before_source:?}, {before_name:?}); got {pairs:?}")
        });

    let after_index = pairs
        .iter()
        .position(|pair| *pair == (after_source, after_name))
        .unwrap_or_else(|| {
            panic!("missing expected hit ({after_source:?}, {after_name:?}); got {pairs:?}")
        });

    assert!(
        before_index < after_index,
        "expected ({before_source:?}, {before_name:?}) to rank before ({after_source:?}, {after_name:?}); got {pairs:?}"
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
fn fuzzy_option_leaf_query_finds_plural_option_name() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "environment.systemPackages",
        "System packages installed in the environment.",
    )];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "systemPackage");

    assert_contains(&hits, "environment.systemPackages");
}

#[test]
fn fuzzy_option_leaf_prefix_query_finds_option_name() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "environment.systemPackages",
        "System packages installed in the environment.",
    )];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "systemPack");

    assert_contains(&hits, "environment.systemPackages");
}

#[test]
fn fuzzy_option_leaf_query_allows_two_edits_for_long_terms() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "environment.systemPackages",
        "Fixture option.",
    )];
    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "stemPackages");

    assert_contains(&hits, "environment.systemPackages");
}

#[test]
fn fuzzy_option_leaf_query_handles_transposed_letters() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "environment.systemPackages",
        "Fixture option.",
    )];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "systmePackages");

    assert_contains(&hits, "environment.systemPackages");
}

#[test]
fn compact_fuzzy_query_finds_split_camel_case_option_name() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "environment.systemPackages",
        "Fixture option.",
    )];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "system packages");

    assert_contains(&hits, "environment.systemPackages");
}

#[test]
fn short_query_does_not_fuzzy_match_nearby_option_name() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![option_doc_for(
        &context,
        "services.rug.enable",
        "Fixture option.",
    )];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "rg");

    assert!(
        !hits
            .iter()
            .any(|hit| hit.document.name() == "services.rug.enable"),
        "short fuzzy query should not match nearby option name; got {:?}",
        names(&hits)
    );
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
fn natural_language_query_uses_text_relevance_without_path_penalty() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![
        option_doc_for(
            &context,
            "zzz.nginx.enable",
            "Nginx provides a fast web server.",
        ),
        option_doc_for(
            &context,
            "aaa.unrelated.enable",
            "This configures a generic server process.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "web server");

    assert_ranks_before(&hits, "zzz.nginx.enable", "aaa.unrelated.enable");
}

#[test]
fn mixed_path_and_text_query_keeps_path_terms_structured() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![
        option_doc_for(
            &context,
            "services.nginx.enable",
            "Nginx provides a fast web server.",
        ),
        option_doc_for(
            &context,
            "programs.nginx.enable",
            "Nginx provides a fast web server.",
        ),
        option_doc_for(
            &context,
            "services.httpd.enable",
            "This configures a generic web server.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "services.nginx web server");

    assert_ranks_before(&hits, "services.nginx.enable", "programs.nginx.enable");
    assert_ranks_before(&hits, "services.nginx.enable", "services.httpd.enable");
}

#[test]
fn all_scope_search_surfaces_relevant_options_among_package_results() {
    const SOURCE_HOME_MANAGER: &str = "home-manager";

    let nixpkgs_context = ingest_context_for(SOURCE_NIXPKGS, REF_SMALL);
    let nixos_context = ingest_context_for(SOURCE_NIXOS, REF_SMALL);
    let home_manager_context = ingest_context_for(SOURCE_HOME_MANAGER, REF_SMALL);

    let docs = vec![
        package_doc_with_main_program(&nixpkgs_context, "git", "Git package.", "git"),
        package_doc_for(&nixpkgs_context, "rubyPackages.git", "Ruby Git package."),
        package_doc_for(
            &nixpkgs_context,
            "rubyPackages_3_4.git",
            "Ruby 3.4 Git package.",
        ),
        package_doc_for(
            &nixpkgs_context,
            "rubyPackages_4_0.git",
            "Ruby 4.0 Git package.",
        ),
        package_doc_for(&nixpkgs_context, "git-doc", "Git documentation package."),
        option_doc_for(&nixos_context, OPTION_GIT_ENABLE, "NixOS Git option."),
        option_doc_for(
            &home_manager_context,
            OPTION_GIT_ENABLE,
            "Home Manager Git option.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let result = index
        .search(SearchOptions {
            query: "git".to_owned(),
            limit: 6,
            scopes: vec![
                SearchScope {
                    source: SOURCE_NIXPKGS.to_owned(),
                    ref_id: REF_SMALL.to_owned(),
                },
                SearchScope {
                    source: SOURCE_NIXOS.to_owned(),
                    ref_id: REF_SMALL.to_owned(),
                },
                SearchScope {
                    source: SOURCE_HOME_MANAGER.to_owned(),
                    ref_id: REF_SMALL.to_owned(),
                },
            ],
            ..Default::default()
        })
        .unwrap();

    let pairs = source_names(&result.hits);

    assert_eq!(pairs.first().copied(), Some((SOURCE_NIXPKGS, "git")));
    assert!(
        pairs
            .iter()
            .take(6)
            .any(|pair| pair.0 != SOURCE_NIXPKGS && pair.1 == OPTION_GIT_ENABLE),
        "expected first results to include a relevant non-nixpkgs option; got {pairs:?}"
    );
    assert_source_name_ranks_before(
        &result.hits,
        SOURCE_NIXOS,
        OPTION_GIT_ENABLE,
        SOURCE_NIXPKGS,
        "rubyPackages_3_4.git",
    );
}

#[test]
fn source_search_demotes_repeated_package_set_variants() {
    let context = ingest_context_for(SOURCE_NIXPKGS, REF_SMALL);
    let docs = vec![
        package_doc_with_main_program(&context, "git", "Git package.", "git"),
        package_doc_for(&context, "rubyPackages.git", "Ruby Git package."),
        package_doc_for(&context, "rubyPackages_3_4.git", "Ruby 3.4 Git package."),
        package_doc_for(&context, "rubyPackages_4_0.git", "Ruby 4.0 Git package."),
        package_doc_for(&context, "git-doc", "Git documentation package."),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "git");

    assert_ranks_before(&hits, "git", "rubyPackages.git");
    assert_ranks_before(&hits, "git-doc", "rubyPackages_3_4.git");
}

#[test]
fn package_set_query_keeps_package_set_result_prominent() {
    let context = ingest_context_for(SOURCE_NIXPKGS, REF_SMALL);
    let docs = vec![
        package_doc_with_main_program(&context, "git", "Git package.", "git"),
        package_doc_for(&context, "rubyPackages.git", "Ruby Git package."),
        package_doc_for(&context, "rubyPackages_3_4.git", "Ruby 3.4 Git package."),
        package_doc_for(&context, "rubyPackages_4_0.git", "Ruby 4.0 Git package."),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "rubyPackages git");

    assert_eq!(
        hits.first().map(|hit| hit.document.name()),
        Some("rubyPackages.git")
    );
}

#[test]
fn structured_option_query_prefers_path_prefix_matches() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![
        option_doc_for(&context, "services.git-sync.enable", "Git sync service."),
        option_doc_for(
            &context,
            "services.gitDaemon.package",
            "Git daemon package.",
        ),
        option_doc_for(
            &context,
            "services.gitlab-runner.services",
            "GitLab Runner services.",
        ),
        option_doc_for(&context, OPTION_GIT_ENABLE, "Git program option."),
        option_doc_for(
            &context,
            "hjem.users.<username>.systemd.services",
            "User services.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "services.git");

    assert_ranks_before(&hits, "services.git-sync.enable", OPTION_GIT_ENABLE);
    assert_ranks_before(
        &hits,
        "services.gitDaemon.package",
        "hjem.users.<username>.systemd.services",
    );
    assert_ranks_before(
        &hits,
        "services.gitlab-runner.services",
        "hjem.users.<username>.systemd.services",
    );
}

#[test]
fn structured_option_query_prefers_shallower_path_prefix_matches() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = vec![
        option_doc_for(&context, "services.git-sync.enable", "Git sync service."),
        option_doc_for(
            &context,
            "services.git-sync.repositories.<name>.extraPackages",
            "Extra packages available to git-sync.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "services.git");

    assert_ranks_before(
        &hits,
        "services.git-sync.enable",
        "services.git-sync.repositories.<name>.extraPackages",
    );
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
fn exact_name_match_ranks_before_fuzzy_name_match() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

    let docs = vec![
        option_doc_for(
            &context,
            "environment.systemPackage",
            "Exact singular system package option.",
        ),
        option_doc_for(
            &context,
            "environment.systemPackages",
            "Plural system packages option.",
        ),
    ];

    let (_tempdir, index) = build_index(docs);

    let hits = search(&index, "systemPackage");

    assert_ranks_before(
        &hits,
        "environment.systemPackage",
        "environment.systemPackages",
    );
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
fn search_total_and_hits_continue_beyond_rerank_window() {
    let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
    let docs = (0..1_010)
        .map(|index| {
            let name = format!("generated.option.{index:04}");
            option_doc_for(&context, &name, "Contains paginationwindowtoken.")
        })
        .collect();

    let (_tempdir, index) = build_index(docs);

    let crossing_result = index
        .search(SearchOptions {
            query: "paginationwindowtoken".to_owned(),
            limit: 20,
            offset: 990,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(crossing_result.total, 1_010);
    assert_eq!(crossing_result.hits.len(), 20);

    let beyond_result = index
        .search(SearchOptions {
            query: "paginationwindowtoken".to_owned(),
            limit: 20,
            offset: 1_000,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(beyond_result.total, 1_010);
    assert_eq!(beyond_result.hits.len(), 10);
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
