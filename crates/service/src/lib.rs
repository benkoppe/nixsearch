use std::fmt;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use camino::Utf8Path;

use nixsearch_config::app::AppConfig;
use nixsearch_config::source::{SourceConfig, SourceKind};
use nixsearch_core::document::DocumentKind;
use nixsearch_index::manifest::{IndexGenerationManifest, validate_generation_id};
use nixsearch_index::search::{
    EntryFacts, EntryLookup, EntryLookupResult, SearchIndex, SearchOptions, SearchResult,
    SearchScope,
};
use nixsearch_index::seo::{SeoEntryFacts, SeoSidecar};
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
struct ServedGeneration {
    generation: LeasedPublishedGeneration,
    index: Arc<SearchIndex>,
    seo_facts: SeoFactsState,
}

#[derive(Clone)]
enum SeoFactsState {
    Verified(Arc<SeoSidecar>),
    Unverified(Arc<SeoSidecar>),
    Verifying(Arc<SeoSidecar>),
    Unavailable,
}

impl SeoFactsState {
    fn verified_facts(&self) -> SeoFactsResult<&SeoSidecar> {
        match self {
            Self::Verified(facts) => Ok(facts),
            Self::Unverified(_) | Self::Verifying(_) | Self::Unavailable => {
                Err(SeoFactsUnavailable)
            }
        }
    }
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
    seo_facts: SeoFactsState,
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
    pub kind: Option<DocumentKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeoFactsVerificationReport {
    AlreadyVerified {
        generation: PublishedGeneration,
    },
    AlreadyUnavailable {
        generation: PublishedGeneration,
    },
    AlreadyVerifying {
        generation: PublishedGeneration,
    },
    Verified {
        generation: PublishedGeneration,
    },
    Invalid {
        generation: PublishedGeneration,
        error: String,
    },
    Superseded,
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
    pub kind: Option<DocumentKind>,
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
        let current = load_servable_generation(&config, generation)?;

        Ok(Self {
            config,
            current: Arc::new(RwLock::new(current)),
        })
    }

    pub fn validate_leased_generation(
        _config: &AppConfig,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        validate_generation_id(generation.manifest())
            .context("failed to validate supplied index generation manifest")?;
        SearchIndex::open(generation.path())
            .with_context(|| format!("failed to open search index {}", generation.path()))?;

        Ok(())
    }

