use std::fs;
use std::path::PathBuf;

use camino::Utf8PathBuf;
use tempfile::{TempDir, tempdir};

use nixsearch_core::{ArtifactKind, SourceLinkConfig};

use crate::AppConfig;
use crate::producer::{DownloadCompression, EvalModuleConfig, ProducerConfig, ProducerKind};
use crate::source::{
    HJEM_COLOR, HJEM_RUM_COLOR, HOME_MANAGER_COLOR, NIX_DARWIN_COLOR, NIXOS_COLOR, NIXPKGS_COLOR,
    SourceKind,
};

const FIXTURES_SOURCE: &str = "fixtures";
const NIXOS_SOURCE: &str = "nixos";
const NIXPKGS_SOURCE: &str = "nixpkgs";
const SMALL_REF: &str = "small";
const UNSTABLE_REF: &str = "unstable";
const NIXOS_UNSTABLE_REF: &str = "nixos-unstable";
const NIXOS_STABLE_REF: &str = "nixos-25.11";
const FIXTURE_OPTIONS_PATH: &str = "fixtures/search-small/options.json";

fn load_toml(toml: &str) -> AppConfig {
    let dir = tempdir().unwrap();
    let path = write_toml(&dir, toml);

    AppConfig::load(Some(&path)).unwrap()
}

fn load_toml_error(toml: &str) -> String {
    let dir = tempdir().unwrap();
    let path = write_toml(&dir, toml);

    AppConfig::load(Some(&path)).unwrap_err().to_string()
}

fn write_toml(dir: &TempDir, toml: &str) -> PathBuf {
    let path = dir.path().join("nixsearch.toml");
    fs::write(&path, toml).unwrap();
    path
}

fn fixture_existing_file_source_toml() -> &'static str {
    r#"
    [sources.fixtures]
    name = "Fixtures"
    kind = "options"

    [sources.fixtures.refs.small.producer]
    type = "existing-file"
    path = "fixtures/search-small/options.json"
    artifact = "options-json"
    "#
}

