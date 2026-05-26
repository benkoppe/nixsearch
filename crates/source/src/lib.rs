use std::collections::BTreeMap;
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

const CHANNELS_BASE_URL: &str = "https://channels.nixos.org";
const CHANNEL_PACKAGES_FILE: &str = "packages.json.br";
const CHANNEL_OPTIONS_FILE: &str = "options.json.br";
const CHANNEL_GIT_REVISION_FILE: &str = "git-revision";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadCompression {
    None,
    Brotli,
}

#[derive(Debug, Clone)]
pub struct DownloadProducer {
    url: String,
    artifact: ArtifactKind,
    revision_url: Option<String>,
    compression: DownloadCompression,
    producer_name: String,
}

impl DownloadProducer {
    pub fn new(
        url: impl Into<String>,
        artifact: ArtifactKind,
        revision_url: Option<String>,
        compression: DownloadCompression,
    ) -> Self {
        Self {
            url: url.into(),
            artifact,
            revision_url,
            compression,
            producer_name: "download".to_owned(),
        }
    }

    async fn fetch_artifact(&self) -> Result<Vec<u8>> {
        match self.compression {
            DownloadCompression::None => fetch_bytes(&self.url)
                .await
                .map(|bytes| bytes.to_vec())
                .with_context(|| format!("failed to download artifact from {}", self.url)),
            DownloadCompression::Brotli => fetch_brotli_artifact(&self.url)
                .await
                .with_context(|| format!("failed to download Brotli artifact from {}", self.url)),
        }
    }
}

