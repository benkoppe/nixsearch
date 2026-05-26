use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use camino::Utf8PathBuf;
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

const MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);

const NIXPKGS_COLOR: &str = "#4ade80";
const NIXOS_COLOR: &str = "#60a5fa";
const HOME_MANAGER_COLOR: &str = "#f59e0b";
const NIX_DARWIN_COLOR: &str = "#a78bfa";

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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct DataConfig {
    pub artifact_url: String,
    pub index_dir: Utf8PathBuf,
}

impl Default for DataConfig {
    fn default() -> Self {
        Self {
            artifact_url: "file://./data/artifacts".to_owned(),
            index_dir: Utf8PathBuf::from("./data/indexes"),
        }
    }
}

impl DataConfig {
    fn validate(&self) -> Result<()> {
        validate_non_empty("data.artifact_url", &self.artifact_url)?;

        if self.index_dir.as_str().is_empty() {
            return Err(ConfigError::Validation(
                "data.index_dir must not be empty".to_owned(),
            ));
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub bootstrap: bool,
    pub schedule: ScheduleConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:3000".to_owned(),
            bootstrap: true,
            schedule: ScheduleConfig::default(),
        }
    }
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        validate_non_empty("server.listen", &self.listen)?;
        self.schedule.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub interval: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: "24h".to_owned(),
        }
    }
}

impl ScheduleConfig {
    pub fn parse_interval(&self) -> std::result::Result<Duration, ConfigError> {
        parse_duration(&self.interval).map_err(|message| {
            ConfigError::Validation(format!("server.schedule.interval: {message}"))
        })
    }

    fn validate(&self) -> Result<()> {
        self.parse_interval()?;
        Ok(())
    }
}

fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();

    if s.is_empty() {
        return Err("interval must not be empty".to_owned());
    }

    let split_pos = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());

    let (number, unit) = s.split_at(split_pos);

    if number.is_empty() {
        return Err(format!("invalid number in {s:?}"));
    }

    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid number in {s:?}"))?;

    if !value.is_finite() || value <= 0.0 {
        return Err("interval must be positive".to_owned());
    }

    let seconds = match unit.trim() {
        "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600.0,
        "d" | "day" | "days" => value * 86400.0,
        other => return Err(format!("unknown time unit {other:?}; use s, m, h, or d")),
    };

    if !seconds.is_finite() {
        return Err("interval is out of range".to_owned());
    }

    let duration = std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| "interval is out of range".to_owned())?;

    if duration < MIN_SCHEDULE_INTERVAL {
        return Err(format!(
            "interval must be at least {}s",
            MIN_SCHEDULE_INTERVAL.as_secs()
        ));
    }

    Ok(duration)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSourceConfig {
    name: Option<String>,
    color: Option<String>,
    kind: Option<SourceKind>,
    default_ref: Option<String>,
    refs: BTreeMap<String, RawRefConfig>,
    preset: Option<SourcePreset>,
    preset_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRefConfig {
    pub producer: ProducerConfig,
    #[serde(default)]
    pub source_links: Option<SourceLinkConfig>,
}

impl RawRefConfig {
    fn into_ref_config(self, id: String) -> RefConfig {
        RefConfig {
            id,
            producer: self.producer,
            source_links: self.source_links,
        }
    }
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

        if !self.preset_refs.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}: preset_refs requires preset"
            )));
        }

        let refs = self
            .refs
            .into_iter()
            .map(|(id, ref_config)| ref_config.into_ref_config(id))
            .collect::<Vec<_>>();
        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name,
            color: self.color,
            kind,
            default_ref,
            refs,
        })
    }

    fn expand_preset(self, source_id: &str, preset: SourcePreset) -> Result<SourceConfig> {
        if !self.refs.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}: preset sources must not define explicit refs"
            )));
        }

        let ref_ids = self.preset_refs.clone();

        if ref_ids.is_empty() {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}: preset sources require at least one ref"
            )));
        }

        match preset {
            SourcePreset::NixpkgsPackages => self.expand_nixpkgs_packages(source_id, ref_ids),
            SourcePreset::NixosOptions => self.expand_nixos_options(source_id, ref_ids),
            SourcePreset::HomeManagerOptions => {
                self.expand_home_manager_options(source_id, ref_ids)
            }
            SourcePreset::NixDarwinOptions => self.expand_nix_darwin_options(source_id, ref_ids),
        }
    }

    fn expand_nixpkgs_packages(
        self,
        source_id: &str,
        ref_ids: Vec<String>,
    ) -> Result<SourceConfig> {
        reject_conflicting_kind(
            self.kind,
            SourceKind::Packages,
            SourcePreset::NixpkgsPackages,
        )?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(nixpkgs_source_links(&ref_id)),
                producer: ProducerConfig::ChannelPackagesJson {
                    channel: ref_id,
                    url: None,
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Nixpkgs".to_owned())),
            color: self.color.or_else(|| Some(NIXPKGS_COLOR.to_owned())),
            kind: SourceKind::Packages,
            default_ref,
            refs,
        })
    }

    fn expand_nixos_options(self, source_id: &str, ref_ids: Vec<String>) -> Result<SourceConfig> {
        reject_conflicting_kind(self.kind, SourceKind::Options, SourcePreset::NixosOptions)?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(nixpkgs_source_links(&ref_id)),
                producer: ProducerConfig::ChannelOptionsJson {
                    channel: ref_id,
                    url: None,
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("NixOS Options".to_owned())),
            color: self.color.or_else(|| Some(NIXOS_COLOR.to_owned())),
            kind: SourceKind::Options,
            default_ref,
            refs,
        })
    }

    fn expand_home_manager_options(
        self,
        source_id: &str,
        ref_ids: Vec<String>,
    ) -> Result<SourceConfig> {
        reject_conflicting_kind(
            self.kind,
            SourceKind::Options,
            SourcePreset::HomeManagerOptions,
        )?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(home_manager_source_links(&ref_id)),
                producer: ProducerConfig::NixBuildOptionsJson {
                    source_ref: format!("github:nix-community/home-manager/{ref_id}"),
                    attribute: "docs.json".to_owned(),
                    import_path: "default.nix".to_owned(),
                    output_path: "share/doc/home-manager/options.json".to_owned(),
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Home Manager".to_owned())),
            color: self.color.or_else(|| Some(HOME_MANAGER_COLOR.to_owned())),
            kind: SourceKind::Options,
            default_ref,
            refs,
        })
    }

    fn expand_nix_darwin_options(
        self,
        source_id: &str,
        ref_ids: Vec<String>,
    ) -> Result<SourceConfig> {
        reject_conflicting_kind(
            self.kind,
            SourceKind::Options,
            SourcePreset::NixDarwinOptions,
        )?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(nix_darwin_source_links(&ref_id)),
                producer: ProducerConfig::NixBuildOptionsJson {
                    source_ref: format!("github:nix-darwin/nix-darwin/{ref_id}"),
                    attribute: "docs.optionsJSON".to_owned(),
                    import_path: "release.nix".to_owned(),
                    output_path: "share/doc/darwin/options.json".to_owned(),
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("nix-darwin".to_owned())),
            color: self.color.or_else(|| Some(NIX_DARWIN_COLOR.to_owned())),
            kind: SourceKind::Options,
            default_ref,
            refs,
        })
    }
}

