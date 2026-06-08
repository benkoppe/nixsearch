use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::process::Command;

use nixsearch_core::artifact::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};
use crate::error::NixCommandFailure;

use super::Producer;
use super::nix::{normalize_nix_path_source, resolve_flake_revision};

#[derive(Debug, Clone)]
pub struct NixBuildOptionsJsonProducer {
    source_ref: String,
    nix_path_name: String,
    attribute: String,
    import_path: String,
    output_path: String,
    producer_name: String,
}

impl NixBuildOptionsJsonProducer {
    pub fn new(
        source_ref: impl Into<String>,
        nix_path_name: impl Into<String>,
        attribute: impl Into<String>,
        import_path: impl Into<String>,
        output_path: impl Into<String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            nix_path_name: nix_path_name.into(),
            attribute: attribute.into(),
            import_path: import_path.into(),
            output_path: output_path.into(),
            producer_name: "nix-build-options-json".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for NixBuildOptionsJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let nix_path_source = normalize_nix_path_source(&self.source_ref);
        let expression = format!("<{}/{}>", self.nix_path_name, self.import_path);

        let output = Command::new("nix-build")
            .arg("--no-build-output")
            .arg(&expression)
            .arg("--attr")
            .arg(&self.attribute)
            .arg("--no-out-link")
            .arg("-I")
            .arg(format!("{}={nix_path_source}", self.nix_path_name))
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to run nix-build for source ref {:?}",
                    self.source_ref
                )
            })?;

        if !output.status.success() {
            return Err(NixCommandFailure {
                command: "nix-build",
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
            }
            .into());
        }

        let result_path = String::from_utf8(output.stdout)
            .context("nix-build stdout was not valid UTF-8")?
            .trim()
            .to_owned();

        if result_path.is_empty() {
            anyhow::bail!("nix-build succeeded but did not print an output path");
        }

        let artifact_path = PathBuf::from(result_path).join(&self.output_path);

        let bytes = tokio::fs::read(&artifact_path).await.with_context(|| {
            format!(
                "failed to read generated options JSON {}",
                artifact_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(self.source_ref.clone());

        match resolve_flake_revision(&self.source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {:?}: {error:#}",
                self.source_ref
            )),
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write options artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}
