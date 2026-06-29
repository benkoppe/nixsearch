use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use nixsearch_config::app::{AppConfig, ResolvedSearchScope};
use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::target::{RefRole, TargetCapabilities};
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::store::IndexStore;
use nixsearch_store::ArtifactRef;

#[derive(Debug, Clone)]
pub struct TargetRef {
    pub source_id: String,
    pub source_kind: SourceKind,
    pub strip_prefixes: Vec<String>,
    pub ref_config: RefConfig,
}

impl TargetRef {
    pub fn artifact_kind(&self) -> ArtifactKind {
        self.ref_config.artifact_kind()
    }

    pub fn capabilities(&self) -> TargetCapabilities {
        self.ref_config.capabilities()
    }

    pub fn is_artifact_only(&self) -> bool {
        self.capabilities().is_artifact_only()
    }

    pub fn indexes_search_documents(&self) -> bool {
        self.capabilities().indexes_search_documents()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetKey {
    pub source: String,
    pub ref_id: String,
    pub artifact_kind: ArtifactKind,
    pub target_role: RefRole,
}

impl TargetKey {
    pub fn new(
        source: impl Into<String>,
        ref_id: impl Into<String>,
        artifact_kind: ArtifactKind,
        target_role: RefRole,
    ) -> Self {
        Self {
            source: source.into(),
            ref_id: ref_id.into(),
            artifact_kind,
            target_role,
        }
    }

    pub fn from_target_ref(target: &TargetRef) -> Self {
        Self::from_ref_config(target.source_id.as_str(), &target.ref_config)
    }

    pub fn from_ref_config(source_id: impl Into<String>, ref_config: &RefConfig) -> Self {
        Self::new(
            source_id,
            ref_config.id.clone(),
            ref_config.artifact_kind(),
            ref_config.role,
        )
    }
}

impl From<&TargetRef> for TargetKey {
    fn from(target: &TargetRef) -> Self {
        Self::from_target_ref(target)
    }
}

impl From<&IndexTargetManifest> for TargetKey {
    fn from(target: &IndexTargetManifest) -> Self {
        Self::new(
            target.source.clone(),
            target.ref_id.clone(),
            target.artifact_kind,
            target.target_role,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetCoverage {
    pub missing_configured_targets: BTreeSet<TargetKey>,
    pub serves_default_scope: bool,
}

impl std::fmt::Display for TargetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{}/{}:{}",
            self.source,
            self.ref_id,
            self.artifact_kind.as_str(),
            self.target_role.as_str()
        )
    }
}

pub fn latest_artifact_ref_for_target(target: &TargetRef) -> ArtifactRef {
    ArtifactRef::latest(
        target.source_id.clone(),
        target.ref_config.id.clone(),
        target.artifact_kind(),
    )
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

pub fn missing_configured_target_keys(
    config: &AppConfig,
    manifest: &IndexGenerationManifest,
) -> BTreeSet<TargetKey> {
    let indexed_targets = manifest
        .targets
        .iter()
        .map(TargetKey::from)
        .collect::<BTreeSet<_>>();

    all_targets(config)
        .iter()
        .map(TargetKey::from)
        .filter(|target| !indexed_targets.contains(target))
        .collect()
}

pub fn target_coverage(
    config: &AppConfig,
    manifest: &IndexGenerationManifest,
) -> Result<TargetCoverage> {
    let missing_configured_targets = missing_configured_target_keys(config, manifest);
    let default_targets = default_indexed_search_target_keys(config)?;
    let indexed_targets = manifest
        .targets
        .iter()
        .map(TargetKey::from)
        .collect::<BTreeSet<_>>();
    let serves_default_scope = !default_targets.is_empty()
        && default_targets
            .iter()
            .all(|target| indexed_targets.contains(target));

    Ok(TargetCoverage {
        missing_configured_targets,
        serves_default_scope,
    })
}

pub fn default_indexed_search_target_keys(config: &AppConfig) -> Result<BTreeSet<TargetKey>> {
    let target_keys = config
        .resolve_search_scopes(None, None, None)?
        .into_iter()
        .map(|scope| target_key_for_scope(config, &scope))
        .collect::<Result<Vec<_>>>()?;

    Ok(target_keys
        .into_iter()
        .filter(|target| {
            TargetCapabilities::new(target.target_role, target.artifact_kind)
                .indexes_search_documents()
        })
        .collect())
}

pub fn target_key_for_scope(config: &AppConfig, scope: &ResolvedSearchScope) -> Result<TargetKey> {
    let source = config
        .sources
        .get(&scope.source)
        .with_context(|| format!("search scope references unknown source {:?}", scope.source))?;

    let ref_config = source
        .refs
        .iter()
        .find(|ref_config| ref_config.id == scope.ref_id)
        .with_context(|| {
            format!(
                "search scope references unknown ref {:?} in source {:?}",
                scope.ref_id, scope.source
            )
        })?;

    Ok(TargetKey::from_ref_config(
        scope.source.as_str(),
        ref_config,
    ))
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
            strip_prefixes: source.strip_prefixes.clone(),
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

    let expected_artifact_kind = ref_config.artifact_kind();
    if manifest_target.artifact_kind != expected_artifact_kind {
        bail!(
            "current index manifest target {}/{}/{} no longer matches configured producer kind {}; run unfiltered `nixsearch update` to refresh all configured refs",
            manifest_target.source,
            manifest_target.ref_id,
            manifest_target.artifact_kind.as_str(),
            expected_artifact_kind.as_str()
        );
    }

    if manifest_target.target_role != ref_config.role
        || manifest_target.indexes_search_documents != ref_config.indexes_search_documents()
    {
        bail!(
            "current index manifest target {}/{}/{} role {} no longer matches configured role {}; run unfiltered `nixsearch update` to refresh all configured refs",
            manifest_target.source,
            manifest_target.ref_id,
            manifest_target.artifact_kind.as_str(),
            manifest_target.target_role.as_str(),
            ref_config.role.as_str()
        );
    }

    Ok(TargetRef {
        source_id: manifest_target.source.clone(),
        source_kind: source.kind,
        strip_prefixes: source.strip_prefixes.clone(),
        ref_config: ref_config.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::tempdir;

    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::SourceKind;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::publish_canonical_options_index;
    use nixsearch_test_support::{REF_SMALL, SOURCE_FIXTURES, app_config, utf8_path_buf};

    use super::current_manifest_targets;

    #[test]
    fn current_manifest_targets_requires_full_update_on_artifact_kind_mismatch() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let index_store = IndexStore::new(&index_dir);
        let mut config = app_config(&index_dir);
        let source = config
            .sources
            .get_mut(SOURCE_FIXTURES)
            .expect("fixture source exists");
        source.kind = SourceKind::Mixed;
        let ref_config = &mut source.refs[0];
        ref_config.producer = ProducerConfig::ExistingFile {
            path: PathBuf::from("unused.json"),
            artifact: ArtifactKind::PackagesJson,
        };
        config.validate().unwrap();

        let error = current_manifest_targets(&config, &index_store).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains(SOURCE_FIXTURES));
        assert!(message.contains(REF_SMALL));
        assert!(message.contains("options-json"));
        assert!(message.contains("packages-json"));
        assert!(message.contains("run unfiltered `nixsearch update`"));
    }
}
