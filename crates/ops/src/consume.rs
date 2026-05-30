use anyhow::{Context, Result, bail};

use nixsearch_config::source::SourceKind;
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::SearchDocument;
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_source::consumer::{Consumer, OptionsJsonConsumer, PackagesJsonConsumer};
use nixsearch_store::ArtifactStore;

use crate::targets::TargetRef;

pub async fn consume_target(
    store: &ArtifactStore,
    target: &TargetRef,
    produced: &ProducedArtifact,
) -> Result<Vec<SearchDocument>> {
    match (target.source_kind, produced.artifact_ref.kind) {
        (SourceKind::Options | SourceKind::Mixed, ArtifactKind::OptionsJson) => {
            let consumer = OptionsJsonConsumer::new(target.strip_prefixes.clone());

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume options artifact for {}/{}",
                    target.source_id, target.ref_config.id
                )
            })
        }

        (SourceKind::Packages | SourceKind::Mixed, ArtifactKind::PackagesJson) => {
            let consumer = PackagesJsonConsumer::new(target.strip_prefixes.clone());

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume packages artifact for {}/{}",
                    target.source_id, target.ref_config.id
                )
            })
        }

        (kind, artifact) => bail!(
            "no consumer implemented for source kind {:?} and artifact kind {:?}",
            kind,
            artifact
        ),
    }
}
