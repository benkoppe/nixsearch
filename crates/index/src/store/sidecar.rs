use std::fs;

use anyhow::{Context, Result};
use camino::Utf8Path;

use crate::atomic_file;
use crate::manifest::{validate_generation_id, validate_index_schema_version};
use crate::search::SearchIndex;
use crate::seo::SeoSidecar;

use super::layout::SEO_SIDECAR_TEMP_PREFIX;
use super::{IndexStore, PublishedGeneration};

impl IndexStore {
    pub fn write_seo_sidecar(
        &self,
        generation: &PublishedGeneration,
        sidecar: &SeoSidecar,
    ) -> Result<()> {
        let generation_path = self.validate_generation_path(&generation.path)?;
        let path = self.seo_sidecar_path(&generation_path);

        validate_generation_id(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;
        validate_index_schema_version(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        let index_path = self.index_path(&generation_path);
        let index = SearchIndex::open(&index_path)
            .with_context(|| format!("failed to open search index {index_path}"))?;

        sidecar
            .validate_for_index(&generation.manifest, &index)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        self.write_serialized_seo_sidecar(&generation_path, &path, sidecar)
    }

    pub fn write_validated_seo_sidecar_unchecked(
        &self,
        generation: &PublishedGeneration,
        sidecar: &SeoSidecar,
    ) -> Result<()> {
        let generation_path = self.validate_generation_path(&generation.path)?;
        let path = self.seo_sidecar_path(&generation_path);

        validate_generation_id(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;
        validate_index_schema_version(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;
        sidecar
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        self.write_serialized_seo_sidecar(&generation_path, &path, sidecar)
    }

    fn write_serialized_seo_sidecar(
        &self,
        generation_path: &Utf8Path,
        path: &Utf8Path,
        sidecar: &SeoSidecar,
    ) -> Result<()> {
        let bytes =
            serde_json::to_vec_pretty(sidecar).context("failed to serialize SEO sidecar")?;
        let temp_path =
            atomic_file::create_temp_file(generation_path, SEO_SIDECAR_TEMP_PREFIX, &bytes)?;

        if let Err(error) = fs::rename(&temp_path, path) {
            let _ = fs::remove_file(&temp_path);

            return Err(error)
                .with_context(|| format!("failed to write SEO sidecar {}", path.as_str()));
        }

        atomic_file::sync_dir(generation_path)?;

        Ok(())
    }

    pub fn require_seo_sidecar_file(&self, generation: &PublishedGeneration) -> Result<()> {
        validate_index_schema_version(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        let path = self.seo_sidecar_path(&generation.path);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to read SEO sidecar metadata {path}"))?;

        if !metadata.is_file() {
            anyhow::bail!("SEO sidecar is not a file {path}");
        }

        Ok(())
    }

    pub fn read_seo_sidecar(&self, generation: &PublishedGeneration) -> Result<SeoSidecar> {
        validate_index_schema_version(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        let path = self.seo_sidecar_path(&generation.path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read SEO sidecar {}", path.as_str()))?;

        let sidecar: SeoSidecar = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse SEO sidecar {}", path.as_str()))?;

        sidecar
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Ok(sidecar)
    }
}
