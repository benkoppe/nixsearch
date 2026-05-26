use nixsearch_core::SearchDocument;
use nixsearch_ingest::{parse_options_json, parse_packages_json};
use nixsearch_test_support::{
    OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, OPTION_SYSTEMD_BOOT_ENABLE, OPTION_TAILSCALE_ENABLE,
    PACKAGE_GIT, PACKAGE_PYTHON_REQUESTS, PACKAGE_RIPGREP, assert_doc_names_eq, ingest_context,
};

#[test]
fn parses_search_small_options_fixture() {
    let docs = parse_options_json(
        include_bytes!("../../../fixtures/search-small/options.json").as_slice(),
        &ingest_context(),
    )
    .unwrap();

    assert_doc_names_eq(
        &docs,
        &[
            OPTION_SYSTEMD_BOOT_ENABLE,
            OPTION_GIT_ENABLE,
            OPTION_NGINX_ENABLE,
            OPTION_TAILSCALE_ENABLE,
        ],
    );

    let git = docs
        .iter()
        .find_map(|doc| match doc {
            SearchDocument::Option(option) if option.common.name == OPTION_GIT_ENABLE => {
                Some(option)
            }
            _ => None,
        })
        .unwrap();

    assert_eq!(git.option_set.as_deref(), Some("programs"));
    assert_eq!(git.parents, ["programs", "programs.git"]);
    assert_eq!(git.declarations.len(), 1);
}

#[test]
fn parses_search_small_packages_fixture() {
    let docs = parse_packages_json(
        include_bytes!("../../../fixtures/search-small/packages.json").as_slice(),
        &ingest_context(),
    )
    .unwrap();

    assert_doc_names_eq(
        &docs,
        &[PACKAGE_GIT, PACKAGE_PYTHON_REQUESTS, PACKAGE_RIPGREP],
    );

    let ripgrep = docs
        .iter()
        .find_map(|doc| match doc {
            SearchDocument::Package(package) if package.attribute == PACKAGE_RIPGREP => {
                Some(package)
            }
            _ => None,
        })
        .unwrap();

    assert_eq!(ripgrep.main_program.as_deref(), Some("rg"));
    assert_eq!(ripgrep.platforms, ["x86_64-linux"]);
}
