use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use nix_search_core::{ArtifactKind, SourceLinkConfig};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Figment(Box<figment::Error>),

    #[error("invalid configuration: {0}")]
    Validation(String),
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        Self::Figment(Box::new(error))
    }
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub data: DataConfig,
    pub server: ServerConfig,
    pub sources: BTreeMap<String, SourceConfig>,
}

impl AppConfig {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        if let Some(path) = path {
            if !path.exists() {
                return Err(ConfigError::Validation(format!(
                    "config file does not exist: {}",
                    path.display()
                )));
            }

            figment = figment.merge(Toml::file(path));
        }

        figment = figment.merge(Env::prefixed("NIX_SEARCH_").split("__"));

        let raw: RawAppConfig = figment.extract()?;
        let config = raw.into_app_config()?;

        config.validate()?;

        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.data.validate()?;
        self.server.validate()?;

        for (source_id, source) in &self.sources {
            source.validate(source_id)?;
        }

        Ok(())
    }

    pub fn resolve_search_scopes(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
    ) -> Result<Vec<ResolvedSearchScope>> {
        match (source, ref_id) {
            (Some(source_id), Some(ref_id)) => {
                let source = self.sources.get(source_id).ok_or_else(|| {
                    ConfigError::Validation(format!("unknown source {source_id:?}"))
                })?;

                if !source.refs.iter().any(|candidate| candidate.id == ref_id) {
                    return Err(ConfigError::Validation(format!(
                        "unknown ref {ref_id:?} for source {source_id:?}"
                    )));
                }

                Ok(vec![ResolvedSearchScope {
                    source: source_id.to_owned(),
                    ref_id: ref_id.to_owned(),
                }])
            }

            (Some(source_id), None) => {
                let source = self.sources.get(source_id).ok_or_else(|| {
                    ConfigError::Validation(format!("unknown source {source_id:?}"))
                })?;

                let default_ref = source.default_ref.as_deref().ok_or_else(|| {
                    ConfigError::Validation(format!("source {source_id:?} has no default ref"))
                })?;

                Ok(vec![ResolvedSearchScope {
                    source: source_id.to_owned(),
                    ref_id: default_ref.to_owned(),
                }])
            }

            (None, Some(_)) => Err(ConfigError::Validation(
                "--ref requires --source".to_owned(),
            )),

            (None, None) => Ok(self
                .sources
                .iter()
                .filter_map(|(source_id, source)| {
                    source
                        .default_ref
                        .as_ref()
                        .map(|default_ref| ResolvedSearchScope {
                            source: source_id.clone(),
                            ref_id: default_ref.clone(),
                        })
                })
                .collect()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RawAppConfig {
    data: DataConfig,
    server: ServerConfig,
    sources: BTreeMap<String, RawSourceConfig>,
}

impl RawAppConfig {
    fn into_app_config(self) -> Result<AppConfig> {
        let mut sources = BTreeMap::new();

        for (source_id, source) in self.sources {
            sources.insert(source_id.clone(), source.into_source_config(&source_id)?);
        }

        Ok(AppConfig {
            data: self.data,
            server: self.server,
            sources,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataConfig {
    pub artifact_url: String,
    pub index_dir: PathBuf,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            artifact_url: "file://./data/artifacts".to_owned(),
            index_dir: PathBuf::from("./data/indexes"),
        }
    }
}

impl DataConfig {
    fn validate(&self) -> Result<()> {
        validate_non_empty("data.artifact_url", &self.artifact_url)?;

        if self.index_dir.as_os_str().is_empty() {
            return Err(ConfigError::Validation(
                "data.index_dir must not be empty".to_owned(),
            ));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub listen: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:3000".to_owned(),
        }
    }
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        validate_non_empty("server.listen", &self.listen)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RawSourceConfig {
    name: Option<String>,
    kind: Option<SourceKind>,
    default_ref: Option<String>,
    refs: Vec<RefConfig>,
    preset: Option<SourcePreset>,
    #[serde(rename = "ref")]
    preset_ref: Option<String>,
}

impl RawSourceConfig {
    fn into_source_config(self, source_id: &str) -> Result<SourceConfig> {
        match self.preset {
            Some(preset) => self.expand_preset(source_id, preset),
            None => self.into_explicit_source(source_id),
        }
    }

    fn into_explicit_source(self, source_id: &str) -> Result<SourceConfig> {
        let kind = self.kind.ok_or_else(|| {
            ConfigError::Validation(format!("sources.{source_id}: kind is required"))
        })?;

        let default_ref = effective_default_ref(source_id, self.default_ref, &self.refs)?;

        Ok(SourceConfig {
            name: self.name,
            kind,
            default_ref,
            refs: self.refs,
        })
    }

    fn expand_preset(self, source_id: &str, preset: SourcePreset) -> Result<SourceConfig> {
        if !self.refs.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}: preset sources must not also define refs"
            )));
        }

        let ref_id = self.preset_ref.clone().ok_or_else(|| {
            ConfigError::Validation(format!("sources.{source_id}: preset sources require ref"))
        })?;

        match preset {
            SourcePreset::NixpkgsPackages => self.expand_nixpkgs_packages(source_id, ref_id),
            SourcePreset::NixosOptions => self.expand_nixos_options(source_id, ref_id),
        }
    }

    fn expand_nixpkgs_packages(self, source_id: &str, ref_id: String) -> Result<SourceConfig> {
        reject_conflicting_kind(
            self.kind,
            SourceKind::Packages,
            SourcePreset::NixpkgsPackages,
        )?;

        let refs = vec![RefConfig {
            id: ref_id.clone(),
            source_links: Some(nixpkgs_source_links(&ref_id)),
            producer: ProducerConfig::ChannelPackagesJson {
                channel: ref_id,
                url: None,
            },
        }];

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Nixpkgs".to_owned())),
            kind: SourceKind::Packages,
            default_ref,
            refs,
        })
    }

    fn expand_nixos_options(self, source_id: &str, ref_id: String) -> Result<SourceConfig> {
        reject_conflicting_kind(self.kind, SourceKind::Options, SourcePreset::NixosOptions)?;

        let refs = vec![RefConfig {
            id: ref_id.clone(),
            source_links: Some(nixpkgs_source_links(&ref_id)),
            producer: ProducerConfig::NixBuildOptionsJson {
                source_ref: format!("github:NixOS/nixpkgs/{ref_id}"),
                attribute: "options".to_owned(),
                import_path: "nixos/release.nix".to_owned(),
                output_path: "share/doc/nixos/options.json".to_owned(),
            },
        }];

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("NixOS Options".to_owned())),
            kind: SourceKind::Options,
            default_ref,
            refs,
        })
    }
}

