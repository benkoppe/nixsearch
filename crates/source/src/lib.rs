use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tempfile::tempdir;
use tokio::process::Command;

use nix_search_core::{ArtifactKind, IngestContext, SearchDocument};
use nix_search_ingest::{parse_options_json, parse_packages_json};
use nix_search_store::{ArtifactMetadata, ArtifactMetadataInput, ArtifactRef, ArtifactStore};

#[derive(Debug, Clone)]
pub struct ProduceRequest {
    pub source: String,
    pub ref_id: String,
}

impl ProduceRequest {
    pub fn artifact_ref(&self, kind: ArtifactKind) -> ArtifactRef {
        ArtifactRef::latest(self.source.clone(), self.ref_id.clone(), kind)
    }

    pub fn ingest_context(&self, revision: Option<String>) -> IngestContext {
        IngestContext {
            source: self.source.clone(),
            ref_id: self.ref_id.clone(),
            revision,
            repo: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProducedArtifact {
    pub artifact_ref: ArtifactRef,
    pub metadata: ArtifactMetadata,
}

#[async_trait]
pub trait Producer: Send + Sync {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact>;
}

#[async_trait]
pub trait Consumer: Send + Sync {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>>;
}

#[derive(Debug, Clone)]
pub struct ExistingFileProducer {
    path: PathBuf,
    kind: ArtifactKind,
    producer_name: String,
}

impl ExistingFileProducer {
    pub fn new(path: impl Into<PathBuf>, kind: ArtifactKind) -> Self {
        Self {
            path: path.into(),
            kind,
            producer_name: "existing-file".to_owned(),
        }
    }

    pub fn with_name(
        path: impl Into<PathBuf>,
        kind: ArtifactKind,
        producer_name: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            producer_name: producer_name.into(),
        }
    }
}

#[async_trait]
impl Producer for ExistingFileProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let bytes = tokio::fs::read(&self.path)
            .await
            .with_context(|| format!("failed to read artifact file {}", self.path.display()))?;

        let artifact_ref = request.artifact_ref(self.kind);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.path.display().to_string());

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NixBuildOptionsJsonProducer {
    source_ref: String,
    attribute: String,
    import_path: String,
    output_path: String,
    producer_name: String,
}

impl NixBuildOptionsJsonProducer {
    pub fn new(
        source_ref: impl Into<String>,
        attribute: impl Into<String>,
        import_path: impl Into<String>,
        output_path: impl Into<String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            attribute: attribute.into(),
            import_path: import_path.into(),
            output_path: output_path.into(),
            producer_name: "nix-build-options-json".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for NixBuildOptionsJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let nix_path_source = normalize_nix_path_source(&self.source_ref);
        let expression = format!("<nixpkgs/{}>", self.import_path);

        let output = Command::new("nix-build")
            .arg("--no-build-output")
            .arg(&expression)
            .arg("--attr")
            .arg(&self.attribute)
            .arg("--no-out-link")
            .arg("-I")
            .arg(format!("nixpkgs={nix_path_source}"))
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to run nix-build for source ref {:?}",
                    self.source_ref
                )
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "nix-build failed with status {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        let result_path = String::from_utf8(output.stdout)
            .context("nix-build stdout was not valid UTF-8")?
            .trim()
            .to_owned();

        if result_path.is_empty() {
            anyhow::bail!("nix-build succeeded but did not print an output path");
        }

        let artifact_path = PathBuf::from(result_path).join(&self.output_path);

