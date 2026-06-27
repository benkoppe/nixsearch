use std::collections::BTreeSet;

use anyhow::{Result, bail};
use async_trait::async_trait;
use camino::Utf8PathBuf;

use nixsearch_config::app::AppConfig;
use nixsearch_config::producer::ProducerConfig;
use nixsearch_index::annotation::EntryAnnotationIndex;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::search::SearchIndex;
use nixsearch_index::seo_sidecar::SeoFactsArtifact;
use nixsearch_index::store::{IndexStore, PublishedGeneration};
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_source::error::NixCommandFailure;
use nixsearch_store::{ArtifactStore, StoreError};

use crate::consume::consume_target;
use crate::produce::{artifact_store_from_config, produce_target, produced_from_existing_artifact};
use crate::spool::DocumentSpool;
use crate::targets::{
    TargetKey, TargetRef, all_targets, default_indexed_search_target_keys,
    latest_artifact_ref_for_target,
};

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
    let mut publish_started = false;

    let result: Result<GenerationBuildResult> = async {
        let spool = DocumentSpool::create()?;
        let mut spool_writer = spool.writer()?;
        let mut annotations = EntryAnnotationIndex::new();

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

            validate_produced_artifact_identity(&target, &produced)?;

            let verified = artifact_store
                .get_verified_artifact(&produced.artifact_ref)
                .await?;

            let documents = consume_target(artifact_store, &target, &produced).await?;

            for document in &documents {
                annotations.observe(document);
                spool_writer.push(document)?;
            }

            total_documents += documents.len();
            successful_targets.push(key);

            manifest_targets.push(IndexTargetManifest {
                source: target.source_id.clone(),
                ref_id: target.ref_config.id.clone(),
                artifact_kind: produced.artifact_ref.kind,
                document_count: documents.len(),
                artifact_hash: Some(verified.metadata.content_hash.clone()),
                revision: verified.metadata.revision.clone(),
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
                && !required_success_targets.is_empty()
                && !successful_targets
                    .iter()
                    .any(|target| required_success_targets.contains(target))
            {
                bail!("bootstrap generation produced no default search targets");
            }
        }

        spool_writer.finish()?;

        let index_path = index_store.index_path(&generation_path);
        let index = SearchIndex::create_or_replace(&index_path)?;
        let mut writer = index.writer()?;

        for document in spool.reader()? {
            let document = document?;
            writer.add_document(&document, &annotations.annotation_for(&document))?;
        }

        writer.commit()?;

        let manifest = IndexGenerationManifest::new(total_documents, manifest_targets)?;
        let published_generation = PublishedGeneration {
            path: generation_path.clone(),
            manifest: manifest.clone(),
        };

        SeoFactsArtifact::write_derived(index_store, &published_generation)?;
        index_store.write_manifest(&generation_path, &manifest)?;
        index_store.write_integrity(&published_generation, true)?;

        publish_started = true;
        index_store.publish(&generation_path)?;

        tracing::info!(
            generation = %generation_path.as_str(),
            documents = total_documents,
            "published index generation"
        );

        Ok(GenerationBuildResult {
            path: generation_path.clone(),
            successful_targets,
            skipped_targets,
            failed_refresh_targets,
        })
    }
    .await;

    if result.is_err()
        && !publish_started
        && let Err(error) = std::fs::remove_dir_all(&generation_path)
    {
        tracing::warn!(
            generation = %generation_path,
            "failed to clean up incomplete index generation: {error}"
        );
    }

    result
}

