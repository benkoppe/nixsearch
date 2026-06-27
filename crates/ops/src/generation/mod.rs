use std::collections::BTreeSet;

use anyhow::Result;
use camino::Utf8PathBuf;

use nixsearch_index::store::IndexStore;
use nixsearch_store::ArtifactStore;

use crate::targets::{TargetKey, TargetRef};

mod documents;
mod index;
mod policy;
mod production;
mod publish;

use self::documents::{
    SpooledDocumentSetBuilder, build_generation_manifest, consume_and_spool_target,
};
use self::index::write_tantivy_index;
use self::policy::{validate_generation_policy, validate_generation_success_requirements};
use self::production::produce_or_retain_target;
use self::publish::{
    IncompleteGenerationGuard, publish_completed_generation, write_generation_artifacts,
};

pub(crate) use self::policy::GenerationFailurePolicy;
pub(crate) use self::production::TargetProducer;

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

pub(crate) async fn build_and_publish_generation_with_policy(
    index_store: &IndexStore,
    artifact_store: &ArtifactStore,
    targets: Vec<TargetRef>,
    refresh_keys: &BTreeSet<TargetKey>,
    policy: GenerationFailurePolicy,
    required_success_targets: Option<&BTreeSet<TargetKey>>,
    producer: &(dyn TargetProducer + Send + Sync),
) -> Result<GenerationBuildResult> {
    validate_generation_policy(&targets, refresh_keys, policy)?;

    let mut generation = IncompleteGenerationGuard::create(index_store)?;

    let mut documents_builder = SpooledDocumentSetBuilder::create()?;
    let mut skipped_targets = Vec::new();
    let mut failed_refresh_targets = Vec::new();

    for target in targets {
        let key = TargetKey::from(&target);
        match produce_or_retain_target(artifact_store, target, refresh_keys, policy, producer)
            .await?
        {
            Some(produced_target) => {
                consume_and_spool_target(artifact_store, &produced_target, &mut documents_builder)
                    .await?;
            }
            None => {
                failed_refresh_targets.push(key.clone());
                skipped_targets.push(key);
            }
        }
    }

    validate_generation_success_requirements(
        policy,
        required_success_targets,
        documents_builder.successful_targets(),
    )?;

    let documents = documents_builder.finish()?;
    write_tantivy_index(index_store, generation.path(), &documents)?;

    let manifest = build_generation_manifest(&documents)?;
    write_generation_artifacts(index_store, generation.path(), manifest)?;

    generation.begin_publish();
    publish_completed_generation(index_store, generation.path(), documents.total_documents)?;

    Ok(GenerationBuildResult {
        path: generation.path().to_owned(),
        successful_targets: documents.successful_targets,
        skipped_targets,
        failed_refresh_targets,
    })
}
