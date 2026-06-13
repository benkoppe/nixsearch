use std::fmt;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use nixsearch_config::app::AppConfig;
use nixsearch_config::source::SourceConfig;
use nixsearch_core::document::DocumentKind;
use nixsearch_index::manifest::{IndexGenerationManifest, validate_generation_id};
use nixsearch_index::search::{
    EntryFacts, EntryLookup, EntryLookupResult, SearchIndex, SearchOptions, SearchResult,
    SearchScope,
};
use nixsearch_index::seo::SeoSidecar;
use nixsearch_index::store::IndexStore;

#[derive(Debug)]
pub struct SearchService {
    config: Arc<AppConfig>,
    current: Arc<RwLock<ServedGeneration>>,
}

impl Clone for SearchService {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            current: Arc::clone(&self.current),
        }
    }
}

#[derive(Clone)]
struct ServedGeneration {
    path: Utf8PathBuf,
    manifest: IndexGenerationManifest,
    index: Arc<SearchIndex>,
    seo_facts: SeoFactsState,
}

impl fmt::Debug for ServedGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedGeneration")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ServedGenerationSnapshot {
    pub path: Utf8PathBuf,
    pub manifest: IndexGenerationManifest,
    pub index: Arc<SearchIndex>,
    pub seo_facts: SeoFactsState,
}

impl fmt::Debug for ServedGenerationSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedGenerationSnapshot")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum SeoFactsState {
    Loaded(Arc<SeoSidecar>),
    Unavailable(String),
}

impl SeoFactsState {
    pub fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Unchanged,
    Reloaded,
}

pub type ServiceResult<T> = std::result::Result<T, ServiceError>;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Resolution(#[from] RequestResolutionError),

    #[error("search failed")]
    Search(#[source] anyhow::Error),

    #[error("entry lookup failed")]
    EntryLookup(#[source] anyhow::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestResolutionError {
    #[error("unknown source {source_id:?}")]
    UnknownSource { source_id: String },

    #[error("unknown ref {ref_id:?} for source {source_id:?}")]
    UnknownRef { source_id: String, ref_id: String },

    #[error("unknown ref set {ref_set:?}")]
    UnknownRefSet { ref_set: String },

    #[error("ref {ref_id:?} for source {source_id:?} is not present in the served manifest")]
    UnservedRef { source_id: String, ref_id: String },

    #[error("source {source_id:?} has no default ref")]
    MissingDefaultRef { source_id: String },

    #[error("ref requires source")]
    RefRequiresSource,

    #[error(
        "ref set {ref_set:?} contains multiple refs for source {source_id:?}; explicit ref is required"
    )]
    AmbiguousRefSetSource { ref_set: String, source_id: String },

    #[error("ref {ref_id:?} is not valid for source {source_id:?} in ref set {ref_set:?}")]
    InvalidRefForRefSet {
        ref_set: String,
        source_id: String,
        ref_id: String,
    },

    #[error("no configured search scopes are present in the served manifest")]
    NoServedSearchScopes,
}

#[derive(Debug, Clone, Default)]
pub struct SearchRequest {
    pub query: String,
    pub source: Option<String>,
    pub ref_id: Option<String>,
    pub ref_set: Option<String>,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct EntryRequest {
    pub source: String,
    pub ref_id: Option<String>,
    pub name: String,
    pub kind: Option<DocumentKind>,
}

impl SearchService {
    pub fn open_current(config: Arc<AppConfig>) -> Result<Self> {
        let index_store = IndexStore::new(&config.data.index_dir);
        let path = index_store.current_path().with_context(|| {
            format!(
                "failed to locate current index in {}",
                config.data.index_dir
            )
        })?;
        let manifest = index_store
            .read_manifest(&path)
            .with_context(|| format!("failed to read current index manifest {}", path.as_str()))?;

        Self::from_generation(config, path, manifest)
    }

    pub fn from_generation(
        config: Arc<AppConfig>,
        path: Utf8PathBuf,
        manifest: IndexGenerationManifest,
    ) -> Result<Self> {
        validate_generation_id(&manifest)
            .context("failed to validate supplied index generation manifest")?;

        let index = open_index(&path)?;
        let seo_facts = load_seo_facts_state(&config, &path, &manifest);

        Ok(Self {
            config,
            current: Arc::new(RwLock::new(ServedGeneration {
                path,
                manifest,
                index: Arc::new(index),
                seo_facts,
            })),
        })
    }