fn nixpkgs_source_links(revision: &str) -> SourceLinkConfig {
    github_source_links("NixOS", "nixpkgs", revision)
}

fn home_manager_source_links(revision: &str) -> SourceLinkConfig {
    github_source_links("nix-community", "home-manager", revision)
}

fn nix_darwin_source_links(revision: &str) -> SourceLinkConfig {
    github_source_links("nix-darwin", "nix-darwin", revision)
}

fn github_source_links(owner: &str, repo: &str, revision: &str) -> SourceLinkConfig {
    SourceLinkConfig::Github {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
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

    if refs.len() > 1 {
        return Err(ConfigError::Validation(format!(
            "sources.{source_id}: default_ref is required when multiple refs are configured"
        )));
    }

    Ok(refs.first().map(|ref_config| ref_config.id.clone()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
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

        if let Some(color) = &self.color {
            validate_hex_color(&format!("sources.{source_id}.color"), color)?;
        }

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
    HomeManagerOptions,
    NixDarwinOptions,
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
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
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

    ChannelOptionsJson {
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
        #[serde(default)]
        inputs: BTreeMap<String, String>,
        modules: Vec<EvalModuleConfig>,
    },

    Download {
        url: String,
        artifact: ArtifactKind,
        #[serde(default)]
        revision_url: Option<String>,
        #[serde(default)]
        compression: DownloadCompression,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadCompression {
    #[default]
    None,
    Brotli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EvalModuleConfig {
    FlakeAttr {
        #[serde(default = "default_eval_module_flake")]
        flake: String,
        attr: String,
    },
    ModuleListOption {
        option: String,
        modules: Vec<EvalModuleRefConfig>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalModuleRefConfig {
    #[serde(default = "default_eval_module_flake")]
    pub flake: String,
    pub attr: String,
}

fn default_eval_module_flake() -> String {
    "self".to_owned()
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

            Self::ChannelOptionsJson { channel, url } => {
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
                inputs,
                modules,
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;

                if modules.is_empty() {
                    return producer_error(source_id, ref_id, "modules must not be empty");
                }

                for (name, source_ref) in inputs {
                    validate_producer_non_empty(source_id, ref_id, "input name", name)?;
                    validate_producer_non_empty(source_id, ref_id, "input ref", source_ref)?;
                }

                for module in modules {
                    validate_eval_module_config(source_id, ref_id, module)?;
                }
            }

            Self::Download {
                url, revision_url, ..
            } => {
                validate_producer_non_empty(source_id, ref_id, "url", url)?;

                if let Some(revision_url) = revision_url {
                    validate_producer_non_empty(source_id, ref_id, "revision_url", revision_url)?;
                }
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
            Self::ChannelOptionsJson { .. } => ProducerKind::ChannelOptionsJson,
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
    ChannelOptionsJson,
    NixBuildOptionsJson,
    EvalModules,
    Download,
    CustomCommand,
    FlakeOutput,
}

fn validate_eval_module_config(
    source_id: &str,
    ref_id: &str,
    module: &EvalModuleConfig,
) -> Result<()> {
    match module {
        EvalModuleConfig::FlakeAttr { flake, attr } => {
            validate_producer_non_empty(source_id, ref_id, "module flake", flake)?;
            validate_producer_non_empty(source_id, ref_id, "module attr", attr)?;
        }
        EvalModuleConfig::ModuleListOption { option, modules } => {
            validate_producer_non_empty(source_id, ref_id, "module list option", option)?;

            if modules.is_empty() {
                return producer_error(source_id, ref_id, "module list modules must not be empty");
            }

            for module in modules {
                validate_eval_module_ref_config(source_id, ref_id, module)?;
            }
        }
    }

    Ok(())
}

fn validate_eval_module_ref_config(
    source_id: &str,
    ref_id: &str,
    module: &EvalModuleRefConfig,
) -> Result<()> {
    validate_producer_non_empty(source_id, ref_id, "module flake", &module.flake)?;
    validate_producer_non_empty(source_id, ref_id, "module attr", &module.attr)
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

fn validate_hex_color(name: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix('#') else {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
        )));
    };

    if hex.len() != 3 && hex.len() != 6 {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
        )));
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
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

    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nix_search_core::{ArtifactKind, SourceLinkConfig};

    use crate::{
        DownloadCompression, EvalModuleConfig, HOME_MANAGER_COLOR, NIX_DARWIN_COLOR, NIXOS_COLOR,
        NIXPKGS_COLOR,
    };

    use super::{AppConfig, ProducerConfig, ProducerKind, SourceKind};

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
        let path = dir.path().join("nix-search.toml");
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
        let path = repo_root.join("nix-search.example.toml");

        let config = AppConfig::load(Some(&path)).unwrap();

        assert!(config.sources.contains_key("eval-fixture"));
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
        assert_eq!(
            ref_config.producer.kind(),
            ProducerKind::NixBuildOptionsJson
        );

        match &ref_config.producer {
            ProducerConfig::NixBuildOptionsJson {
                source_ref,
                attribute,
                import_path,
                output_path,
            } => {
                assert_eq!(source_ref, "github:nix-community/home-manager/master");
                assert_eq!(attribute, "docs.json");
                assert_eq!(import_path, "default.nix");
                assert_eq!(output_path, "share/doc/home-manager/options.json");
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
            ProducerConfig::NixBuildOptionsJson { source_ref, .. } => {
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
            name = "nix-darwin"
            preset = "nix-darwin-options"
            preset_refs = ["master"]
            "#,
        );

        let source = &config.sources["darwin"];

        assert_eq!(source.name.as_deref(), Some("nix-darwin"));
        assert_eq!(source.kind, SourceKind::Options);
        assert_eq!(source.color.as_deref(), Some(NIX_DARWIN_COLOR));
        assert_eq!(source.refs.len(), 1);

        let ref_config = &source.refs[0];

        assert_eq!(ref_config.id, "master");
        assert_eq!(
            ref_config.producer.kind(),
            ProducerKind::NixBuildOptionsJson
        );

        match &ref_config.producer {
            ProducerConfig::NixBuildOptionsJson {
                source_ref,
                attribute,
                import_path,
                output_path,
            } => {
                assert_eq!(source_ref, "github:nix-darwin/nix-darwin/master");
                assert_eq!(attribute, "docs.optionsJSON");
                assert_eq!(import_path, "release.nix");
                assert_eq!(output_path, "share/doc/darwin/options.json");
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
            ProducerConfig::NixBuildOptionsJson { source_ref, .. } => {
                assert_eq!(source_ref, "github:nix-darwin/nix-darwin/nix-darwin-25.11");
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
            super::parse_duration("24h").unwrap(),
            std::time::Duration::from_secs(86_400)
        );
        assert_eq!(
            super::parse_duration("12h").unwrap(),
            std::time::Duration::from_secs(43_200)
        );
        assert_eq!(
            super::parse_duration("1d").unwrap(),
            std::time::Duration::from_secs(86_400)
        );
        assert_eq!(
            super::parse_duration("30m").unwrap(),
            std::time::Duration::from_secs(1_800)
        );
        assert_eq!(
            super::parse_duration("3600s").unwrap(),
            std::time::Duration::from_secs(3_600)
        );
        assert_eq!(
            super::parse_duration("0.5d").unwrap(),
            std::time::Duration::from_secs(43_200)
        );
        assert_eq!(
            super::parse_duration(" 24h ").unwrap(),
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
                super::parse_duration(value).is_err(),
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
}