fn validate_produced_artifact_identity(
    target: &TargetRef,
    produced: &ProducedArtifact,
) -> Result<()> {
    let expected_ref = latest_artifact_ref_for_target(target);

    if produced.artifact_ref != expected_ref {
        bail!(
            "produced artifact ref mismatch for {}/{}: expected {:?}, got {:?}",
            target.source_id,
            target.ref_config.id,
            expected_ref,
            produced.artifact_ref
        );
    }

    if produced.metadata.source != target.source_id
        || produced.metadata.ref_id != target.ref_config.id
        || produced.metadata.kind != expected_ref.kind
    {
        bail!(
            "produced artifact metadata mismatch for {}/{}: expected source={:?} ref_id={:?} kind={:?}, got source={:?} ref_id={:?} kind={:?}",
            target.source_id,
            target.ref_config.id,
            target.source_id,
            target.ref_config.id,
            expected_ref.kind,
            produced.metadata.source,
            produced.metadata.ref_id,
            produced.metadata.kind
        );
    }

    Ok(())
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
    let Some(source_ref) = tolerable_nix_source_ref(target) else {
        return ProduceFailureDisposition::Fatal;
    };

    if !is_remote_nix_ref(source_ref) {
        return ProduceFailureDisposition::Fatal;
    }

    if error.chain().any(|cause| cause.is::<StoreError>()) {
        return ProduceFailureDisposition::Fatal;
    }

    let Some(failure) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<NixCommandFailure>())
    else {
        if error.chain().any(|cause| cause.is::<std::io::Error>()) {
            return ProduceFailureDisposition::Fatal;
        }

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
    let required_success_targets = default_indexed_search_target_keys(config)?;

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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bytes::Bytes;
    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_config::source::{RefConfig, SourceKind};
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_index::generation_validator::GenerationValidator;
    use nixsearch_store::{ArtifactMetadataInput, ArtifactRef};

    use super::*;

    const SOURCE: &str = "apps";
    const REF: &str = "unstable";

    #[derive(Debug, Clone, Copy)]
    enum MockBehavior {
        Matching,
        MismatchedArtifactRef,
        MismatchedMetadata,
    }

    struct MockProducer {
        behavior: MockBehavior,
    }

    #[async_trait]
    impl TargetProducer for MockProducer {
        async fn produce(
            &self,
            store: &ArtifactStore,
            target: &TargetRef,
        ) -> Result<ProducedArtifact> {
            match self.behavior {
                MockBehavior::Matching => write_flake_info_artifact(store, target).await,
                MockBehavior::MismatchedArtifactRef => {
                    let artifact_ref = ArtifactRef::revision(
                        target.source_id.clone(),
                        target.ref_config.id.clone(),
                        ArtifactKind::FlakeInfoJson,
                        "wrong-revision",
                    );
                    let metadata = store
                        .put_artifact(
                            &artifact_ref,
                            Bytes::from_static(br#"[{"type":"app","app_attr_name":"hello"}]"#),
                            ArtifactMetadataInput::new("test-producer"),
                        )
                        .await?;

                    Ok(ProducedArtifact {
                        artifact_ref,
                        metadata,
                    })
                }
                MockBehavior::MismatchedMetadata => {
                    let mut produced = write_flake_info_artifact(store, target).await?;
                    produced.metadata.kind = ArtifactKind::PackagesJson;

                    Ok(produced)
                }
            }
        }
    }

    async fn write_flake_info_artifact(
        store: &ArtifactStore,
        target: &TargetRef,
    ) -> Result<ProducedArtifact> {
        let artifact_ref = latest_artifact_ref_for_target(target);
        let metadata = store
            .put_artifact(
                &artifact_ref,
                Bytes::from_static(br#"[{"type":"app","app_attr_name":"hello"}]"#),
                ArtifactMetadataInput::new("test-producer"),
            )
            .await?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }

    fn flake_info_target() -> TargetRef {
        TargetRef {
            source_id: SOURCE.to_owned(),
            source_kind: SourceKind::Apps,
            strip_prefixes: Vec::new(),
            ref_config: RefConfig {
                id: REF.to_owned(),
                artifact_only: true,
                producer: ProducerConfig::ExistingFile {
                    path: PathBuf::from("unused.json"),
                    artifact: ArtifactKind::FlakeInfoJson,
                },
                source_links: None,
            },
        }
    }

    fn utf8_path(path: PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
    }

    fn stores(tempdir: &TempDir) -> (IndexStore, ArtifactStore) {
        let index_store = IndexStore::new(utf8_path(tempdir.path().join("index")));
        let artifact_store = ArtifactStore::local(tempdir.path().join("artifacts")).unwrap();

        (index_store, artifact_store)
    }

    async fn build_with_mock(
        index_store: &IndexStore,
        artifact_store: &ArtifactStore,
        target: TargetRef,
        behavior: MockBehavior,
    ) -> Result<GenerationBuildResult> {
        let refresh_keys = BTreeSet::from([TargetKey::from(&target)]);
        let producer = MockProducer { behavior };

        build_and_publish_generation_with_policy(
            index_store,
            artifact_store,
            vec![target],
            &refresh_keys,
            GenerationFailurePolicy::Strict,
            None,
            &producer,
        )
        .await
    }

    #[tokio::test]
    async fn generation_publishes_flake_info_artifact_target_with_zero_documents() {
        let tempdir = tempdir().unwrap();
        let (index_store, artifact_store) = stores(&tempdir);
        let target = flake_info_target();

        let result = build_with_mock(
            &index_store,
            &artifact_store,
            target,
            MockBehavior::Matching,
        )
        .await
        .unwrap();

        assert_eq!(index_store.current_path().unwrap(), result.path);

        let manifest = index_store.read_manifest(&result.path).unwrap();
        assert_eq!(manifest.document_count, 0);
        assert_eq!(manifest.targets.len(), 1);

        let manifest_target = &manifest.targets[0];
        assert_eq!(manifest_target.source, SOURCE);
        assert_eq!(manifest_target.ref_id, REF);
        assert_eq!(manifest_target.artifact_kind, ArtifactKind::FlakeInfoJson);
        assert_eq!(manifest_target.document_count, 0);

        let published_generation = PublishedGeneration {
            path: result.path,
            manifest: manifest.clone(),
        };
        let sidecar = SeoFactsArtifact::read(&published_generation).unwrap();
        sidecar.validate_for_manifest(&manifest).unwrap();
        assert!(sidecar.refs.is_empty());
        assert!(sidecar.entries.is_empty());

        let leased = index_store.current_leased_generation().unwrap();
        let complete = GenerationValidator::new(index_store.clone())
            .open_seo_complete_leased_generation(&leased)
            .unwrap();
        assert_eq!(complete.sidecar, sidecar);
    }

    #[tokio::test]
    async fn tolerant_bootstrap_allows_artifact_only_targets_without_default_search_targets() {
        let tempdir = tempdir().unwrap();
        let (index_store, artifact_store) = stores(&tempdir);
        let target = flake_info_target();
        let refresh_keys = BTreeSet::from([TargetKey::from(&target)]);
        let required_success_targets = BTreeSet::new();
        let producer = MockProducer {
            behavior: MockBehavior::Matching,
        };

        let result = build_and_publish_generation_with_policy(
            &index_store,
            &artifact_store,
            vec![target],
            &refresh_keys,
            GenerationFailurePolicy::TolerateBootstrapNixFailures,
            Some(&required_success_targets),
            &producer,
        )
        .await
        .unwrap();

        assert_eq!(result.successful_targets.len(), 1);
        assert!(result.failed_refresh_targets.is_empty());
        assert!(result.skipped_targets.is_empty());
    }

    #[tokio::test]
    async fn generation_rejects_mismatched_produced_artifact_ref_before_publish() {
        let tempdir = tempdir().unwrap();
        let (index_store, artifact_store) = stores(&tempdir);
        let target = flake_info_target();

        let error = build_with_mock(
            &index_store,
            &artifact_store,
            target,
            MockBehavior::MismatchedArtifactRef,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("produced artifact ref mismatch"));
        assert!(index_store.try_current_manifest().unwrap().is_none());
    }

    #[tokio::test]
    async fn generation_rejects_mismatched_produced_metadata_before_publish() {
        let tempdir = tempdir().unwrap();
        let (index_store, artifact_store) = stores(&tempdir);
        let target = flake_info_target();

        let error = build_with_mock(
            &index_store,
            &artifact_store,
            target,
            MockBehavior::MismatchedMetadata,
        )
        .await
        .unwrap_err();

        assert!(format!("{error:#}").contains("produced artifact metadata mismatch"));
        assert!(index_store.try_current_manifest().unwrap().is_none());
    }
}
