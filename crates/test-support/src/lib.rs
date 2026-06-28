use std::path::PathBuf;

use camino::{Utf8Path, Utf8PathBuf};
use nixsearch_config::maintenance::MaintenanceConfig;
use tempfile::TempDir;

use nixsearch_config::app::{AppConfig, RefSetConfig};
use nixsearch_config::data::DataConfig;
use nixsearch_config::producer::ProducerConfig;
use nixsearch_config::server::ServerConfig;
use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::{DocText, OptionDoc, PackageDoc, SearchDocument};
use nixsearch_core::ingest::IngestContext;
use nixsearch_core::source_link::{Declaration, SourceLinkConfig};
use nixsearch_core::target::RefRole;

pub const SOURCE_FIXTURES: &str = "fixtures";
pub const SOURCE_NIXOS: &str = "nixos";
pub const SOURCE_NIXPKGS: &str = "nixpkgs";
pub const REF_SMALL: &str = "small";
pub const REF_STABLE: &str = "stable";
pub const REF_UNSTABLE: &str = "unstable";
pub const TEST_PUBLIC_ORIGIN: &str = "https://search.example.com";

pub const OPTION_GIT_ENABLE: &str = "programs.git.enable";
pub const OPTION_NGINX_ENABLE: &str = "services.nginx.enable";
pub const OPTION_TAILSCALE_ENABLE: &str = "services.tailscale.enable";
pub const OPTION_SYSTEMD_BOOT_ENABLE: &str = "boot.loader.systemd-boot.enable";

pub const PACKAGE_GIT: &str = "git";
pub const PACKAGE_RIPGREP: &str = "ripgrep";
pub const PACKAGE_PYTHON_REQUESTS: &str = "python3Packages.requests";

fn workspace_fixture_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

pub fn ingest_context() -> IngestContext {
    IngestContext {
        source: SOURCE_FIXTURES.to_owned(),
        ref_id: REF_SMALL.to_owned(),
        revision: Some("fixture-revision".to_owned()),
        repo: None,
    }
}

pub fn ingest_context_for(source: &str, ref_id: &str) -> IngestContext {
    IngestContext {
        source: source.to_owned(),
        ref_id: ref_id.to_owned(),
        revision: Some(format!("{source}-{ref_id}-revision")),
        repo: None,
    }
}

pub fn option_doc(name: &str, description: &str) -> SearchDocument {
    option_doc_for(&ingest_context(), name, description)
}

pub fn option_doc_for(context: &IngestContext, name: &str, description: &str) -> SearchDocument {
    let mut doc = OptionDoc::new(context, name);

    doc.description = Some(DocText::Markdown(description.to_owned()));
    doc.loc = name.split('.').map(ToOwned::to_owned).collect();
    doc.option_set = doc.loc.first().cloned();
    doc.parents = (1..doc.loc.len())
        .map(|end| doc.loc[..end].join("."))
        .collect();

    SearchDocument::Option(doc)
}

pub fn option_doc_with_declaration(
    context: &IngestContext,
    name: &str,
    declaration: &str,
) -> SearchDocument {
    let mut doc = match option_doc_for(context, name, "Fixture option.") {
        SearchDocument::Option(doc) => doc,
        SearchDocument::Package(_) => unreachable!(),
    };

    doc.declarations.push(Declaration {
        name: declaration.to_owned(),
        url: None,
    });

    SearchDocument::Option(doc)
}

pub fn package_doc(attribute: &str, description: &str) -> SearchDocument {
    package_doc_for(&ingest_context(), attribute, description)
}

pub fn package_doc_for(
    context: &IngestContext,
    attribute: &str,
    description: &str,
) -> SearchDocument {
    let mut doc = PackageDoc::new(context, attribute);

    doc.description = Some(description.to_owned());
    doc.pname = Some(attribute.rsplit('.').next().unwrap_or(attribute).to_owned());
    doc.version = Some("1.0.0".to_owned());
    doc.platforms = vec!["x86_64-linux".to_owned(), "aarch64-linux".to_owned()];

    SearchDocument::Package(doc)
}

pub fn package_doc_with_main_program(
    context: &IngestContext,
    attribute: &str,
    description: &str,
    main_program: &str,
) -> SearchDocument {
    let mut doc = match package_doc_for(context, attribute, description) {
        SearchDocument::Package(doc) => doc,
        SearchDocument::Option(_) => unreachable!(),
    };

    doc.main_program = Some(main_program.to_owned());

    SearchDocument::Package(doc)
}

pub fn canonical_documents() -> Vec<SearchDocument> {
    let context = ingest_context();

    vec![
        option_doc_for(
            &context,
            OPTION_GIT_ENABLE,
            "Whether to enable Git integration.",
        ),
        option_doc_for(
            &context,
            OPTION_NGINX_ENABLE,
            "Whether to enable the Nginx web server.",
        ),
        option_doc_for(
            &context,
            OPTION_TAILSCALE_ENABLE,
            "Whether to enable Tailscale networking.",
        ),
        option_doc_for(
            &context,
            OPTION_SYSTEMD_BOOT_ENABLE,
            "Whether to enable the systemd-boot EFI boot manager.",
        ),
        package_doc_with_main_program(
            &context,
            PACKAGE_GIT,
            "Distributed version control system.",
            "git",
        ),
        package_doc_with_main_program(
            &context,
            PACKAGE_RIPGREP,
            "Line-oriented search tool.",
            "rg",
        ),
        package_doc_for(&context, PACKAGE_PYTHON_REQUESTS, "Python HTTP library."),
    ]
}

