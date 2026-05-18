use std::path::{Path, PathBuf};

use tempfile::TempDir;

use nix_search_config::{
    AppConfig, DataConfig, ProducerConfig, RefConfig, ServerConfig, SourceConfig, SourceKind,
};
use nix_search_core::{
    ArtifactKind, Declaration, IngestContext, OptionDoc, PackageDoc, SearchDocument,
    SourceLinkConfig,
};

pub const SOURCE_FIXTURES: &str = "fixtures";
pub const SOURCE_NIXOS: &str = "nixos";
pub const SOURCE_NIXPKGS: &str = "nixpkgs";
pub const REF_SMALL: &str = "small";
pub const REF_STABLE: &str = "stable";
pub const REF_UNSTABLE: &str = "unstable";

pub const OPTION_GIT_ENABLE: &str = "programs.git.enable";
pub const OPTION_NGINX_ENABLE: &str = "services.nginx.enable";
pub const OPTION_TAILSCALE_ENABLE: &str = "services.tailscale.enable";
pub const OPTION_SYSTEMD_BOOT_ENABLE: &str = "boot.loader.systemd-boot.enable";

pub const PACKAGE_GIT: &str = "git";
pub const PACKAGE_RIPGREP: &str = "ripgrep";
pub const PACKAGE_PYTHON_REQUESTS: &str = "python3Packages.requests";

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

    doc.description = Some(description.to_owned());
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

pub fn app_config(index_dir: impl Into<PathBuf>) -> AppConfig {
    AppConfig {
        data: DataConfig {
            artifact_url: "file://./data/artifacts".to_owned(),
            index_dir: index_dir.into(),
        },
        server: ServerConfig::default(),
        sources: [(
            SOURCE_FIXTURES.to_owned(),
            SourceConfig {
                name: Some("Fixtures".to_owned()),
                kind: SourceKind::Options,
                default_ref: Some(REF_SMALL.to_owned()),
                refs: vec![RefConfig {
                    id: REF_SMALL.to_owned(),
                    source_links: Some(SourceLinkConfig::Github {
                        owner: "example".to_owned(),
                        repo: "repo".to_owned(),
                        revision: Some("main".to_owned()),
                        strip_prefixes: Vec::new(),
                    }),
                    producer: ProducerConfig::ExistingFile {
                        path: PathBuf::from("fixtures/search-small/options.json"),
                        artifact: ArtifactKind::OptionsJson,
                    },
                }],
            },
        )]
        .into(),
    }
}

pub fn config_toml(index_dir: &Path) -> String {
    format!(
        r#"
   [data]
   artifact_url = "file://./data/artifacts"
   index_dir = "{}"

   [server]
   listen = "127.0.0.1:0"

   [sources.fixtures]
   name = "Fixtures"
   kind = "options"
   default_ref = "small"

   [[sources.fixtures.refs]]
   id = "small"

   [sources.fixtures.refs.source_links]
   type = "github"
   owner = "example"
   repo = "repo"
   revision = "main"

   [sources.fixtures.refs.producer]
   type = "existing-file"
   path = "fixtures/search-small/options.json"
   artifact = "options-json"
   "#,
        index_dir.display()
    )
}

pub fn write_config(dir: &TempDir, index_dir: &Path) -> PathBuf {
    let path = dir.path().join("nix-search.toml");
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
