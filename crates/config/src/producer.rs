use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use nixsearch_core::artifact::ArtifactKind;

use crate::error::Result;
use crate::validation::{
    producer_error, validate_nix_path_name, validate_producer_non_empty,
    validate_relative_output_path,
};

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
        #[serde(default = "default_nix_build_nix_path_name")]
        nix_path_name: String,
        attribute: String,
        import_path: String,
        output_path: String,
    },

    EvalModules {
        #[serde(rename = "ref")]
        source_ref: String,
        #[serde(default)]
        inputs: BTreeMap<String, String>,
        #[serde(default = "default_eval_modules_options")]
        options: String,
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

    FlakeFile {
        #[serde(rename = "ref")]
        source_ref: String,
        attribute: String,
        output_path: PathBuf,
        artifact: ArtifactKind,
        #[serde(default = "default_flake_file_fallback_inputs")]
        fallback_inputs: BTreeMap<String, String>,
    },

    FlakeInfo {
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

fn default_nix_build_nix_path_name() -> String {
    "nixpkgs".to_owned()
}

fn default_eval_modules_options() -> String {
    "evaluatedModules.options".to_owned()
}

pub(crate) fn default_flake_file_fallback_inputs() -> BTreeMap<String, String> {
    BTreeMap::from([(
        "nixpkgs".to_owned(),
        "github:NixOS/nixpkgs/nixpkgs-unstable".to_owned(),
    )])
}

impl ProducerConfig {
    pub(crate) fn validate(&self, source_id: &str, ref_id: &str) -> Result<()> {
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
                nix_path_name,
                attribute,
                import_path,
                output_path,
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
                validate_nix_path_name(source_id, ref_id, nix_path_name)?;
                validate_producer_non_empty(source_id, ref_id, "attribute", attribute)?;
                validate_producer_non_empty(source_id, ref_id, "import_path", import_path)?;
                validate_producer_non_empty(source_id, ref_id, "output_path", output_path)?;
            }

            Self::EvalModules {
                source_ref,
                inputs,
                options,
                modules,
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(source_id, ref_id, "options", options)?;

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

            Self::FlakeFile {
                source_ref,
                attribute,
                output_path,
                fallback_inputs,
                ..
            } => {
                validate_producer_non_empty(source_id, ref_id, "ref", source_ref)?;
                validate_producer_non_empty(source_id, ref_id, "attribute", attribute)?;
                validate_relative_output_path(source_id, ref_id, "output_path", output_path)?;

                for (name, source_ref) in fallback_inputs {
                    validate_producer_non_empty(source_id, ref_id, "fallback input name", name)?;
                    validate_producer_non_empty(
                        source_id,
                        ref_id,
                        "fallback input ref",
                        source_ref,
                    )?;
                }
            }

            Self::FlakeInfo { source_ref } => {
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
            Self::FlakeFile { .. } => ProducerKind::FlakeFile,
            Self::FlakeInfo { .. } => ProducerKind::FlakeInfo,
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
    FlakeFile,
    FlakeInfo,
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
