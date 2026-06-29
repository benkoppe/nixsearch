use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::{Context, Result};
use camino::Utf8Path;

use nixsearch_config::app::AppConfig;
use nixsearch_config::source::{RefConfig, SourceConfig, SourceKind};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::{DocumentKind, IndexedEntryKind, SearchDocument};
use nixsearch_core::target::RefRole;
use nixsearch_index::generation_validator::GenerationValidator;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::search::{
    EntryFacts, EntryLookup, EntryLookupResult, SearchIndex, SearchOptions, SearchResult,
    SearchScope,
};
use nixsearch_index::seo::SeoEntryFacts;
use nixsearch_index::seo_sidecar::{IndexVerifiedSeoFacts, SeoFactsArtifact};
use nixsearch_index::store::{IndexStore, LeasedPublishedGeneration, PublishedGeneration};

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
struct LazySeoFacts {
    value: Arc<OnceLock<Arc<IndexVerifiedSeoFacts>>>,
}

impl LazySeoFacts {
    fn unloaded() -> Self {
        Self {
            value: Arc::new(OnceLock::new()),
        }
    }

    fn loaded(seo_facts: IndexVerifiedSeoFacts) -> Self {
        let lazy = Self::unloaded();
        let _ = lazy.value.set(Arc::new(seo_facts));
        lazy
    }

    fn get_or_load(
        &self,
        generation: &PublishedGeneration,
        index: &SearchIndex,
    ) -> SeoFactsResult<Arc<IndexVerifiedSeoFacts>> {
        if let Some(loaded) = self.value.get() {
            return Ok(Arc::clone(loaded));
        }

        let loaded = SeoFactsArtifact::read_index_verified(generation, index)
            .map(Arc::new)
            .map_err(|error| {
                tracing::warn!(
                    generation = %generation.path,
                    "failed to load SEO facts for served generation: {error:#}"
                );
                SeoFactsUnavailable
            })?;
        let _ = self.value.set(Arc::clone(&loaded));

        Ok(self.value.get().map(Arc::clone).unwrap_or(loaded))
    }
}

#[derive(Clone)]
struct ServedGeneration {
    generation: LeasedPublishedGeneration,
    index: Arc<SearchIndex>,
    seo_facts: Option<LazySeoFacts>,
}

impl ServedGeneration {
    fn to_published_generation(&self) -> PublishedGeneration {
        self.generation.to_published_generation()
    }

    fn matches(&self, generation: &PublishedGeneration) -> bool {
        self.generation.published_generation() == generation
    }
}

impl fmt::Debug for ServedGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedGeneration")
            .field("path", &self.generation.path())
            .field("manifest", &self.generation.manifest())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct ServedGenerationSnapshot {
    generation: LeasedPublishedGeneration,
    pub index: Arc<SearchIndex>,
    seo_facts: Option<LazySeoFacts>,
}

impl fmt::Debug for ServedGenerationSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedGenerationSnapshot")
            .field("path", &self.generation.path())
            .field("manifest", &self.generation.manifest())
            .finish_non_exhaustive()
    }
}

impl ServedGenerationSnapshot {
    pub fn path(&self) -> &Utf8Path {
        self.generation.path()
    }

    pub fn manifest(&self) -> &IndexGenerationManifest {
        self.generation.manifest()
    }

    pub fn published_generation(&self) -> &PublishedGeneration {
        self.generation.published_generation()
    }