    pub fn validate_generation(path: impl AsRef<Utf8Path>) -> Result<()> {
        open_index(path).map(drop)
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn current_index(&self) -> Arc<SearchIndex> {
        Arc::clone(
            &self
                .current
                .read()
                .expect("served generation lock poisoned")
                .index,
        )
    }

    pub fn snapshot(&self) -> ServedGenerationSnapshot {
        let current = self
            .current
            .read()
            .expect("served generation lock poisoned");

        ServedGenerationSnapshot {
            path: current.path.clone(),
            manifest: current.manifest.clone(),
            index: Arc::clone(&current.index),
            seo_facts: current.seo_facts.clone(),
        }
    }

    pub fn reconcile_generation(
        &self,
        path: Utf8PathBuf,
        manifest: IndexGenerationManifest,
    ) -> Result<ReconcileOutcome> {
        validate_generation_id(&manifest)
            .context("failed to validate supplied index generation manifest")?;

        let should_retry_seo_facts = {
            let current = self
                .current
                .read()
                .expect("served generation lock poisoned");

            if current.path != path || current.manifest != manifest {
                false
            } else if current.seo_facts.is_loaded() {
                return Ok(ReconcileOutcome::Unchanged);
            } else {
                true
            }
        };

        if should_retry_seo_facts {
            let seo_facts = load_seo_facts_state(&self.config, &path, &manifest);
            let mut current = self
                .current
                .write()
                .expect("served generation lock poisoned");

            if current.path == path && current.manifest == manifest {
                current.seo_facts = seo_facts;
                return Ok(ReconcileOutcome::Unchanged);
            }
        }

        self.reload_generation(path, manifest)
    }

    fn reload_generation(
        &self,
        path: Utf8PathBuf,
        manifest: IndexGenerationManifest,
    ) -> Result<ReconcileOutcome> {
        let index = open_index(&path)
            .with_context(|| format!("failed to open published index generation {path}"))?;
        let seo_facts = load_seo_facts_state(&self.config, &path, &manifest);

        let mut current = self
            .current
            .write()
            .expect("served generation lock poisoned");

        if current.path == path && current.manifest == manifest && current.seo_facts.is_loaded() {
            return Ok(ReconcileOutcome::Unchanged);
        }

        *current = ServedGeneration {
            path,
            manifest,
            index: Arc::new(index),
            seo_facts,
        };

        Ok(ReconcileOutcome::Reloaded)
    }

    pub fn search_current(&self, request: SearchRequest) -> ServiceResult<SearchResult> {
        let snapshot = self.snapshot();
        self.search_with_snapshot(&snapshot, request)
    }

    pub fn search_with_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: SearchRequest,
    ) -> ServiceResult<SearchResult> {
        let scopes = self.search_scopes_for_snapshot(
            snapshot,
            request.source.as_deref(),
            request.ref_id.as_deref(),
            request.ref_set.as_deref(),
        )?;

        snapshot
            .index
            .search(SearchOptions {
                query: request.query,
                limit: request.limit,
                offset: request.offset,
                scopes,
            })
            .map_err(ServiceError::Search)
    }

    pub fn find_entry_current(&self, request: EntryRequest) -> ServiceResult<EntryLookupResult> {
        let snapshot = self.snapshot();
        self.find_entry_with_snapshot(&snapshot, request)
    }

    pub fn find_entry_with_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: EntryRequest,
    ) -> ServiceResult<EntryLookupResult> {
        let ref_id = self.resolve_entry_ref_for_snapshot(
            snapshot,
            &request.source,
            request.ref_id.as_deref(),
        )?;

        snapshot
            .index
            .find_entry(EntryLookup {
                source: request.source,
                ref_id,
                name: request.name,
                kind: request.kind,
            })
            .map_err(ServiceError::EntryLookup)
    }

