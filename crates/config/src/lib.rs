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
    pub projects: BTreeMap<String, ProjectConfig>,
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

        for (project_key, project) in &self.projects {
            project.validate(project_key)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RawAppConfig {
    data: DataConfig,
    server: ServerConfig,
    projects: BTreeMap<String, RawProjectConfig>,
}

impl RawAppConfig {
    fn into_app_config(self) -> Result<AppConfig> {
        let mut projects = BTreeMap::new();

        for (project_id, project) in self.projects {
            projects.insert(
                project_id.clone(),
                project.into_project_config(&project_id)?,
            );
        }

        Ok(AppConfig {
            data: self.data,
            server: self.server,
            projects,
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
struct RawProjectConfig {
    name: Option<String>,
    datasets: Vec<RawDatasetConfig>,
}

impl RawProjectConfig {
    fn into_project_config(self, project_id: &str) -> Result<ProjectConfig> {
        let mut datasets = Vec::with_capacity(self.datasets.len());

        for dataset in self.datasets {
            datasets.push(dataset.into_dataset_config(project_id)?);
        }

        Ok(ProjectConfig {
            name: self.name,
            datasets,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RawDatasetConfig {
    id: Option<String>,
    name: Option<String>,
    kind: Option<DatasetKind>,
    refs: Vec<RefConfig>,
    preset: Option<DatasetPreset>,
    #[serde(rename = "ref")]
    preset_ref: Option<String>,
}

impl RawDatasetConfig {
    fn into_dataset_config(self, project_id: &str) -> Result<DatasetConfig> {
        match self.preset {
            Some(preset) => self.expand_preset(project_id, preset),
            None => self.into_explicit_dataset(project_id),
        }
    }

    fn into_explicit_dataset(self, project_id: &str) -> Result<DatasetConfig> {
        let id = self.id.ok_or_else(|| {
            ConfigError::Validation(format!(
                "projects.{project_id}.datasets: dataset id is required"
            ))
        })?;

        Ok(DatasetConfig {
            id,
            name: self.name,
            kind: self.kind.unwrap_or_default(),
            refs: self.refs,
        })
    }

    fn expand_preset(self, project_id: &str, preset: DatasetPreset) -> Result<DatasetConfig> {
        if !self.refs.is_empty() {
            return Err(ConfigError::Validation(format!(
                "projects.{project_id}.datasets: preset datasets must not also define refs"
            )));
        }

        let ref_id = self.preset_ref.clone().ok_or_else(|| {
            ConfigError::Validation(format!(
                "projects.{project_id}.datasets: preset datasets require ref"
            ))
        })?;

        match preset {
            DatasetPreset::NixpkgsPackages => self.expand_nixpkgs_packages(ref_id),
            DatasetPreset::NixosOptions => self.expand_nixos_options(ref_id),
        }
    }

    fn expand_nixpkgs_packages(self, ref_id: String) -> Result<DatasetConfig> {
        reject_conflicting_kind(
            self.kind,
            DatasetKind::Packages,
            DatasetPreset::NixpkgsPackages,
        )?;

        Ok(DatasetConfig {
            id: self.id.unwrap_or_else(|| "packages".to_owned()),
            name: self.name.or_else(|| Some("Nix Packages".to_owned())),
            kind: DatasetKind::Packages,
            refs: vec![RefConfig {
                id: ref_id.clone(),
                source_links: Some(nixpkgs_source_links(&ref_id)),
                producer: ProducerConfig::ChannelPackagesJson {
                    channel: ref_id,
                    url: None,
                },
            }],
        })
    }

    fn expand_nixos_options(self, ref_id: String) -> Result<DatasetConfig> {
        reject_conflicting_kind(self.kind, DatasetKind::Options, DatasetPreset::NixosOptions)?;

        Ok(DatasetConfig {
            id: self.id.unwrap_or_else(|| "nixos-options".to_owned()),
            name: self.name.or_else(|| Some("NixOS Options".to_owned())),
            kind: DatasetKind::Options,
            refs: vec![RefConfig {
                id: ref_id.clone(),
                source_links: Some(nixpkgs_source_links(&ref_id)),
                producer: ProducerConfig::NixBuildOptionsJson {
                    source_ref: format!("github:NixOS/nixpkgs/{ref_id}"),
                    attribute: "options".to_owned(),
                    import_path: "nixos/release.nix".to_owned(),
                    output_path: "share/doc/nixos/options.json".to_owned(),
                },
            }],
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
    configured: Option<DatasetKind>,
    expected: DatasetKind,
    preset: DatasetPreset,
) -> Result<()> {
    if let Some(configured) = configured
        && configured != expected
    {
        return Err(ConfigError::Validation(format!(
            "preset {preset:?} requires dataset kind {expected:?}, got {configured:?}"
        )));
    }

    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub datasets: Vec<DatasetConfig>,
}

impl ProjectConfig {
    fn validate(&self, project_key: &str) -> Result<()> {
        validate_id("project key", project_key)?;

        for dataset in &self.datasets {
            dataset.validate(project_key)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: DatasetKind,
    #[serde(default)]
    pub refs: Vec<RefConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatasetPreset {
    NixpkgsPackages,
    NixosOptions,
}

impl DatasetConfig {
    fn validate(&self, project_key: &str) -> Result<()> {
        validate_id("dataset id", &self.id)?;

        for ref_config in &self.refs {
            ref_config.validate(project_key, &self.id)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatasetKind {
    #[default]
    Options,
    Packages,
    Apps,
    Services,
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefConfig {
    pub id: String,
    pub producer: ProducerConfig,
    #[serde(default)]
    pub source_links: Option<SourceLinkConfig>,
}

impl RefConfig {
    fn validate(&self, project_id: &str, dataset_id: &str) -> Result<()> {
        validate_id("ref id", &self.id)?;
        self.producer.validate(project_id, dataset_id, &self.id)
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
    fn validate(&self, project_id: &str, dataset_id: &str, ref_id: &str) -> Result<()> {
        match self {
            Self::ExistingFile { path, .. } => {
                if path.as_os_str().is_empty() {
                    return producer_error(
                        project_id,
                        dataset_id,
                        ref_id,
                        "path must not be empty",
                    );
                }
            }

            Self::ChannelPackagesJson { channel, url } => {
                validate_producer_non_empty(project_id, dataset_id, ref_id, "channel", channel)?;

                if let Some(url) = url {
                    validate_producer_non_empty(project_id, dataset_id, ref_id, "url", url)?;
                }
            }

            Self::NixBuildOptionsJson {
                source_ref,
                attribute,
                import_path,
                output_path,
            } => {
                validate_producer_non_empty(project_id, dataset_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(
                    project_id,
                    dataset_id,
                    ref_id,
                    "attribute",
                    attribute,
                )?;
                validate_producer_non_empty(
                    project_id,
                    dataset_id,
                    ref_id,
                    "import_path",
                    import_path,
                )?;
                validate_producer_non_empty(
                    project_id,
                    dataset_id,
                    ref_id,
                    "output_path",
                    output_path,
                )?;
            }

            Self::EvalModules {
                source_ref,
                modules_attr,
                url_prefix,
            } => {
                validate_producer_non_empty(project_id, dataset_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(
                    project_id,
                    dataset_id,
                    ref_id,
                    "modules_attr",
                    modules_attr,
                )?;

                if let Some(url_prefix) = url_prefix {
                    validate_producer_non_empty(
                        project_id,
                        dataset_id,
                        ref_id,
                        "url_prefix",
                        url_prefix,
                    )?;
                }
            }

            Self::Download { url, .. } => {
                validate_producer_non_empty(project_id, dataset_id, ref_id, "url", url)?;
            }

            Self::CustomCommand { command, .. } => {
                if command.is_empty() {
                    return producer_error(
                        project_id,
                        dataset_id,
                        ref_id,
                        "command must not be empty",
                    );
                }

                for item in command {
                    validate_producer_non_empty(
                        project_id,
                        dataset_id,
                        ref_id,
                        "command item",
                        item,
                    )?;
                }
            }

            Self::FlakeOutput { source_ref } => {
                validate_producer_non_empty(project_id, dataset_id, ref_id, "ref", source_ref)?;
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
    project_id: &str,
    dataset_id: &str,
    ref_id: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    if value.trim().is_empty() {
        return producer_error(
            project_id,
            dataset_id,
            ref_id,
            &format!("{field} must not be empty"),
        );
    }

    Ok(())
}

fn producer_error<T>(project_id: &str, dataset_id: &str, ref_id: &str, message: &str) -> Result<T> {
    Err(ConfigError::Validation(format!(
        "projects.{project_id}.datasets.{dataset_id}.refs.{ref_id}: {message}"
    )))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use nix_search_core::{ArtifactKind, SourceLinkConfig};

    use super::{AppConfig, DatasetKind, ProducerConfig, ProducerKind};

    #[test]
    fn default_config_is_valid() {
        let config = AppConfig::load(None).unwrap();

        assert_eq!(config.data.artifact_url, "file://./data/artifacts");
        assert_eq!(
            config.data.index_dir,
            std::path::PathBuf::from("./data/indexes")
        );
        assert_eq!(config.server.listen, "127.0.0.1:3000");
        assert!(config.projects.is_empty());
    }

    #[test]
    fn loads_config_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
               [data]
               artifact_url = "file://./tmp/artifacts"
               index_dir = "./tmp/indexes"

               [server]
               listen = "0.0.0.0:8080"

               [projects.nixpkgs]
               name = "Nixpkgs"

               [[projects.nixpkgs.datasets]]
               id = "nixos-options"
               name = "NixOS Options"
               kind = "options"

               [[projects.nixpkgs.datasets.refs]]
               id = "unstable"

               [projects.nixpkgs.datasets.refs.producer]
               type = "nix-build-options-json"
               ref = "github:NixOS/nixpkgs/nixos-unstable"
               attribute = "options"
               import_path = "nixos/release.nix"
               output_path = "share/doc/nixos/options.json"

               [[projects.nixpkgs.datasets]]
               id = "packages"
               name = "Nix Packages"
               kind = "packages"

               [[projects.nixpkgs.datasets.refs]]
               id = "unstable"

               [projects.nixpkgs.datasets.refs.producer]
               type = "channel-packages-json"
               channel = "nixos-unstable"
               "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        assert_eq!(config.data.artifact_url, "file://./tmp/artifacts");
        assert_eq!(config.server.listen, "0.0.0.0:8080");

        let project = config.projects.get("nixpkgs").unwrap();
        assert_eq!(project.name.as_deref(), Some("Nixpkgs"));
        assert_eq!(project.datasets.len(), 2);

        let options = &project.datasets[0];
        assert_eq!(options.id, "nixos-options");
        assert_eq!(options.kind, DatasetKind::Options);
        assert_eq!(
            options.refs[0].producer.kind(),
            ProducerKind::NixBuildOptionsJson
        );

        let packages = &project.datasets[1];
        assert_eq!(packages.id, "packages");
        assert_eq!(packages.kind, DatasetKind::Packages);
        assert_eq!(
            packages.refs[0].producer.kind(),
            ProducerKind::ChannelPackagesJson
        );
    }

    #[test]
    fn loads_existing_file_producer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
               [projects.fixtures]
               name = "Fixtures"

               [[projects.fixtures.datasets]]
               id = "options"
               kind = "options"

               [[projects.fixtures.datasets.refs]]
               id = "small"

               [projects.fixtures.datasets.refs.producer]
               type = "existing-file"
               path = "fixtures/options-small.json"
               artifact = "options-json"
               "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        let producer = &config.projects["fixtures"].datasets[0].refs[0].producer;

        assert_eq!(producer.kind(), ProducerKind::ExistingFile);

        match producer {
            ProducerConfig::ExistingFile { path, artifact } => {
                assert_eq!(path, &PathBuf::from("fixtures/options-small.json"));
                assert_eq!(*artifact, ArtifactKind::OptionsJson);
            }
            other => panic!("unexpected producer: {other:?}"),
        }
    }

    #[test]
    fn loads_eval_modules_producer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.fixtures]
              name = "Fixtures"

              [[projects.fixtures.datasets]]
              id = "options"
              kind = "options"

              [[projects.fixtures.datasets.refs]]
              id = "eval"

              [projects.fixtures.datasets.refs.producer]
              type = "eval-modules"
              ref = "path:/some/flake"
              modules_attr = "nixosModules.default"
              url_prefix = "https://example.com/blob/main/"
              "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        let producer = &config.projects["fixtures"].datasets[0].refs[0].producer;

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
    fn rejects_invalid_project_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
               [projects."bad/project"]
               name = "Bad Project"
               "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("must not contain '/'"));
    }

    #[test]
    fn validates_nix_build_options_required_fields_by_deserialization() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
               [projects.nixpkgs]
               name = "Nixpkgs"

               [[projects.nixpkgs.datasets]]
               id = "nixos-options"
               kind = "options"

               [[projects.nixpkgs.datasets.refs]]
               id = "unstable"

               [projects.nixpkgs.datasets.refs.producer]
               type = "nix-build-options-json"
               ref = "github:NixOS/nixpkgs/nixos-unstable"
               "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("attribute"));
    }

    #[test]
    fn validates_custom_command_is_not_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
               [projects.custom]
               name = "Custom"

               [[projects.custom.datasets]]
               id = "options"
               kind = "options"

               [[projects.custom.datasets.refs]]
               id = "main"

               [projects.custom.datasets.refs.producer]
               type = "custom-command"
               command = []
               artifact = "options-json"
               "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("command must not be empty"));
    }

    #[test]
    fn loads_ref_source_links() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.fixtures]
              name = "Fixtures"

              [[projects.fixtures.datasets]]
              id = "options"
              kind = "options"

              [[projects.fixtures.datasets.refs]]
              id = "main"

              [projects.fixtures.datasets.refs.source_links]
              type = "github"
              owner = "example"
              repo = "modules"
              revision = "abc123"
              strip_prefixes = ["/build/source/"]

              [projects.fixtures.datasets.refs.producer]
              type = "existing-file"
              path = "fixtures/options-small.json"
              artifact = "options-json"
              "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();
        let source_links = config.projects["fixtures"].datasets[0].refs[0]
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
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixpkgs-packages"
              ref = "nixos-unstable"
              "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        let dataset = &config.projects["nixpkgs"].datasets[0];

        assert_eq!(dataset.id, "packages");
        assert_eq!(dataset.name.as_deref(), Some("Nix Packages"));
        assert_eq!(dataset.kind, DatasetKind::Packages);
        assert_eq!(dataset.refs.len(), 1);

        let ref_config = &dataset.refs[0];

        assert_eq!(ref_config.id, "nixos-unstable");
        assert_eq!(
            ref_config.producer.kind(),
            ProducerKind::ChannelPackagesJson
        );

        match &ref_config.producer {
            ProducerConfig::ChannelPackagesJson { channel, url } => {
                assert_eq!(channel, "nixos-unstable");
                assert_eq!(url, &None);
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
                assert_eq!(owner, "NixOS");
                assert_eq!(repo, "nixpkgs");
                assert_eq!(revision.as_deref(), Some("nixos-unstable"));
                assert!(strip_prefixes.is_empty());
            }
            other => panic!("unexpected source links config: {other:?}"),
        }
    }

    #[test]
    fn loads_nixos_options_preset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixos-options"
              ref = "nixos-unstable"
              "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        let dataset = &config.projects["nixpkgs"].datasets[0];

        assert_eq!(dataset.id, "nixos-options");
        assert_eq!(dataset.name.as_deref(), Some("NixOS Options"));
        assert_eq!(dataset.kind, DatasetKind::Options);
        assert_eq!(dataset.refs.len(), 1);

        let ref_config = &dataset.refs[0];

        assert_eq!(ref_config.id, "nixos-unstable");
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
                assert_eq!(source_ref, "github:NixOS/nixpkgs/nixos-unstable");
                assert_eq!(attribute, "options");
                assert_eq!(import_path, "nixos/release.nix");
                assert_eq!(output_path, "share/doc/nixos/options.json");
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
                assert_eq!(owner, "NixOS");
                assert_eq!(repo, "nixpkgs");
                assert_eq!(revision.as_deref(), Some("nixos-unstable"));
                assert!(strip_prefixes.is_empty());
            }
            other => panic!("unexpected source links config: {other:?}"),
        }
    }

    #[test]
    fn preset_allows_dataset_id_and_name_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixpkgs-packages"
              id = "unstable-packages"
              name = "Unstable Packages"
              ref = "nixos-unstable"
              "#,
        )
        .unwrap();

        let config = AppConfig::load(Some(&path)).unwrap();

        let dataset = &config.projects["nixpkgs"].datasets[0];

        assert_eq!(dataset.id, "unstable-packages");
        assert_eq!(dataset.name.as_deref(), Some("Unstable Packages"));
        assert_eq!(dataset.kind, DatasetKind::Packages);
    }

    #[test]
    fn preset_rejects_missing_ref() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixpkgs-packages"
              "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("preset datasets require ref"));
    }

    #[test]
    fn preset_rejects_explicit_refs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixpkgs-packages"
              ref = "nixos-unstable"

              [[projects.nixpkgs.datasets.refs]]
              id = "manual"

              [projects.nixpkgs.datasets.refs.producer]
              type = "existing-file"
              path = "fixtures/options-small.json"
              artifact = "options-json"
              "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("preset datasets must not also define refs"));
    }

    #[test]
    fn preset_rejects_conflicting_kind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nix-search.toml");

        fs::write(
            &path,
            r#"
              [projects.nixpkgs]
              name = "Nixpkgs"

              [[projects.nixpkgs.datasets]]
              preset = "nixpkgs-packages"
              kind = "options"
              ref = "nixos-unstable"
              "#,
        )
        .unwrap();

        let error = AppConfig::load(Some(&path)).unwrap_err().to_string();

        assert!(error.contains("requires dataset kind"));
    }
}