    pub fn to_published_generation(&self) -> PublishedGeneration {
        self.generation.to_published_generation()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingSeoPolicy {
    Disabled,
    EagerVerified,
    LazyVerifiedOnUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServingGenerationPolicy {
    pub seo: ServingSeoPolicy,
}

impl ServingGenerationPolicy {
    pub fn eager_for_config(config: &AppConfig) -> Self {
        Self {
            seo: if config.public_seo_enabled() {
                ServingSeoPolicy::EagerVerified
            } else {
                ServingSeoPolicy::Disabled
            },
        }
    }

    pub fn lazy_for_config(config: &AppConfig) -> Self {
        Self {
            seo: if config.public_seo_enabled() {
                ServingSeoPolicy::LazyVerifiedOnUse
            } else {
                ServingSeoPolicy::Disabled
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Unchanged,
    Reloaded,
    Superseded,
}

#[derive(Debug, Clone)]
pub enum ReconcileReport {
    Unchanged { generation: PublishedGeneration },
    Reloaded { generation: PublishedGeneration },
    Superseded,
}

impl ReconcileReport {
    pub fn outcome(&self) -> ReconcileOutcome {
        match self {
            Self::Unchanged { .. } => ReconcileOutcome::Unchanged,
            Self::Reloaded { .. } => ReconcileOutcome::Reloaded,
            Self::Superseded => ReconcileOutcome::Superseded,
        }
    }

    fn from_outcome(outcome: ReconcileOutcome, generation: PublishedGeneration) -> Self {
        match outcome {
            ReconcileOutcome::Unchanged => Self::Unchanged { generation },
            ReconcileOutcome::Reloaded => Self::Reloaded { generation },
            ReconcileOutcome::Superseded => Self::Superseded,
        }
    }
}

pub type ServiceResult<T> = std::result::Result<T, ServiceError>;

pub type SeoFactsResult<T> = std::result::Result<T, SeoFactsUnavailable>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("SEO facts are unavailable")]
pub struct SeoFactsUnavailable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitemapCandidate {
    pub source: String,
    pub name: String,
}

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
}

#[derive(Debug, Clone)]
struct ConfiguredSearchTarget {
    source: String,
    ref_id: String,
    artifact_kind: ArtifactKind,
    target_role: RefRole,
    entry_kind: IndexedEntryKind,
}

impl SearchService {
    pub fn open_current(config: Arc<AppConfig>) -> Result<Self> {
        let index_store = IndexStore::new(&config.data.index_dir);
        let generation = index_store.current_leased_generation().with_context(|| {
            format!(
                "failed to locate current index in {}",
                config.data.index_dir
            )
        })?;

        Self::from_leased_generation(config, generation)
    }

    pub fn from_leased_generation(
        config: Arc<AppConfig>,
        generation: LeasedPublishedGeneration,
    ) -> Result<Self> {
        let policy = ServingGenerationPolicy::eager_for_config(&config);
        Self::from_leased_generation_with_policy(config, generation, policy)
    }

    pub fn from_leased_generation_with_policy(
        config: Arc<AppConfig>,
        generation: LeasedPublishedGeneration,
        policy: ServingGenerationPolicy,
    ) -> Result<Self> {
        let current = load_servable_generation(&config, generation, policy)?;

        Ok(Self::from_loaded_generation(config, current))
    }

    /// Loads a generation that the caller has already checked against its
    /// integrity attestation.
    ///
    /// This avoids repeating the expensive fingerprint validation during server
    /// startup after `ensure_current_generation` has already accepted the same
    /// generation.
    pub fn from_integrity_attested_leased_generation_with_policy(
        config: Arc<AppConfig>,
        generation: LeasedPublishedGeneration,
        policy: ServingGenerationPolicy,
    ) -> Result<Self> {
        let current = load_integrity_attested_servable_generation(&config, generation, policy)?;

        Ok(Self::from_loaded_generation(config, current))
    }

    fn from_loaded_generation(config: Arc<AppConfig>, current: ServedGeneration) -> Self {
        Self {
            config,
            current: Arc::new(RwLock::new(current)),
        }
    }

    pub fn verify_leased_generation_structural(
        config: &AppConfig,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        let validator = GenerationValidator::new(IndexStore::new(&config.data.index_dir));
        validator
            .open_integrity_attested_published_index(generation.published_generation(), false)
            .context("failed to validate structurally verified generation")
            .map(|_| ())
    }

    pub fn verify_leased_generation_seo(
        config: &AppConfig,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        let validator = GenerationValidator::new(IndexStore::new(&config.data.index_dir));
        validator
            .validate_seo_verified_leased_generation(generation)
            .context("failed to validate SEO-verified generation")
    }

    pub fn verify_leased_generation_seo_sidecar_present(
        config: &AppConfig,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        let validator = GenerationValidator::new(IndexStore::new(&config.data.index_dir));
        validator.require_seo_sidecar_file_present(generation.published_generation())
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
            generation: current.generation.clone(),
            index: Arc::clone(&current.index),
            seo_facts: current.seo_facts.clone(),
        }
    }

    pub fn reconcile_current_generation(&self) -> Result<ReconcileReport> {
        let index_store = IndexStore::new(&self.config.data.index_dir);
        let current_path = index_store.current_path().with_context(|| {
            format!(
                "failed to locate current index in {}",
                self.config.data.index_dir
            )
        })?;

        let observed_current = {
            let current = self
                .current
                .read()
                .expect("served generation lock poisoned");

            if current.generation.path() == current_path {
                return Ok(ReconcileReport::Unchanged {
                    generation: current.to_published_generation(),
                });
            }

            current.to_published_generation()
        };

        let generation = index_store.current_leased_generation().with_context(|| {
            format!(
                "failed to locate current index in {}",
                self.config.data.index_dir
            )
        })?;
        let identity = generation.to_published_generation();
        let outcome = self.reload_generation(&index_store, generation, observed_current)?;

        Ok(ReconcileReport::from_outcome(outcome, identity))
    }

    fn reload_generation(
        &self,
        index_store: &IndexStore,
        generation: LeasedPublishedGeneration,
        observed_current: PublishedGeneration,
    ) -> Result<ReconcileOutcome> {
        let candidate_path = generation.path().to_owned();
        let identity = generation.to_published_generation();

        if !published_generation_is_current(index_store, &identity)? {
            return Ok(ReconcileOutcome::Superseded);
        }

        let policy = ServingGenerationPolicy::eager_for_config(&self.config);
        let loaded =
            load_servable_generation(&self.config, generation, policy).with_context(|| {
                format!("failed to load published index generation {candidate_path}")
            })?;

        let mut current = self
            .current
            .write()
            .expect("served generation lock poisoned");

        if !published_generation_is_current(index_store, &identity)? {
            return Ok(ReconcileOutcome::Superseded);
        }

        if current.matches(&identity) {
            return Ok(ReconcileOutcome::Unchanged);
        }

        if current.to_published_generation() != observed_current {
            return Ok(ReconcileOutcome::Superseded);
        }

        *current = loaded;

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
        let lookup = self.resolve_entry_lookup_for_snapshot(snapshot, request)?;

        snapshot
            .index
            .find_entry(lookup)
            .map_err(ServiceError::EntryLookup)
    }

    pub fn find_entry_with_facts_with_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: EntryRequest,
        facts: &EntryFacts,
    ) -> ServiceResult<EntryLookupResult> {
        let lookup = self.resolve_entry_lookup_for_snapshot(snapshot, request)?;

        snapshot
            .index
            .find_entry_with_facts(lookup, facts)
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
        let lookup = self.resolve_entry_lookup_for_snapshot(snapshot, request)?;

        snapshot
            .index
            .entry_facts(lookup)
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
        self.resolve_served_search_targets(snapshot, source, ref_id, ref_set)
            .map(|targets| targets.into_iter().map(search_scope_for_target).collect())
    }

    pub fn served_search_document_count_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<usize, RequestResolutionError> {
        let targets = self.resolve_served_search_targets(snapshot, source, ref_id, ref_set)?;

        Ok(targets
            .iter()
            .map(|expected| {
                Self::matching_manifest_targets(snapshot, expected)
                    .map(|target| target.document_count)
                    .sum::<usize>()
            })
            .sum())
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
        Ok(self
            .resolve_entry_target_for_snapshot(snapshot, source_id, ref_id)?
            .ref_id)
    }

    fn resolve_entry_target_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: Option<&str>,
    ) -> std::result::Result<ConfiguredSearchTarget, RequestResolutionError> {
        let ref_id = match ref_id.and_then(non_empty) {
            Some(ref_id) => {
                self.ensure_configured_ref(source_id, ref_id)?;
                ref_id.to_owned()
            }
            None => self.configured_default_ref(source_id)?.to_owned(),
        };

        let Some(target) = self.configured_search_target(source_id, &ref_id)? else {
            return Err(RequestResolutionError::UnservedRef {
                source_id: source_id.to_owned(),
                ref_id,
            });
        };

        if !self.search_target_exists_in_snapshot(snapshot, &target) {
            return Err(RequestResolutionError::UnservedRef {
                source_id: source_id.to_owned(),
                ref_id,
            });
        }

        Ok(target)
    }

    fn resolve_entry_lookup_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        request: EntryRequest,
    ) -> std::result::Result<EntryLookup, RequestResolutionError> {
        let target = self.resolve_entry_target_for_snapshot(
            snapshot,
            &request.source,
            request.ref_id.as_deref(),
        )?;

        Ok(EntryLookup {
            source: request.source,
            ref_id: target.ref_id,
            entry_kind: target.entry_kind,
            name: request.name,
        })
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
        self.served_ref_exists_in_snapshot(&snapshot, source_id, ref_id)
    }

    pub fn served_ref_exists_in_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> bool {
        let Ok(Some(target)) = self.configured_search_target(source_id, ref_id) else {
            return false;
        };

        self.search_target_exists_in_snapshot(snapshot, &target)
    }