    pub fn find_entry_with_facts_with_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: EntryRequest,
        facts: &EntryFacts,
    ) -> ServiceResult<EntryLookupResult> {
        let ref_id = self.resolve_entry_ref_for_snapshot(
            snapshot,
            &request.source,
            request.ref_id.as_deref(),
        )?;

        snapshot
            .index
            .find_entry_with_facts(
                EntryLookup {
                    source: request.source,
                    ref_id,
                    name: request.name,
                    kind: request.kind,
                },
                facts,
            )
            .map_err(ServiceError::EntryLookup)
    }

    pub fn entry_facts_current(&self, request: EntryRequest) -> ServiceResult<EntryFacts> {
        let snapshot = self.snapshot();
        self.entry_facts_with_snapshot(&snapshot, request)
    }

    pub fn entry_facts_with_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: EntryRequest,
    ) -> ServiceResult<EntryFacts> {
        let ref_id = self.resolve_entry_ref_for_snapshot(
            snapshot,
            &request.source,
            request.ref_id.as_deref(),
        )?;

        snapshot
            .index
            .entry_facts(EntryLookup {
                source: request.source,
                ref_id,
                name: request.name,
                kind: request.kind,
            })
            .map_err(ServiceError::EntryLookup)
    }

    pub fn search_scopes(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        let snapshot = self.snapshot();
        self.search_scopes_for_snapshot(&snapshot, source, ref_id, ref_set)
    }

    pub fn search_scopes_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        let source = source.and_then(non_empty);
        let ref_id = ref_id.and_then(non_empty);
        let ref_set = ref_set.and_then(non_empty);
        let source_specific = source.is_some();

        let scopes = self.resolve_configured_search_scopes(source, ref_id, ref_set)?;

        if source_specific {
            let scope = scopes
                .into_iter()
                .next()
                .ok_or(RequestResolutionError::NoServedSearchScopes)?;

            if !Self::served_ref_exists_in_snapshot(snapshot, &scope.source, &scope.ref_id) {
                return Err(RequestResolutionError::UnservedRef {
                    source_id: scope.source,
                    ref_id: scope.ref_id,
                });
            }

            return Ok(vec![scope]);
        }

        let served_scopes = scopes
            .into_iter()
            .filter(|scope| {
                Self::served_ref_exists_in_snapshot(snapshot, &scope.source, &scope.ref_id)
            })
            .collect::<Vec<_>>();

        if served_scopes.is_empty() {
            return Err(RequestResolutionError::NoServedSearchScopes);
        }

        Ok(served_scopes)
    }

    pub fn resolve_entry_ref(
        &self,
        source_id: &str,
        ref_id: Option<&str>,
    ) -> std::result::Result<String, RequestResolutionError> {
        let snapshot = self.snapshot();
        self.resolve_entry_ref_for_snapshot(&snapshot, source_id, ref_id)
    }

    pub fn resolve_entry_ref_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: Option<&str>,
    ) -> std::result::Result<String, RequestResolutionError> {
        let ref_id = match ref_id.and_then(non_empty) {
            Some(ref_id) => {
                self.ensure_configured_ref(source_id, ref_id)?;
                ref_id.to_owned()
            }
            None => self.configured_default_ref(source_id)?.to_owned(),
        };

        if !Self::served_ref_exists_in_snapshot(snapshot, source_id, &ref_id) {
            return Err(RequestResolutionError::UnservedRef {
                source_id: source_id.to_owned(),
                ref_id,
            });
        }

        Ok(ref_id)
    }

    pub fn configured_source_exists(&self, source_id: &str) -> bool {
        self.config.sources.contains_key(source_id)
    }

    pub fn configured_ref_exists(&self, source_id: &str, ref_id: &str) -> bool {
        self.config
            .sources
            .get(source_id)
            .is_some_and(|source| source.refs.iter().any(|candidate| candidate.id == ref_id))
    }

    pub fn served_ref_exists(&self, source_id: &str, ref_id: &str) -> bool {
        let snapshot = self.snapshot();
        Self::served_ref_exists_in_snapshot(&snapshot, source_id, ref_id)
    }

    pub fn served_ref_exists_in_snapshot(
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> bool {
        snapshot
            .manifest
            .targets
            .iter()
            .any(|target| target.source == source_id && target.ref_id == ref_id)
    }

    pub fn is_indexable_ref(&self, source_id: &str, ref_id: &str) -> bool {
        let snapshot = self.snapshot();
        self.is_indexable_ref_in_snapshot(&snapshot, source_id, ref_id)
    }

    pub fn is_indexable_ref_in_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> bool {
        let Some(source) = self.config.sources.get(source_id) else {
            return false;
        };

        self.ref_allowed_to_be_indexed(source, ref_id)
            && Self::served_ref_exists_in_snapshot(snapshot, source_id, ref_id)
    }

    fn ref_allowed_to_be_indexed(&self, source: &SourceConfig, ref_id: &str) -> bool {
        source.default_ref.as_deref() == Some(ref_id)
    }

    fn resolve_configured_search_scopes(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        match (source, ref_id, ref_set) {
            (Some(source_id), _, Some(ref_set_id)) => {
                self.resolve_source_ref_set_scope(source_id, ref_id, ref_set_id)
            }
            (Some(source_id), Some(ref_id), None) => {
                self.ensure_configured_ref(source_id, ref_id)?;

                Ok(vec![SearchScope {
                    source: source_id.to_owned(),
                    ref_id: ref_id.to_owned(),
                }])
            }
            (Some(source_id), None, None) => {
                let default_ref = self.configured_default_ref(source_id)?;

                Ok(vec![SearchScope {
                    source: source_id.to_owned(),
                    ref_id: default_ref.to_owned(),
                }])
            }
            (None, Some(_), _) => Err(RequestResolutionError::RefRequiresSource),
            (None, None, Some(ref_set_id)) => self.resolve_all_ref_set_scopes(ref_set_id),
            (None, None, None) => self.resolve_default_all_scopes(),
        }
    }

    fn resolve_default_all_scopes(
        &self,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        if let Some(default_ref_set) = self.config.default_ref_set() {
            return self.resolve_all_ref_set_scopes(default_ref_set);
        }

        Ok(self
            .config
            .sources
            .iter()
            .filter_map(|(source_id, source)| {
                source.default_ref.as_ref().map(|default_ref| SearchScope {
                    source: source_id.clone(),
                    ref_id: default_ref.clone(),
                })
            })
            .collect())
    }

    fn resolve_all_ref_set_scopes(
        &self,
        ref_set_id: &str,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        let ref_set = self.config.ref_sets.get(ref_set_id).ok_or_else(|| {
            RequestResolutionError::UnknownRefSet {
                ref_set: ref_set_id.to_owned(),
            }
        })?;

        Ok(ref_set
            .refs
            .iter()
            .flat_map(|(source_id, ref_ids)| {
                ref_ids.iter().map(|ref_id| SearchScope {
                    source: source_id.clone(),
                    ref_id: ref_id.clone(),
                })
            })
            .collect())
    }

    fn resolve_source_ref_set_scope(
        &self,
        source_id: &str,
        ref_id: Option<&str>,
        ref_set_id: &str,
    ) -> std::result::Result<Vec<SearchScope>, RequestResolutionError> {
        self.configured_source(source_id)?;

        let ref_set = self.config.ref_sets.get(ref_set_id).ok_or_else(|| {
            RequestResolutionError::UnknownRefSet {
                ref_set: ref_set_id.to_owned(),
            }
        })?;

        let refs = ref_set.refs.get(source_id).ok_or_else(|| {
            RequestResolutionError::InvalidRefForRefSet {
                ref_set: ref_set_id.to_owned(),
                source_id: source_id.to_owned(),
                ref_id: ref_id.unwrap_or("").to_owned(),
            }
        })?;

        let selected_ref = if refs.len() == 1 {
            let selected_ref = refs[0].as_str();

            if let Some(ref_id) = ref_id {
                self.ensure_configured_ref(source_id, ref_id)?;

                if ref_id != selected_ref {
                    return Err(RequestResolutionError::InvalidRefForRefSet {
                        ref_set: ref_set_id.to_owned(),
                        source_id: source_id.to_owned(),
                        ref_id: ref_id.to_owned(),
                    });
                }
            }

            selected_ref
        } else {
            let Some(ref_id) = ref_id else {
                return Err(RequestResolutionError::AmbiguousRefSetSource {
                    ref_set: ref_set_id.to_owned(),
                    source_id: source_id.to_owned(),
                });
            };

            self.ensure_configured_ref(source_id, ref_id)?;

            if !refs.iter().any(|candidate| candidate == ref_id) {
                return Err(RequestResolutionError::InvalidRefForRefSet {
                    ref_set: ref_set_id.to_owned(),
                    source_id: source_id.to_owned(),
                    ref_id: ref_id.to_owned(),
                });
            }

            ref_id
        };

        Ok(vec![SearchScope {
            source: source_id.to_owned(),
            ref_id: selected_ref.to_owned(),
        }])
    }

    fn configured_source(
        &self,
        source_id: &str,
    ) -> std::result::Result<&SourceConfig, RequestResolutionError> {
        self.config
            .sources
            .get(source_id)
            .ok_or_else(|| RequestResolutionError::UnknownSource {
                source_id: source_id.to_owned(),
            })
    }

    fn configured_default_ref(
        &self,
        source_id: &str,
    ) -> std::result::Result<&str, RequestResolutionError> {
        let source = self.configured_source(source_id)?;

        source
            .default_ref
            .as_deref()
            .ok_or_else(|| RequestResolutionError::MissingDefaultRef {
                source_id: source_id.to_owned(),
            })
    }

    fn ensure_configured_ref(
        &self,
        source_id: &str,
        ref_id: &str,
    ) -> std::result::Result<(), RequestResolutionError> {
        self.configured_source(source_id)?;

        if !self.configured_ref_exists(source_id, ref_id) {
            return Err(RequestResolutionError::UnknownRef {
                source_id: source_id.to_owned(),
                ref_id: ref_id.to_owned(),
            });
        }

        Ok(())
    }
}

