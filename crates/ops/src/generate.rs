use std::collections::BTreeSet;

use anyhow::Result;
use camino::Utf8PathBuf;

use nixsearch_config::AppConfig;
use nixsearch_index::{IndexGenerationManifest, IndexStore, IndexTargetManifest, SearchIndex};
use nixsearch_store::ArtifactStore;

use crate::consume::consume_target;
use crate::produce::{artifact_store_from_config, produce_target, produced_from_existing_artifact};
use crate::targets::{TargetKey, TargetRef, all_targets};

pub async fn build_and_publish_generation(
    index_store: &IndexStore,
    artifact_store: &ArtifactStore,
    targets: Vec<TargetRef>,
    refresh_keys: &BTreeSet<TargetKey>,
) -> Result<Utf8PathBuf> {
    let generation_path = index_store.create_generation_path()?;

    let index = SearchIndex::create_or_replace(&generation_path)?;
    let mut writer = index.writer()?;

    let mut total_documents = 0usize;
    let mut manifest_targets = Vec::new();

    for target in targets {
        let key = TargetKey::from(&target);

        let produced = if refresh_keys.contains(&key) {
            produce_target(artifact_store, &target).await?
        } else {
            produced_from_existing_artifact(artifact_store, &target).await?
        };

        let documents = consume_target(artifact_store, &target, &produced).await?;

        for document in &documents {
            writer.add_document(document)?;
        }

        total_documents += documents.len();

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
            if refresh_keys.contains(&key) {
                "refreshed"
            } else {
                "retained"
            },
            documents.len(),
            target.source_id,
            target.ref_config.id
        );
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

    Ok(generation_path)
}

/// Regenerate all configured sources. Returns the new generation path.
pub async fn regenerate_all(config: &AppConfig) -> Result<Utf8PathBuf> {
    let store = artifact_store_from_config(config)?;
    let index_store = IndexStore::new(&config.data.index_dir);
    let targets = all_targets(config);

    if targets.is_empty() {
        anyhow::bail!("no sources configured to regenerate");
    }

    let refresh_keys: BTreeSet<TargetKey> = targets.iter().map(TargetKey::from).collect();
    build_and_publish_generation(&index_store, &store, targets, &refresh_keys).await
}