pub fn app_config(index_dir: impl AsRef<Utf8Path>) -> AppConfig {
    AppConfig {
        data: DataConfig {
            artifact_url: "file://./data/artifacts".to_owned(),
            index_dir: index_dir.as_ref().to_owned(),
        },
        server: ServerConfig::default(),
        maintenance: MaintenanceConfig::default(),
        sources: [(
            SOURCE_FIXTURES.to_owned(),
            SourceConfig {
                name: Some("Fixtures".to_owned()),
                color: None,
                kind: SourceKind::Options,
                strip_prefixes: Vec::new(),
                default_ref: Some(REF_SMALL.to_owned()),
                refs: vec![RefConfig {
                    id: REF_SMALL.to_owned(),
                    role: RefRole::Search,
                    source_links: Some(SourceLinkConfig::Github {
                        owner: "example".to_owned(),
                        repo: "repo".to_owned(),
                        revision: Some("main".to_owned()),
                        strip_prefixes: Vec::new(),
                    }),
                    producer: ProducerConfig::ExistingFile {
                        path: workspace_fixture_path("fixtures/search-small/options.json"),
                        artifact: ArtifactKind::OptionsJson,
                    },
                }],
            },
        )]
        .into(),
        ref_sets: Default::default(),
    }
}

pub fn existing_file_ref_config(id: &str) -> RefConfig {
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

pub fn multi_ref_app_config(index_dir: impl AsRef<Utf8Path>) -> AppConfig {
    let mut config = app_config(index_dir);

    config
        .sources
        .get_mut(SOURCE_FIXTURES)
        .expect("fixture source exists")
        .refs
        .push(existing_file_ref_config(REF_STABLE));

    config.ref_sets = [
        (
            "single".to_owned(),
            RefSetConfig {
                refs: [(SOURCE_FIXTURES.to_owned(), vec![REF_SMALL.to_owned()])].into(),
            },
        ),
        (
            "multi".to_owned(),
            RefSetConfig {
                refs: [(
                    SOURCE_FIXTURES.to_owned(),
                    vec![REF_SMALL.to_owned(), REF_STABLE.to_owned()],
                )]
                .into(),
            },
        ),
    ]
    .into();

    config
}

pub fn app_config_with_public_url(index_dir: impl AsRef<Utf8Path>) -> AppConfig {
    let mut config = app_config(index_dir);
    config.server.public_url = Some(TEST_PUBLIC_ORIGIN.to_owned());
    config
}

pub fn multi_ref_app_config_with_public_url(index_dir: impl AsRef<Utf8Path>) -> AppConfig {
    let mut config = multi_ref_app_config(index_dir);
    config.server.public_url = Some(TEST_PUBLIC_ORIGIN.to_owned());
    config
}

pub fn app_config_with_extra_fixture_source(
    index_dir: impl AsRef<Utf8Path>,
    source_id: &str,
) -> AppConfig {
    let mut config = app_config(index_dir);
    let extra_source = config.sources[SOURCE_FIXTURES].clone();
    config.sources.insert(source_id.to_owned(), extra_source);
    config
}

pub fn utf8_path_buf(path: PathBuf) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
}

pub fn config_toml(index_dir: &Utf8Path) -> String {
    let fixture_path = workspace_fixture_path("fixtures/search-small/options.json");
    let index_dir = toml_string(index_dir.as_str());
    let fixture_path = toml_string(&fixture_path.display().to_string());

    format!(
        r#"
   [data]
   artifact_url = "file://./data/artifacts"
   index_dir = {}

   [server]
   listen = "127.0.0.1:0"

   [sources.fixtures]
   name = "Fixtures"
   kind = "options"
   default_ref = "small"

   [sources.fixtures.refs.small.source_links]
   type = "github"
   owner = "example"
   repo = "repo"
   revision = "main"

   [sources.fixtures.refs.small.producer]
   type = "existing-file"
   path = {}
   artifact = "options-json"
    "#,
        index_dir, fixture_path
    )
}

pub fn write_config(dir: &TempDir, index_dir: &Utf8Path) -> PathBuf {
    let path = dir.path().join("nixsearch.toml");
    std::fs::write(&path, config_toml(index_dir)).unwrap();
    path
}

pub fn assert_doc_names_eq(docs: &[SearchDocument], expected: &[&str]) {
    let actual = docs.iter().map(SearchDocument::name).collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

pub fn assert_contains_doc(docs: &[SearchDocument], name: &str) {
    assert!(
        docs.iter().any(|doc| doc.name() == name),
        "expected docs to contain {name:?}; got {:?}",
        docs.iter().map(SearchDocument::name).collect::<Vec<_>>()
    );
}

#[cfg(test)]
mod tests {
    use super::{config_toml, toml_string, utf8_path_buf};

    #[test]
    fn toml_string_escapes_special_characters() {
        let value = "quote\" slash\\ newline\n";
        let document = format!("value = {}", toml_string(value));
        let parsed: toml::Value = toml::from_str(&document).unwrap();

        assert_eq!(parsed["value"].as_str(), Some(value));
    }

    #[test]
    fn config_toml_escapes_index_dir_path() {
        let tempdir = tempfile::tempdir().unwrap();
        let config_path = tempdir.path().join("nixsearch.toml");
        let index_dir = utf8_path_buf(tempdir.path().join("index\"dir"));

        std::fs::write(&config_path, config_toml(&index_dir)).unwrap();

        let config = nixsearch_config::app::AppConfig::load(Some(&config_path)).unwrap();

        assert_eq!(config.data.index_dir, index_dir);
    }
}
