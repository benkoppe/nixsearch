use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use camino::Utf8PathBuf;

use nixsearch_config::app::AppConfig;
use nixsearch_config::producer::ProducerConfig;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::search::SearchIndex;
use nixsearch_index::store::IndexStore;
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_source::error::NixCommandFailure;
use nixsearch_store::{ArtifactStore, StoreError};

use crate::consume::consume_target;
use crate::produce::{artifact_store_from_config, produce_target, produced_from_existing_artifact};
use crate::targets::{TargetKey, TargetRef, all_targets, default_search_target_keys};

#[derive(Debug, Clone)]
pub struct GenerationBuildResult {
    pub path: Utf8PathBuf,
    pub successful_targets: Vec<TargetKey>,
    pub skipped_targets: Vec<TargetKey>,
    pub failed_refresh_targets: Vec<TargetKey>,
}

impl GenerationBuildResult {
    pub fn is_degraded(&self) -> bool {
        !self.skipped_targets.is_empty() || !self.failed_refresh_targets.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationFailurePolicy {
    Strict,
    TolerateBootstrapNixFailures,
}

#[derive(Debug)]
enum TargetProduceOutcome {
    Refreshed(ProducedArtifact),
    Retained(ProducedArtifact),
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProduceFailureDisposition {
    Fatal,
    TolerableSkip,
}

#[async_trait]
trait TargetProducer: Send + Sync {
    async fn produce(&self, store: &ArtifactStore, target: &TargetRef) -> Result<ProducedArtifact>;
}

struct RealTargetProducer;

#[async_trait]
impl TargetProducer for RealTargetProducer {
    async fn produce(&self, store: &ArtifactStore, target: &TargetRef) -> Result<ProducedArtifact> {
        produce_target(store, target).await
    }
}

pub async fn build_and_publish_generation(
    index_store: &IndexStore,
    artifact_store: &ArtifactStore,
    targets: Vec<TargetRef>,
    refresh_keys: &BTreeSet<TargetKey>,
) -> Result<Utf8PathBuf> {
    let result = build_and_publish_generation_with_policy(
        index_store,
        artifact_store,
        targets,
        refresh_keys,
        GenerationFailurePolicy::Strict,
        None,
        &RealTargetProducer,
    )
    .await?;

    Ok(result.path)
}

async fn build_and_publish_generation_with_policy(
    index_store: &IndexStore,
    artifact_store: &ArtifactStore,
    targets: Vec<TargetRef>,
    refresh_keys: &BTreeSet<TargetKey>,
    policy: GenerationFailurePolicy,
    required_success_targets: Option<&BTreeSet<TargetKey>>,
    producer: &(dyn TargetProducer + Send + Sync),
) -> Result<GenerationBuildResult> {
    if policy == GenerationFailurePolicy::TolerateBootstrapNixFailures {
        let target_keys = targets.iter().map(TargetKey::from).collect::<BTreeSet<_>>();

        if &target_keys != refresh_keys {
            bail!("tolerant bootstrap generation must refresh every target");
        }
    }

    let generation_path = index_store.create_generation_path()?;

    let index = SearchIndex::create_or_replace(&generation_path)?;
    let mut writer = index.writer()?;

    let mut total_documents = 0usize;
    let mut manifest_targets = Vec::new();
    let mut successful_targets = Vec::new();
    let mut skipped_targets = Vec::new();
    let mut failed_refresh_targets = Vec::new();

    for target in targets {
        let key = TargetKey::from(&target);

        let outcome = produce_target_with_policy(
            artifact_store,
            &target,
            refresh_keys,
            policy,
            producer,
            &mut failed_refresh_targets,
            &mut skipped_targets,
        )
        .await?;

        let (produced, status) = match outcome {
            TargetProduceOutcome::Refreshed(produced) => (produced, "refreshed"),
            TargetProduceOutcome::Retained(produced) => (produced, "retained"),
            TargetProduceOutcome::Skipped => continue,
        };

        let documents = consume_target(artifact_store, &target, &produced).await?;

        for document in &documents {
            writer.add_document(document)?;
        }

        total_documents += documents.len();
        successful_targets.push(key);

        manifest_targets.push(IndexTargetManifest {
            source: target.source_id.clone(),
            ref_id: target.ref_config.id.clone(),
            artifact_kind: produced.artifact_ref.kind,
            document_count: documents.len(),
            artifact_hash: Some(produced.metadata.content_hash.clone()),
            revision: produced.metadata.revision.clone(),
        });

        tracing::info!(
            "{} {} documents: {}/{}",
            status,
            documents.len(),
            target.source_id,
            target.ref_config.id
        );
    }

    if policy == GenerationFailurePolicy::TolerateBootstrapNixFailures {
        if successful_targets.is_empty() {
            bail!("bootstrap generation produced no targets; all configured targets failed");
        }

        if let Some(required_success_targets) = required_success_targets
            && (required_success_targets.is_empty()
                || !successful_targets
                    .iter()
                    .any(|target| required_success_targets.contains(target)))
        {
            bail!("bootstrap generation produced no default search targets");
        }
    }

    writer.commit()?;

    let manifest = IndexGenerationManifest::new(total_documents, manifest_targets);
    index_store.write_manifest(&generation_path, &manifest)?;
    index_store.publish(&generation_path)?;

    tracing::info!(
        generation = %generation_path.as_str(),
        documents = total_documents,
        "published index generation"
    );

    Ok(GenerationBuildResult {
        path: generation_path,
        successful_targets,
        skipped_targets,
        failed_refresh_targets,
    })
}

async fn produce_target_with_policy(
    artifact_store: &ArtifactStore,
    target: &TargetRef,
    refresh_keys: &BTreeSet<TargetKey>,
    policy: GenerationFailurePolicy,
    producer: &(dyn TargetProducer + Send + Sync),
    failed_refresh_targets: &mut Vec<TargetKey>,
    skipped_targets: &mut Vec<TargetKey>,
) -> Result<TargetProduceOutcome> {
    let key = TargetKey::from(target);

    if refresh_keys.contains(&key) {
        return match producer.produce(artifact_store, target).await {
            Ok(produced) => Ok(TargetProduceOutcome::Refreshed(produced)),
            Err(error) => match policy {
                GenerationFailurePolicy::Strict => Err(error),
                GenerationFailurePolicy::TolerateBootstrapNixFailures => {
                    match classify_bootstrap_produce_error(target, &error) {
                        ProduceFailureDisposition::Fatal => Err(error),
                        ProduceFailureDisposition::TolerableSkip => {
                            tracing::warn!(
                                source = %target.source_id,
                                ref_id = %target.ref_config.id,
                                "skipping target after bootstrap production failure: {error:#}"
                            );

                            failed_refresh_targets.push(key.clone());
                            skipped_targets.push(key);

                            Ok(TargetProduceOutcome::Skipped)
                        }
                    }
                }
            },
        };
    }

    let produced = produced_from_existing_artifact(artifact_store, target).await?;
    Ok(TargetProduceOutcome::Retained(produced))
}

fn classify_bootstrap_produce_error(
    target: &TargetRef,
    error: &anyhow::Error,
) -> ProduceFailureDisposition {
    if error.chain().any(|cause| cause.is::<StoreError>()) {
        return ProduceFailureDisposition::Fatal;
    }

    if error.chain().any(|cause| cause.is::<std::io::Error>()) {
        return ProduceFailureDisposition::Fatal;
    }

    let Some(source_ref) = tolerable_nix_source_ref(target) else {
        return ProduceFailureDisposition::Fatal;
    };

    if !is_remote_nix_ref(source_ref) {
        return ProduceFailureDisposition::Fatal;
    }

    let Some(failure) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<NixCommandFailure>())
    else {
        return ProduceFailureDisposition::Fatal;
    };

    if failure.status.code().is_some() {
        ProduceFailureDisposition::TolerableSkip
    } else {
        ProduceFailureDisposition::Fatal
    }
}

fn tolerable_nix_source_ref(target: &TargetRef) -> Option<&str> {
    match &target.ref_config.producer {
        ProducerConfig::FlakeFile { source_ref, .. }
        | ProducerConfig::NixBuildOptionsJson { source_ref, .. } => Some(source_ref),
        _ => None,
    }
}

fn is_remote_nix_ref(source_ref: &str) -> bool {
    if source_ref.is_empty()
        || source_ref.starts_with('/')
        || source_ref.starts_with("./")
        || source_ref.starts_with("../")
        || source_ref.starts_with("path:")
        || source_ref.starts_with("file:")
        || source_ref.starts_with("git+file:")
    {
        return false;
    }

    source_ref.starts_with("github:")
        || source_ref.starts_with("gitlab:")
        || source_ref.starts_with("sourcehut:")
        || source_ref.starts_with("git+https://")
        || source_ref.starts_with("https://")
}

/// Bootstrap all configured sources, tolerating only remote Nix command failures
/// that are safe to omit from the initial index.
pub async fn bootstrap_all_tolerant(config: &AppConfig) -> Result<GenerationBuildResult> {
    let store = artifact_store_from_config(config)?;
    let index_store = IndexStore::new(&config.data.index_dir);
    let targets = all_targets(config);

    if targets.is_empty() {
        bail!("no sources configured to bootstrap");
    }

    let refresh_keys: BTreeSet<TargetKey> = targets.iter().map(TargetKey::from).collect();
    let required_success_targets = default_search_target_keys(config)?;

    build_and_publish_generation_with_policy(
        &index_store,
        &store,
        targets,
        &refresh_keys,
        GenerationFailurePolicy::TolerateBootstrapNixFailures,
        Some(&required_success_targets),
        &RealTargetProducer,
    )
    .await
}

/// Regenerate all configured sources. Returns the new generation path.
pub async fn regenerate_all(config: &AppConfig) -> Result<Utf8PathBuf> {
    let store = artifact_store_from_config(config)?;
    let index_store = IndexStore::new(&config.data.index_dir);
    let targets = all_targets(config);

    if targets.is_empty() {
        bail!("no sources configured to regenerate");
    }

    let refresh_keys: BTreeSet<TargetKey> = targets.iter().map(TargetKey::from).collect();
    build_and_publish_generation(&index_store, &store, targets, &refresh_keys).await
}
