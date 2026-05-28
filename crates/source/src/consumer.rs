use anyhow::{Context, Result};
use async_trait::async_trait;

use nixsearch_core::{ArtifactKind, IngestContext, SearchDocument};
use nixsearch_ingest::{parse_options_json, parse_packages_json};
use nixsearch_store::ArtifactStore;

use crate::artifact::ProducedArtifact;

#[async_trait]
pub trait Consumer: Send + Sync {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>>;
}

#[derive(Debug, Default, Clone)]
pub struct OptionsJsonConsumer;

#[async_trait]
impl Consumer for OptionsJsonConsumer {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>> {
        if artifact.artifact_ref.kind != ArtifactKind::OptionsJson {
            anyhow::bail!(
                "OptionsJsonConsumer cannot consume artifact kind {:?}",
                artifact.artifact_ref.kind
            );
        }

        let bytes = store
            .get_artifact(&artifact.artifact_ref)
            .await
            .context("failed to read options artifact")?;

        let context = IngestContext {
            source: artifact.metadata.source.clone(),
            ref_id: artifact.metadata.ref_id.clone(),
            revision: artifact.metadata.revision.clone(),
            repo: None,
        };

        parse_options_json(bytes.as_ref(), &context).context("failed to parse options artifact")
    }
}

#[derive(Debug, Default, Clone)]
pub struct PackagesJsonConsumer;

#[async_trait]
impl Consumer for PackagesJsonConsumer {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>> {
        if artifact.artifact_ref.kind != ArtifactKind::PackagesJson {
            anyhow::bail!(
                "PackagesJsonConsumer cannot consume artifact kind {:?}",
                artifact.artifact_ref.kind
            );
        }

        let bytes = store
            .get_artifact(&artifact.artifact_ref)
            .await
            .context("failed to read packages artifact")?;

        let context = IngestContext {
            source: artifact.metadata.source.clone(),
            ref_id: artifact.metadata.ref_id.clone(),
            revision: artifact.metadata.revision.clone(),
            repo: None,
        };

        parse_packages_json(bytes.as_ref(), &context).context("failed to parse packages artifact")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use nixsearch_core::ArtifactKind;
    use nixsearch_store::ArtifactStore;
    use nixsearch_test_support::{
        OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, REF_SMALL, SOURCE_FIXTURES,
    };

    use crate::{Consumer, ExistingFileProducer, OptionsJsonConsumer, ProduceRequest, Producer};

    #[tokio::test]
    async fn options_json_consumer_reads_produced_artifact() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        fs::write(
            &artifact_path,
            include_bytes!("../../../fixtures/search-small/options.json"),
        )
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        let consumer = OptionsJsonConsumer;
        let docs = consumer.consume(&store, &produced).await.unwrap();

        assert_eq!(docs.len(), 4);
        assert!(docs.iter().any(|doc| doc.name() == OPTION_GIT_ENABLE));
        assert!(docs.iter().any(|doc| doc.name() == OPTION_NGINX_ENABLE));
    }
}
