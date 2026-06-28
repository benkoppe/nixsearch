use std::path::Path;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::data::DataConfig;
use crate::error::{ConfigError, Result};
use crate::maintenance::MaintenanceConfig;
use crate::server::ServerConfig;
use crate::source::{RawSourceConfig, SourceConfig, source_key_order_from_toml};
use crate::validation::validate_id;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub data: DataConfig,
    pub server: ServerConfig,
    pub maintenance: MaintenanceConfig,
    pub sources: IndexMap<String, SourceConfig>,
    pub ref_sets: IndexMap<String, RefSetConfig>,
}

impl AppConfig {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        let (source_order, ref_set_order) = if let Some(path) = path {
            if !path.exists() {
                return Err(ConfigError::Validation(format!(
                    "config file does not exist: {}",
                    path.display()
                )));
            }

            figment = figment.merge(Toml::file(path));
            (
                source_key_order_from_toml(path),
                ref_set_key_order_from_toml(path),
            )
        } else {
            (Vec::new(), Vec::new())
        };

        figment = figment.merge(Env::prefixed("NIXSEARCH_").ignore(&["config"]).split("__"));

        let raw: RawAppConfig = figment.extract()?;
        let mut config = raw.into_app_config()?;

        if !source_order.is_empty() {
            config.reorder_sources(&source_order);
        }

        if !ref_set_order.is_empty() {
            config.reorder_ref_sets(&ref_set_order);
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

    fn reorder_ref_sets(&mut self, order: &[String]) {
        self.ref_sets.sort_by(|a, _, b, _| {
            let pos_a = order.iter().position(|k| k == a).unwrap_or(usize::MAX);
            let pos_b = order.iter().position(|k| k == b).unwrap_or(usize::MAX);
            pos_a.cmp(&pos_b)
        });
    }

    pub fn validate(&self) -> Result<()> {
        self.data.validate()?;
        self.server.validate()?;
        self.maintenance.validate()?;

        for (source_id, source) in &self.sources {
            source.validate(source_id)?;
        }

        self.validate_ref_sets()?;

        Ok(())
    }

    fn validate_ref_sets(&self) -> Result<()> {
        for (ref_set_id, ref_set) in &self.ref_sets {
            validate_id("ref set id", ref_set_id)?;

            if ref_set.refs.is_empty() && self.sources.values().any(SourceConfig::has_ref_set_refs)
            {
                return Err(ConfigError::Validation(format!(
                    "ref_sets.{ref_set_id} must cover every ref-set eligible source"
                )));
            }

            for (source_id, source) in &self.sources {
                if source.has_ref_set_refs() && !ref_set.refs.contains_key(source_id) {
                    return Err(ConfigError::Validation(format!(
                        "ref_sets.{ref_set_id} is missing source {source_id:?}"
                    )));
                }
            }

            for (source_id, ref_ids) in &ref_set.refs {
                let source = self.sources.get(source_id).ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "ref_sets.{ref_set_id} contains unknown source {source_id:?}"
                    ))
                })?;

                if !source.has_ref_set_refs() {
                    return Err(ConfigError::Validation(format!(
                        "ref_sets.{ref_set_id} contains source {source_id:?} with no ref-set eligible refs"
                    )));
                }

                if ref_ids.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "ref_sets.{ref_set_id}.{source_id} must list at least one ref"
                    )));
                }

                for (index, ref_id) in ref_ids.iter().enumerate() {
                    validate_id("ref id", ref_id)?;

                    if ref_ids[..index].iter().any(|seen| seen == ref_id) {
                        return Err(ConfigError::Validation(format!(
                            "ref_sets.{ref_set_id}.{source_id} contains duplicate ref {ref_id:?}"
                        )));
                    }

                    if source.ref_allowed_in_ref_set(ref_id).is_none() {
                        return Err(ConfigError::Validation(format!(
                            "ref_sets.{ref_set_id}.{source_id} contains unknown ref {ref_id:?} or ref cannot appear in ref sets"
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    pub fn default_ref_set(&self) -> Option<&str> {
        self.ref_sets.keys().next().map(String::as_str)
    }

    pub fn public_seo_enabled(&self) -> bool {
        self.server.public_url.is_some()
    }

    pub fn refs_for_ref_set_source(&self, ref_set_id: &str, source_id: &str) -> Option<&[String]> {
        self.ref_sets
            .get(ref_set_id)?
            .refs
            .get(source_id)
            .map(Vec::as_slice)
    }

    pub fn first_ref_for_ref_set_source(&self, ref_set_id: &str, source_id: &str) -> Option<&str> {
        self.refs_for_ref_set_source(ref_set_id, source_id)?
            .first()
            .map(String::as_str)
    }

    pub fn ref_set_contains_source_ref(
        &self,
        ref_set_id: &str,
        source_id: &str,
        ref_id: &str,
    ) -> bool {
        self.refs_for_ref_set_source(ref_set_id, source_id)
            .is_some_and(|refs| refs.iter().any(|candidate| candidate == ref_id))
    }

    pub fn resolve_search_scopes(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> Result<Vec<ResolvedSearchScope>> {
        match (source, ref_id, ref_set) {
            (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => Err(ConfigError::Validation(
                "ref_set cannot be combined with source or ref".to_owned(),
            )),

            (Some(source_id), Some(ref_id), None) => {
                let source = self.sources.get(source_id).ok_or_else(|| {
                    ConfigError::Validation(format!("unknown source {source_id:?}"))
                })?;

                if source.searchable_ref(ref_id).is_none() {
                    return Err(ConfigError::Validation(format!(
                        "unknown ref {ref_id:?} for source {source_id:?} or ref is not searchable"
                    )));
                }

                Ok(vec![ResolvedSearchScope {
                    source: source_id.to_owned(),
                    ref_id: ref_id.to_owned(),
                }])
            }

            (Some(source_id), None, None) => {
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

            (None, Some(_), None) => Err(ConfigError::Validation(
                "--ref requires --source".to_owned(),
            )),

            (None, None, ref_set) => {
                if self.ref_sets.is_empty() {
                    return Ok(self
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
                        .collect());
                }

                let ref_set_id = ref_set
                    .or_else(|| self.default_ref_set())
                    .expect("ref_sets is not empty");
                let ref_set = self.ref_sets.get(ref_set_id).ok_or_else(|| {
                    ConfigError::Validation(format!("unknown ref set {ref_set_id:?}"))
                })?;

                let mut scopes = Vec::new();
                for (source_id, ref_ids) in &ref_set.refs {
                    for ref_id in ref_ids {
                        scopes.push(ResolvedSearchScope {
                            source: source_id.clone(),
                            ref_id: ref_id.clone(),
                        });
                    }
                }

                Ok(scopes)
            }
        }
    }
}

fn ref_set_key_order_from_toml(path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };

    let table: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(_) => return Vec::new(),
    };

    match table.get("ref_sets").and_then(|v| v.as_table()) {
        Some(ref_sets) => ref_sets.keys().cloned().collect(),
        None => Vec::new(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAppConfig {
    data: DataConfig,
    server: ServerConfig,
    maintenance: MaintenanceConfig,
    sources: IndexMap<String, RawSourceConfig>,
    ref_sets: IndexMap<String, RefSetConfig>,
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
            maintenance: self.maintenance,
            sources,
            ref_sets: self.ref_sets,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefSetConfig {
    #[serde(flatten)]
    pub refs: IndexMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSearchScope {
    pub source: String,
    pub ref_id: String,
}
