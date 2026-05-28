use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;

use nixsearch_core::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};

use super::Producer;

#[derive(Debug, Clone)]
pub struct ExistingFileProducer {
    path: PathBuf,
    kind: ArtifactKind,
    producer_name: String,
}

impl ExistingFileProducer {
    pub fn new(path: impl Into<PathBuf>, kind: ArtifactKind) -> Self {
        Self {
            path: path.into(),
            kind,
            producer_name: "existing-file".to_owned(),
        }
    }

    pub fn with_name(
        path: impl Into<PathBuf>,
        kind: ArtifactKind,
        producer_name: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            producer_name: producer_name.into(),
        }
    }
}

#[async_trait]
impl Producer for ExistingFileProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let bytes = tokio::fs::read(&self.path)
            .await
            .with_context(|| format!("failed to read artifact file {}", self.path.display()))?;

        let artifact_ref = request.artifact_ref(self.kind);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.path.display().to_string());

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use nixsearch_core::ArtifactKind;
    use nixsearch_store::ArtifactStore;
    use nixsearch_test_support::{REF_SMALL, SOURCE_FIXTURES};

    use crate::{ExistingFileProducer, ProduceRequest, Producer};

    #[tokio::test]
    async fn existing_file_producer_writes_artifact_to_store() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        tokio::fs::write(
            &artifact_path,
            include_bytes!("../../../../fixtures/search-small/options.json"),
        )
        .await
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.source, "fixtures");
        assert_eq!(produced.metadata.ref_id, "small");
        assert_eq!(produced.metadata.producer, "existing-file");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(artifact_path.to_string_lossy().as_ref())
        );

        assert!(store.exists(&produced.artifact_ref).await.unwrap());
    }
}
