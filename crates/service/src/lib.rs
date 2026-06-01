use std::fmt;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::DocumentKind;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::search::{
    EntryLookup, EntryLookupResult, SearchIndex, SearchOptions, SearchResult, SearchScope,
};
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
}

impl fmt::Debug for ServedGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedGeneration")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct ServedGenerationSnapshot {
    pub path: Utf8PathBuf,
    pub manifest: IndexGenerationManifest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Unchanged,
    ManifestUpdated,
    Swapped,
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
        let index = open_index(&path)?;

        Ok(Self {
            config,
            current: Arc::new(RwLock::new(ServedGeneration {
                path,
                manifest,
                index: Arc::new(index),
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
        }
    }

    pub fn reconcile_generation(
        &self,
        path: Utf8PathBuf,
        manifest: IndexGenerationManifest,
    ) -> Result<ReconcileOutcome> {
        if self
            .current
            .read()
            .expect("served generation lock poisoned")
            .path
            != path
        {
            return self.swap_generation(path, manifest);
        }

        let mut current = self
            .current
            .write()
            .expect("served generation lock poisoned");

        if current.path != path {
            drop(current);
            return self.swap_generation(path, manifest);
        }

        if current.manifest == manifest {
            return Ok(ReconcileOutcome::Unchanged);
        }

        current.manifest = manifest;

        Ok(ReconcileOutcome::ManifestUpdated)
    }

    fn swap_generation(
        &self,
        path: Utf8PathBuf,
        manifest: IndexGenerationManifest,
    ) -> Result<ReconcileOutcome> {
        let index = open_index(&path)
            .with_context(|| format!("failed to open published index generation {path}"))?;
        let mut current = self
            .current
            .write()
            .expect("served generation lock poisoned");

        if current.path == path {
            if current.manifest == manifest {
                return Ok(ReconcileOutcome::Unchanged);
            }

            current.manifest = manifest;
            return Ok(ReconcileOutcome::ManifestUpdated);
        }

        *current = ServedGeneration {
            path,
            manifest,
            index: Arc::new(index),
        };

        Ok(ReconcileOutcome::Swapped)
    }

    pub fn search_current(&self, request: SearchRequest) -> Result<SearchResult> {
        let index = self.current_index();
        self.search_with_index(&index, request)
    }

    pub fn search_with_index(
        &self,
        index: &SearchIndex,
        request: SearchRequest,
    ) -> Result<SearchResult> {
        let scopes = self.search_scopes(
            request.source.as_deref(),
            request.ref_id.as_deref(),
            request.ref_set.as_deref(),
        )?;

        index
            .search(SearchOptions {
                query: request.query,
                limit: request.limit,
                offset: request.offset,
                scopes,
            })
            .context("search failed")
    }

    pub fn find_entry_current(&self, request: EntryRequest) -> Result<EntryLookupResult> {
        let index = self.current_index();
        self.find_entry_with_index(&index, request)
    }

    pub fn find_entry_with_index(
        &self,
        index: &SearchIndex,
        request: EntryRequest,
    ) -> Result<EntryLookupResult> {
        let ref_id = self.resolve_entry_ref(&request.source, request.ref_id.as_deref())?;

        index
            .find_entry(EntryLookup {
                source: request.source,
                ref_id,
                name: request.name,
                kind: request.kind,
            })
            .context("entry lookup failed")
    }

    pub fn search_scopes(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> Result<Vec<SearchScope>> {
        let scopes = self
            .config
            .resolve_search_scopes(source, ref_id, ref_set)
            .context("failed to resolve search scope")?
            .into_iter()
            .map(|scope| SearchScope {
                source: scope.source,
                ref_id: scope.ref_id,
            })
            .collect();

        Ok(scopes)
    }

    pub fn resolve_entry_ref(&self, source_id: &str, ref_id: Option<&str>) -> Result<String> {
        if let Some(ref_id) = ref_id.and_then(non_empty) {
            return Ok(ref_id.to_owned());
        }

        let source = self
            .config
            .sources
            .get(source_id)
            .with_context(|| format!("unknown source {source_id:?}"))?;

        source
            .default_ref
            .clone()
            .with_context(|| format!("source {source_id:?} has no default ref"))
    }
}

fn open_index(path: impl AsRef<Utf8Path>) -> Result<SearchIndex> {
    let path = path.as_ref();

    SearchIndex::open(path)
        .with_context(|| format!("failed to open search index {}", path.as_str()))
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
    use nixsearch_index::search::EntryLookupResult;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        publish_canonical_index, publish_canonical_index_with_generated_at,
    };
    use nixsearch_test_support::{REF_SMALL, SOURCE_FIXTURES, app_config, utf8_path_buf};
    use time::Duration as TimeDuration;

    use super::{EntryRequest, ReconcileOutcome, SearchRequest, SearchService};

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
    fn reconcile_generation_updates_manifest_for_current_path() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let mut manifest = service.snapshot().manifest;
        manifest.document_count += 1;

        let outcome = service
            .reconcile_generation(path, manifest.clone())
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::ManifestUpdated);
        assert_eq!(service.snapshot().manifest, manifest);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
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

        assert_eq!(outcome, ReconcileOutcome::Swapped);
        assert_eq!(service.snapshot().path, next_path);
        assert_eq!(service.snapshot().manifest.generated_at, next_time);
        assert!(!Arc::ptr_eq(&before, &service.current_index()));
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