fn fixture_two_ref_source_toml(default_ref: Option<&str>) -> String {
    let default_ref = default_ref
        .map(|value| format!(r#"default_ref = "{value}""#))
        .unwrap_or_default();

    format!(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"
        {default_ref}

        [sources.fixtures.refs.stable.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"

        [sources.fixtures.refs.unstable.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "#
    )
}

fn assert_single_scope(
    scopes: &[crate::app::ResolvedSearchScope],
    expected_source: &str,
    expected_ref: &str,
) {
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].source, expected_source);
    assert_eq!(scopes[0].ref_id, expected_ref);
}

fn assert_error_contains(error: &str, expected: &str) {
    assert!(
        error.contains(expected),
        "expected error to contain {expected:?}, got {error:?}"
    );
}

#[test]
fn default_config_is_valid() {
    let config = AppConfig::load(None).unwrap();

    assert_eq!(config.data.artifact_url, "file://./data/artifacts");
    assert_eq!(config.data.index_dir, Utf8PathBuf::from("./data/indexes"));
    assert_eq!(config.server.listen, "127.0.0.1:3000");
    assert!(config.sources.is_empty());
}

#[test]
fn loads_config_file() {
    let config = load_toml(
        r#"
        [data]
        artifact_url = "file://./tmp/artifacts"
        index_dir = "./tmp/indexes"

        [server]
        listen = "0.0.0.0:8080"

        [sources.nixos]
        name = "NixOS Options"
        kind = "options"

        [sources.nixos.refs.unstable.producer]
        type = "nix-build-options-json"
        ref = "github:NixOS/nixpkgs/nixos-unstable"
        attribute = "options"
        import_path = "nixos/release.nix"
        output_path = "share/doc/nixos/options.json"

        [sources.nixpkgs]
        name = "Nixpkgs"
        kind = "packages"

        [sources.nixpkgs.refs.unstable.producer]
        type = "channel-packages-json"
        channel = "nixos-unstable"
        "#,
    );

    assert_eq!(config.data.artifact_url, "file://./tmp/artifacts");
    assert_eq!(config.server.listen, "0.0.0.0:8080");
    assert!(config.server.bootstrap);
    assert!(!config.server.schedule.enabled);
    assert_eq!(config.server.schedule.interval, "24h");

    let options = &config.sources[NIXOS_SOURCE];
    assert_eq!(options.name.as_deref(), Some("NixOS Options"));
    assert_eq!(options.kind, SourceKind::Options);
    assert_eq!(
        options.refs[0].producer.kind(),
        ProducerKind::NixBuildOptionsJson
    );

    match &options.refs[0].producer {
        ProducerConfig::NixBuildOptionsJson { nix_path_name, .. } => {
            assert_eq!(nix_path_name, "nixpkgs");
        }
        other => panic!("unexpected producer: {other:?}"),
    }

    let packages = &config.sources[NIXPKGS_SOURCE];
    assert_eq!(packages.name.as_deref(), Some("Nixpkgs"));
    assert_eq!(packages.kind, SourceKind::Packages);
    assert_eq!(
        packages.refs[0].producer.kind(),
        ProducerKind::ChannelPackagesJson
    );
}

#[test]
fn loads_example_config_file() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("config crate should live under crates/config");
    let path = repo_root.join("nixsearch.example.toml");

    let config = AppConfig::load(Some(&path)).unwrap();

    assert!(config.sources.contains_key("eval-fixture"));
}

#[test]
fn preserves_source_order_from_config_file() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("config crate should live under crates/config");
    let path = repo_root.join("nixsearch.example.toml");

    let config = AppConfig::load(Some(&path)).unwrap();
    let source_ids: Vec<&str> = config.sources.keys().map(|s| s.as_str()).collect();

    assert_eq!(
        source_ids,
        vec![
            "nixpkgs",
            "nixos",
            "home-manager",
            "darwin",
            "hjem",
            "hjem-rum",
            "fixtures",
            "eval-fixture",
        ]
    );
}

