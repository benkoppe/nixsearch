use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::data::DataConfig;
use crate::error::{ConfigError, Result};
use crate::server::ServerConfig;
use crate::source::{RawSourceConfig, SourceConfig, source_key_order_from_toml};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub data: DataConfig,
    pub server: ServerConfig,
    pub sources: IndexMap<String, SourceConfig>,
}

impl AppConfig {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        let source_order = if let Some(path) = path {
            if !path.exists() {
                return Err(ConfigError::Validation(format!(
                    "config file does not exist: {}",
                    path.display()
                )));
            }

            figment = figment.merge(Toml::file(path));
            source_key_order_from_toml(path)
        } else {
            Vec::new()
        };

        figment = figment.merge(Env::prefixed("NIXSEARCH_").ignore(&["config"]).split("__"));

        let raw: RawAppConfig = figment.extract()?;
        let mut config = raw.into_app_config()?;

        if !source_order.is_empty() {
            config.reorder_sources(&source_order);
        }

        config.validate()?;

        Ok(config)
    }

    fn reorder_sources(&mut self, order: &[String]) {
        self.sources.sort_by(|a, _, b, _| {
            let pos_a = order.iter().position(|k| k == a).unwrap_or(usize::MAX);
            let pos_b = order.iter().position(|k| k == b).unwrap_or(usize::MAX);
            pos_a.cmp(&pos_b)
        });
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
    sources: IndexMap<String, RawSourceConfig>,
}

impl RawAppConfig {
    fn into_app_config(self) -> Result<AppConfig> {
        let mut sources = IndexMap::new();

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSearchScope {
    pub source: String,
    pub ref_id: String,
}
