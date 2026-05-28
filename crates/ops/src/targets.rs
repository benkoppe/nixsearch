use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use nixsearch_config::AppConfig;
use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
use nixsearch_index::{IndexStore, IndexTargetManifest};

#[derive(Debug, Clone)]
pub struct TargetRef {
    pub source_id: String,
    pub source_kind: SourceKind,
    pub ref_config: RefConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetKey {
    pub source: String,
    pub ref_id: String,
}

impl TargetKey {
    pub fn new(source: impl Into<String>, ref_id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ref_id: ref_id.into(),
        }
    }
}

impl From<&TargetRef> for TargetKey {
    fn from(target: &TargetRef) -> Self {
        Self::new(target.source_id.clone(), target.ref_config.id.clone())
    }
}

impl From<&IndexTargetManifest> for TargetKey {
    fn from(target: &IndexTargetManifest) -> Self {
        Self::new(target.source.clone(), target.ref_id.clone())
    }
}

/// Collect all targets from all configured sources (no filtering).
pub fn all_targets(config: &AppConfig) -> Vec<TargetRef> {
    let mut targets = Vec::new();

    for (source_id, source) in &config.sources {
        collect_source_targets(source_id, source, None, &mut targets);
    }

    targets
}

/// Select targets with optional source/ref filters.
pub fn select_targets(
    config: &AppConfig,
    source: Option<&str>,
    ref_id: Option<&str>,
) -> Result<Vec<TargetRef>> {
    let mut targets = Vec::new();

    for (source_id, source_config) in &config.sources {
        if source.is_some_and(|selected| selected != source_id) {
            continue;
        }

        collect_source_targets(source_id, source_config, ref_id, &mut targets);
    }

    if let Some(source_id) = source
        && !config.sources.contains_key(source_id)
    {
        bail!("unknown source {source_id:?}");
    }

    Ok(targets)
}

fn collect_source_targets(
    source_id: &str,
    source: &SourceConfig,
    ref_filter: Option<&str>,
    targets: &mut Vec<TargetRef>,
) {
    for ref_config in &source.refs {
        if ref_filter.is_some_and(|selected| selected != ref_config.id) {
            continue;
        }

        targets.push(TargetRef {
            source_id: source_id.to_owned(),
            source_kind: source.kind,
            ref_config: ref_config.clone(),
        });
    }
}

pub fn current_manifest_targets(
    config: &AppConfig,
    index_store: &IndexStore,
) -> Result<BTreeMap<TargetKey, TargetRef>> {
    let Some(manifest) = index_store.try_current_manifest()? else {
        return Ok(BTreeMap::new());
    };

    let mut targets = BTreeMap::new();

    for manifest_target in &manifest.targets {
        let target = resolve_manifest_target(config, manifest_target)?;
        targets.insert(TargetKey::from(manifest_target), target);
    }

    Ok(targets)
}

fn resolve_manifest_target(
    config: &AppConfig,
    manifest_target: &IndexTargetManifest,
) -> Result<TargetRef> {
    let source = config
        .sources
        .get(&manifest_target.source)
        .with_context(|| {
            format!(
                "current index manifest contains unknown source {:?}",
                manifest_target.source
            )
        })?;

    let ref_config = source
        .refs
        .iter()
        .find(|ref_config| ref_config.id == manifest_target.ref_id)
        .with_context(|| {
            format!(
                "current index manifest contains unknown ref {:?} in source {:?}",
                manifest_target.ref_id, manifest_target.source
            )
        })?;

    Ok(TargetRef {
        source_id: manifest_target.source.clone(),
        source_kind: source.kind,
        ref_config: ref_config.clone(),
    })
}