        let bytes = tokio::fs::read(&artifact_path).await.with_context(|| {
            format!(
                "failed to read generated options JSON {}",
                artifact_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.source_ref.clone());

        match resolve_flake_revision(&self.source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {:?}: {error:#}",
                self.source_ref
            )),
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write options artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

fn normalize_nix_path_source(source_ref: &str) -> String {
    if let Some(rest) = source_ref.strip_prefix("github:") {
        let mut parts = rest.splitn(3, '/');

        if let (Some(owner), Some(repo), Some(branch_or_ref)) =
            (parts.next(), parts.next(), parts.next())
        {
            return format!(
                "https://github.com/{owner}/{repo}/archive/refs/heads/{branch_or_ref}.tar.gz"
            );
        }
    }

    source_ref.to_owned()
}

#[derive(Debug, Clone)]
pub struct EvalModulesProducer {
    source_ref: String,
    modules_attr: String,
    url_prefix: Option<String>,
    producer_name: String,
}

impl EvalModulesProducer {
    pub fn new(
        source_ref: impl Into<String>,
        modules_attr: impl Into<String>,
        url_prefix: Option<String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            modules_attr: modules_attr.into(),
            url_prefix,
            producer_name: "eval-modules".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for EvalModulesProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let tempdir = tempdir().context("failed to create temporary eval-modules directory")?;
        let expression_path = tempdir.path().join("eval-modules-options.nix");

        let source_ref = normalize_flake_ref(&self.source_ref)?;
        let expression = eval_modules_expression(&source_ref, &self.modules_attr);
        tokio::fs::write(&expression_path, expression)
            .await
            .with_context(|| {
                format!(
                    "failed to write eval-modules expression {}",
                    expression_path.display()
                )
            })?;

        let output_path = run_nix_build_expression(&expression_path)
            .await
            .with_context(|| {
                format!(
                    "failed to build options JSON for flake {:?}, module attr {:?}",
                    self.source_ref, self.modules_attr
                )
            })?;

        let options_json_path = output_path.join("share/doc/nixos/options.json");

        let bytes = tokio::fs::read(&options_json_path).await.with_context(|| {
            format!(
                "failed to read generated options JSON {}",
                options_json_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(source_ref.clone());

        match resolve_flake_revision(&source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {source_ref:?}: {error:#}"
            )),
        }

        if let Some(url_prefix) = &self.url_prefix {
            metadata_input.warnings.push(format!(
                "url_prefix is configured but source-link enrichment is not implemented yet: {url_prefix}"
            ));
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write eval-modules options artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

async fn run_nix_build_expression(expression_path: &Path) -> Result<PathBuf> {
    let output = Command::new("nix")
        .arg("build")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("--impure")
        .arg("--no-link")
        .arg("--print-out-paths")
        .arg("--file")
        .arg(expression_path)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to run nix build for expression {}",
                expression_path.display()
            )
        })?;

    if !output.status.success() {
        anyhow::bail!(
            "nix build failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let stdout = String::from_utf8(output.stdout).context("nix stdout was not valid UTF-8")?;

    let output_path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .context("nix build succeeded but did not print an output path")?;

    Ok(PathBuf::from(output_path))
}

fn eval_modules_expression(source_ref: &str, modules_attr: &str) -> String {
    format!(
        r#"
let
  flake = builtins.getFlake {source_ref};
  pkgs = import <nixpkgs> {{ }};
  lib = pkgs.lib;

  getAttrPath = path: value:
    builtins.foldl'
      (acc: name: acc.${{name}})
      value
      path;

  module = getAttrPath (lib.splitString "." {modules_attr}) flake;

  eval = lib.evalModules {{
    specialArgs = {{
      inherit pkgs lib;
      inputs = flake.inputs or {{}};
      self = flake;
    }};

    modules = [
      module
      ({{ lib, ... }}: {{
        options._module.args = lib.mkOption {{
          internal = true;
        }};

        config._module.check = false;
      }})
    ];
  }};
in
  (pkgs.nixosOptionsDoc {{
    inherit (eval) options;
    warningsAreErrors = false;
  }}).optionsJSON
"#,
        source_ref = nix_string(source_ref),
        modules_attr = nix_string(modules_attr),
    )
}

fn nix_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn normalize_flake_ref(source_ref: &str) -> Result<String> {
    let Some(path) = source_ref.strip_prefix("path:") else {
        return Ok(source_ref.to_owned());
    };

    let path = PathBuf::from(path);

    if path.is_absolute() {
        return Ok(source_ref.to_owned());
    }

    let absolute = std::env::current_dir()
        .context("failed to get current directory while normalizing relative path flake ref")?
        .join(path)
        .canonicalize()
        .with_context(|| format!("failed to canonicalize relative flake ref {source_ref:?}"))?;

    Ok(format!("path:{}", absolute.display()))
}

#[derive(Debug, Clone)]
pub struct ChannelPackagesJsonProducer {
    channel: String,
    url: Option<String>,
    producer_name: String,
}

impl ChannelPackagesJsonProducer {
    pub fn new(channel: impl Into<String>, url: Option<String>) -> Self {
        Self {
            channel: channel.into(),
            url,
            producer_name: "channel-packages-json".to_owned(),
        }
    }

    fn url(&self) -> String {
        self.url.clone().unwrap_or_else(|| {
            format!(
                "https://channels.nixos.org/{}/packages.json.br",
                self.channel
            )
        })
    }
}

#[async_trait]
impl Producer for ChannelPackagesJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let url = self.url();

        let compressed = reqwest::get(&url)
            .await
            .with_context(|| format!("failed to fetch {url}"))?
            .error_for_status()
            .with_context(|| format!("HTTP error fetching {url}"))?
            .bytes()
            .await
            .with_context(|| format!("failed to read response body from {url}"))?;

        let decompressed = decompress_brotli(&compressed)
            .with_context(|| format!("failed to decompress Brotli response from {url}"))?;

        let artifact_ref = request.artifact_ref(ArtifactKind::PackagesJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(url.clone());

        match fetch_channel_git_revision(&self.channel).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => metadata_input.warnings.push(format!(
                "channel git-revision was empty for channel {:?}",
                self.channel
            )),
            Err(error) => metadata_input.warnings.push(format!(
                "failed to fetch channel git-revision for channel {:?}: {error:#}",
                self.channel
            )),
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(decompressed), metadata_input)
            .await
            .context("failed to write packages artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

fn decompress_brotli(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decompressor = brotli::Decompressor::new(bytes, 4096);
    let mut output = Vec::new();

    decompressor
        .read_to_end(&mut output)
        .context("Brotli decompression failed")?;

    Ok(output)
}

#[derive(Debug, Default, Clone)]
pub struct OptionsJsonConsumer;

#[async_trait]
impl Consumer for OptionsJsonConsumer {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>> {
        if artifact.artifact_ref.kind != ArtifactKind::OptionsJson {
            anyhow::bail!(
                "OptionsJsonConsumer cannot consume artifact kind {:?}",
                artifact.artifact_ref.kind
            );
        }

        let bytes = store
            .get_artifact(&artifact.artifact_ref)
            .await
            .context("failed to read options artifact")?;

        let context = IngestContext {
            source: artifact.metadata.source.clone(),
            ref_id: artifact.metadata.ref_id.clone(),
            revision: artifact.metadata.revision.clone(),
            repo: None,
        };

        parse_options_json(bytes.as_ref(), &context).context("failed to parse options artifact")
    }
}

#[derive(Debug, Default, Clone)]
pub struct PackagesJsonConsumer;

#[async_trait]
impl Consumer for PackagesJsonConsumer {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>> {
        if artifact.artifact_ref.kind != ArtifactKind::PackagesJson {
            anyhow::bail!(
                "PackagesJsonConsumer cannot consume artifact kind {:?}",
                artifact.artifact_ref.kind
            );
        }

        let bytes = store
            .get_artifact(&artifact.artifact_ref)
            .await
            .context("failed to read packages artifact")?;

        let context = IngestContext {
            source: artifact.metadata.source.clone(),
            ref_id: artifact.metadata.ref_id.clone(),
            revision: artifact.metadata.revision.clone(),
            repo: None,
        };

        parse_packages_json(bytes.as_ref(), &context).context("failed to parse packages artifact")
    }
}

async fn resolve_flake_revision(source_ref: &str) -> Result<Option<String>> {
    let output = Command::new("nix")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("flake")
        .arg("metadata")
        .arg("--json")
        .arg(source_ref)
        .output()
        .await
        .with_context(|| format!("failed to run nix flake metadata for {source_ref:?}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "nix flake metadata failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    parse_flake_metadata_revision(&output.stdout)
}

fn parse_flake_metadata_revision(bytes: &[u8]) -> Result<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("failed to parse nix flake metadata JSON")?;

    Ok(value
        .get("locked")
        .and_then(|locked| locked.get("rev"))
        .and_then(|rev| rev.as_str())
        .filter(|rev| !rev.trim().is_empty())
        .map(ToOwned::to_owned))
}

fn channel_git_revision_url(channel: &str) -> String {
    format!("https://channels.nixos.org/{channel}/git-revision")
}

async fn fetch_channel_git_revision(channel: &str) -> Result<Option<String>> {
    let url = channel_git_revision_url(channel);

    let text = reqwest::get(&url)
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read response body from {url}"))?;

    let revision = text.trim();

    if revision.is_empty() {
        Ok(None)
    } else {
        Ok(Some(revision.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use nix_search_core::{ArtifactKind, SearchDocument};
    use nix_search_store::ArtifactStore;
    use nix_search_test_support::{
        OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, REF_SMALL, SOURCE_FIXTURES,
    };

    use crate::ChannelPackagesJsonProducer;

    use super::{
        Consumer, EvalModulesProducer, ExistingFileProducer, OptionsJsonConsumer, ProduceRequest,
        Producer, eval_modules_expression, normalize_nix_path_source,
    };

    #[tokio::test]
    async fn existing_file_producer_writes_artifact_to_store() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        tokio::fs::write(
            &artifact_path,
            include_bytes!("../../../fixtures/search-small/options.json"),
        )
        .await
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.source, "fixtures");
        assert_eq!(produced.metadata.ref_id, "small");
        assert_eq!(produced.metadata.producer, "existing-file");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(artifact_path.to_string_lossy().as_ref())
        );

        assert!(store.exists(&produced.artifact_ref).await.unwrap());
    }

    #[tokio::test]
    async fn options_json_consumer_reads_produced_artifact() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        fs::write(
            &artifact_path,
            include_bytes!("../../../fixtures/search-small/options.json"),
        )
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        let consumer = OptionsJsonConsumer;
        let docs = consumer.consume(&store, &produced).await.unwrap();

        assert_eq!(docs.len(), 4);
        assert!(docs.iter().any(|doc| doc.name() == OPTION_GIT_ENABLE));
        assert!(docs.iter().any(|doc| doc.name() == OPTION_NGINX_ENABLE));
    }

    #[tokio::test]
    #[ignore = "requires nix with flakes enabled"]
    async fn eval_modules_producer_builds_fixture_options_json() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = crate_dir
            .parent()
            .and_then(|path| path.parent())
            .expect("source crate should live under crates/source");

        let fixture_path = repo_root
            .join("fixtures")
            .join("eval-modules-flake")
            .canonicalize()
            .expect("eval-modules fixture should exist");

        let source_ref = format!("path:{}", fixture_path.display());

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let producer = EvalModulesProducer::new(
            source_ref,
            "nixosModules.default",
            Some("https://example.com/blob/main/".to_owned()),
        );

        let request = ProduceRequest {
            source: "eval-fixture".into(),
            ref_id: "local".into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.producer, "eval-modules");
        assert!(
            produced
                .metadata
                .warnings
                .iter()
                .any(|warning| warning.contains("source-link enrichment is not implemented yet"))
        );

        let consumer = OptionsJsonConsumer;
        let docs = consumer.consume(&store, &produced).await.unwrap();

        let fixture_option = docs
            .iter()
            .find(|doc| doc.name() == "programs.fixture.enable")
            .expect("expected fixture option in docs");

        let SearchDocument::Option(option) = fixture_option else {
            panic!("expected fixture option to be an option document");
        };

        assert!(
            !option.declarations.is_empty(),
            "expected fixture option to have source declarations: {option:#?}"
        );
    }

    #[test]
    fn normalizes_github_flake_ref_to_tarball_url() {
        let normalized = normalize_nix_path_source("github:NixOS/nixpkgs/nixos-unstable");

        assert_eq!(
            normalized,
            "https://github.com/NixOS/nixpkgs/archive/refs/heads/nixos-unstable.tar.gz"
        );
    }

    #[test]
    fn leaves_non_github_sources_unchanged() {
        let source = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

        assert_eq!(normalize_nix_path_source(source), source);
    }

    #[test]
    fn channel_packages_json_producer_builds_default_url() {
        let producer = ChannelPackagesJsonProducer::new("nixos-unstable", None);

        assert_eq!(
            producer.url(),
            "https://channels.nixos.org/nixos-unstable/packages.json.br"
        );
    }

    #[test]
    fn eval_modules_expression_contains_flake_ref_module_attr_and_default_eval_args() {
        let expression = eval_modules_expression("github:example/project", "nixosModules.default");

        assert!(expression.contains("\"github:example/project\""));
        assert!(expression.contains("\"nixosModules.default\""));
        assert!(expression.contains("builtins.getFlake"));
        assert!(expression.contains("lib.evalModules"));
        assert!(expression.contains("specialArgs"));
        assert!(expression.contains("inherit pkgs lib;"));
        assert!(expression.contains("inputs = flake.inputs or {};"));
        assert!(expression.contains("self = flake;"));
        assert!(expression.contains("config._module.check = false;"));
        assert!(expression.contains("pkgs.nixosOptionsDoc"));
    }

    #[test]
    fn nix_string_escapes_values() {
        let escaped = super::nix_string("hello\"world");

        assert_eq!(escaped, "\"hello\\\"world\"");
    }

    #[test]
    fn normalize_flake_ref_canonicalizes_relative_path_refs() {
        let tempdir = tempdir().unwrap();
        let flake_dir = tempdir.path().join("flake");
        fs::create_dir(&flake_dir).unwrap();

        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tempdir.path()).unwrap();

        let normalized = super::normalize_flake_ref("path:./flake").unwrap();

        std::env::set_current_dir(previous_dir).unwrap();

        assert_eq!(
            normalized,
            format!("path:{}", flake_dir.canonicalize().unwrap().display())
        );
    }

    #[test]
    fn normalize_flake_ref_leaves_non_path_refs_unchanged() {
        let source_ref = "github:NixOS/nixpkgs/nixos-unstable";

        let normalized = super::normalize_flake_ref(source_ref).unwrap();

        assert_eq!(normalized, source_ref);
    }

    #[test]
    fn parse_flake_metadata_revision_reads_locked_rev() {
        let json = br#"
          {
            "locked": {
              "rev": "abc123"
            }
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_flake_metadata_revision_returns_none_when_missing() {
        let json = br#"
          {
            "locked": {}
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision, None);
    }

    #[test]
    fn parse_flake_metadata_revision_returns_none_for_path_flake_metadata() {
        let json = br#"
          {
            "path": "/tmp/flake",
            "resolved": {
              "type": "path",
              "path": "/tmp/flake"
            }
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision, None);
    }

    #[test]
    fn parse_flake_metadata_revision_rejects_malformed_json() {
        let error = super::parse_flake_metadata_revision(b"{ not json").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to parse nix flake metadata JSON")
        );
    }

    #[test]
    fn channel_git_revision_url_uses_channel() {
        assert_eq!(
            super::channel_git_revision_url("nixos-unstable"),
            "https://channels.nixos.org/nixos-unstable/git-revision"
        );
    }

    #[tokio::test]
    #[ignore = "requires nix and network"]
    async fn resolves_github_flake_revision() {
        let revision = super::resolve_flake_revision("github:NixOS/nixpkgs/nixos-unstable")
            .await
            .unwrap();

        let revision = revision.expect("expected github flake to resolve to a revision");

        assert!(!revision.is_empty());
    }
}
