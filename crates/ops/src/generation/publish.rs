use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use nixsearch_index::generation::open_structurally_verified_generation;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::seo_sidecar::{ManifestCheckedSeoFacts, SeoFactsArtifact};
use nixsearch_index::store::{IndexStore, PublishedGeneration};

pub(crate) struct IncompleteGenerationGuard {
    path: Utf8PathBuf,
    cleanup_enabled: bool,
}

impl IncompleteGenerationGuard {
    pub(crate) fn create(index_store: &IndexStore) -> Result<Self> {
        Ok(Self {
            path: index_store.create_generation_path()?,
            cleanup_enabled: true,
        })
    }

    pub(crate) fn path(&self) -> &Utf8Path {
        &self.path
    }

    pub(crate) fn mark_published(&mut self) {
        self.cleanup_enabled = false;
    }
}

impl Drop for IncompleteGenerationGuard {
    fn drop(&mut self) {
        if self.cleanup_enabled
            && let Err(error) = std::fs::remove_dir_all(&self.path)
        {
            tracing::warn!(
                generation = %self.path,
                "failed to clean up incomplete index generation: {error}"
            );
        }
    }
}

pub(crate) fn write_generation_artifacts(
    index_store: &IndexStore,
    generation_path: &Utf8Path,
    manifest: IndexGenerationManifest,
) -> Result<()> {
    let published_generation = PublishedGeneration {
        path: generation_path.to_owned(),
        manifest: manifest.clone(),
    };

    index_store.write_manifest(generation_path, &manifest)?;

    let structural = open_structurally_verified_generation(
        &index_store.index_path(generation_path),
        &published_generation.manifest,
    )?;
    let sidecar =
        ManifestCheckedSeoFacts::new(structural.scan.seo_sidecar, &published_generation.manifest)?;
    SeoFactsArtifact::write_manifest_checked_without_index_validation(
        index_store,
        &published_generation,
        &sidecar,
    )?;
    index_store.write_integrity(&published_generation, true)?;

    Ok(())
}

pub(crate) fn publish_completed_generation(
    index_store: &IndexStore,
    generation_path: &Utf8Path,
    total_documents: usize,
) -> Result<()> {
    index_store.publish(generation_path)?;

    tracing::info!(
        generation = %generation_path.as_str(),
        documents = total_documents,
        "published index generation"
    );

    Ok(())
}
