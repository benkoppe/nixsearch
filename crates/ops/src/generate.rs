use std::collections::BTreeSet;

use anyhow::{Result, bail};
use async_trait::async_trait;
use camino::Utf8PathBuf;

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::IndexStore;
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_store::ArtifactStore;

use crate::generation::{
    GenerationBuildPlan, GenerationFailurePolicy, TargetProducer,
    build_and_publish_generation_with_policy,
};
pub use crate::generation::{GenerationBuildResult, RetainedGeneration};
use crate::produce::{artifact_store_from_config, produce_target};
use crate::targets::{TargetKey, TargetRef, all_targets, default_indexed_search_target_keys};

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
    retained_generation: Option<&RetainedGeneration>,
) -> Result<Utf8PathBuf> {
    let result = build_and_publish_generation_with_policy(
        index_store,
        artifact_store,
        GenerationBuildPlan {
            targets,
            refresh_keys,
            policy: GenerationFailurePolicy::Strict,
            required_success_targets: None,
            retained_generation,
        },
        &RealTargetProducer,
    )
    .await?;

    Ok(result.path)
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
        GenerationBuildPlan {
            targets,
            refresh_keys: &refresh_keys,
            policy: GenerationFailurePolicy::TolerateBootstrapNixFailures,
            required_success_targets: Some(&required_success_targets),
            retained_generation: None,
        },
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
    build_and_publish_generation(&index_store, &store, targets, &refresh_keys, None).await
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use bytes::Bytes;
    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::{RefConfig, SourceKind};
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::target::RefRole;
    use nixsearch_index::generation_validator::GenerationValidator;
    use nixsearch_index::search::SearchIndex;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::PublishedGeneration;
    use nixsearch_index_test_support::publish_fixture_options_index_for_refs;
    use nixsearch_store::{ArtifactMetadataInput, ArtifactRef};
    use nixsearch_test_support::{REF_SMALL, REF_STABLE, SOURCE_FIXTURES, multi_ref_app_config};

    use crate::targets::{current_manifest_targets, latest_artifact_ref_for_target};

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

    struct FixtureOptionsProducer;

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

    #[async_trait]
    impl TargetProducer for FixtureOptionsProducer {
        async fn produce(
            &self,
            store: &ArtifactStore,
            target: &TargetRef,
        ) -> Result<ProducedArtifact> {
            let artifact_ref = latest_artifact_ref_for_target(target);
            let metadata = store
                .put_artifact(
                    &artifact_ref,
                    Bytes::from_static(include_bytes!(
                        "../../../fixtures/search-small/options.json"
                    )),
                    ArtifactMetadataInput::new("test-options-producer"),
                )
                .await?;

            Ok(ProducedArtifact {
                artifact_ref,
                metadata,
            })
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
                role: RefRole::ArtifactOnly,
                producer: ProducerConfig::ExistingFile {
                    path: PathBuf::from("unused.json"),
                    artifact: ArtifactKind::FlakeInfoJson,
                },
                source_links: None,
            },
        }
    }

    fn artifact_only_options_target() -> TargetRef {
        TargetRef {
            source_id: SOURCE.to_owned(),
            source_kind: SourceKind::Options,
            strip_prefixes: Vec::new(),
            ref_config: RefConfig {
                id: REF.to_owned(),
                role: RefRole::ArtifactOnly,
                producer: ProducerConfig::ExistingFile {
                    path: PathBuf::from("unused.json"),
                    artifact: ArtifactKind::OptionsJson,
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
            GenerationBuildPlan {
                targets: vec![target],
                refresh_keys: &refresh_keys,
                policy: GenerationFailurePolicy::Strict,
                required_success_targets: None,
                retained_generation: None,
            },
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
        let sidecar = SeoFactsArtifact::read_manifest_checked(&published_generation).unwrap();
        assert!(sidecar.sidecar().refs.is_empty());
        assert!(sidecar.sidecar().entries.is_empty());

        let leased = index_store.current_leased_generation().unwrap();
        let complete = GenerationValidator::new(index_store.clone())
            .open_seo_verified_leased_generation(&leased)
            .unwrap();
        assert_eq!(complete.sidecar.sidecar(), sidecar.sidecar());
    }

    #[tokio::test]
    async fn generation_skips_document_consumption_for_artifact_only_search_artifacts() {
        let tempdir = tempdir().unwrap();
        let (index_store, artifact_store) = stores(&tempdir);
        let target = artifact_only_options_target();

        let result = build_with_mock(
            &index_store,
            &artifact_store,
            target,
            MockBehavior::Matching,
        )
        .await
        .unwrap();

        let manifest = index_store.read_manifest(&result.path).unwrap();
        assert_eq!(manifest.document_count, 0);
        assert_eq!(manifest.targets.len(), 1);

        let manifest_target = &manifest.targets[0];
        assert_eq!(manifest_target.artifact_kind, ArtifactKind::OptionsJson);
        assert_eq!(manifest_target.target_role, RefRole::ArtifactOnly);
        assert!(!manifest_target.indexes_search_documents);
        assert_eq!(manifest_target.document_count, 0);
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
            GenerationBuildPlan {
                targets: vec![target],
                refresh_keys: &refresh_keys,
                policy: GenerationFailurePolicy::TolerateBootstrapNixFailures,
                required_success_targets: Some(&required_success_targets),
                retained_generation: None,
            },
            &producer,
        )
        .await
        .unwrap();

        assert_eq!(result.successful_targets.len(), 1);
        assert!(result.failed_refresh_targets.is_empty());
        assert!(result.skipped_targets.is_empty());
    }

    #[tokio::test]
    async fn partial_generation_retains_unselected_targets_from_current_index() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path(tempdir.path().join("index"));
        let artifact_store = ArtifactStore::local(tempdir.path().join("artifacts")).unwrap();
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let index_store = IndexStore::new(&index_dir);
        let config = multi_ref_app_config(&index_dir);
        let targets = current_manifest_targets(&config, &index_store)
            .unwrap()
            .into_values()
            .collect::<Vec<_>>();
        let refresh_keys = BTreeSet::from([TargetKey::new(
            SOURCE_FIXTURES,
            REF_SMALL,
            ArtifactKind::OptionsJson,
            RefRole::Search,
        )]);

        let current = index_store.current_leased_generation().unwrap();
        let complete = GenerationValidator::new(index_store.clone())
            .open_structurally_verified_published_generation(current.published_generation())
            .unwrap();
        let retained = RetainedGeneration::from_index(current.manifest(), &complete.index).unwrap();

        let stale_latest =
            ArtifactRef::latest(SOURCE_FIXTURES, REF_STABLE, ArtifactKind::OptionsJson);
        artifact_store
            .put_artifact(
                &stale_latest,
                Bytes::from_static(include_bytes!(
                    "../../../fixtures/search-small/options.json"
                )),
                ArtifactMetadataInput::new("unselected-latest"),
            )
            .await
            .unwrap();

        let result = build_and_publish_generation_with_policy(
            &index_store,
            &artifact_store,
            GenerationBuildPlan {
                targets,
                refresh_keys: &refresh_keys,
                policy: GenerationFailurePolicy::Strict,
                required_success_targets: None,
                retained_generation: Some(&retained),
            },
            &FixtureOptionsProducer,
        )
        .await
        .unwrap();

        let manifest = index_store.read_manifest(&result.path).unwrap();
        let stable_target = manifest
            .targets
            .iter()
            .find(|target| target.ref_id == REF_STABLE)
            .unwrap();
        assert_eq!(stable_target.document_count, 1);

        let index = SearchIndex::open(index_store.index_path(&result.path)).unwrap();
        let stable_docs = index
            .scan_indexed_documents()
            .unwrap()
            .into_iter()
            .filter(|indexed| indexed.document.common().ref_id == REF_STABLE)
            .collect::<Vec<_>>();

        assert_eq!(stable_docs.len(), 1);
        assert_eq!(stable_docs[0].document.name(), "programs.stable.git.enable");
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
    async fn generation_cleans_incomplete_generation_after_pre_publish_failure() {
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

        let generations_dir = index_store.generations_dir();
        let remaining_generations = if generations_dir.exists() {
            fs::read_dir(&generations_dir).unwrap().count()
        } else {
            0
        };
        assert_eq!(remaining_generations, 0);
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