#[async_trait]
impl Producer for DownloadProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let bytes = self.fetch_artifact().await?;
        let artifact_ref = request.artifact_ref(self.artifact);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.url.clone());

        if let Some(revision_url) = &self.revision_url {
            populate_download_revision(&mut metadata_input, revision_url).await;
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write downloaded artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NixBuildOptionsJsonProducer {
    source_ref: String,
    nix_path_name: String,
    attribute: String,
    import_path: String,
    output_path: String,
    producer_name: String,
}

impl NixBuildOptionsJsonProducer {
    pub fn new(
        source_ref: impl Into<String>,
        nix_path_name: impl Into<String>,
        attribute: impl Into<String>,
        import_path: impl Into<String>,
        output_path: impl Into<String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            nix_path_name: nix_path_name.into(),
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
        let expression = format!("<{}/{}>", self.nix_path_name, self.import_path);

        let output = Command::new("nix-build")
            .arg("--no-build-output")
            .arg(&expression)
            .arg("--attr")
            .arg(&self.attribute)
            .arg("--no-out-link")
            .arg("-I")
            .arg(format!("{}={nix_path_source}", self.nix_path_name))
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

#[derive(Debug, Clone)]
pub struct FlakeFileProducer {
    source_ref: String,
    attribute: String,
    output_path: PathBuf,
    artifact: ArtifactKind,
    producer_name: String,
}

impl FlakeFileProducer {
    pub fn new(
        source_ref: impl Into<String>,
        attribute: impl Into<String>,
        output_path: impl Into<PathBuf>,
        artifact: ArtifactKind,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            attribute: attribute.into(),
            output_path: output_path.into(),
            artifact,
            producer_name: "flake-file".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for FlakeFileProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let source_ref = normalize_flake_ref(&self.source_ref)?;
        let installable = flake_installable(&source_ref, &self.attribute);

        let output_path = run_nix_build_installable(&installable)
            .await
            .with_context(|| {
                format!(
                    "failed to build flake installable {:?} for source ref {:?}",
                    installable, self.source_ref
                )
            })?;

        let artifact_path = output_path.join(&self.output_path);

        let bytes = tokio::fs::read(&artifact_path).await.with_context(|| {
            format!(
                "failed to read flake output artifact {}",
                artifact_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(self.artifact);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(source_ref.clone());

        match resolve_flake_revision(&source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {source_ref:?}: {error:#}"
            )),
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write flake output artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

fn flake_installable(source_ref: &str, attribute: &str) -> String {
    format!("{source_ref}#{attribute}")
}

async fn run_nix_build_installable(installable: &str) -> Result<PathBuf> {
    let output = Command::new("nix")
        .arg("build")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("--no-link")
        .arg("--print-out-paths")
        .arg(installable)
        .output()
        .await
        .with_context(|| format!("failed to run nix build for installable {installable:?}"))?;

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
    inputs: BTreeMap<String, String>,
    modules: Vec<EvalModule>,
    producer_name: String,
}

#[derive(Debug, Clone)]
pub enum EvalModule {
    FlakeAttr(EvalModuleRef),
    ModuleListOption {
        option: String,
        modules: Vec<EvalModuleRef>,
    },
}

#[derive(Debug, Clone)]
pub struct EvalModuleRef {
    pub flake: String,
    pub attr: String,
}

impl EvalModulesProducer {
    pub fn new(
        source_ref: impl Into<String>,
        inputs: BTreeMap<String, String>,
        modules: Vec<EvalModule>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            inputs,
            modules,
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
        let inputs = normalize_eval_module_inputs(&self.inputs)?;
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: &source_ref,
            inputs: &inputs,
            modules: &self.modules,
        });
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
                    "failed to build options JSON for flake {:?}",
                    self.source_ref
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

struct EvalModulesExpression<'a> {
    source_ref: &'a str,
    inputs: &'a BTreeMap<String, String>,
    modules: &'a [EvalModule],
}

fn eval_modules_expression(input: EvalModulesExpression<'_>) -> String {
    let explicit_inputs = eval_module_input_bindings(input.inputs);
    let modules = eval_module_list(input.modules);

    format!(
        r#"
let
  self = builtins.getFlake {source_ref};
  pkgs = import <nixpkgs> {{ }};
  lib = pkgs.lib;

  explicitInputs = {{
{explicit_inputs}
  }};

  getAttrPath = path: value:
    builtins.foldl'
      (acc: name: acc.${{name}})
      value
      path;

  resolveFlake = name:
    if name == "self" then self
    else if builtins.hasAttr name explicitInputs then builtins.getAttr name explicitInputs
    else getAttrPath (lib.splitString "." name) self;

  moduleAttr = flake: attr:
    getAttrPath (lib.splitString "." attr) (resolveFlake flake);

  moduleListOption = option: modules:
    {{ lib, ... }}:
    lib.setAttrByPath (lib.splitString "." option) modules;

  eval = lib.evalModules {{
    specialArgs = {{
      inherit pkgs lib;
      inputs = (self.inputs or {{}}) // explicitInputs;
      self = self;
    }};

    modules = [
{modules}
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
        source_ref = nix_string(input.source_ref),
        explicit_inputs = explicit_inputs,
        modules = modules,
    )
}

fn eval_module_input_bindings(inputs: &BTreeMap<String, String>) -> String {
    inputs
        .iter()
        .map(|(name, source_ref)| {
            format!(
                "    {} = builtins.getFlake {};",
                nix_attr_name(name),
                nix_string(source_ref)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn eval_module_list(modules: &[EvalModule]) -> String {
    modules
        .iter()
        .map(eval_module_expression)
        .map(|module| indent_lines(&module, 6))
        .collect::<Vec<_>>()
        .join("\n")
}

fn eval_module_expression(module: &EvalModule) -> String {
    match module {
        EvalModule::FlakeAttr(module_ref) => eval_module_ref_expression(module_ref),
        EvalModule::ModuleListOption { option, modules } => {
            let modules = modules
                .iter()
                .map(eval_module_ref_expression)
                .map(|module| indent_lines(&module, 8))
                .collect::<Vec<_>>()
                .join("\n");

            format!(
                "(moduleListOption {} [\n{}\n])",
                nix_string(option),
                modules
            )
        }
    }
}

fn eval_module_ref_expression(module_ref: &EvalModuleRef) -> String {
    format!(
        "(moduleAttr {} {})",
        nix_string(&module_ref.flake),
        nix_string(&module_ref.attr)
    )
}

fn nix_attr_name(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn indent_lines(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);

    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
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

fn normalize_eval_module_inputs(
    inputs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    inputs
        .iter()
        .map(|(name, source_ref)| Ok((name.clone(), normalize_flake_ref(source_ref)?)))
        .collect()
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
        self.url
            .clone()
            .unwrap_or_else(|| channel_artifact_url(&self.channel, CHANNEL_PACKAGES_FILE))
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
        let bytes = fetch_brotli_artifact(&url)
            .await
            .with_context(|| format!("failed to fetch channel packages artifact from {url}"))?;

        let artifact_ref = request.artifact_ref(ArtifactKind::PackagesJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(url.clone());
        populate_channel_revision(&mut metadata_input, &self.channel).await;

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write packages artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChannelOptionsJsonProducer {
    channel: String,
    url: Option<String>,
    producer_name: String,
}

impl ChannelOptionsJsonProducer {
    pub fn new(channel: impl Into<String>, url: Option<String>) -> Self {
        Self {
            channel: channel.into(),
            url,
            producer_name: "channel-options-json".to_owned(),
        }
    }

    fn url(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| channel_artifact_url(&self.channel, CHANNEL_OPTIONS_FILE))
    }
}

#[async_trait]
impl Producer for ChannelOptionsJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let url = self.url();
        let bytes = fetch_brotli_artifact(&url)
            .await
            .with_context(|| format!("failed to fetch channel options artifact from {url}"))?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(url.clone());
        populate_channel_revision(&mut metadata_input, &self.channel).await;

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

fn channel_artifact_url(channel: &str, file_name: &str) -> String {
    format!("{CHANNELS_BASE_URL}/{channel}/{file_name}")
}

async fn fetch_bytes(url: &str) -> Result<Bytes> {
    reqwest::get(url)
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {url}"))
}
async fn fetch_text(url: &str) -> Result<String> {
    let bytes = fetch_bytes(url).await?;

    String::from_utf8(bytes.to_vec()).with_context(|| format!("response from {url} was not UTF-8"))
}

async fn populate_download_revision(
    metadata_input: &mut ArtifactMetadataInput,
    revision_url: &str,
) {
    match fetch_text(revision_url).await {
        Ok(text) => {
            let revision = text.trim();

            if revision.is_empty() {
                metadata_input.warnings.push(format!(
                    "download revision_url returned an empty revision: {revision_url}"
                ));
            } else {
                metadata_input.revision = Some(revision.to_owned());
            }
        }
        Err(error) => metadata_input.warnings.push(format!(
            "failed to fetch download revision from {revision_url}: {error:#}"
        )),
    }
}

async fn fetch_brotli_artifact(url: &str) -> Result<Vec<u8>> {
    let compressed = fetch_bytes(url).await?;

    decompress_brotli(&compressed)
        .with_context(|| format!("failed to decompress Brotli response from {url}"))
}

fn decompress_brotli(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decompressor = brotli::Decompressor::new(bytes, 4096);
    let mut output = Vec::new();

    decompressor
        .read_to_end(&mut output)
        .context("Brotli decompression failed")?;

    Ok(output)
}

async fn populate_channel_revision(metadata_input: &mut ArtifactMetadataInput, channel: &str) {
    match fetch_channel_git_revision(channel).await {
        Ok(Some(revision)) => metadata_input.revision = Some(revision),
        Ok(None) => metadata_input.warnings.push(format!(
            "channel git-revision was empty for channel {channel:?}"
        )),
        Err(error) => metadata_input.warnings.push(format!(
            "failed to fetch channel git-revision for channel {channel:?}: {error:#}"
        )),
    }
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
    channel_artifact_url(channel, CHANNEL_GIT_REVISION_FILE)
}

async fn fetch_channel_git_revision(channel: &str) -> Result<Option<String>> {
    let url = channel_git_revision_url(channel);
    let text = fetch_text(&url).await?;
    let revision = text.trim();

    if revision.is_empty() {
        Ok(None)
    } else {
        Ok(Some(revision.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::{collections::BTreeMap, fs};

    use httpmock::prelude::*;
    use tempfile::tempdir;

    use nix_search_core::{ArtifactKind, SearchDocument};
    use nix_search_store::ArtifactStore;
    use nix_search_test_support::{
        OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, REF_SMALL, SOURCE_FIXTURES,
    };

    use crate::{
        ChannelOptionsJsonProducer, ChannelPackagesJsonProducer, EvalModule, EvalModuleRef,
        EvalModulesExpression,
    };

    use super::{
        Consumer, DownloadCompression, DownloadProducer, EvalModulesProducer, ExistingFileProducer,
        OptionsJsonConsumer, ProduceRequest, Producer, eval_modules_expression,
        normalize_nix_path_source,
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
    async fn download_producer_writes_artifact_to_store() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();
        let stored = store.get_artifact(&produced.artifact_ref).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.producer, "download");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(server.url("/options.json").as_str())
        );
        assert_eq!(
            stored.as_ref(),
            include_bytes!("../../../fixtures/search-small/options.json").as_slice()
        );
    }

    #[tokio::test]
    async fn download_producer_decompresses_brotli_artifact() {
        let server = MockServer::start_async().await;
        let artifact_bytes = include_bytes!("../../../fixtures/search-small/options.json");
        let compressed = brotli_compress(artifact_bytes.as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json.br");
                then.status(200).body(compressed);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json.br"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::Brotli,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();
        let stored = store.get_artifact(&produced.artifact_ref).await.unwrap();

        assert_eq!(stored.as_ref(), artifact_bytes.as_slice());
    }

    #[tokio::test]
    async fn download_producer_records_revision() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let _revision_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/revision");
                then.status(200).body("abc123\n");
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            Some(server.url("/revision")),
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.metadata.revision.as_deref(), Some("abc123"));
        assert!(produced.metadata.warnings.is_empty());
    }

    #[tokio::test]
    async fn download_producer_warns_when_revision_fetch_fails() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let _revision_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/revision");
                then.status(500);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            Some(server.url("/revision")),
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.metadata.revision, None);
        assert!(
            produced
                .metadata
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to fetch download revision"))
        );
    }

    #[tokio::test]
    async fn download_producer_fails_when_artifact_fetch_fails() {
        let server = MockServer::start_async().await;

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(404);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let error = producer.produce(&store, &request).await.unwrap_err();

        assert!(format!("{error:#}").contains("HTTP error fetching"));
    }

    fn brotli_compress(bytes: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();

        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(bytes).unwrap();
        }

        compressed
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
            source_ref.clone(),
            BTreeMap::new(),
            vec![EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        );

        let request = ProduceRequest {
            source: "eval-fixture".into(),
            ref_id: "local".into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.producer, "eval-modules");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(source_ref.as_str())
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
    fn channel_options_json_producer_builds_default_url() {
        let producer = ChannelOptionsJsonProducer::new("nixos-unstable", None);

        assert_eq!(
            producer.url(),
            "https://channels.nixos.org/nixos-unstable/options.json.br"
        );
    }

    #[test]
    fn eval_modules_expression_contains_flake_ref_module_attr_and_default_eval_args() {
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &BTreeMap::new(),
            modules: &[EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        });

        assert!(expression.contains("\"github:example/project\""));
        assert!(expression.contains("(moduleAttr \"self\" \"nixosModules.default\")"));
        assert!(expression.contains("builtins.getFlake"));
        assert!(expression.contains("lib.evalModules"));
        assert!(expression.contains("specialArgs"));
        assert!(expression.contains("inherit pkgs lib;"));
        assert!(expression.contains("inputs = (self.inputs or {}) // explicitInputs;"));
        assert!(expression.contains("config._module.check = false;"));
        assert!(expression.contains("pkgs.nixosOptionsDoc"));
    }

    #[test]
    fn eval_modules_expression_supports_explicit_inputs_and_module_list_options() {
        let inputs = BTreeMap::from([(
            "dependency".to_owned(),
            "github:example/dependency".to_owned(),
        )]);

        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &inputs,
            modules: &[
                EvalModule::FlakeAttr(EvalModuleRef {
                    flake: "dependency".to_owned(),
                    attr: "nixosModules.default".to_owned(),
                }),
                EvalModule::ModuleListOption {
                    option: "example.extraModules".to_owned(),
                    modules: vec![EvalModuleRef {
                        flake: "self".to_owned(),
                        attr: "modules.extra".to_owned(),
                    }],
                },
            ],
        });

        assert!(
            expression
                .contains("\"dependency\" = builtins.getFlake \"github:example/dependency\";")
        );
        assert!(expression.contains("(moduleAttr \"dependency\" \"nixosModules.default\")"));
        assert!(expression.contains("(moduleListOption \"example.extraModules\" ["));
        assert!(expression.contains("(moduleAttr \"self\" \"modules.extra\")"));
    }

    #[test]
    fn eval_modules_expression_resolves_primary_flake_inputs() {
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &BTreeMap::new(),
            modules: &[EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self.inputs.hjem".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        });

        assert!(expression.contains("(moduleAttr \"self.inputs.hjem\" \"nixosModules.default\")"));
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

    #[test]
    fn flake_installable_joins_ref_and_attribute() {
        assert_eq!(
            super::flake_installable("github:example/project/main", "docs-json"),
            "github:example/project/main#docs-json"
        );
    }
}