#[test]
fn loads_existing_file_producer() {
    let config = load_toml(fixture_existing_file_source_toml());
    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    assert_eq!(producer.kind(), ProducerKind::ExistingFile);

    match producer {
        ProducerConfig::ExistingFile { path, artifact } => {
            assert_eq!(path, &PathBuf::from(FIXTURE_OPTIONS_PATH));
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_eval_modules_producer() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.eval.producer]
        type = "eval-modules"
        ref = "path:/some/flake"

        [[sources.fixtures.refs.eval.producer.modules]]
        type = "flake-attr"
        attr = "nixosModules.default"
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    assert_eq!(producer.kind(), ProducerKind::EvalModules);

    match producer {
        ProducerConfig::EvalModules {
            source_ref,
            inputs,
            modules,
        } => {
            assert_eq!(source_ref, "path:/some/flake");
            assert!(inputs.is_empty());
            assert_eq!(modules.len(), 1);

            match &modules[0] {
                EvalModuleConfig::FlakeAttr { flake, attr } => {
                    assert_eq!(flake, "self");
                    assert_eq!(attr, "nixosModules.default");
                }
                other => panic!("unexpected module: {other:?}"),
            }
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_eval_modules_producer_with_inputs_and_module_list_option() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.eval.producer]
        type = "eval-modules"
        ref = "github:example/root"

        [sources.fixtures.refs.eval.producer.inputs]
        dependency = "github:example/dependency"

        [[sources.fixtures.refs.eval.producer.modules]]
        type = "flake-attr"
        flake = "dependency"
        attr = "nixosModules.default"

        [[sources.fixtures.refs.eval.producer.modules]]
        type = "module-list-option"
        option = "example.extraModules"
        modules = [
          { flake = "self", attr = "modules.extra" },
        ]
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    match producer {
        ProducerConfig::EvalModules {
            source_ref,
            inputs,
            modules,
        } => {
            assert_eq!(source_ref, "github:example/root");
            assert_eq!(
                inputs.get("dependency").map(String::as_str),
                Some("github:example/dependency")
            );
            assert_eq!(modules.len(), 2);

            match &modules[1] {
                EvalModuleConfig::ModuleListOption { option, modules } => {
                    assert_eq!(option, "example.extraModules");
                    assert_eq!(modules.len(), 1);
                    assert_eq!(modules[0].flake, "self");
                    assert_eq!(modules[0].attr, "modules.extra");
                }
                other => panic!("unexpected module: {other:?}"),
            }
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn rejects_eval_modules_without_modules() {
    let error = load_toml_error(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.eval.producer]
        type = "eval-modules"
        ref = "github:example/root"
        "#,
    );

    assert_error_contains(&error, "modules");
}

#[test]
fn loads_download_producer() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "download"
        url = "https://example.com/options.json"
        artifact = "options-json"
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    assert_eq!(producer.kind(), ProducerKind::Download);

    match producer {
        ProducerConfig::Download {
            url,
            artifact,
            revision_url,
            compression,
        } => {
            assert_eq!(url, "https://example.com/options.json");
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
            assert_eq!(revision_url, &None);
            assert_eq!(*compression, DownloadCompression::None);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_download_producer_with_revision_and_compression() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "download"
        url = "https://example.com/options.json.br"
        revision_url = "https://example.com/revision"
        artifact = "options-json"
        compression = "brotli"
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    match producer {
        ProducerConfig::Download {
            url,
            artifact,
            revision_url,
            compression,
        } => {
            assert_eq!(url, "https://example.com/options.json.br");
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
            assert_eq!(
                revision_url.as_deref(),
                Some("https://example.com/revision")
            );
            assert_eq!(*compression, DownloadCompression::Brotli);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn rejects_empty_download_url() {
    let error = load_toml_error(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "download"
        url = ""
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "url must not be empty");
}

#[test]
fn rejects_empty_download_revision_url() {
    let error = load_toml_error(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "download"
        url = "https://example.com/options.json"
        revision_url = ""
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "revision_url must not be empty");
}

#[test]
fn loads_flake_file_producer() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.main.producer]
        type = "flake-file"
        ref = "github:example/project/main"
        attribute = "docs-json"
        output_path = "share/doc/example/options.json"
        artifact = "options-json"
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    assert_eq!(producer.kind(), ProducerKind::FlakeFile);

    match producer {
        ProducerConfig::FlakeFile {
            source_ref,
            attribute,
            output_path,
            artifact,
        } => {
            assert_eq!(source_ref, "github:example/project/main");
            assert_eq!(attribute, "docs-json");
            assert_eq!(
                output_path,
                &PathBuf::from("share/doc/example/options.json")
            );
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_flake_info_producer() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "mixed"

        [sources.fixtures.refs.main.producer]
        type = "flake-info"
        ref = "github:example/project/main"
        "#,
    );

    let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

    assert_eq!(producer.kind(), ProducerKind::FlakeInfo);

    match producer {
        ProducerConfig::FlakeInfo { source_ref } => {
            assert_eq!(source_ref, "github:example/project/main");
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn rejects_invalid_source_ids() {
    let error = load_toml_error(
        r#"
        [sources."bad/source"]
        name = "Bad Source"
        kind = "options"
        "#,
    );

    assert_error_contains(&error, "must not contain '/'");
}

#[test]
fn validates_nix_build_options_required_fields_by_deserialization() {
    let error = load_toml_error(
        r#"
        [sources.nixos]
        name = "NixOS Options"
        kind = "options"

        [sources.nixos.refs.unstable.producer]
        type = "nix-build-options-json"
        ref = "github:NixOS/nixpkgs/nixos-unstable"
        "#,
    );

    assert_error_contains(&error, "attribute");
}

#[test]
fn validates_custom_command_is_not_empty() {
    let error = load_toml_error(
        r#"
        [sources.custom]
        name = "Custom"
        kind = "options"

        [sources.custom.refs.main.producer]
        type = "custom-command"
        command = []
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "command must not be empty");
}

#[test]
fn loads_ref_source_links() {
    let config = load_toml(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [sources.fixtures.refs.main.source_links]
        type = "github"
        owner = "example"
        repo = "modules"
        revision = "abc123"
        strip_prefixes = ["/build/source/"]

        [sources.fixtures.refs.main.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "#,
    );

    let source_links = config.sources[FIXTURES_SOURCE].refs[0]
        .source_links
        .as_ref()
        .unwrap();

    match source_links {
        SourceLinkConfig::Github {
            owner,
            repo,
            revision,
            strip_prefixes,
        } => {
            assert_eq!(owner, "example");
            assert_eq!(repo, "modules");
            assert_eq!(revision.as_deref(), Some("abc123"));
            assert_eq!(strip_prefixes, &vec!["/build/source/".to_owned()]);
        }
        other => panic!("unexpected source links config: {other:?}"),
    }
}

#[test]
fn loads_nixpkgs_packages_preset() {
    let config = load_toml(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        preset_refs = ["nixos-unstable"]
        "#,
    );

    let source = &config.sources[NIXPKGS_SOURCE];

    assert_eq!(source.name.as_deref(), Some("Nixpkgs"));
    assert_eq!(source.kind, SourceKind::Packages);
    assert_eq!(source.color.as_deref(), Some(NIXPKGS_COLOR));
    assert_eq!(source.refs.len(), 1);

    let ref_config = &source.refs[0];

    assert_eq!(ref_config.id, NIXOS_UNSTABLE_REF);
    assert_eq!(
        ref_config.producer.kind(),
        ProducerKind::ChannelPackagesJson
    );

    match &ref_config.producer {
        ProducerConfig::ChannelPackagesJson { channel, url } => {
            assert_eq!(channel, NIXOS_UNSTABLE_REF);
            assert_eq!(url, &None);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_nixos_options_preset() {
    let config = load_toml(
        r#"
        [sources.nixos]
        name = "NixOS Options"
        preset = "nixos-options"
        preset_refs = ["nixos-unstable"]
        "#,
    );

    let source = &config.sources[NIXOS_SOURCE];

    assert_eq!(source.name.as_deref(), Some("NixOS Options"));
    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.color.as_deref(), Some(NIXOS_COLOR));
    assert_eq!(source.refs.len(), 1);

    let ref_config = &source.refs[0];

    assert_eq!(ref_config.id, NIXOS_UNSTABLE_REF);
    assert_eq!(ref_config.producer.kind(), ProducerKind::ChannelOptionsJson);

    match &ref_config.producer {
        ProducerConfig::ChannelOptionsJson { channel, url } => {
            assert_eq!(channel, NIXOS_UNSTABLE_REF);
            assert_eq!(url, &None);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_home_manager_options_preset() {
    let config = load_toml(
        r#"
        [sources.home-manager]
        name = "Home Manager"
        preset = "home-manager-options"
        preset_refs = ["master"]
        "#,
    );

    let source = &config.sources["home-manager"];

    assert_eq!(source.name.as_deref(), Some("Home Manager"));
    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.color.as_deref(), Some(HOME_MANAGER_COLOR));
    assert_eq!(source.refs.len(), 1);

    let ref_config = &source.refs[0];

    assert_eq!(ref_config.id, "master");
    assert_eq!(ref_config.producer.kind(), ProducerKind::FlakeFile);

    match &ref_config.producer {
        ProducerConfig::FlakeFile {
            source_ref,
            attribute,
            output_path,
            artifact,
        } => {
            assert_eq!(source_ref, "github:nix-community/home-manager/master");
            assert_eq!(attribute, "docs-json");
            assert_eq!(
                output_path,
                &PathBuf::from("share/doc/home-manager/options.json")
            );
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
        }
        other => panic!("unexpected producer: {other:?}"),
    }

    match ref_config.source_links.as_ref().unwrap() {
        SourceLinkConfig::Github {
            owner,
            repo,
            revision,
            strip_prefixes,
        } => {
            assert_eq!(owner, "nix-community");
            assert_eq!(repo, "home-manager");
            assert_eq!(revision.as_deref(), Some("master"));
            assert!(strip_prefixes.is_empty());
        }
        other => panic!("unexpected source links: {other:?}"),
    }
}

#[test]
fn loads_home_manager_options_preset_with_multiple_refs() {
    let config = load_toml(
        r#"
        [sources.home-manager]
        name = "Home Manager"
        preset = "home-manager-options"
        default_ref = "master"
        preset_refs = ["master", "release-26.05"]
        "#,
    );

    let source = &config.sources["home-manager"];

    assert_eq!(source.default_ref.as_deref(), Some("master"));
    assert_eq!(source.refs.len(), 2);
    assert_eq!(source.refs[0].id, "master");
    assert_eq!(source.refs[1].id, "release-26.05");

    match &source.refs[1].producer {
        ProducerConfig::FlakeFile { source_ref, .. } => {
            assert_eq!(
                source_ref,
                "github:nix-community/home-manager/release-26.05"
            );
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_nix_darwin_options_preset() {
    let config = load_toml(
        r#"
        [sources.darwin]
        preset = "nix-darwin-options"
        preset_refs = ["master"]
        "#,
    );

    let source = &config.sources["darwin"];

    assert_eq!(source.name.as_deref(), Some("Darwin"));
    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.color.as_deref(), Some(NIX_DARWIN_COLOR));
    assert_eq!(source.refs.len(), 1);

    let ref_config = &source.refs[0];

    assert_eq!(ref_config.id, "master");
    assert_eq!(ref_config.producer.kind(), ProducerKind::FlakeFile);

    match &ref_config.producer {
        ProducerConfig::FlakeFile {
            source_ref,
            attribute,
            output_path,
            artifact,
        } => {
            assert_eq!(source_ref, "github:nix-darwin/nix-darwin/master");
            assert_eq!(attribute, "optionsJSON");
            assert_eq!(output_path, &PathBuf::from("share/doc/darwin/options.json"));
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
        }
        other => panic!("unexpected producer: {other:?}"),
    }

    match ref_config.source_links.as_ref().unwrap() {
        SourceLinkConfig::Github {
            owner,
            repo,
            revision,
            strip_prefixes,
        } => {
            assert_eq!(owner, "nix-darwin");
            assert_eq!(repo, "nix-darwin");
            assert_eq!(revision.as_deref(), Some("master"));
            assert!(strip_prefixes.is_empty());
        }
        other => panic!("unexpected source links: {other:?}"),
    }
}

#[test]
fn loads_nix_darwin_options_preset_with_multiple_refs() {
    let config = load_toml(
        r#"
        [sources.darwin]
        name = "nix-darwin"
        preset = "nix-darwin-options"
        default_ref = "master"
        preset_refs = ["master", "nix-darwin-25.11"]
        "#,
    );

    let source = &config.sources["darwin"];

    assert_eq!(source.default_ref.as_deref(), Some("master"));
    assert_eq!(source.refs.len(), 2);
    assert_eq!(source.refs[0].id, "master");
    assert_eq!(source.refs[1].id, "nix-darwin-25.11");

    match &source.refs[1].producer {
        ProducerConfig::FlakeFile { source_ref, .. } => {
            assert_eq!(source_ref, "github:nix-darwin/nix-darwin/nix-darwin-25.11");
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_hjem_options_preset() {
    let config = load_toml(
        r#"
        [sources.hjem]
        preset = "hjem-options"
        preset_refs = ["main"]
        "#,
    );

    let source = &config.sources["hjem"];
    assert_eq!(source.name.as_deref(), Some("Hjem"));
    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.color.as_deref(), Some(HJEM_COLOR));

    let ref_config = &source.refs[0];
    assert_eq!(ref_config.id, "main");

    match &ref_config.producer {
        ProducerConfig::FlakeFile {
            source_ref,
            attribute,
            output_path,
            artifact,
        } => {
            assert_eq!(source_ref, "github:feel-co/hjem/main");
            assert_eq!(attribute, "docs-json");
            assert_eq!(output_path, &PathBuf::from("share/doc/hjem/options.json"));
            assert_eq!(*artifact, ArtifactKind::OptionsJson);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_hjem_rum_options_preset() {
    let config = load_toml(
        r#"
        [sources.hjem-rum]
        preset = "hjem-rum-options"
        preset_refs = ["main"]
        "#,
    );

    let source = &config.sources["hjem-rum"];
    assert_eq!(source.name.as_deref(), Some("Hjem-Rum"));
    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.color.as_deref(), Some(HJEM_RUM_COLOR));

    let ref_config = &source.refs[0];
    assert_eq!(ref_config.id, "main");

    match &ref_config.producer {
        ProducerConfig::EvalModules {
            source_ref,
            inputs,
            modules,
        } => {
            assert_eq!(source_ref, "github:snugnug/hjem-rum/main");
            assert!(inputs.is_empty());
            assert_eq!(modules.len(), 2);

            match &modules[0] {
                EvalModuleConfig::FlakeAttr { flake, attr } => {
                    assert_eq!(flake, "inputs.hjem");
                    assert_eq!(attr, "nixosModules.default");
                }
                other => panic!("unexpected module: {other:?}"),
            }

            match &modules[1] {
                EvalModuleConfig::ModuleListOption { option, modules } => {
                    assert_eq!(option, "hjem.extraModules");
                    assert_eq!(modules.len(), 1);
                    assert_eq!(modules[0].flake, "self");
                    assert_eq!(modules[0].attr, "hjemModules.default");
                }
                other => panic!("unexpected module: {other:?}"),
            }
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn preset_source_color_can_be_overridden() {
    let config = load_toml(
        r##"
        [sources.nixpkgs]
        name = "Nixpkgs"
        color = "#abcdef"
        preset = "nixpkgs-packages"
        preset_refs = ["nixos-unstable"]
        "##,
    );

    assert_eq!(
        config.sources[NIXPKGS_SOURCE].color.as_deref(),
        Some("#abcdef")
    );
}

#[test]
fn loads_nixpkgs_packages_preset_with_multiple_refs() {
    let config = load_toml(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        default_ref = "nixos-unstable"
        preset_refs = ["nixos-unstable", "nixos-25.11"]
        "#,
    );

    let source = &config.sources[NIXPKGS_SOURCE];

    assert_eq!(source.kind, SourceKind::Packages);
    assert_eq!(source.default_ref.as_deref(), Some(NIXOS_UNSTABLE_REF));
    assert_eq!(source.refs.len(), 2);
    assert_eq!(source.refs[0].id, NIXOS_UNSTABLE_REF);
    assert_eq!(source.refs[1].id, NIXOS_STABLE_REF);

    match &source.refs[1].producer {
        ProducerConfig::ChannelPackagesJson { channel, url } => {
            assert_eq!(channel, NIXOS_STABLE_REF);
            assert_eq!(url, &None);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn loads_nixos_options_preset_with_multiple_refs() {
    let config = load_toml(
        r#"
        [sources.nixos]
        name = "NixOS Options"
        preset = "nixos-options"
        default_ref = "nixos-unstable"
        preset_refs = ["nixos-unstable", "nixos-25.11"]
        "#,
    );

    let source = &config.sources[NIXOS_SOURCE];

    assert_eq!(source.kind, SourceKind::Options);
    assert_eq!(source.default_ref.as_deref(), Some(NIXOS_UNSTABLE_REF));
    assert_eq!(source.refs.len(), 2);
    assert_eq!(source.refs[0].id, NIXOS_UNSTABLE_REF);
    assert_eq!(source.refs[1].id, NIXOS_STABLE_REF);

    match &source.refs[1].producer {
        ProducerConfig::ChannelOptionsJson { channel, url } => {
            assert_eq!(channel, NIXOS_STABLE_REF);
            assert_eq!(url, &None);
        }
        other => panic!("unexpected producer: {other:?}"),
    }
}

#[test]
fn preset_rejects_empty_ref_array() {
    let error = load_toml_error(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        preset_refs = []
        "#,
    );

    assert_error_contains(&error, "preset sources require at least one ref");
}

#[test]
fn preset_rejects_missing_ref() {
    let error = load_toml_error(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        "#,
    );

    assert_error_contains(&error, "preset sources require at least one ref");
}

#[test]
fn preset_rejects_explicit_refs() {
    let error = load_toml_error(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        preset_refs = ["nixos-unstable"]

        [sources.nixpkgs.refs.manual.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "preset sources must not define explicit refs");
}

#[test]
fn preset_rejects_conflicting_kind() {
    let error = load_toml_error(
        r#"
        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        kind = "options"
        preset_refs = ["nixos-unstable"]
        "#,
    );

    assert_error_contains(&error, "requires source kind");
}

#[test]
fn infers_default_ref_from_single_ref() {
    let config = load_toml(fixture_existing_file_source_toml());

    assert_eq!(
        config.sources[FIXTURES_SOURCE].default_ref.as_deref(),
        Some(SMALL_REF)
    );
}

#[test]
fn rejects_missing_default_ref_with_multiple_refs() {
    let error = load_toml_error(&fixture_two_ref_source_toml(None));

    assert_error_contains(
        &error,
        "default_ref is required when multiple refs are configured",
    );
}

#[test]
fn explicit_default_ref_overrides_first_ref() {
    let config = load_toml(&fixture_two_ref_source_toml(Some(UNSTABLE_REF)));

    assert_eq!(
        config.sources[FIXTURES_SOURCE].default_ref.as_deref(),
        Some(UNSTABLE_REF)
    );
}

#[test]
fn rejects_unknown_default_ref() {
    let error = load_toml_error(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"
        default_ref = "missing"

        [sources.fixtures.refs.small.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "default_ref");
    assert_error_contains(&error, "does not match any configured ref");
}

#[test]
fn resolves_search_scopes_to_all_source_defaults() {
    let config = load_toml(&format!(
        r#"
        {}

        [sources.nixpkgs]
        name = "Nixpkgs"
        preset = "nixpkgs-packages"
        preset_refs = ["nixos-unstable"]
        "#,
        fixture_existing_file_source_toml()
    ));

    let scopes = config.resolve_search_scopes(None, None).unwrap();

    assert_eq!(scopes.len(), 2);
    assert!(
        scopes
            .iter()
            .any(|scope| scope.source == FIXTURES_SOURCE && scope.ref_id == SMALL_REF)
    );
    assert!(
        scopes
            .iter()
            .any(|scope| scope.source == NIXPKGS_SOURCE && scope.ref_id == NIXOS_UNSTABLE_REF)
    );
}

#[test]
fn resolves_search_scope_to_source_default() {
    let config = load_toml(fixture_existing_file_source_toml());
    let scopes = config
        .resolve_search_scopes(Some(FIXTURES_SOURCE), None)
        .unwrap();

    assert_single_scope(&scopes, FIXTURES_SOURCE, SMALL_REF);
}

#[test]
fn resolves_search_scope_to_explicit_source_ref() {
    let config = load_toml(fixture_existing_file_source_toml());
    let scopes = config
        .resolve_search_scopes(Some(FIXTURES_SOURCE), Some(SMALL_REF))
        .unwrap();

    assert_single_scope(&scopes, FIXTURES_SOURCE, SMALL_REF);
}

#[test]
fn resolve_search_scope_rejects_ref_without_source() {
    let config = AppConfig::load(None).unwrap();

    let error = config
        .resolve_search_scopes(None, Some(SMALL_REF))
        .unwrap_err()
        .to_string();

    assert_error_contains(&error, "--ref requires --source");
}

#[test]
fn resolve_search_scope_rejects_unknown_source() {
    let config = AppConfig::load(None).unwrap();

    let error = config
        .resolve_search_scopes(Some("missing"), None)
        .unwrap_err()
        .to_string();

    assert_error_contains(&error, "unknown source");
}

#[test]
fn resolve_search_scope_rejects_unknown_ref() {
    let config = load_toml(fixture_existing_file_source_toml());

    let error = config
        .resolve_search_scopes(Some(FIXTURES_SOURCE), Some("missing"))
        .unwrap_err()
        .to_string();

    assert_error_contains(&error, "unknown ref");
}

#[test]
fn loads_source_color() {
    let config = load_toml(
        r##"
        [sources.fixtures]
        name = "Fixtures"
        color = "#abc"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "##,
    );

    assert_eq!(
        config.sources[FIXTURES_SOURCE].color.as_deref(),
        Some("#abc")
    );
}

#[test]
fn rejects_invalid_source_color() {
    let error = load_toml_error(
        r#"
        [sources.fixtures]
        name = "Fixtures"
        color = "red"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "#,
    );

    assert_error_contains(&error, "must be a hex color");
}

#[test]
fn rejects_unsafe_source_color() {
    let error = load_toml_error(
        r##"
        [sources.fixtures]
        name = "Fixtures"
        color = "#fff; color: red"
        kind = "options"

        [sources.fixtures.refs.small.producer]
        type = "existing-file"
        path = "fixtures/search-small/options.json"
        artifact = "options-json"
        "##,
    );

    assert_error_contains(&error, "must be a hex color");
}

#[test]
fn parses_schedule_config() {
    let config = load_toml(
        r#"
        [server]
        bootstrap = false

        [server.schedule]
        enabled = true
        interval = "12h"
        "#,
    );

    assert!(!config.server.bootstrap);
    assert!(config.server.schedule.enabled);
    assert_eq!(config.server.schedule.interval, "12h");
    assert_eq!(
        config.server.schedule.parse_interval().unwrap(),
        std::time::Duration::from_secs(12 * 60 * 60)
    );
}

#[test]
fn parse_duration_accepts_supported_units() {
    assert_eq!(
        crate::server::parse_duration("24h").unwrap(),
        std::time::Duration::from_secs(86_400)
    );
    assert_eq!(
        crate::server::parse_duration("12h").unwrap(),
        std::time::Duration::from_secs(43_200)
    );
    assert_eq!(
        crate::server::parse_duration("1d").unwrap(),
        std::time::Duration::from_secs(86_400)
    );
    assert_eq!(
        crate::server::parse_duration("30m").unwrap(),
        std::time::Duration::from_secs(1_800)
    );
    assert_eq!(
        crate::server::parse_duration("3600s").unwrap(),
        std::time::Duration::from_secs(3_600)
    );
    assert_eq!(
        crate::server::parse_duration("0.5d").unwrap(),
        std::time::Duration::from_secs(43_200)
    );
    assert_eq!(
        crate::server::parse_duration(" 24h ").unwrap(),
        std::time::Duration::from_secs(86_400)
    );
}

#[test]
fn parse_duration_rejects_invalid_values() {
    for value in [
        "",
        "0h",
        "1s",
        "-1h",
        "24x",
        "abc",
        "NaNh",
        "infh",
        "999999999999999999999999999999999999999999999999999d",
    ] {
        assert!(
            crate::server::parse_duration(value).is_err(),
            "expected {value:?} to be invalid"
        );
    }
}

#[test]
fn disabled_schedule_still_validates_interval() {
    let error = load_toml_error(
        r#"
        [server.schedule]
        enabled = false
        interval = "1s"
        "#,
    );

    assert_error_contains(&error, "server.schedule.interval");
}