    fn search_target_exists_in_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        expected: &ConfiguredSearchTarget,
    ) -> bool {
        Self::matching_manifest_targets(snapshot, expected)
            .next()
            .is_some()
    }

    fn matching_manifest_targets<'a>(
        snapshot: &'a ServedGenerationSnapshot,
        expected: &'a ConfiguredSearchTarget,
    ) -> impl Iterator<Item = &'a IndexTargetManifest> + 'a {
        snapshot.manifest().targets.iter().filter(|target| {
            target.source == expected.source
                && target.ref_id == expected.ref_id
                && target.artifact_kind == expected.artifact_kind
                && target.target_role == expected.target_role
                && target.indexes_search_documents
        })
    }

    pub fn document_allowed_for_public_seo(
        &self,
        snapshot: &ServedGenerationSnapshot,
        document: &SearchDocument,
    ) -> SeoFactsResult<bool> {
        if !self.config.public_seo_enabled() {
            return Ok(false);
        }

        let common = document.common();
        let Some(document_kind) =
            self.configured_served_document_kind_for_seo(snapshot, &common.source, &common.ref_id)
        else {
            return Ok(false);
        };

        if &document_kind != document.kind() {
            return Ok(false);
        }

        let seo_facts = self.seo_facts_for_snapshot(snapshot)?;
        Ok(seo_facts.sidecar().entries.iter().any(|entry| {
            entry.source == common.source.as_str()
                && entry.ref_id == common.ref_id.as_str()
                && entry.name == common.name.as_str()
                && entry_is_unique_eligible_for_configured_kind(entry, &document_kind)
        }))
    }

    pub fn source_has_indexable_entries(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> SeoFactsResult<bool> {
        let seo_facts = self.seo_facts_for_snapshot(snapshot)?;

        let Some(document_kind) =
            self.configured_served_document_kind_for_seo(snapshot, source_id, ref_id)
        else {
            return Ok(false);
        };

        Ok(seo_facts.sidecar().entries.iter().any(|entry| {
            entry.source == source_id
                && entry.ref_id == ref_id
                && entry_is_unique_eligible_for_configured_kind(entry, &document_kind)
        }))
    }

    pub fn sitemap_candidates(
        &self,
        snapshot: &ServedGenerationSnapshot,
    ) -> SeoFactsResult<Vec<SitemapCandidate>> {
        let seo_facts = self.seo_facts_for_snapshot(snapshot)?;
        let mut candidates = Vec::new();

        for entry in &seo_facts.sidecar().entries {
            let Some(document_kind) = self.configured_served_document_kind_for_seo(
                snapshot,
                &entry.source,
                &entry.ref_id,
            ) else {
                continue;
            };

            if entry_is_unique_eligible_for_configured_kind(entry, &document_kind) {
                candidates.push(SitemapCandidate {
                    source: entry.source.clone(),
                    name: entry.name.clone(),
                });
            }
        }

        Ok(candidates)
    }

    fn seo_facts_for_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
    ) -> SeoFactsResult<Arc<IndexVerifiedSeoFacts>> {
        let seo_facts = snapshot.seo_facts.as_ref().ok_or(SeoFactsUnavailable)?;

        seo_facts.get_or_load(snapshot.published_generation(), &snapshot.index)
    }

    fn ref_allowed_to_be_indexed(&self, source: &SourceConfig, ref_id: &str) -> bool {
        source.default_ref.as_deref() == Some(ref_id)
    }

    fn configured_served_document_kind_for_seo(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> Option<DocumentKind> {
        if !self.config.public_seo_enabled() || snapshot.seo_facts.is_none() {
            return None;
        }

        let source = self.config.sources.get(source_id)?;
        let ref_config = source
            .refs
            .iter()
            .find(|candidate| candidate.id == ref_id)?;
        if matches!(source.kind, SourceKind::Apps | SourceKind::Services)
            || !ref_config.capabilities().is_public_seo_candidate()
            || !self.ref_allowed_to_be_indexed(source, ref_id)
        {
            return None;
        }

        let target = self
            .configured_search_target(source_id, ref_id)
            .ok()
            .flatten()?;
        if !self.search_target_exists_in_snapshot(snapshot, &target) {
            return None;
        }

        Some(target.entry_kind.document_kind())
    }

    fn resolve_served_search_targets(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<Vec<ConfiguredSearchTarget>, RequestResolutionError> {
        let source = source.and_then(non_empty);
        let ref_id = ref_id.and_then(non_empty);
        let ref_set = ref_set.and_then(non_empty);
        let source_specific = source.is_some();

        let refs = self.resolve_configured_search_refs(source, ref_id, ref_set)?;

        if source_specific {
            let (source, ref_id) = refs
                .into_iter()
                .next()
                .ok_or(RequestResolutionError::NoServedSearchScopes)?;
            let Some(target) = self.configured_search_target(&source, &ref_id)? else {
                return Err(RequestResolutionError::UnservedRef {
                    source_id: source,
                    ref_id,
                });
            };

            if !self.search_target_exists_in_snapshot(snapshot, &target) {
                return Err(RequestResolutionError::UnservedRef {
                    source_id: target.source,
                    ref_id: target.ref_id,
                });
            }

            return Ok(vec![target]);
        }

        let served_targets = refs
            .into_iter()
            .filter_map(|(source, ref_id)| {
                let target = self
                    .configured_search_target(&source, &ref_id)
                    .ok()
                    .flatten()?;
                self.search_target_exists_in_snapshot(snapshot, &target)
                    .then_some(target)
            })
            .collect::<Vec<_>>();

        if served_targets.is_empty() {
            return Err(RequestResolutionError::NoServedSearchScopes);
        }

        Ok(served_targets)
    }

    fn resolve_configured_search_refs(
        &self,
        source: Option<&str>,
        ref_id: Option<&str>,
        ref_set: Option<&str>,
    ) -> std::result::Result<Vec<(String, String)>, RequestResolutionError> {
        match (source, ref_id, ref_set) {
            (Some(source_id), _, Some(ref_set_id)) => {
                self.resolve_source_ref_set_ref(source_id, ref_id, ref_set_id)
            }
            (Some(source_id), Some(ref_id), None) => {
                self.ensure_configured_ref(source_id, ref_id)?;
                Ok(vec![(source_id.to_owned(), ref_id.to_owned())])
            }
            (Some(source_id), None, None) => {
                let default_ref = self.configured_default_ref(source_id)?;

                Ok(vec![(source_id.to_owned(), default_ref.to_owned())])
            }
            (None, Some(_), _) => Err(RequestResolutionError::RefRequiresSource),
            (None, None, Some(ref_set_id)) => self.resolve_all_ref_set_refs(ref_set_id),
            (None, None, None) => self.resolve_default_all_refs(),
        }
    }

    fn resolve_default_all_refs(
        &self,
    ) -> std::result::Result<Vec<(String, String)>, RequestResolutionError> {
        if let Some(default_ref_set) = self.config.default_ref_set() {
            return self.resolve_all_ref_set_refs(default_ref_set);
        }

        self.config
            .sources
            .iter()
            .filter_map(|(source_id, source)| {
                source
                    .default_ref
                    .as_ref()
                    .map(|default_ref| Ok((source_id.clone(), default_ref.clone())))
            })
            .collect()
    }

    fn resolve_all_ref_set_refs(
        &self,
        ref_set_id: &str,
    ) -> std::result::Result<Vec<(String, String)>, RequestResolutionError> {
        let ref_set = self.config.ref_sets.get(ref_set_id).ok_or_else(|| {
            RequestResolutionError::UnknownRefSet {
                ref_set: ref_set_id.to_owned(),
            }
        })?;

        ref_set
            .refs
            .iter()
            .flat_map(|(source_id, ref_ids)| {
                ref_ids.iter().map(|ref_id| {
                    self.ensure_configured_ref(source_id, ref_id)?;
                    Ok((source_id.clone(), ref_id.clone()))
                })
            })
            .collect()
    }

    fn resolve_source_ref_set_ref(
        &self,
        source_id: &str,
        ref_id: Option<&str>,
        ref_set_id: &str,
    ) -> std::result::Result<Vec<(String, String)>, RequestResolutionError> {
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

        Ok(vec![(source_id.to_owned(), selected_ref.to_owned())])
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

    fn configured_ref(
        &self,
        source_id: &str,
        ref_id: &str,
    ) -> std::result::Result<&RefConfig, RequestResolutionError> {
        let source = self.configured_source(source_id)?;

        source
            .refs
            .iter()
            .find(|candidate| candidate.id == ref_id)
            .ok_or_else(|| RequestResolutionError::UnknownRef {
                source_id: source_id.to_owned(),
                ref_id: ref_id.to_owned(),
            })
    }

    fn configured_search_target(
        &self,
        source_id: &str,
        ref_id: &str,
    ) -> std::result::Result<Option<ConfiguredSearchTarget>, RequestResolutionError> {
        let ref_config = self.configured_ref(source_id, ref_id)?;
        if !ref_config.is_searchable() {
            return Ok(None);
        }

        let artifact_kind = ref_config.artifact_kind();
        let Some(entry_kind) = ref_config.indexed_entry_kind() else {
            return Ok(None);
        };

        Ok(Some(ConfiguredSearchTarget {
            source: source_id.to_owned(),
            ref_id: ref_id.to_owned(),
            artifact_kind,
            target_role: ref_config.role,
            entry_kind,
        }))
    }
}

fn search_scope_for_target(target: ConfiguredSearchTarget) -> SearchScope {
    SearchScope {
        source: target.source,
        ref_id: target.ref_id,
        entry_kind: target.entry_kind,
    }
}

fn load_integrity_attested_servable_generation(
    config: &AppConfig,
    generation: LeasedPublishedGeneration,
    policy: ServingGenerationPolicy,
) -> Result<ServedGeneration> {
    if policy.seo == ServingSeoPolicy::EagerVerified {
        return load_servable_generation(config, generation, policy);
    }

    let index_store = IndexStore::new(&config.data.index_dir);
    let index_path = index_store.index_path(generation.path());
    let index = SearchIndex::open(&index_path)
        .with_context(|| format!("failed to open search index {index_path}"))?;
    let seo_facts = match policy.seo {
        ServingSeoPolicy::Disabled => None,
        ServingSeoPolicy::EagerVerified => unreachable!("eager SEO policy returned above"),
        ServingSeoPolicy::LazyVerifiedOnUse => {
            let validator = GenerationValidator::new(index_store);
            validator
                .require_seo_sidecar_file_present(generation.published_generation())
                .context("failed to require SEO sidecar file for lazy SEO serving")?;
            Some(LazySeoFacts::unloaded())
        }
    };

    Ok(ServedGeneration {
        generation,
        index: Arc::new(index),
        seo_facts,
    })
}

fn load_servable_generation(
    config: &AppConfig,
    generation: LeasedPublishedGeneration,
    policy: ServingGenerationPolicy,
) -> Result<ServedGeneration> {
    let index_store = IndexStore::new(&config.data.index_dir);
    let validator = GenerationValidator::new(index_store);
    let (index, seo_facts) = match policy.seo {
        ServingSeoPolicy::Disabled => {
            let index = validator
                .open_integrity_attested_published_index(generation.published_generation(), false)
                .context("failed to open structurally verified served generation")?;
            (index, None)
        }
        ServingSeoPolicy::EagerVerified => {
            let verified = validator
                .open_integrity_attested_seo_generation(generation.published_generation())
                .context("failed to open SEO-verified served generation")?;
            (verified.index, Some(LazySeoFacts::loaded(verified.sidecar)))
        }
        ServingSeoPolicy::LazyVerifiedOnUse => {
            let index = validator
                .open_integrity_attested_published_index(generation.published_generation(), false)
                .context("failed to open structurally verified served generation")?;
            validator
                .require_seo_sidecar_file_present(generation.published_generation())
                .context("failed to require SEO sidecar file for lazy SEO serving")?;
            (index, Some(LazySeoFacts::unloaded()))
        }
    };

    Ok(ServedGeneration {
        generation,
        index: Arc::new(index),
        seo_facts,
    })
}

fn entry_is_unique_eligible_for_configured_kind(
    entry: &SeoEntryFacts,
    configured_kind: &DocumentKind,
) -> bool {
    let (supported_count, eligible_count) = match configured_kind {
        DocumentKind::Package => (entry.package_supported_count, entry.package_eligible_count),
        DocumentKind::Option => (entry.option_supported_count, entry.option_eligible_count),
        DocumentKind::App | DocumentKind::Service => return false,
    };

    supported_count == 1 && eligible_count == 1
}

fn published_generation_is_current(
    index_store: &IndexStore,
    generation: &PublishedGeneration,
) -> Result<bool> {
    let Some(current) = index_store.try_current_generation_metadata()? else {
        return Ok(false);
    };

    Ok(current == *generation)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc};

    use tempfile::tempdir;

    use nixsearch_config::app::AppConfig;
    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::SourceKind;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::SearchDocument;
    use nixsearch_core::target::RefRole;
    use nixsearch_index::manifest::{
        IndexTargetManifest, canonical_generation_id, refresh_generation_id,
    };
    use nixsearch_index::search::{EntryFactsStatus, EntryLookupResult};
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::{IndexStore, LeasedPublishedGeneration, PublishedGeneration};
    use nixsearch_index_test_support::{
        index_target, options_target, publish_canonical_index,
        publish_canonical_index_with_generated_at, publish_documents_with_manifest_targets,
        publish_fixture_options_index_for_refs, write_raw_manifest, write_raw_seo_sidecar,
    };
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, app_config, app_config_with_extra_fixture_source,
        app_config_with_public_url, ingest_context_for, multi_ref_app_config,
        multi_ref_app_config_with_public_url, option_doc_for, package_doc_for, utf8_path_buf,
    };
    use time::Duration as TimeDuration;

    use super::{
        EntryRequest, ReconcileOutcome, ReconcileReport, RequestResolutionError, SearchRequest,
        SearchService, SeoFactsUnavailable, ServiceError,
    };

    fn leased_generation(
        index_dir: &camino::Utf8Path,
        path: camino::Utf8PathBuf,
        manifest: nixsearch_index::manifest::IndexGenerationManifest,
    ) -> LeasedPublishedGeneration {
        IndexStore::new(index_dir)
            .lease_published_generation(PublishedGeneration { path, manifest })
            .unwrap()
    }

    fn candidate_tuples(
        service: &SearchService,
        snapshot: &super::ServedGenerationSnapshot,
    ) -> Vec<(String, String)> {
        service
            .sitemap_candidates(snapshot)
            .unwrap()
            .into_iter()
            .map(|candidate| (candidate.source, candidate.name))
            .collect()
    }

    fn flake_info_only_config(index_dir: &camino::Utf8Path) -> AppConfig {
        let mut config = app_config(index_dir);
        set_fixture_refs_artifact_kind(&mut config, ArtifactKind::FlakeInfoJson);

        config
    }

    fn multi_ref_flake_info_only_config(index_dir: &camino::Utf8Path) -> AppConfig {
        let mut config = multi_ref_app_config(index_dir);
        set_fixture_refs_artifact_kind(&mut config, ArtifactKind::FlakeInfoJson);

        config
    }

    fn set_fixture_refs_artifact_kind(config: &mut AppConfig, artifact_kind: ArtifactKind) {
        let source = config
            .sources
            .get_mut(SOURCE_FIXTURES)
            .expect("fixture source exists");

        for ref_config in &mut source.refs {
            ref_config.role = RefRole::default_for_artifact_kind(artifact_kind);
            ref_config.producer = ProducerConfig::ExistingFile {
                path: PathBuf::from("unused.json"),
                artifact: artifact_kind,
            };
        }
    }

    fn publish_flake_info_only_index(index_dir: &camino::Utf8Path, ref_ids: &[&str]) {
        publish_documents_with_manifest_targets(
            index_dir,
            time::OffsetDateTime::now_utc(),
            Vec::new(),
            ref_ids
                .iter()
                .map(|ref_id| index_target(SOURCE_FIXTURES, ref_id, ArtifactKind::FlakeInfoJson, 0))
                .collect(),
        );
    }

    fn assert_document_allowed_for_public_seo(
        config: AppConfig,
        document_source: &str,
        document_ref: &str,
        expected: bool,
    ) {
        let service = SearchService::open_current(Arc::new(config)).unwrap();
        let snapshot = service.snapshot();
        let document = option_doc_for(
            &ingest_context_for(document_source, document_ref),
            "programs.git.enable",
            "Git option.",
        );

        assert_eq!(
            service.document_allowed_for_public_seo(&snapshot, &document),
            Ok(expected)
        );
    }

    #[test]
    fn search_current_uses_configured_default_scopes() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(Arc::clone(&config)).unwrap();

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

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(Arc::clone(&config)).unwrap();

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

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let result = service
            .find_entry_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_SMALL.to_owned()),
                name: "programs.git.enable".to_owned(),
            })
            .unwrap();

        assert!(matches!(result, EntryLookupResult::Found(_)));
    }

    #[test]
    fn explicit_search_rejects_flake_info_only_ref_as_unserved() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_flake_info_only_index(&index_dir, &[REF_SMALL]);

        let config = Arc::new(flake_info_only_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_current(SearchRequest {
                query: "git".to_owned(),
                source: Some(SOURCE_FIXTURES.to_owned()),
                ref_id: Some(REF_SMALL.to_owned()),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_SMALL
        ));
    }

    #[test]
    fn served_search_document_count_uses_exact_searchable_targets() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                package_doc_for(&context, "git", "Git package."),
                package_doc_for(&context, "git", "Duplicate Git package."),
                option_doc_for(&context, "git", "Git option."),
            ],
            vec![
                index_target(SOURCE_FIXTURES, REF_SMALL, ArtifactKind::PackagesJson, 2),
                options_target(SOURCE_FIXTURES, REF_SMALL, 1),
            ],
        );

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.served_search_document_count_for_snapshot(
                &snapshot,
                Some(SOURCE_FIXTURES),
                Some(REF_SMALL),
                None,
            ),
            Ok(1)
        );
        assert_eq!(
            service.served_search_document_count_for_snapshot(&snapshot, None, None, None),
            Ok(1)
        );
    }

    #[test]
    fn served_search_document_count_handles_zero_and_artifact_only_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            Vec::new(),
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 0)],
        );

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.served_search_document_count_for_snapshot(&snapshot, None, None, None),
            Ok(0)
        );

        let index_dir = utf8_path_buf(tempdir.path().join("flake-info-indexes"));
        publish_flake_info_only_index(&index_dir, &[REF_SMALL]);
        let config = Arc::new(flake_info_only_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(matches!(
            service.served_search_document_count_for_snapshot(&snapshot, None, None, None),
            Err(RequestResolutionError::NoServedSearchScopes)
        ));
    }

    #[test]
    fn entry_lookup_rejects_flake_info_only_ref_as_unserved() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_flake_info_only_index(&index_dir, &[REF_SMALL]);

        let config = Arc::new(flake_info_only_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .find_entry_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_SMALL.to_owned()),
                name: "programs.git.enable".to_owned(),
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_SMALL
        ));
    }

    #[test]
    fn stale_searchable_artifact_kind_does_not_serve_configured_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![package_doc_for(&context, "git", "Git package.")],
            vec![index_target(
                SOURCE_FIXTURES,
                REF_SMALL,
                ArtifactKind::PackagesJson,
                1,
            )],
        );

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(!service.served_ref_exists(SOURCE_FIXTURES, REF_SMALL));

        let error = service
            .search_current(SearchRequest {
                query: "git".to_owned(),
                source: Some(SOURCE_FIXTURES.to_owned()),
                ref_id: Some(REF_SMALL.to_owned()),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_SMALL
        ));
    }

    #[test]
    fn stale_searchable_target_role_does_not_serve_configured_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![option_doc_for(
                &context,
                "programs.git.enable",
                "Git option.",
            )],
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::IndexOnly,
                indexes_search_documents: true,
                document_count: 1,
                artifact_hash: Some("fixture-hash".to_owned()),
                revision: None,
            }],
        );

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(!service.served_ref_exists(SOURCE_FIXTURES, REF_SMALL));

        let error = service
            .search_current(SearchRequest {
                query: "git".to_owned(),
                source: Some(SOURCE_FIXTURES.to_owned()),
                ref_id: Some(REF_SMALL.to_owned()),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_SMALL
        ));
    }

    #[test]
    fn flake_info_only_refs_do_not_provide_default_or_ref_set_search_scopes() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_flake_info_only_index(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_flake_info_only_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        for error in [
            service.search_scopes(None, None, None).unwrap_err(),
            service
                .search_scopes(None, None, Some("single"))
                .unwrap_err(),
            service
                .search_scopes(None, None, Some("multi"))
                .unwrap_err(),
        ] {
            assert!(matches!(
                error,
                RequestResolutionError::NoServedSearchScopes
            ));
        }
    }

    #[test]
    fn entry_facts_current_resolves_unique_entry() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let facts = service
            .entry_facts_current(EntryRequest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: Some(REF_SMALL.to_owned()),
                name: "programs.git.enable".to_owned(),
            })
            .unwrap();

        assert_eq!(facts.status(), EntryFactsStatus::Unique);
        assert_eq!(facts.count, 1);
        assert_eq!(facts.seo_eligible(), Some(true));
    }

    #[test]
    fn helpers_report_configured_and_served_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
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

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(true)
        );
    }

    #[test]
    fn document_allowed_for_public_seo_accepts_default_served_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        assert_document_allowed_for_public_seo(
            multi_ref_app_config_with_public_url(&index_dir),
            SOURCE_FIXTURES,
            REF_SMALL,
            true,
        );
    }

    #[test]
    fn document_allowed_for_public_seo_rejects_non_default_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        assert_document_allowed_for_public_seo(
            multi_ref_app_config_with_public_url(&index_dir),
            SOURCE_FIXTURES,
            REF_STABLE,
            false,
        );
    }

    #[test]
    fn document_allowed_for_public_seo_rejects_unserved_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        assert_document_allowed_for_public_seo(
            multi_ref_app_config_with_public_url(&index_dir),
            SOURCE_FIXTURES,
            REF_STABLE,
            false,
        );
    }

    #[test]
    fn document_allowed_for_public_seo_rejects_unknown_source() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        assert_document_allowed_for_public_seo(
            multi_ref_app_config_with_public_url(&index_dir),
            "missing",
            REF_SMALL,
            false,
        );
    }

    #[test]
    fn document_allowed_for_public_seo_rejects_app_and_service_sources() {
        for source_kind in [SourceKind::Apps, SourceKind::Services] {
            let tempdir = tempdir().unwrap();
            let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
            publish_canonical_index(&index_dir);
            let mut config = multi_ref_app_config_with_public_url(&index_dir);
            config
                .sources
                .get_mut(SOURCE_FIXTURES)
                .expect("fixture source exists")
                .kind = source_kind;

            assert_document_allowed_for_public_seo(config, SOURCE_FIXTURES, REF_SMALL, false);
        }
    }

    #[test]
    fn loaded_sidecar_facts_are_available_immediately() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(service.sitemap_candidates(&snapshot).is_ok());
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(true)
        );
    }

    #[test]
    fn default_served_ref_without_eligible_sidecar_facts_is_not_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut hidden = match option_doc_for(
            &ingest_context_for(SOURCE_FIXTURES, REF_SMALL),
            "programs.hidden.enable",
            "Hidden option.",
        ) {
            SearchDocument::Option(doc) => doc,
            SearchDocument::Package(_) => unreachable!(),
        };
        hidden.visible = Some(false);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![SearchDocument::Option(hidden)],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
        );

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_SMALL));
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(false)
        );
    }

    #[test]
    fn open_current_rejects_manifest_valid_but_index_invalid_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();
        let generation = PublishedGeneration {
            path,
            manifest: manifest.clone(),
        };
        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&generation)
            .unwrap()
            .into_sidecar();

        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &generation, &sidecar);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(Arc::clone(&config)).unwrap_err();
        assert!(format!("{error:#}").contains("SEO sidecar integrity metadata does not match"));

        let leased = leased_generation(&index_dir, generation.path, manifest);
        let error = SearchService::verify_leased_generation_seo(&config, &leased).unwrap_err();

        assert!(format!("{error:#}").contains("SEO sidecar integrity metadata does not match"));
    }

    #[test]
    fn lazily_loaded_sidecar_is_validated_against_index() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();
        let generation = PublishedGeneration {
            path,
            manifest: manifest.clone(),
        };
        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&generation)
            .unwrap()
            .into_sidecar();

        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &generation, &sidecar);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let leased = leased_generation(&index_dir, generation.path, manifest);
        let service = SearchService::from_leased_generation_with_policy(
            Arc::clone(&config),
            leased,
            super::ServingGenerationPolicy::lazy_for_config(&config),
        )
        .unwrap();
        let snapshot = service.snapshot();
        let document = option_doc_for(
            &ingest_context_for(SOURCE_FIXTURES, REF_SMALL),
            "programs.git.enable",
            "Git option.",
        );

        assert_eq!(
            service.sitemap_candidates(&snapshot),
            Err(SeoFactsUnavailable)
        );
        assert_eq!(
            service.document_allowed_for_public_seo(&snapshot, &document),
            Err(SeoFactsUnavailable)
        );
    }

    #[test]
    fn open_current_rejects_forged_artifact_only_manifest_document_count() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();
        let generation = PublishedGeneration {
            path,
            manifest: manifest.clone(),
        };
        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&generation)
            .unwrap()
            .into_sidecar();
        let mut forged_manifest = manifest;

        forged_manifest.document_count += 1;
        forged_manifest.targets.push(index_target(
            SOURCE_FIXTURES,
            REF_SMALL,
            ArtifactKind::FlakeInfoJson,
            1,
        ));
        refresh_generation_id(&mut forged_manifest).unwrap();
        sidecar.generation_id = forged_manifest.generation_id.clone();

        let forged_generation = PublishedGeneration {
            path: generation.path,
            manifest: forged_manifest.clone(),
        };
        write_raw_manifest(&store, &forged_generation, &forged_manifest);
        write_raw_seo_sidecar(&store, &forged_generation, &sidecar);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("artifact-only"));
        assert!(message.contains("flake-info-json"));
    }

    #[test]
    fn open_current_without_public_seo_rejects_manifest_index_count_mismatch() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let mut manifest = store.read_manifest(&path).unwrap();

        manifest.document_count += 1;
        manifest.targets[0].document_count += 1;
        refresh_generation_id(&mut manifest).unwrap();
        let generation = PublishedGeneration {
            path,
            manifest: manifest.clone(),
        };
        write_raw_manifest(&store, &generation, &manifest);

        let config = Arc::new(app_config(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();

        assert!(format!("{error:#}").contains("integrity metadata does not match"));
    }

    #[test]
    fn non_default_served_ref_is_not_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_STABLE));
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_STABLE),
            Ok(false)
        );
    }

    #[test]
    fn open_current_rejects_missing_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        fs::remove_file(SeoFactsArtifact::path(&path)).unwrap();

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();
        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
    }

    #[test]
    fn open_current_accepts_missing_sidecar_without_public_seo() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let sidecar_path = SeoFactsArtifact::path(&path);
        fs::remove_file(&sidecar_path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        SearchService::open_current(config).unwrap();

        assert!(!sidecar_path.exists());
    }

    #[test]
    fn source_has_indexable_entries_uses_sitemap_candidate_rules() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![option_doc_for(
                &context,
                "programs.git.enable",
                "Git option.",
            )],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
        );

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(true)
        );
        assert_eq!(
            candidate_tuples(&service, &snapshot),
            vec![(SOURCE_FIXTURES.to_owned(), "programs.git.enable".to_owned())]
        );
    }

    #[test]
    fn sitemap_candidates_ignore_stale_cross_kind_facts() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                package_doc_for(&context, "git", "Git package."),
                option_doc_for(&context, "git", "Git option."),
            ],
            vec![
                index_target(SOURCE_FIXTURES, REF_SMALL, ArtifactKind::PackagesJson, 1),
                options_target(SOURCE_FIXTURES, REF_SMALL, 1),
            ],
        );

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            candidate_tuples(&service, &snapshot),
            vec![(SOURCE_FIXTURES.to_owned(), "git".to_owned())]
        );
    }

    #[test]
    fn sitemap_candidates_exclude_hidden_and_internal_only_options() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        let mut internal = match option_doc_for(&context, "internal.entry", "Internal option.") {
            SearchDocument::Option(option) => option,
            SearchDocument::Package(_) => unreachable!(),
        };
        internal.internal = Some(true);
        let mut hidden = match option_doc_for(&context, "hidden.entry", "Hidden option.") {
            SearchDocument::Option(option) => option,
            SearchDocument::Package(_) => unreachable!(),
        };
        hidden.visible = Some(false);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                SearchDocument::Option(internal),
                SearchDocument::Option(hidden),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 2)],
        );

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(false)
        );
        assert!(service.sitemap_candidates(&snapshot).unwrap().is_empty());
    }

    #[test]
    fn sitemap_candidates_exclude_same_kind_duplicates() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                option_doc_for(&context, "duplicate.entry", "First duplicate option."),
                option_doc_for(&context, "duplicate.entry", "Second duplicate option."),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 2)],
        );

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(service.sitemap_candidates(&snapshot).unwrap().is_empty());
    }

    #[test]
    fn sitemap_candidates_exclude_non_default_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_STABLE),
            Ok(false)
        );
        assert_eq!(candidate_tuples(&service, &snapshot).len(), 1);
        assert!(
            candidate_tuples(&service, &snapshot)
                .iter()
                .all(|(_, name)| !name.contains(REF_STABLE))
        );
    }

    #[test]
    fn sitemap_candidates_exclude_app_and_service_sources() {
        for source_kind in [SourceKind::Apps, SourceKind::Services] {
            let tempdir = tempdir().unwrap();
            let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
            let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
            publish_documents_with_manifest_targets(
                &index_dir,
                time::OffsetDateTime::now_utc(),
                vec![option_doc_for(
                    &context,
                    "programs.git.enable",
                    "Git option.",
                )],
                vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
            );

            let mut config = app_config_with_public_url(&index_dir);
            config
                .sources
                .get_mut(SOURCE_FIXTURES)
                .expect("fixture source exists")
                .kind = source_kind;
            let service = SearchService::open_current(Arc::new(config)).unwrap();
            let snapshot = service.snapshot();

            assert_eq!(
                service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
                Ok(false)
            );
            assert!(service.sitemap_candidates(&snapshot).unwrap().is_empty());
        }
    }

    #[test]
    fn unknown_source_returns_typed_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
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

        let config = Arc::new(app_config_with_public_url(&index_dir));
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

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
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
    fn search_scope_ref_without_source_rejects_even_with_ref_set() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(None, Some(REF_SMALL), Some("single"))
            .unwrap_err();

        assert!(matches!(error, RequestResolutionError::RefRequiresSource));
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

        let config = Arc::new(multi_ref_app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        let scopes = service
            .search_scopes(Some(SOURCE_FIXTURES), Some(REF_STABLE), None)
            .unwrap();

        assert_eq!(scopes.len(), 1);
        assert_eq!(scopes[0].source, SOURCE_FIXTURES);
        assert_eq!(scopes[0].ref_id, REF_STABLE);
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_STABLE),
            Ok(false)
        );
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
    fn source_ref_set_selected_non_searchable_ref_returns_unserved_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_flake_info_only_index(&index_dir, &[REF_SMALL]);

        let config = Arc::new(multi_ref_flake_info_only_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let error = service
            .search_scopes(Some(SOURCE_FIXTURES), None, Some("single"))
            .unwrap_err();

        assert!(matches!(
            error,
            RequestResolutionError::UnservedRef { source_id, ref_id }
                if source_id == SOURCE_FIXTURES && ref_id == REF_SMALL
        ));
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

        let config = Arc::new(app_config_with_public_url(&index_dir));
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
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceError::Resolution(RequestResolutionError::UnservedRef { source_id, ref_id })
                if source_id == SOURCE_FIXTURES && ref_id == REF_STABLE
        ));
    }

    #[test]
    fn from_leased_generation_uses_supplied_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::from_leased_generation(
            config,
            leased_generation(&index_dir, path.clone(), manifest),
        )
        .unwrap();

        assert_eq!(service.snapshot().path(), path);
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

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert_eq!(snapshot.path(), path);
        assert_eq!(snapshot.manifest().document_count, 7);
        assert_eq!(
            snapshot.manifest().generation_id,
            canonical_generation_id(snapshot.manifest()).unwrap()
        );
        assert_ne!(snapshot.manifest().generation_id, snapshot.path().as_str());
        assert!(Arc::ptr_eq(&snapshot.index, &service.current_index()));
    }

    #[test]
    fn lease_published_generation_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let mut manifest = store.read_manifest(&path).unwrap();
        manifest.generation_id = "sha256:wrong".to_owned();

        let error = store
            .lease_published_generation(PublishedGeneration { path, manifest })
            .unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn reconcile_current_generation_keeps_loaded_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();

        let report = service.reconcile_current_generation().unwrap();

        assert_eq!(report.outcome(), ReconcileOutcome::Unchanged);
        let ReconcileReport::Unchanged { generation } = report else {
            panic!("expected unchanged reconcile report");
        };
        assert_eq!(generation.path, path);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
    }

    #[test]
    fn from_leased_generation_rejects_missing_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        fs::remove_file(SeoFactsArtifact::path(&path)).unwrap();

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::from_leased_generation(
            config,
            leased_generation(&index_dir, path.clone(), manifest.clone()),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
    }

    #[test]
    fn reconcile_current_generation_does_not_revalidate_unchanged_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::from_leased_generation(
            config,
            leased_generation(&index_dir, path.clone(), manifest.clone()),
        )
        .unwrap();

        let before = service.current_index();
        fs::remove_file(SeoFactsArtifact::path(&path)).unwrap();

        let report = service.reconcile_current_generation().unwrap();

        assert_eq!(report.outcome(), ReconcileOutcome::Unchanged);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
        assert_eq!(
            service.snapshot().manifest().generation_id,
            manifest.generation_id
        );
        assert!(service.sitemap_candidates(&service.snapshot()).is_ok());
    }

    #[test]
    fn reconcile_current_generation_rejects_new_current_with_missing_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        fs::remove_file(SeoFactsArtifact::path(&next_path)).unwrap();

        let error = service.reconcile_current_generation().unwrap_err();
        let snapshot = service.snapshot();

        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
        assert_ne!(snapshot.path(), next_path);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
        assert!(service.sitemap_candidates(&snapshot).is_ok());
    }

    #[test]
    fn reconcile_current_generation_rejects_new_current_with_invalid_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let service = SearchService::open_current(Arc::clone(&config)).unwrap();
        let before = service.current_index();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();
        let next_generation = PublishedGeneration {
            path: next_path,
            manifest: next_manifest,
        };
        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&next_generation)
            .unwrap()
            .into_sidecar();
        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &next_generation, &sidecar);

        let error = service.reconcile_current_generation().unwrap_err();
        let snapshot = service.snapshot();

        assert!(format!("{error:#}").contains("SEO sidecar integrity metadata does not match"));
        assert_ne!(snapshot.path(), next_generation.path);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
        assert!(service.sitemap_candidates(&snapshot).is_ok());

        let leased = leased_generation(
            &index_dir,
            next_generation.path.clone(),
            next_generation.manifest.clone(),
        );
        let error = SearchService::verify_leased_generation_seo(&config, &leased).unwrap_err();

        assert!(format!("{error:#}").contains("SEO sidecar integrity metadata does not match"));
    }

    #[test]
    fn stale_reload_does_not_restore_old_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let old_snapshot = service.snapshot();
        let observed_old = old_snapshot.to_published_generation();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let store = IndexStore::new(&index_dir);
        let next_manifest = store.read_manifest(&next_path).unwrap();

        service.reconcile_current_generation().unwrap();

        let stale = store
            .lease_published_generation(PublishedGeneration {
                path: old_path,
                manifest: old_snapshot.manifest().clone(),
            })
            .unwrap();

        let outcome = service
            .reload_generation(&store, stale, observed_old)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Superseded);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest(), &next_manifest);
    }

    #[test]
    fn stale_published_generation_candidate_is_superseded() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let old_manifest = store.read_manifest(&old_path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();

        service.reconcile_current_generation().unwrap();
        let observed_current = service.snapshot().to_published_generation();

        let stale = store
            .lease_published_generation(PublishedGeneration {
                path: old_path,
                manifest: old_manifest,
            })
            .unwrap();

        let outcome = service
            .reload_generation(&store, stale, observed_current)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Superseded);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest(), &next_manifest);
    }

    #[test]
    fn stale_candidate_with_missing_sidecar_is_superseded() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let old_manifest = store.read_manifest(&old_path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();
        service.reconcile_current_generation().unwrap();
        let observed_current = service.snapshot().to_published_generation();

        fs::remove_file(SeoFactsArtifact::path(&old_path)).unwrap();
        let stale = store
            .lease_published_generation(PublishedGeneration {
                path: old_path,
                manifest: old_manifest,
            })
            .unwrap();

        let outcome = service
            .reload_generation(&store, stale, observed_current)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Superseded);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest(), &next_manifest);
    }

    #[test]
    fn stale_candidate_with_invalid_sidecar_is_superseded() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let old_manifest = store.read_manifest(&old_path).unwrap();
        let old_generation = PublishedGeneration {
            path: old_path.clone(),
            manifest: old_manifest.clone(),
        };

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();
        service.reconcile_current_generation().unwrap();
        let observed_current = service.snapshot().to_published_generation();

        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&old_generation)
            .unwrap()
            .into_sidecar();
        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &old_generation, &sidecar);
        let stale = store
            .lease_published_generation(PublishedGeneration {
                path: old_path,
                manifest: old_manifest,
            })
            .unwrap();

        let outcome = service
            .reload_generation(&store, stale, observed_current)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Superseded);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest(), &next_manifest);
    }

    #[test]
    fn stale_loaded_generation_candidate_is_superseded() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let old_manifest = store.read_manifest(&old_path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();

        service.reconcile_current_generation().unwrap();
        let observed_current = service.snapshot().to_published_generation();

        let stale = store
            .lease_published_generation(PublishedGeneration {
                path: old_path,
                manifest: old_manifest,
            })
            .unwrap();

        let outcome = service
            .reload_generation(&store, stale, observed_current)
            .unwrap();

        assert_eq!(outcome, ReconcileOutcome::Superseded);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest(), &next_manifest);
    }

    #[test]
    fn lease_published_generation_rejects_mismatched_generation_id_before_reconcile() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let mut manifest = store.read_manifest(&path).unwrap();
        manifest.generation_id = "sha256:wrong".to_owned();

        let error = store
            .lease_published_generation(PublishedGeneration { path, manifest })
            .unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn reconcile_current_generation_loads_new_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);

        let report = service.reconcile_current_generation().unwrap();

        assert_eq!(report.outcome(), ReconcileOutcome::Reloaded);
        let ReconcileReport::Reloaded { generation } = report else {
            panic!("expected reloaded reconcile report");
        };
        assert_eq!(generation.path, next_path);
        assert_eq!(service.snapshot().path(), next_path);
        assert_eq!(service.snapshot().manifest().generated_at, next_time);
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

        let report = service.reconcile_current_generation().unwrap();

        let ReconcileReport::Reloaded { generation } = report else {
            panic!("expected reloaded reconcile report");
        };
        assert_eq!(generation.path, next_path);

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

        assert_eq!(
            held.manifest().generated_at,
            time::OffsetDateTime::UNIX_EPOCH
        );
        assert!(result.total > 0);
    }

    #[test]
    fn served_generation_holds_generation_lease() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        let store = IndexStore::new(&index_dir);
        assert!(
            store
                .try_acquire_exclusive_generation_lease(&path)
                .unwrap()
                .is_none()
        );

        drop(service);

        assert!(
            store
                .try_acquire_exclusive_generation_lease(&path)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn snapshot_holds_old_generation_lease_after_reload() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let old_path =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let new_path = publish_canonical_index_with_generated_at(&index_dir, next_time);

        let report = service.reconcile_current_generation().unwrap();
        assert_eq!(report.outcome(), ReconcileOutcome::Reloaded);

        let store = IndexStore::new(&index_dir);
        assert!(
            store
                .try_acquire_exclusive_generation_lease(&old_path)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .try_acquire_exclusive_generation_lease(&new_path)
                .unwrap()
                .is_none()
        );

        drop(snapshot);

        assert!(
            store
                .try_acquire_exclusive_generation_lease(&old_path)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .try_acquire_exclusive_generation_lease(&new_path)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconcile_current_generation_preserves_served_generation_when_current_is_unopenable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();
        let store = IndexStore::new(&index_dir);
        let broken = store.create_generation_path().unwrap();
        let manifest = service.snapshot().manifest().clone();

        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();

        let error = service.reconcile_current_generation().unwrap_err();

        assert!(format!("{error:#}").contains("failed to load published index generation"));
        assert_eq!(service.snapshot().path(), path);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
    }
}