fn open_index(path: impl AsRef<Utf8Path>) -> Result<SearchIndex> {
    let path = path.as_ref();

    SearchIndex::open(path)
        .with_context(|| format!("failed to open search index {}", path.as_str()))
}

fn load_seo_facts_state(
    config: &AppConfig,
    path: &Utf8Path,
    manifest: &IndexGenerationManifest,
) -> SeoFactsState {
    let index_store = IndexStore::new(&config.data.index_dir);

    match index_store.read_seo_sidecar(path, manifest) {
        Ok(sidecar) => SeoFactsState::Loaded(Arc::new(sidecar)),
        Err(error) => SeoFactsState::Unavailable(format!("{error:#}")),
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use nixsearch_core::document::DocumentKind;
    use nixsearch_index::manifest::{canonical_generation_id, refresh_generation_id};
    use nixsearch_index::search::{EntryFactsStatus, EntryLookupResult};
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        options_target, publish_canonical_index, publish_canonical_index_with_generated_at,
        publish_documents_with_manifest_targets, publish_fixture_options_index_for_refs,
    };
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, app_config, app_config_with_extra_fixture_source,
        ingest_context_for, multi_ref_app_config, option_doc_for, utf8_path_buf,
    };
    use time::Duration as TimeDuration;

    use super::{
        EntryRequest, ReconcileOutcome, RequestResolutionError, SearchRequest, SearchService,
        ServiceError,
    };

    #[test]
    fn search_current_uses_configured_default_scopes() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let result = service
            .search_current(SearchRequest {
                query: "git".to_owned(),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap();

        assert!(result.total > 0);
        assert!(!result.hits.is_empty());
    }

    #[test]
    fn search_with_explicit_source_and_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let result = service
            .search_current(SearchRequest {
                query: "programs.git.enable".to_owned(),
                source: Some(SOURCE_FIXTURES.to_owned()),
                ref_id: Some(REF_SMALL.to_owned()),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap();

        assert!(result.total > 0);
        assert!(
            result
                .hits
                .iter()
                .any(|hit| hit.document.name() == "programs.git.enable")
        );
    }

    #[test]
    fn find_entry_current_resolves_entry() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let result = service
            .find_entry_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_SMALL.to_owned()),
                name: "programs.git.enable".to_owned(),
                kind: Some(DocumentKind::Option),
            })
            .unwrap();

        assert!(matches!(result, EntryLookupResult::Found(_)));
    }

    #[test]
    fn entry_facts_current_resolves_unique_entry() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let facts = service
            .entry_facts_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_SMALL.to_owned()),
                name: "programs.git.enable".to_owned(),
                kind: Some(DocumentKind::Option),
            })
            .unwrap();

        assert_eq!(facts.status(), EntryFactsStatus::Unique);
        assert_eq!(facts.option_count, 1);
        assert_eq!(facts.package_count, 0);
        assert_eq!(facts.seo_eligible(), Some(true));
    }

    #[test]
    fn helpers_report_configured_and_served_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(service.configured_source_exists(SOURCE_FIXTURES));
        assert!(!service.configured_source_exists("missing"));
        assert!(service.configured_ref_exists(SOURCE_FIXTURES, REF_SMALL));
        assert!(service.configured_ref_exists(SOURCE_FIXTURES, REF_STABLE));
        assert!(!service.configured_ref_exists(SOURCE_FIXTURES, "missing"));
        assert!(!service.configured_ref_exists("missing", REF_SMALL));
        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_SMALL));
        assert!(!service.served_ref_exists(SOURCE_FIXTURES, REF_STABLE));
    }

    #[test]
    fn default_served_ref_is_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(service.is_indexable_ref(SOURCE_FIXTURES, REF_SMALL));
    }

    #[test]
    fn non_default_served_ref_is_not_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_STABLE));
        assert!(!service.is_indexable_ref(SOURCE_FIXTURES, REF_STABLE));
    }

    #[test]
    fn unknown_source_returns_typed_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some("missing"), None, None)
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::UnknownSource { source_id } if source_id == "missing"
        ));
    }

    #[test]
    fn unknown_ref_returns_typed_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some(SOURCE_FIXTURES), Some("missing"), None)
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::UnknownRef { source_id, ref_id }
                if source_id == SOURCE_FIXTURES && ref_id == "missing"
        ));
    }

    #[test]
    fn unknown_ref_set_returns_typed_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(None, None, Some("missing"))
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::UnknownRefSet { ref_set } if ref_set == "missing"
        ));
    }

    #[test]
    fn configured_but_unserved_ref_returns_typed_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some(SOURCE_FIXTURES), Some(REF_STABLE), None)
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::UnservedRef { source_id, ref_id }
                if source_id == SOURCE_FIXTURES && ref_id == REF_STABLE
        ));
    }

    #[test]
    fn default_served_ref_resolves() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let scopes = service
            .search_scopes(Some(SOURCE_FIXTURES), None, None)
            .unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_SMALL);
    }

    #[test]
    fn non_default_served_ref_resolves_but_is_not_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let scopes = service
            .search_scopes(Some(SOURCE_FIXTURES), Some(REF_STABLE), None)
            .unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_STABLE);
        assert!(!service.is_indexable_ref(SOURCE_FIXTURES, REF_STABLE));
    }

    #[test]
    fn single_ref_ref_set_source_resolves_without_explicit_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let scopes = service
            .search_scopes(Some(SOURCE_FIXTURES), None, Some("single"))
            .unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_SMALL);
    }

    #[test]
    fn multi_ref_ref_set_source_without_explicit_ref_returns_ambiguous_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some(SOURCE_FIXTURES), None, Some("multi"))
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::AmbiguousRefSetSource { ref_set, source_id }
                if ref_set == "multi" && source_id == SOURCE_FIXTURES
        ));
    }

    #[test]
    fn multi_ref_ref_set_source_with_explicit_valid_ref_resolves() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let scopes = service
            .search_scopes(Some(SOURCE_FIXTURES), Some(REF_STABLE), Some("multi"))
            .unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_STABLE);
    }

    #[test]
    fn ref_set_source_with_explicit_ref_outside_set_returns_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some(SOURCE_FIXTURES), Some(REF_STABLE), Some("single"))
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::InvalidRefForRefSet { ref_set, source_id, ref_id }
                if ref_set == "single" && source_id == SOURCE_FIXTURES && ref_id == REF_STABLE
        ));
    }

    #[test]
    fn all_source_scopes_filter_to_served_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config_with_extra_fixture_source(&index_dir, "extra"));
        let service = SearchService::open_current(config).unwrap();

        let scopes = service.search_scopes(None, None, None).unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_SMALL);
    }

    #[test]
    fn all_source_scopes_error_when_none_are_served() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![option_doc_for(
                &ingest_context_for("other", REF_SMALL),
                "programs.git.enable",
                "Other source option.",
            )],
            vec![options_target("other", REF_SMALL, 1)],
        );

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service.search_scopes(None, None, None).unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::NoServedSearchScopes
        ));
    }

    #[test]
    fn all_source_search_works_when_some_configured_targets_are_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config_with_extra_fixture_source(&index_dir, "extra"));
        let service = SearchService::open_current(config).unwrap();

        let result = service
            .search_current(SearchRequest {
                query: "git".to_owned(),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap();

        assert!(result.total > 0);
    }

    #[test]
    fn entry_lookup_rejects_configured_but_unserved_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .find_entry_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_STABLE.to_owned()),
                name: "programs.git.enable".to_owned(),
                kind: Some(DocumentKind::Option),
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_STABLE
        ));
    }

    #[test]
    fn from_generation_uses_supplied_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::from_generation(config, path.clone(), manifest).unwrap();

        assert_eq!(service.snapshot().path, path);
        assert!(
            service
                .search_current(SearchRequest {
                    query: "git".to_owned(),
                    limit: 10,
                    ..SearchRequest::default()
                })
                .unwrap()
                .total
                > 0
        );
    }

    #[test]
    fn snapshot_exposes_path_and_manifest() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(snapshot.path, path);
        assert_eq!(snapshot.manifest.document_count, 7);
        assert_eq!(
            snapshot.manifest.generation_id,
            canonical_generation_id(&snapshot.manifest).unwrap()
        );
        assert_ne!(snapshot.manifest.generation_id, snapshot.path.as_str());
        assert!(Arc::ptr_eq(&snapshot.index, &service.current_index()));
    }

    #[test]
    fn from_generation_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let mut manifest = store.read_manifest(&path).unwrap();
        manifest.generation_id = "sha256:wrong".to_owned();

        let config = Arc::new(app_config(&index_dir));
        let error = SearchService::from_generation(config, path, manifest).unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn reconcile_generation_keeps_current_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let manifest = service.snapshot().manifest;

        let outcome = service.reconcile_generation(path, manifest).unwrap();

        assert_eq!(outcome, ReconcileOutcome::Unchanged);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
    }

    #[test]
    fn reconcile_generation_reloads_current_path_when_manifest_changes() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let mut manifest = service.snapshot().manifest;
        manifest.document_count += 1;
        refresh_generation_id(&mut manifest).unwrap();

        let outcome = service
            .reconcile_generation(path, manifest.clone())
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Reloaded);
        assert_eq!(service.snapshot().manifest, manifest);
        assert!(!Arc::ptr_eq(&before, &service.current_index()));
    }

    #[test]
    fn reconcile_generation_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let mut manifest = service.snapshot().manifest;
        manifest.generation_id = "sha256:wrong".to_owned();

        let error = service.reconcile_generation(path, manifest).unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn reconcile_generation_swaps_valid_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let manifest = IndexStore::new(&index_dir)
            .read_manifest(&next_path)
            .unwrap();

        let outcome = service
            .reconcile_generation(next_path.clone(), manifest)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Reloaded);
        assert_eq!(service.snapshot().path, next_path);
        assert_eq!(service.snapshot().manifest.generated_at, next_time);
        assert!(!Arc::ptr_eq(&before, &service.current_index()));
    }

    #[test]
    fn held_snapshot_remains_usable_after_generation_reload() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let held = service.snapshot();
        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let manifest = IndexStore::new(&index_dir)
            .read_manifest(&next_path)
            .unwrap();

        service.reconcile_generation(next_path, manifest).unwrap();

        let result = service
            .search_with_snapshot(
                &held,
                SearchRequest {
                    query: "git".to_owned(),
                    limit: 10,
                    ..SearchRequest::default()
                },
            )
            .unwrap();

        assert_eq!(held.manifest.generated_at, time::OffsetDateTime::UNIX_EPOCH);
        assert!(result.total > 0);
    }

    #[test]
    fn reconcile_generation_keeps_old_generation_when_new_index_cannot_open() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let store = IndexStore::new(&index_dir);
        let broken = store.create_generation_path().unwrap();
        let manifest = service.snapshot().manifest;

        let error = service.reconcile_generation(broken, manifest).unwrap_err();

        assert!(format!("{error:#}").contains("failed to open published index generation"));
        assert_eq!(service.snapshot().path, path);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
    }
}