fn nixpkgs_source_links(revision: &str) -> SourceLinkConfig {
    SourceLinkConfig::Github {
        owner: "NixOS".to_owned(),
        repo: "nixpkgs".to_owned(),
        revision: Some(revision.to_owned()),
        strip_prefixes: Vec::new(),
    }
}

fn reject_conflicting_kind(
    configured: Option<SourceKind>,
    expected: SourceKind,
    preset: SourcePreset,
) -> Result<()> {
    if let Some(configured) = configured
        && configured != expected
    {
        return Err(ConfigError::Validation(format!(
            "preset {preset:?} requires source kind {expected:?}, got {configured:?}"
        )));
    }

    Ok(())
}

fn effective_default_ref(
    source_id: &str,
    configured: Option<String>,
    refs: &[RefConfig],
) -> Result<Option<String>> {
    if let Some(configured) = configured {
        validate_id("default_ref", &configured)?;

        if !refs.iter().any(|ref_config| ref_config.id == configured) {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}.default_ref {configured:?} does not match any configured ref"
            )));
        }

        return Ok(Some(configured));
    }

    Ok(refs.first().map(|ref_config| ref_config.id.clone()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: SourceKind,
    #[serde(default)]
    pub default_ref: Option<String>,
    #[serde(default)]
    pub refs: Vec<RefConfig>,
}

impl SourceConfig {
    fn validate(&self, source_id: &str) -> Result<()> {
        validate_id("source id", source_id)?;

        for ref_config in &self.refs {
            ref_config.validate(source_id)?;
        }

        if let Some(default_ref) = &self.default_ref {
            validate_id("default_ref", default_ref)?;

            if !self
                .refs
                .iter()
                .any(|ref_config| &ref_config.id == default_ref)
            {
                return Err(ConfigError::Validation(format!(
                    "sources.{source_id}.default_ref {default_ref:?} does not match any configured ref"
                )));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSearchScope {
    pub source: String,
    pub ref_id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    #[default]
    Options,
    Packages,
    Apps,
    Services,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourcePreset {
    NixpkgsPackages,
    NixosOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefConfig {
    pub id: String,
    pub producer: ProducerConfig,
    #[serde(default)]
    pub source_links: Option<SourceLinkConfig>,
}

impl RefConfig {
    fn validate(&self, source_id: &str) -> Result<()> {
        validate_id("ref id", &self.id)?;
        self.producer.validate(source_id, &self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProducerConfig {
    ExistingFile {
        path: PathBuf,
        artifact: ArtifactKind,
    },

    ChannelPackagesJson {
        channel: String,
        #[serde(default)]
        url: Option<String>,
    },

    NixBuildOptionsJson {
        #[serde(rename = "ref")]
        source_ref: String,
        attribute: String,
        import_path: String,
        output_path: String,
    },

    EvalModules {
        #[serde(rename = "ref")]
        source_ref: String,
        modules_attr: String,
        #[serde(default)]
        url_prefix: Option<String>,
    },

    Download {
        url: String,
        artifact: ArtifactKind,
    },

    CustomCommand {
        command: Vec<String>,
        artifact: ArtifactKind,
    },

    FlakeOutput {
        #[serde(rename = "ref")]
        source_ref: String,
    },
}

impl ProducerConfig {
    fn validate(&self, source_id: &str, ref_id: &str) -> Result<()> {
        match self {
            Self::ExistingFile { path, .. } => {
                if path.as_os_str().is_empty() {
                    return producer_error(source_id, ref_id, "path must not be empty");
                }
            }

            Self::ChannelPackagesJson { channel, url } => {
                validate_producer_non_empty(source_id, ref_id, "channel", channel)?;

                if let Some(url) = url {
                    validate_producer_non_empty(source_id, ref_id, "url", url)?;
                }
            }

            Self::NixBuildOptionsJson {
                source_ref,
                attribute,
                import_path,
                output_path,
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(source_id, ref_id, "attribute", attribute)?;
                validate_producer_non_empty(source_id, ref_id, "import_path", import_path)?;
                validate_producer_non_empty(source_id, ref_id, "output_path", output_path)?;
            }

            Self::EvalModules {
                source_ref,
                modules_attr,
                url_prefix,
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(source_id, ref_id, "modules_attr", modules_attr)?;

                if let Some(url_prefix) = url_prefix {
                    validate_producer_non_empty(source_id, ref_id, "url_prefix", url_prefix)?;
                }
            }

            Self::Download { url, .. } => {
                validate_producer_non_empty(source_id, ref_id, "url", url)?;
            }

            Self::CustomCommand { command, .. } => {
                if command.is_empty() {
                    return producer_error(source_id, ref_id, "command must not be empty");
                }

                for item in command {
                    validate_producer_non_empty(source_id, ref_id, "command item", item)?;
                }
            }

            Self::FlakeOutput { source_ref } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
            }
        }

        Ok(())
    }

    pub fn kind(&self) -> ProducerKind {
        match self {
            Self::ExistingFile { .. } => ProducerKind::ExistingFile,
            Self::ChannelPackagesJson { .. } => ProducerKind::ChannelPackagesJson,
            Self::NixBuildOptionsJson { .. } => ProducerKind::NixBuildOptionsJson,
            Self::EvalModules { .. } => ProducerKind::EvalModules,
            Self::Download { .. } => ProducerKind::Download,
            Self::CustomCommand { .. } => ProducerKind::CustomCommand,
            Self::FlakeOutput { .. } => ProducerKind::FlakeOutput,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProducerKind {
    ExistingFile,
    ChannelPackagesJson,
    NixBuildOptionsJson,
    EvalModules,
    Download,
    CustomCommand,
    FlakeOutput,
}

fn validate_non_empty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ConfigError::Validation(format!("{name} must not be empty")));
    }

    Ok(())
}

fn validate_id(name: &str, value: &str) -> Result<()> {
    validate_non_empty(name, value)?;

    if value.contains('/') {
        return Err(ConfigError::Validation(format!(
            "{name} must not contain '/': {value:?}"
        )));
    }

    Ok(())
}

fn validate_producer_non_empty(
    source_id: &str,
    ref_id: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    if value.trim().is_empty() {
        return producer_error(source_id, ref_id, &format!("{field} must not be empty"));
    }

    Ok(())
}

fn producer_error<T>(source_id: &str, ref_id: &str, message: &str) -> Result<T> {
    Err(ConfigError::Validation(format!(
        "sources.{source_id}.refs.{ref_id}: {message}"
    )))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::{TempDir, tempdir};

    use nix_search_core::{ArtifactKind, SourceLinkConfig};

    use super::{AppConfig, ProducerConfig, ProducerKind, SourceKind};

    const FIXTURES_SOURCE: &str = "fixtures";
    const NIXOS_SOURCE: &str = "nixos";
    const NIXPKGS_SOURCE: &str = "nixpkgs";
    const SMALL_REF: &str = "small";
    const STABLE_REF: &str = "stable";
    const UNSTABLE_REF: &str = "unstable";
    const NIXOS_UNSTABLE_REF: &str = "nixos-unstable";
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
        let path = dir.path().join("nix-search.toml");
        fs::write(&path, toml).unwrap();
        path
    }

    fn fixture_existing_file_source_toml() -> &'static str {
        r#"
        [sources.fixtures]
        name = "Fixtures"
        kind = "options"

        [[sources.fixtures.refs]]
        id = "small"

        [sources.fixtures.refs.producer]
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

            [[sources.fixtures.refs]]
            id = "stable"

            [sources.fixtures.refs.producer]
            type = "existing-file"
            path = "fixtures/search-small/options.json"
            artifact = "options-json"

            [[sources.fixtures.refs]]
            id = "unstable"

            [sources.fixtures.refs.producer]
            type = "existing-file"
            path = "fixtures/search-small/options.json"
            artifact = "options-json"
            "#
        )
    }

    fn assert_single_scope(
        scopes: &[super::ResolvedSearchScope],
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
        assert_eq!(config.data.index_dir, PathBuf::from("./data/indexes"));
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

            [[sources.nixos.refs]]
            id = "unstable"

            [sources.nixos.refs.producer]
            type = "nix-build-options-json"
            ref = "github:NixOS/nixpkgs/nixos-unstable"
            attribute = "options"
            import_path = "nixos/release.nix"
            output_path = "share/doc/nixos/options.json"

            [sources.nixpkgs]
            name = "Nixpkgs"
            kind = "packages"

            [[sources.nixpkgs.refs]]
            id = "unstable"

            [sources.nixpkgs.refs.producer]
            type = "channel-packages-json"
            channel = "nixos-unstable"
            "#,
        );

        assert_eq!(config.data.artifact_url, "file://./tmp/artifacts");
        assert_eq!(config.server.listen, "0.0.0.0:8080");

        let options = &config.sources[NIXOS_SOURCE];
        assert_eq!(options.name.as_deref(), Some("NixOS Options"));
        assert_eq!(options.kind, SourceKind::Options);
        assert_eq!(
            options.refs[0].producer.kind(),
            ProducerKind::NixBuildOptionsJson
        );

        let packages = &config.sources[NIXPKGS_SOURCE];
        assert_eq!(packages.name.as_deref(), Some("Nixpkgs"));
        assert_eq!(packages.kind, SourceKind::Packages);
        assert_eq!(
            packages.refs[0].producer.kind(),
            ProducerKind::ChannelPackagesJson
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

            [[sources.fixtures.refs]]
            id = "eval"

            [sources.fixtures.refs.producer]
            type = "eval-modules"
            ref = "path:/some/flake"
            modules_attr = "nixosModules.default"
            url_prefix = "https://example.com/blob/main/"
            "#,
        );

        let producer = &config.sources[FIXTURES_SOURCE].refs[0].producer;

        assert_eq!(producer.kind(), ProducerKind::EvalModules);

        match producer {
            ProducerConfig::EvalModules {
                source_ref,
                modules_attr,
                url_prefix,
            } => {
                assert_eq!(source_ref, "path:/some/flake");
                assert_eq!(modules_attr, "nixosModules.default");
                assert_eq!(
                    url_prefix.as_deref(),
                    Some("https://example.com/blob/main/")
                );
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

            [[sources.nixos.refs]]
            id = "unstable"

            [sources.nixos.refs.producer]
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

            [[sources.custom.refs]]
            id = "main"

            [sources.custom.refs.producer]
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

            [[sources.fixtures.refs]]
            id = "main"

            [sources.fixtures.refs.source_links]
            type = "github"
            owner = "example"
            repo = "modules"
            revision = "abc123"
            strip_prefixes = ["/build/source/"]

            [sources.fixtures.refs.producer]
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
            ref = "nixos-unstable"
            "#,
        );

        let source = &config.sources[NIXPKGS_SOURCE];

        assert_eq!(source.name.as_deref(), Some("Nixpkgs"));
        assert_eq!(source.kind, SourceKind::Packages);
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
            ref = "nixos-unstable"
            "#,
        );

        let source = &config.sources[NIXOS_SOURCE];

        assert_eq!(source.name.as_deref(), Some("NixOS Options"));
        assert_eq!(source.kind, SourceKind::Options);
        assert_eq!(source.refs.len(), 1);

        let ref_config = &source.refs[0];

        assert_eq!(ref_config.id, NIXOS_UNSTABLE_REF);
        assert_eq!(
            ref_config.producer.kind(),
            ProducerKind::NixBuildOptionsJson
        );
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

        assert_error_contains(&error, "preset sources require ref");
    }

    #[test]
    fn preset_rejects_explicit_refs() {
        let error = load_toml_error(
            r#"
            [sources.nixpkgs]
            name = "Nixpkgs"
            preset = "nixpkgs-packages"
            ref = "nixos-unstable"

            [[sources.nixpkgs.refs]]
            id = "manual"

            [sources.nixpkgs.refs.producer]
            type = "existing-file"
            path = "fixtures/search-small/options.json"
            artifact = "options-json"
            "#,
        );

        assert_error_contains(&error, "preset sources must not also define refs");
    }

    #[test]
    fn preset_rejects_conflicting_kind() {
        let error = load_toml_error(
            r#"
            [sources.nixpkgs]
            name = "Nixpkgs"
            preset = "nixpkgs-packages"
            kind = "options"
            ref = "nixos-unstable"
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
    fn infers_default_ref_from_first_ref() {
        let config = load_toml(&fixture_two_ref_source_toml(None));

        assert_eq!(
            config.sources[FIXTURES_SOURCE].default_ref.as_deref(),
            Some(STABLE_REF)
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

            [[sources.fixtures.refs]]
            id = "small"

            [sources.fixtures.refs.producer]
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
            ref = "nixos-unstable"
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
}
