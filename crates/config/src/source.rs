use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::source_link::SourceLinkConfig;

use crate::error::{ConfigError, Result};
use crate::producer::{
    EvalModuleConfig, EvalModuleRefConfig, ProducerConfig, default_flake_file_fallback_inputs,
};
use crate::validation::{validate_hex_color, validate_id, validate_source_id};

pub(crate) const NIXPKGS_COLOR: &str = "#4ade80";
pub(crate) const NIXOS_COLOR: &str = "#60a5fa";
pub(crate) const HOME_MANAGER_COLOR: &str = "#f59e0b";
pub(crate) const NIX_DARWIN_COLOR: &str = "#a78bfa";
pub(crate) const HJEM_COLOR: &str = "#14b8a6";
pub(crate) const HJEM_RUM_COLOR: &str = "#ec4899";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RawSourceConfig {
    name: Option<String>,
    color: Option<String>,
    kind: Option<SourceKind>,
    default_ref: Option<String>,
    strip_prefixes: Option<Vec<String>>,
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
    pub(crate) fn into_source_config(self, source_id: &str) -> Result<SourceConfig> {
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
            strip_prefixes: self.strip_prefixes.unwrap_or_default(),
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
            SourcePreset::HjemOptions => self.expand_hjem_options(source_id, ref_ids),
            SourcePreset::HjemRumOptions => self.expand_hjem_rum_options(source_id, ref_ids),
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
            strip_prefixes: self.strip_prefixes.unwrap_or_default(),
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
            name: self.name.or_else(|| Some("NixOS".to_owned())),
            color: self.color.or_else(|| Some(NIXOS_COLOR.to_owned())),
            kind: SourceKind::Options,
            strip_prefixes: self.strip_prefixes.unwrap_or_default(),
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
                producer: ProducerConfig::FlakeFile {
                    source_ref: format!("github:nix-community/home-manager/{ref_id}"),
                    attribute: "docs-json".to_owned(),
                    output_path: PathBuf::from("share/doc/home-manager/options.json"),
                    artifact: ArtifactKind::OptionsJson,
                    fallback_inputs: default_flake_file_fallback_inputs(),
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Home Manager".to_owned())),
            color: self.color.or_else(|| Some(HOME_MANAGER_COLOR.to_owned())),
            kind: SourceKind::Options,
            strip_prefixes: self.strip_prefixes.unwrap_or_default(),
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
                producer: ProducerConfig::FlakeFile {
                    source_ref: format!("github:nix-darwin/nix-darwin/{ref_id}"),
                    attribute: "optionsJSON".to_owned(),
                    output_path: PathBuf::from("share/doc/darwin/options.json"),
                    artifact: ArtifactKind::OptionsJson,
                    fallback_inputs: default_flake_file_fallback_inputs(),
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Darwin".to_owned())),
            color: self.color.or_else(|| Some(NIX_DARWIN_COLOR.to_owned())),
            kind: SourceKind::Options,
            strip_prefixes: self.strip_prefixes.unwrap_or_default(),
            default_ref,
            refs,
        })
    }

    fn expand_hjem_options(self, source_id: &str, ref_ids: Vec<String>) -> Result<SourceConfig> {
        reject_conflicting_kind(self.kind, SourceKind::Options, SourcePreset::HjemOptions)?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(hjem_source_links(&ref_id)),
                producer: ProducerConfig::FlakeFile {
                    source_ref: format!("github:feel-co/hjem/{ref_id}"),
                    attribute: "docs-json".to_owned(),
                    output_path: PathBuf::from("share/doc/hjem/options.json"),
                    artifact: ArtifactKind::OptionsJson,
                    fallback_inputs: default_flake_file_fallback_inputs(),
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;
        let strip_prefixes = default_strip_prefixes(self.strip_prefixes, "hjem.");

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Hjem".to_owned())),
            color: self.color.or_else(|| Some(HJEM_COLOR.to_owned())),
            kind: SourceKind::Options,
            strip_prefixes,
            default_ref,
            refs,
        })
    }

    fn expand_hjem_rum_options(
        self,
        source_id: &str,
        ref_ids: Vec<String>,
    ) -> Result<SourceConfig> {
        reject_conflicting_kind(self.kind, SourceKind::Options, SourcePreset::HjemRumOptions)?;

        let refs = ref_ids
            .into_iter()
            .map(|ref_id| RefConfig {
                id: ref_id.clone(),
                source_links: Some(hjem_rum_source_links(&ref_id)),
                producer: ProducerConfig::EvalModules {
                    source_ref: format!("github:snugnug/hjem-rum/{ref_id}"),
                    inputs: BTreeMap::new(),
                    options: "(evaluatedModules.options.hjem.users.type.getSubOptions []).rum"
                        .to_owned(),
                    modules: vec![
                        EvalModuleConfig::FlakeAttr {
                            flake: "inputs.hjem".to_owned(),
                            attr: "nixosModules.default".to_owned(),
                        },
                        EvalModuleConfig::ModuleListOption {
                            option: "hjem.extraModules".to_owned(),
                            modules: vec![EvalModuleRefConfig {
                                flake: "self".to_owned(),
                                attr: "hjemModules.default".to_owned(),
                            }],
                        },
                    ],
                },
            })
            .collect::<Vec<_>>();

        let default_ref = effective_default_ref(source_id, self.default_ref, &refs)?;
        let strip_prefixes = default_strip_prefixes(self.strip_prefixes, "<name>.");

        Ok(SourceConfig {
            name: self.name.or_else(|| Some("Hjem-Rum".to_owned())),
            color: self.color.or_else(|| Some(HJEM_RUM_COLOR.to_owned())),
            kind: SourceKind::Options,
            strip_prefixes,
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

fn hjem_source_links(revision: &str) -> SourceLinkConfig {
    github_source_links_with_strip_prefixes("feel-co", "hjem", revision, vec!["hjem"])
}

fn hjem_rum_source_links(revision: &str) -> SourceLinkConfig {
    github_source_links_with_strip_prefixes("snugnug", "hjem-rum", revision, vec!["hjem-rum"])
}

fn github_source_links(owner: &str, repo: &str, revision: &str) -> SourceLinkConfig {
    github_source_links_with_strip_prefixes(owner, repo, revision, Vec::new())
}

fn github_source_links_with_strip_prefixes(
    owner: &str,
    repo: &str,
    revision: &str,
    strip_prefixes: Vec<&str>,
) -> SourceLinkConfig {
    SourceLinkConfig::Github {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        revision: Some(revision.to_owned()),
        strip_prefixes: strip_prefixes.into_iter().map(ToOwned::to_owned).collect(),
    }
}

pub(crate) fn source_key_order_from_toml(path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };

    let table: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(_) => return Vec::new(),
    };

    match table.get("sources").and_then(|v| v.as_table()) {
        Some(sources) => sources.keys().cloned().collect(),
        None => Vec::new(),
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

fn default_strip_prefixes(configured: Option<Vec<String>>, default_prefix: &str) -> Vec<String> {
    configured.unwrap_or_else(|| vec![default_prefix.to_owned()])
}

fn validate_strip_prefixes(source_id: &str, strip_prefixes: &[String]) -> Result<()> {
    for (index, prefix) in strip_prefixes.iter().enumerate() {
        if prefix.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "sources.{source_id}.strip_prefixes[{index}] must not be empty"
            )));
        }
    }

    Ok(())
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
    pub strip_prefixes: Vec<String>,
    #[serde(default)]
    pub default_ref: Option<String>,
    #[serde(default)]
    pub refs: Vec<RefConfig>,
}

impl SourceConfig {
    pub(crate) fn validate(&self, source_id: &str) -> Result<()> {
        validate_source_id("source id", source_id)?;

        if let Some(color) = &self.color {
            validate_hex_color(&format!("sources.{source_id}.color"), color)?;
        }

        validate_strip_prefixes(source_id, &self.strip_prefixes)?;

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
    HjemOptions,
    HjemRumOptions,
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
