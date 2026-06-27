use std::collections::BTreeSet;

use anyhow::{Result, bail};
use async_trait::async_trait;

use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_store::{ArtifactMetadata, ArtifactStore};

use crate::produce::produced_from_existing_artifact;
use crate::targets::{TargetKey, TargetRef, latest_artifact_ref_for_target};

use super::policy::{
    GenerationFailurePolicy, ProduceFailureDisposition, classify_bootstrap_produce_error,
};

#[async_trait]
pub(crate) trait TargetProducer: Send + Sync {
    async fn produce(&self, store: &ArtifactStore, target: &TargetRef) -> Result<ProducedArtifact>;
}

#[derive(Debug)]
pub(crate) struct ProducedTarget {
    pub(crate) key: TargetKey,
    pub(crate) target: TargetRef,
    pub(crate) status: TargetProductionStatus,
    pub(crate) produced: ProducedArtifact,
    pub(crate) verified_metadata: ArtifactMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetProductionStatus {
    Refreshed,
    Retained,
}

impl TargetProductionStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Refreshed => "refreshed",
            Self::Retained => "retained",
        }
    }
}

pub(crate) async fn produce_or_retain_target(
    artifact_store: &ArtifactStore,
    target: TargetRef,
    refresh_keys: &BTreeSet<TargetKey>,
    policy: GenerationFailurePolicy,
    producer: &(dyn TargetProducer + Send + Sync),
) -> Result<Option<ProducedTarget>> {
    let key = TargetKey::from(&target);

    if refresh_keys.contains(&key) {
        return match producer.produce(artifact_store, &target).await {
            Ok(produced) => {
                let produced = verify_produced_target(
                    artifact_store,
                    key,
                    target,
                    TargetProductionStatus::Refreshed,
                    produced,
                )
                .await?;

                Ok(Some(produced))
            }
            Err(error) => match policy {
                GenerationFailurePolicy::Strict => Err(error),
                GenerationFailurePolicy::TolerateBootstrapNixFailures => {
                    match classify_bootstrap_produce_error(&target, &error) {
                        ProduceFailureDisposition::Fatal => Err(error),
                        ProduceFailureDisposition::TolerableSkip => {
                            tracing::warn!(
                                source = %target.source_id,
                                ref_id = %target.ref_config.id,
                                "skipping target after bootstrap production failure: {error:#}"
                            );

                            Ok(None)
                        }
                    }
                }
            },
        };
    }

    let produced = produced_from_existing_artifact(artifact_store, &target).await?;
    let produced = verify_produced_target(
        artifact_store,
        key,
        target,
        TargetProductionStatus::Retained,
        produced,
    )
    .await?;

    Ok(Some(produced))
}

async fn verify_produced_target(
    artifact_store: &ArtifactStore,
    key: TargetKey,
    target: TargetRef,
    status: TargetProductionStatus,
    produced: ProducedArtifact,
) -> Result<ProducedTarget> {
    validate_produced_artifact_identity(&target, &produced)?;

    let verified = artifact_store
        .get_verified_artifact(&produced.artifact_ref)
        .await?;

    Ok(ProducedTarget {
        key,
        target,
        status,
        produced,
        verified_metadata: verified.metadata,
    })
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
