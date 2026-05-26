use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

use nixsearch_index_test_support::publish_canonical_options_index;
use nixsearch_test_support::{OPTION_GIT_ENABLE, SOURCE_FIXTURES, utf8_path_buf, write_config};

#[test]
fn check_config_accepts_valid_fixture_config() {
    let tempdir = tempdir().unwrap();
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    Command::cargo_bin("nixsearch")
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
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    publish_canonical_options_index(&index_dir);

    Command::cargo_bin("nixsearch")
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
    let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
    let config_path = write_config(&tempdir, &index_dir);

    publish_canonical_options_index(&index_dir);

    Command::cargo_bin("nixsearch")
        .unwrap()
        .args(["index", "inspect", "--config"])
        .arg(&config_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("documents = 4"))
        .stdout(predicate::str::contains(SOURCE_FIXTURES));
}

#[test]
fn missing_config_file_fails_cleanly() {
    Command::cargo_bin("nixsearch")
        .unwrap()
        .args([
            "check-config",
            "--config",
            "/definitely/missing/nixsearch.toml",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("configuration check failed"));
}
