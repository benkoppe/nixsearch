use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;

use nixsearch_core::artifact::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};

use super::Producer;
use super::nix::{
    normalize_flake_ref, resolve_flake_revision, run_nix_build_installable,
    run_nix_build_installable_with_overrides,
};

#[derive(Debug, Clone)]
pub struct FlakeFileProducer {
    source_ref: String,
    attribute: String,
    output_path: PathBuf,
    artifact: ArtifactKind,
    fallback_inputs: BTreeMap<String, String>,
    producer_name: String,
}

impl FlakeFileProducer {
    pub fn new(
        source_ref: impl Into<String>,
        attribute: impl Into<String>,
        output_path: impl Into<PathBuf>,
        artifact: ArtifactKind,
        fallback_inputs: BTreeMap<String, String>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            attribute: attribute.into(),
            output_path: output_path.into(),
            artifact,
            fallback_inputs,
            producer_name: "flake-file".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for FlakeFileProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let source_ref = normalize_flake_ref(&self.source_ref)?;
        let installable = flake_installable(&source_ref, &self.attribute);

        let mut fallback_warning = None;

        let output_path = match run_nix_build_installable(&installable).await {
            Ok(output_path) => output_path,
            Err(pure_error) if self.fallback_inputs.is_empty() => {
                return Err(pure_error).with_context(|| {
                    format!(
                        "failed to build flake installable {:?} for source ref {:?}",
                        installable, self.source_ref
                    )
                });
            }
            Err(pure_error) => {
                tracing::warn!(
                    installable = %installable,
                    fallback_inputs = ?self.fallback_inputs,
                    error = %pure_error,
                    "pure flake build failed; retrying with fallback input overrides"
                );

                match run_nix_build_installable_with_overrides(&installable, &self.fallback_inputs)
                    .await
                {
                    Ok(output_path) => {
                        tracing::info!(
                            installable = %installable,
                            fallback_inputs = ?self.fallback_inputs,
                            "flake build succeeded with fallback input overrides"
                        );

                        fallback_warning = Some(format!(
                            "pure flake build failed; rebuilt with fallback input overrides: {}",
                            format_fallback_inputs(&self.fallback_inputs)
                        ));

                        output_path
                    }
                    Err(fallback_error) => {
                        return Err(fallback_error).with_context(|| {
                            format!(
                                "failed to build flake installable {:?} for source ref {:?} with fallback input overrides {:?}; pure build also failed: {pure_error:#}",
                                installable, self.source_ref, self.fallback_inputs
                            )
                        });
                    }
                }
            }
        };

        let artifact_path = output_path.join(&self.output_path);

        let bytes = tokio::fs::read(&artifact_path).await.with_context(|| {
            format!(
                "failed to read flake output artifact {}",
                artifact_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(self.artifact);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(source_ref.clone());

        match resolve_flake_revision(&source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {source_ref:?}: {error:#}"
            )),
        }

        if let Some(warning) = fallback_warning {
            metadata_input.warnings.push(warning);
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write flake output artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

fn flake_installable(source_ref: &str, attribute: &str) -> String {
    format!("{source_ref}#{attribute}")
}

fn format_fallback_inputs(inputs: &BTreeMap<String, String>) -> String {
    inputs
        .iter()
        .map(|(name, source_ref)| format!("{name}={source_ref}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn flake_installable_joins_ref_and_attribute() {
        assert_eq!(
            super::flake_installable("github:example/project/main", "docs-json"),
            "github:example/project/main#docs-json"
        );
    }
}
