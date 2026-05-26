use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use nixsearch_core::ArtifactKind;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("object store error: {0}")]
    ObjectStore(#[from] object_store::Error),

    #[error("failed to serialize artifact metadata: {0}")]
    SerializeMetadata(#[source] serde_json::Error),

    #[error("failed to deserialize artifact metadata: {0}")]
    DeserializeMetadata(#[source] serde_json::Error),

    #[error("invalid local artifact store path {path:?}: {source}")]
    InvalidLocalStorePath {
        path: String,
        #[source]
        source: object_store::Error,
    },
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub source: String,
    pub ref_id: String,
    pub kind: ArtifactKind,
    pub version: ArtifactVersion,
}

impl ArtifactRef {
    pub fn latest(
        source: impl Into<String>,
        ref_id: impl Into<String>,
        kind: ArtifactKind,
    ) -> Self {
        Self {
            source: source.into(),
            ref_id: ref_id.into(),
            kind,
            version: ArtifactVersion::Latest,
        }
    }

    pub fn revision(
        source: impl Into<String>,
        ref_id: impl Into<String>,
        kind: ArtifactKind,
        revision: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            ref_id: ref_id.into(),
            kind,
            version: ArtifactVersion::Revision(revision.into()),
        }
    }

    pub fn artifact_key(&self) -> ObjectPath {
        let mut parts = vec![self.source.as_str(), self.ref_id.as_str()];

        self.version.push_key_parts(&mut parts);
        parts.push(self.kind.file_name());

        ObjectPath::from_iter(parts)
    }

    pub fn metadata_key(&self) -> ObjectPath {
        let mut parts = vec![self.source.as_str(), self.ref_id.as_str()];

        self.version.push_key_parts(&mut parts);
        parts.push("meta.json");

        ObjectPath::from_iter(parts)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactVersion {
    Latest,
    Revision(String),
}

impl ArtifactVersion {
    fn push_key_parts<'a>(&'a self, parts: &mut Vec<&'a str>) {
        match self {
            Self::Latest => parts.push("latest"),
            Self::Revision(revision) => {
                parts.push("revisions");
                parts.push(revision.as_str());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadataInput {
    pub producer: String,
    pub revision: Option<String>,
    pub source_url: Option<String>,
    pub warnings: Vec<String>,
}

impl ArtifactMetadataInput {
    pub fn new(producer: impl Into<String>) -> Self {
        Self {
            producer: producer.into(),
            revision: None,
            source_url: None,
            warnings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub source: String,
    pub ref_id: String,
    pub kind: ArtifactKind,
    pub producer: String,
    pub revision: Option<String>,
    pub source_url: Option<String>,
    pub content_hash: String,
    pub size_bytes: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub produced_at: OffsetDateTime,
    pub warnings: Vec<String>,
}

pub struct ArtifactStore {
    store: Arc<dyn ObjectStore>,
}

impl ArtifactStore {
    pub fn local(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        std::fs::create_dir_all(path).map_err(|source| StoreError::InvalidLocalStorePath {
            path: path.display().to_string(),
            source: object_store::Error::Generic {
                store: "local",
                source: Box::new(source),
            },
        })?;

        let store = LocalFileSystem::new_with_prefix(path).map_err(|source| {
            StoreError::InvalidLocalStorePath {
                path: path.display().to_string(),
                source,
            }
        })?;

        Ok(Self {
            store: Arc::new(store),
        })
    }

    pub async fn put_artifact(
        &self,
        artifact_ref: &ArtifactRef,
        bytes: Bytes,
        input: ArtifactMetadataInput,
    ) -> Result<ArtifactMetadata> {
        let artifact_key = artifact_ref.artifact_key();

        self.store.put(&artifact_key, bytes.clone().into()).await?;

        let metadata = ArtifactMetadata {
            source: artifact_ref.source.clone(),
            ref_id: artifact_ref.ref_id.clone(),
            kind: artifact_ref.kind,
            producer: input.producer,
            revision: input.revision,
            source_url: input.source_url,
            content_hash: sha256_hex(&bytes),
            size_bytes: bytes.len() as u64,
            produced_at: OffsetDateTime::now_utc(),
            warnings: input.warnings,
        };

        self.put_metadata(artifact_ref, &metadata).await?;

        Ok(metadata)
    }

    pub async fn get_artifact(&self, artifact_ref: &ArtifactRef) -> Result<Bytes> {
        let result = self.store.get(&artifact_ref.artifact_key()).await?;
        let bytes = result.bytes().await?;

        Ok(bytes)
    }

    pub async fn put_metadata(
        &self,
        artifact_ref: &ArtifactRef,
        metadata: &ArtifactMetadata,
    ) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(metadata).map_err(StoreError::SerializeMetadata)?;

        self.store
            .put(&artifact_ref.metadata_key(), Bytes::from(bytes).into())
            .await?;

        Ok(())
    }

    pub async fn get_metadata(&self, artifact_ref: &ArtifactRef) -> Result<ArtifactMetadata> {
        let result = self.store.get(&artifact_ref.metadata_key()).await?;
        let bytes = result.bytes().await?;

        let metadata = serde_json::from_slice(&bytes).map_err(StoreError::DeserializeMetadata)?;

        Ok(metadata)
    }

    pub async fn exists(&self, artifact_ref: &ArtifactRef) -> Result<bool> {
        match self.store.head(&artifact_ref.artifact_key()).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(StoreError::ObjectStore(error)),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tempfile::tempdir;

    use nixsearch_core::ArtifactKind;

    use super::{ArtifactMetadataInput, ArtifactRef, ArtifactStore};

    #[test]
    fn latest_artifact_keys_are_stable() {
        let artifact_ref = ArtifactRef::latest("nixos", "unstable", ArtifactKind::OptionsJson);

        assert_eq!(
            artifact_ref.artifact_key().as_ref(),
            "nixos/unstable/latest/options.json"
        );
        assert_eq!(
            artifact_ref.metadata_key().as_ref(),
            "nixos/unstable/latest/meta.json"
        );
    }

    #[test]
    fn revision_artifact_keys_are_stable_and_escape_slashes() {
        let artifact_ref = ArtifactRef::revision(
            "nixpkgs",
            "stable",
            ArtifactKind::PackagesJson,
            "release/25.05",
        );

        assert_eq!(
            artifact_ref.artifact_key().as_ref(),
            "nixpkgs/stable/revisions/release%2F25.05/packages.json"
        );
        assert_eq!(
            artifact_ref.metadata_key().as_ref(),
            "nixpkgs/stable/revisions/release%2F25.05/meta.json"
        );
    }

    #[tokio::test]
    async fn writes_and_reads_artifact_bytes() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let artifact_ref = ArtifactRef::latest("nixos", "unstable", ArtifactKind::OptionsJson);

        let bytes = Bytes::from_static(br#"{"hello":"world"}"#);

        store
            .put_artifact(
                &artifact_ref,
                bytes.clone(),
                ArtifactMetadataInput::new("test-producer"),
            )
            .await
            .unwrap();

        let loaded = store.get_artifact(&artifact_ref).await.unwrap();

        assert_eq!(loaded, bytes);
    }

    #[tokio::test]
    async fn writes_and_reads_metadata() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let artifact_ref = ArtifactRef::latest("nixos", "unstable", ArtifactKind::OptionsJson);

        let mut input = ArtifactMetadataInput::new("test-producer");
        input.revision = Some("abc123".to_owned());
        input.source_url = Some("github:NixOS/nixpkgs/nixos-unstable".to_owned());
        input.warnings.push("example warning".to_owned());

        let metadata = store
            .put_artifact(
                &artifact_ref,
                Bytes::from_static(b"artifact contents"),
                input,
            )
            .await
            .unwrap();

        let loaded = store.get_metadata(&artifact_ref).await.unwrap();

        assert_eq!(loaded, metadata);
        assert_eq!(loaded.source, "nixos");
        assert_eq!(loaded.ref_id, "unstable");
        assert_eq!(loaded.kind, ArtifactKind::OptionsJson);
        assert_eq!(loaded.producer, "test-producer");
        assert_eq!(loaded.revision.as_deref(), Some("abc123"));
        assert_eq!(
            loaded.source_url.as_deref(),
            Some("github:NixOS/nixpkgs/nixos-unstable")
        );
        assert_eq!(loaded.warnings, ["example warning"]);
    }

    #[tokio::test]
    async fn computes_stable_sha256() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let artifact_ref = ArtifactRef::latest("fixtures", "small", ArtifactKind::OptionsJson);

        let metadata = store
            .put_artifact(
                &artifact_ref,
                Bytes::from_static(b"hello"),
                ArtifactMetadataInput::new("test-producer"),
            )
            .await
            .unwrap();

        assert_eq!(
            metadata.content_hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(metadata.size_bytes, 5);
    }

    #[tokio::test]
    async fn exists_returns_false_for_missing_artifact() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let artifact_ref = ArtifactRef::latest("missing", "main", ArtifactKind::OptionsJson);

        assert!(!store.exists(&artifact_ref).await.unwrap());
    }

    #[tokio::test]
    async fn exists_returns_true_for_written_artifact() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let artifact_ref = ArtifactRef::latest("fixtures", "small", ArtifactKind::OptionsJson);

        store
            .put_artifact(
                &artifact_ref,
                Bytes::from_static(b"{}"),
                ArtifactMetadataInput::new("test-producer"),
            )
            .await
            .unwrap();

        assert!(store.exists(&artifact_ref).await.unwrap());
    }
}
