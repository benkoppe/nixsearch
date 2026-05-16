use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;

use nix_search_core::{ArtifactKind, IngestContext, SearchDocument};
use nix_search_ingest::parse_options_json;
use nix_search_store::{ArtifactMetadata, ArtifactMetadataInput, ArtifactRef, ArtifactStore};

#[derive(Debug, Clone)]
pub struct ProduceRequest {
    pub project: String,
    pub dataset: String,
    pub ref_id: String,
}

impl ProduceRequest {
    pub fn artifact_ref(&self, kind: ArtifactKind) -> ArtifactRef {
        ArtifactRef::latest(
            self.project.clone(),
            self.dataset.clone(),
            self.ref_id.clone(),
            kind,
        )
    }

    pub fn ingest_context(&self, revision: Option<String>) -> IngestContext {
        IngestContext {
            project: self.project.clone(),
            dataset: self.dataset.clone(),
            ref_id: self.ref_id.clone(),
            revision,
            repo: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProducedArtifact {
    pub artifact_ref: ArtifactRef,
    pub metadata: ArtifactMetadata,
}

#[async_trait]
pub trait Producer: Send + Sync {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact>;
}

#[async_trait]
pub trait Consumer: Send + Sync {
    async fn consume(
        &self,
        store: &ArtifactStore,
        artifact: &ProducedArtifact,
    ) -> Result<Vec<SearchDocument>>;
}

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
        metadata_input.source = Some(self.path.display().to_string());

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
            project: artifact.metadata.project.clone(),
            dataset: artifact.metadata.dataset.clone(),
            ref_id: artifact.metadata.ref_id.clone(),
            revision: artifact.metadata.revision.clone(),
            repo: None,
        };

        parse_options_json(bytes.as_ref(), &context).context("failed to parse options artifact")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nix_search_core::ArtifactKind;
    use nix_search_store::ArtifactStore;
    use tempfile::tempdir;

    use super::{Consumer, ExistingFileProducer, OptionsJsonConsumer, ProduceRequest, Producer};

    #[tokio::test]
    async fn existing_file_producer_writes_artifact_to_store() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        fs::write(
            &artifact_path,
            r#"
               {
                 "programs.git.enable": {
                   "description": "Whether to enable Git."
                 }
               }
               "#,
        )
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            project: "fixtures".into(),
            dataset: "options".into(),
            ref_id: "small".into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.project, "fixtures");
        assert_eq!(produced.metadata.dataset, "options");
        assert_eq!(produced.metadata.ref_id, "small");
        assert_eq!(produced.metadata.producer, "existing-file");
        assert_eq!(
            produced.metadata.source.as_deref(),
            Some(artifact_path.to_string_lossy().as_ref())
        );

        assert!(store.exists(&produced.artifact_ref).await.unwrap());
    }

    #[tokio::test]
    async fn options_json_consumer_reads_produced_artifact() {
        let tempdir = tempdir().unwrap();
        let artifact_path = tempdir.path().join("options.json");
        let store_path = tempdir.path().join("store");

        fs::write(
            &artifact_path,
            r#"
               {
                 "programs.git.enable": {
                   "description": "Whether to enable Git.",
                   "loc": ["programs", "git", "enable"]
                 },
                 "services.nginx.enable": {
                   "description": "Whether to enable Nginx.",
                   "loc": ["services", "nginx", "enable"]
                 }
               }
               "#,
        )
        .unwrap();

        let store = ArtifactStore::local(&store_path).unwrap();

        let producer = ExistingFileProducer::new(&artifact_path, ArtifactKind::OptionsJson);
        let request = ProduceRequest {
            project: "fixtures".into(),
            dataset: "options".into(),
            ref_id: "small".into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        let consumer = OptionsJsonConsumer;
        let docs = consumer.consume(&store, &produced).await.unwrap();

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].name(), "programs.git.enable");
        assert_eq!(docs[1].name(), "services.nginx.enable");
    }
}
