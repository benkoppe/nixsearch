use std::io::Read;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;

use nixsearch_core::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};

use super::Producer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadCompression {
    None,
    Brotli,
}

#[derive(Debug, Clone)]
pub struct DownloadProducer {
    url: String,
    artifact: ArtifactKind,
    revision_url: Option<String>,
    compression: DownloadCompression,
    producer_name: String,
}

impl DownloadProducer {
    pub fn new(
        url: impl Into<String>,
        artifact: ArtifactKind,
        revision_url: Option<String>,
        compression: DownloadCompression,
    ) -> Self {
        Self {
            url: url.into(),
            artifact,
            revision_url,
            compression,
            producer_name: "download".to_owned(),
        }
    }

    async fn fetch_artifact(&self) -> Result<Vec<u8>> {
        match self.compression {
            DownloadCompression::None => fetch_bytes(&self.url)
                .await
                .map(|bytes| bytes.to_vec())
                .with_context(|| format!("failed to download artifact from {}", self.url)),
            DownloadCompression::Brotli => fetch_brotli_artifact(&self.url)
                .await
                .with_context(|| format!("failed to download Brotli artifact from {}", self.url)),
        }
    }
}

#[async_trait]
impl Producer for DownloadProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let bytes = self.fetch_artifact().await?;
        let artifact_ref = request.artifact_ref(self.artifact);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.url.clone());

        if let Some(revision_url) = &self.revision_url {
            populate_download_revision(&mut metadata_input, revision_url).await;
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write downloaded artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

pub(crate) async fn fetch_bytes(url: &str) -> Result<Bytes> {
    reqwest::get(url)
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error fetching {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {url}"))
}

pub(crate) async fn fetch_text(url: &str) -> Result<String> {
    let bytes = fetch_bytes(url).await?;

    String::from_utf8(bytes.to_vec()).with_context(|| format!("response from {url} was not UTF-8"))
}

async fn populate_download_revision(
    metadata_input: &mut ArtifactMetadataInput,
    revision_url: &str,
) {
    match fetch_text(revision_url).await {
        Ok(text) => {
            let revision = text.trim();

            if revision.is_empty() {
                metadata_input.warnings.push(format!(
                    "download revision_url returned an empty revision: {revision_url}"
                ));
            } else {
                metadata_input.revision = Some(revision.to_owned());
            }
        }
        Err(error) => metadata_input.warnings.push(format!(
            "failed to fetch download revision from {revision_url}: {error:#}"
        )),
    }
}

pub(crate) async fn fetch_brotli_artifact(url: &str) -> Result<Vec<u8>> {
    let compressed = fetch_bytes(url).await?;

    decompress_brotli(&compressed)
        .with_context(|| format!("failed to decompress Brotli response from {url}"))
}

fn decompress_brotli(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decompressor = brotli::Decompressor::new(bytes, 4096);
    let mut output = Vec::new();

    decompressor
        .read_to_end(&mut output)
        .context("Brotli decompression failed")?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use httpmock::prelude::*;
    use tempfile::tempdir;

    use nixsearch_core::ArtifactKind;
    use nixsearch_store::ArtifactStore;
    use nixsearch_test_support::{REF_SMALL, SOURCE_FIXTURES};

    use crate::{DownloadCompression, DownloadProducer, ProduceRequest, Producer};

    #[tokio::test]
    async fn download_producer_writes_artifact_to_store() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();
        let stored = store.get_artifact(&produced.artifact_ref).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.producer, "download");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(server.url("/options.json").as_str())
        );
        assert_eq!(
            stored.as_ref(),
            include_bytes!("../../../../fixtures/search-small/options.json").as_slice()
        );
    }

    #[tokio::test]
    async fn download_producer_decompresses_brotli_artifact() {
        let server = MockServer::start_async().await;
        let artifact_bytes = include_bytes!("../../../../fixtures/search-small/options.json");
        let compressed = brotli_compress(artifact_bytes.as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json.br");
                then.status(200).body(compressed);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json.br"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::Brotli,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();
        let stored = store.get_artifact(&produced.artifact_ref).await.unwrap();

        assert_eq!(stored.as_ref(), artifact_bytes.as_slice());
    }

    #[tokio::test]
    async fn download_producer_records_revision() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let _revision_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/revision");
                then.status(200).body("abc123\n");
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            Some(server.url("/revision")),
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.metadata.revision.as_deref(), Some("abc123"));
        assert!(produced.metadata.warnings.is_empty());
    }

    #[tokio::test]
    async fn download_producer_warns_when_revision_fetch_fails() {
        let server = MockServer::start_async().await;
        let artifact_bytes =
            Vec::from(include_bytes!("../../../../fixtures/search-small/options.json").as_slice());

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(200).body(artifact_bytes);
            })
            .await;

        let _revision_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/revision");
                then.status(500);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            Some(server.url("/revision")),
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.metadata.revision, None);
        assert!(
            produced
                .metadata
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to fetch download revision"))
        );
    }

    #[tokio::test]
    async fn download_producer_fails_when_artifact_fetch_fails() {
        let server = MockServer::start_async().await;

        let _artifact_mock = server
            .mock_async(|when, then| {
                when.method(GET).path("/options.json");
                then.status(404);
            })
            .await;

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let producer = DownloadProducer::new(
            server.url("/options.json"),
            ArtifactKind::OptionsJson,
            None,
            DownloadCompression::None,
        );
        let request = ProduceRequest {
            source: SOURCE_FIXTURES.into(),
            ref_id: REF_SMALL.into(),
        };

        let error = producer.produce(&store, &request).await.unwrap_err();

        assert!(format!("{error:#}").contains("HTTP error fetching"));
    }

    fn brotli_compress(bytes: &[u8]) -> Vec<u8> {
        let mut compressed = Vec::new();

        {
            let mut writer = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 22);
            writer.write_all(bytes).unwrap();
        }

        compressed
    }
}