    pub fn validate_leased_generation_seo_facts(
        config: &AppConfig,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        let index_store = IndexStore::new(&config.data.index_dir);
        let index = SearchIndex::open(generation.path())
            .with_context(|| format!("failed to open search index {}", generation.path()))?;
        let sidecar = index_store.read_seo_sidecar(generation.published_generation())?;

        sidecar
            .validate_for_index(generation.manifest(), &index)
            .context("failed to validate SEO sidecar against index")
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

    pub fn verify_current_seo_facts(&self) -> SeoFactsVerificationReport {
        let (generation, index, facts) = {
            let mut current = self
                .current
                .write()
                .expect("served generation lock poisoned");

            match &current.seo_facts {
                SeoFactsState::Verified(_) => {
                    return SeoFactsVerificationReport::AlreadyVerified {
                        generation: current.to_published_generation(),
                    };
                }
                SeoFactsState::Verifying(_) => {
                    return SeoFactsVerificationReport::AlreadyVerifying {
                        generation: current.to_published_generation(),
                    };
                }
                SeoFactsState::Unavailable => {
                    return SeoFactsVerificationReport::AlreadyUnavailable {
                        generation: current.to_published_generation(),
                    };
                }
                SeoFactsState::Unverified(facts) => {
                    let generation = current.generation.clone();
                    let index = Arc::clone(&current.index);
                    let facts = Arc::clone(facts);
                    current.seo_facts = SeoFactsState::Verifying(Arc::clone(&facts));

                    (generation, index, facts)
                }
            }
        };

        let published = generation.to_published_generation();
        let verification = facts.validate_for_index(generation.manifest(), &index);

        let mut current = self
            .current
            .write()
            .expect("served generation lock poisoned");

        if !current.matches(&published) {
            return SeoFactsVerificationReport::Superseded;
        }

        let SeoFactsState::Verifying(current_facts) = &current.seo_facts else {
            return match &current.seo_facts {
                SeoFactsState::Verified(_) => SeoFactsVerificationReport::AlreadyVerified {
                    generation: current.to_published_generation(),
                },
                SeoFactsState::Verifying(_) => unreachable!(),
                SeoFactsState::Unavailable => SeoFactsVerificationReport::AlreadyUnavailable {
                    generation: current.to_published_generation(),
                },
                SeoFactsState::Unverified(_) => SeoFactsVerificationReport::Superseded,
            };
        };

        if !Arc::ptr_eq(current_facts, &facts) {
            return SeoFactsVerificationReport::Superseded;
        }

        match verification {
            Ok(()) => {
                current.seo_facts = SeoFactsState::Verified(facts);
                SeoFactsVerificationReport::Verified {
                    generation: published,
                }
            }
            Err(error) => {
                current.seo_facts = SeoFactsState::Unavailable;
                SeoFactsVerificationReport::Invalid {
                    generation: published,
                    error: format!("{error:#}"),
                }
            }
        }
    }

    pub fn current_seo_facts_can_start_verification(&self) -> bool {
        let current = self
            .current
            .read()
            .expect("served generation lock poisoned");

        matches!(current.seo_facts, SeoFactsState::Unverified(_))
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

        let loaded = load_servable_generation(&self.config, generation).with_context(|| {
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
            .manifest()
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
        self.source_has_indexable_entries(snapshot, source_id, ref_id)
            .unwrap_or(false)
    }

    pub fn is_indexable_entry_in_snapshot(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
        name: &str,
    ) -> bool {
        let Some(source) = self.config.sources.get(source_id) else {
            return false;
        };
        let Ok(seo_facts) = snapshot.seo_facts.verified_facts() else {
            return false;
        };

        self.ref_allowed_to_be_indexed(source, ref_id)
            && Self::served_ref_exists_in_snapshot(snapshot, source_id, ref_id)
            && seo_facts.ref_is_indexable(source_id, ref_id)
            && seo_facts.entry_is_indexable(source_id, ref_id, name)
    }

    pub fn source_has_indexable_entries(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> SeoFactsResult<bool> {
        let seo_facts = snapshot.seo_facts.verified_facts()?;

        if !self.source_ref_allowed_for_seo(snapshot, source_id, ref_id) {
            return Ok(false);
        }

        Ok(seo_facts.entries.iter().any(|entry| {
            entry.source == source_id
                && entry.ref_id == ref_id
                && !candidate_kinds_for_entry(entry).is_empty()
        }))
    }

    pub fn sitemap_candidates(
        &self,
        snapshot: &ServedGenerationSnapshot,
    ) -> SeoFactsResult<Vec<SitemapCandidate>> {
        let seo_facts = snapshot.seo_facts.verified_facts()?;
        let mut candidates = Vec::new();

        for entry in &seo_facts.entries {
            if !self.entry_can_contribute_to_sitemap(snapshot, entry) {
                continue;
            }

            for kind in candidate_kinds_for_entry(entry) {
                candidates.push(SitemapCandidate {
                    source: entry.source.clone(),
                    name: entry.name.clone(),
                    kind,
                });
            }
        }

        Ok(candidates)
    }

    fn ref_allowed_to_be_indexed(&self, source: &SourceConfig, ref_id: &str) -> bool {
        source.default_ref.as_deref() == Some(ref_id)
    }

    fn source_ref_allowed_for_seo(
        &self,
        snapshot: &ServedGenerationSnapshot,
        source_id: &str,
        ref_id: &str,
    ) -> bool {
        let Some(source) = self.config.sources.get(source_id) else {
            return false;
        };

        !matches!(source.kind, SourceKind::Apps | SourceKind::Services)
            && self.ref_allowed_to_be_indexed(source, ref_id)
            && Self::served_ref_exists_in_snapshot(snapshot, source_id, ref_id)
    }

    fn entry_can_contribute_to_sitemap(
        &self,
        snapshot: &ServedGenerationSnapshot,
        entry: &SeoEntryFacts,
    ) -> bool {
        self.source_ref_allowed_for_seo(snapshot, &entry.source, &entry.ref_id)
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

fn load_servable_generation(
    config: &AppConfig,
    generation: LeasedPublishedGeneration,
) -> Result<ServedGeneration> {
    let index_store = IndexStore::new(&config.data.index_dir);
    validate_generation_id(generation.manifest())
        .context("failed to validate supplied index generation manifest")?;
    let index = SearchIndex::open(generation.path())
        .with_context(|| format!("failed to open search index {}", generation.path()))?;
    let seo_facts = load_seo_facts(&index_store, &generation);

    Ok(ServedGeneration {
        generation,
        index: Arc::new(index),
        seo_facts,
    })
}

fn load_seo_facts(
    index_store: &IndexStore,
    generation: &LeasedPublishedGeneration,
) -> SeoFactsState {
    let published = generation.published_generation();
    let path = generation.path();
    let result = index_store.read_seo_sidecar(published);

    match result {
        Ok(sidecar) => SeoFactsState::Unverified(Arc::new(sidecar)),
        Err(error) => {
            tracing::warn!(
                generation = %path,
                "SEO facts are unavailable for served generation: {error:#}"
            );
            SeoFactsState::Unavailable
        }
    }
}

fn candidate_kinds_for_entry(entry: &SeoEntryFacts) -> Vec<Option<DocumentKind>> {
    let eligible_count = entry.package_eligible_count + entry.option_eligible_count;

    if entry.total_supported_indexed_count == 1 && eligible_count == 1 {
        return vec![None];
    }

    if entry.total_supported_indexed_count <= 1 {
        return Vec::new();
    }

    let mut kinds = Vec::new();

    if entry.package_supported_count == 1 && entry.package_eligible_count == 1 {
        kinds.push(Some(DocumentKind::Package));
    }

    if entry.option_supported_count == 1 && entry.option_eligible_count == 1 {
        kinds.push(Some(DocumentKind::Option));
    }

    kinds
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
    use std::{fs, sync::Arc};

    use tempfile::tempdir;

    use nixsearch_config::source::SourceKind;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::{DocumentKind, SearchDocument};
    use nixsearch_index::manifest::canonical_generation_id;
    use nixsearch_index::search::{EntryFactsStatus, EntryLookupResult};
    use nixsearch_index::store::{IndexStore, LeasedPublishedGeneration, PublishedGeneration};
    use nixsearch_index_test_support::{
        index_target, options_target, publish_canonical_index,
        publish_canonical_index_with_generated_at, publish_documents_with_manifest_targets,
        publish_fixture_options_index_for_refs, write_raw_seo_sidecar,
    };
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, app_config, app_config_with_extra_fixture_source,
        ingest_context_for, multi_ref_app_config, option_doc_for, package_doc_for, utf8_path_buf,
    };
    use time::Duration as TimeDuration;

    use super::{
        EntryRequest, ReconcileOutcome, ReconcileReport, RequestResolutionError, SearchRequest,
        SearchService, SeoFactsVerificationReport, ServiceError,
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
    ) -> Vec<(String, String, Option<DocumentKind>)> {
        service
            .sitemap_candidates(snapshot)
            .unwrap()
            .into_iter()
            .map(|candidate| (candidate.source, candidate.name, candidate.kind))
            .collect()
    }

    fn verify_current_seo_facts(service: &SearchService) {
        let report = service.verify_current_seo_facts();

        assert!(
            matches!(
                report,
                SeoFactsVerificationReport::Verified { .. }
                    | SeoFactsVerificationReport::AlreadyVerified { .. }
            ),
            "expected verified SEO facts, got {report:?}"
        );
    }

    #[test]
    fn search_current_uses_configured_default_scopes() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(app_config(&index_dir));
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

        let config = Arc::new(app_config(&index_dir));
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
        verify_current_seo_facts(&service);

        assert!(service.is_indexable_ref(SOURCE_FIXTURES, REF_SMALL));
    }

    #[test]
    fn loaded_sidecar_facts_are_unavailable_until_verified() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(service.current_seo_facts_can_start_verification());
        assert!(service.sitemap_candidates(&snapshot).is_err());
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Err(super::SeoFactsUnavailable)
        );
        assert!(!service.is_indexable_ref_in_snapshot(&snapshot, SOURCE_FIXTURES, REF_SMALL));

        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert!(service.sitemap_candidates(&snapshot).is_ok());
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(true)
        );
        assert!(!service.current_seo_facts_can_start_verification());

        let report = service.verify_current_seo_facts();
        assert!(matches!(
            report,
            SeoFactsVerificationReport::AlreadyVerified { .. }
        ));
    }

    #[test]
    fn claimed_sidecar_verification_is_not_started_twice() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();

        assert!(service.current_seo_facts_can_start_verification());
        {
            let mut current = service
                .current
                .write()
                .expect("served generation lock poisoned");
            let super::SeoFactsState::Unverified(facts) = &current.seo_facts else {
                panic!("expected unverified SEO facts");
            };
            let facts = Arc::clone(facts);

            current.seo_facts = super::SeoFactsState::Verifying(facts);
        }

        assert!(!service.current_seo_facts_can_start_verification());
        let report = service.verify_current_seo_facts();
        assert!(matches!(
            report,
            SeoFactsVerificationReport::AlreadyVerifying { .. }
        ));
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

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);

        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_SMALL));
        assert!(!service.is_indexable_ref(SOURCE_FIXTURES, REF_SMALL));
    }

    #[test]
    fn indexable_entry_requires_loaded_sidecar_entry_facts() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert!(service.is_indexable_entry_in_snapshot(
            &snapshot,
            SOURCE_FIXTURES,
            REF_SMALL,
            "programs.git.enable"
        ));
        assert!(!service.is_indexable_entry_in_snapshot(
            &snapshot,
            SOURCE_FIXTURES,
            REF_SMALL,
            "missing"
        ));
    }

    #[test]
    fn open_current_does_not_use_manifest_valid_sidecar_until_deep_validation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();
        let generation = PublishedGeneration {
            path,
            manifest: manifest.clone(),
        };
        let mut sidecar = store.read_seo_sidecar(&generation).unwrap();

        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &generation, &sidecar);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(Arc::clone(&config)).unwrap();
        let snapshot = service.snapshot();

        assert!(service.sitemap_candidates(&snapshot).is_err());
        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Err(super::SeoFactsUnavailable)
        );
        assert!(!service.is_indexable_ref_in_snapshot(&snapshot, SOURCE_FIXTURES, REF_SMALL));

        let report = service.verify_current_seo_facts();
        assert!(matches!(report, SeoFactsVerificationReport::Invalid { .. }));
        assert!(service.sitemap_candidates(&service.snapshot()).is_err());
        assert!(!service.current_seo_facts_can_start_verification());

        let report = service.verify_current_seo_facts();
        assert!(matches!(
            report,
            SeoFactsVerificationReport::AlreadyUnavailable { .. }
        ));

        let leased = leased_generation(&index_dir, generation.path, manifest);
        let error =
            SearchService::validate_leased_generation_seo_facts(&config, &leased).unwrap_err();

        assert!(format!("{error:#}").contains("entry facts do not match indexed documents"));
    }

    #[test]
    fn non_default_served_ref_is_not_indexable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);

        assert!(service.served_ref_exists(SOURCE_FIXTURES, REF_STABLE));
        assert!(!service.is_indexable_ref(SOURCE_FIXTURES, REF_STABLE));
    }

    #[test]
    fn source_has_indexable_entries_requires_loaded_sidecar_facts() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        fs::remove_file(store.seo_sidecar_path(&path)).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let snapshot = service.snapshot();

        assert!(
            service
                .source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL)
                .is_err()
        );
        assert!(service.sitemap_candidates(&snapshot).is_err());
        assert!(!service.is_indexable_ref_in_snapshot(&snapshot, SOURCE_FIXTURES, REF_SMALL));
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

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_SMALL),
            Ok(true)
        );
        assert_eq!(
            candidate_tuples(&service, &snapshot),
            vec![(
                SOURCE_FIXTURES.to_owned(),
                "programs.git.enable".to_owned(),
                None,
            )]
        );
    }

    #[test]
    fn sitemap_candidates_include_kind_for_cross_kind_ambiguous_entries() {
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

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert_eq!(
            candidate_tuples(&service, &snapshot),
            vec![
                (
                    SOURCE_FIXTURES.to_owned(),
                    "git".to_owned(),
                    Some(DocumentKind::Package),
                ),
                (
                    SOURCE_FIXTURES.to_owned(),
                    "git".to_owned(),
                    Some(DocumentKind::Option),
                ),
            ]
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

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
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

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert!(service.sitemap_candidates(&snapshot).unwrap().is_empty());
    }

    #[test]
    fn sitemap_candidates_exclude_non_default_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let config = Arc::new(multi_ref_app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        verify_current_seo_facts(&service);
        let snapshot = service.snapshot();

        assert_eq!(
            service.source_has_indexable_entries(&snapshot, SOURCE_FIXTURES, REF_STABLE),
            Ok(false)
        );
        assert_eq!(candidate_tuples(&service, &snapshot).len(), 1);
        assert!(
            candidate_tuples(&service, &snapshot)
                .iter()
                .all(|(_, name, _)| !name.contains(REF_STABLE))
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

            let mut config = app_config(&index_dir);
            config
                .sources
                .get_mut(SOURCE_FIXTURES)
                .expect("fixture source exists")
                .kind = source_kind;
            let service = SearchService::open_current(Arc::new(config)).unwrap();
            verify_current_seo_facts(&service);
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
        verify_current_seo_facts(&service);

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
        verify_current_seo_facts(&service);

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
    fn from_leased_generation_uses_supplied_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        let config = Arc::new(app_config(&index_dir));
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

        let config = Arc::new(app_config(&index_dir));
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
    fn from_leased_generation_marks_missing_sidecar_unavailable() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        fs::remove_file(store.seo_sidecar_path(&path)).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::from_leased_generation(
            config,
            leased_generation(&index_dir, path.clone(), manifest.clone()),
        )
        .unwrap();
        let snapshot = service.snapshot();

        assert_eq!(snapshot.path(), path);
        assert!(service.sitemap_candidates(&snapshot).is_err());
    }

    #[test]
    fn reconcile_current_generation_does_not_revalidate_unchanged_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.read_manifest(&path).unwrap();

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::from_leased_generation(
            config,
            leased_generation(&index_dir, path.clone(), manifest.clone()),
        )
        .unwrap();

        let before = service.current_index();
        fs::remove_file(store.seo_sidecar_path(&path)).unwrap();

        let report = service.reconcile_current_generation().unwrap();

        assert_eq!(report.outcome(), ReconcileOutcome::Unchanged);
        assert!(Arc::ptr_eq(&before, &service.current_index()));
        assert_eq!(
            service.snapshot().manifest().generation_id,
            manifest.generation_id
        );
        assert!(service.sitemap_candidates(&service.snapshot()).is_err());
    }

    #[test]
    fn reconcile_current_generation_loads_new_current_with_missing_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(config).unwrap();
        let before = service.current_index();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        fs::remove_file(store.seo_sidecar_path(&next_path)).unwrap();

        let report = service.reconcile_current_generation().unwrap();
        let snapshot = service.snapshot();

        assert_eq!(report.outcome(), ReconcileOutcome::Reloaded);
        assert_eq!(snapshot.path(), next_path);
        assert!(!Arc::ptr_eq(&before, &service.current_index()));
        assert!(service.sitemap_candidates(&snapshot).is_err());
    }

    #[test]
    fn reconcile_current_generation_loads_new_current_with_invalid_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);

        let config = Arc::new(app_config(&index_dir));
        let service = SearchService::open_current(Arc::clone(&config)).unwrap();
        let before = service.current_index();

        let next_time = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1);
        let next_path = publish_canonical_index_with_generated_at(&index_dir, next_time);
        let next_manifest = store.read_manifest(&next_path).unwrap();
        let next_generation = PublishedGeneration {
            path: next_path,
            manifest: next_manifest,
        };
        let mut sidecar = store.read_seo_sidecar(&next_generation).unwrap();
        sidecar.entries[0].name = "not-real".to_owned();
        write_raw_seo_sidecar(&store, &next_generation, &sidecar);

        let report = service.reconcile_current_generation().unwrap();
        let snapshot = service.snapshot();

        assert_eq!(report.outcome(), ReconcileOutcome::Reloaded);
        assert_eq!(snapshot.path(), next_generation.path);
        assert!(!Arc::ptr_eq(&before, &service.current_index()));
        assert!(service.sitemap_candidates(&snapshot).is_err());

        let report = service.verify_current_seo_facts();
        assert!(matches!(report, SeoFactsVerificationReport::Invalid { .. }));
        assert!(service.sitemap_candidates(&service.snapshot()).is_err());

        let leased = leased_generation(
            &index_dir,
            next_generation.path.clone(),
            next_generation.manifest.clone(),
        );
        let error =
            SearchService::validate_leased_generation_seo_facts(&config, &leased).unwrap_err();

        assert!(format!("{error:#}").contains("entry facts do not match indexed documents"));
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

        fs::remove_file(store.seo_sidecar_path(&old_path)).unwrap();
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

        let mut sidecar = store.read_seo_sidecar(&old_generation).unwrap();
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
