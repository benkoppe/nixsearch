use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use nix_search_index::{IndexGenerationManifest, IndexStore, IndexTargetManifest, SearchIndex};
use nix_search_test_support::{
    OPTION_GIT_ENABLE, REF_SMALL, SOURCE_FIXTURES, canonical_documents, write_config,
};

fn build_published_index(index_root: &std::path::Path) {
    let store = IndexStore::new(index_root);
    let generation = store.create_generation_path().unwrap();

    let index = SearchIndex::create_or_replace(&generation).unwrap();
    let mut writer = index.writer().unwrap();

    for doc in canonical_documents() {
        writer.add_document(&doc).unwrap();
    }

    writer.commit().unwrap();

    let manifest = IndexGenerationManifest::new(
        7,
        vec![IndexTargetManifest {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            artifact_kind: nix_search_core::ArtifactKind::OptionsJson,
            document_count: 7,
            artifact_hash: Some("fixture-hash".to_owned()),
            revision: Some("fixture-revision".to_owned()),
        }],
    );

    store.write_manifest(&generation, &manifest).unwrap();
    store.publish(&generation).unwrap();
}

#[test]
fn check_config_accepts_valid_fixture_config() {
    let tempdir = tempdir().unwrap();
    let index_dir = tempdir.path().join("indexes");
    let config_path = write_config(&tempdir, &index_dir);

    Command::cargo_bin("nix-search")
        .unwrap()
        .args(["check-config", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"))
        .stdout(predicate::str::contains("sources = 1"));
}

#[test]
fn search_reads_published_index_and_prints_result() {
    let tempdir = tempdir().unwrap();
    let index_dir = tempdir.path().join("indexes");
    let config_path = write_config(&tempdir, &index_dir);

    build_published_index(&index_dir);

    Command::cargo_bin("nix-search")
        .unwrap()
        .args(["search", OPTION_GIT_ENABLE, "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains(OPTION_GIT_ENABLE));
}

#[test]
fn index_inspect_prints_current_manifest() {
    let tempdir = tempdir().unwrap();
    let index_dir = tempdir.path().join("indexes");
    let config_path = write_config(&tempdir, &index_dir);

    build_published_index(&index_dir);

    Command::cargo_bin("nix-search")
        .unwrap()
        .args(["index", "inspect", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("documents = 7"))
        .stdout(predicate::str::contains(SOURCE_FIXTURES));
}

#[test]
fn missing_config_file_fails_cleanly() {
    Command::cargo_bin("nix-search")
        .unwrap()
        .args([
            "check-config",
            "--config",
            "/definitely/missing/nix-search.toml",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configuration check failed"));
}
