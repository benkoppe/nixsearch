use std::fs;

use anyhow::{Context, Result};

use crate::generation::{
    GenerationScan, SeoVerifiedGeneration, StructurallyVerifiedGeneration,
    open_seo_verified_generation, open_structurally_verified_generation,
    validate_manifest_invariants,
};
use crate::integrity::{self as generation_integrity, GenerationIntegrityPaths};
use crate::manifest::{validate_generation_id, validate_index_schema_version};
use crate::search::SearchIndex;
use crate::seo_sidecar::SeoFactsArtifact;
use crate::store::{IndexStore, LeasedPublishedGeneration, PublishedGeneration};

#[derive(Debug, Clone)]
pub struct GenerationValidator {
    store: IndexStore,
}

impl GenerationValidator {
    pub fn new(store: IndexStore) -> Self {
        Self { store }
    }

    pub fn open_structurally_verified_published_generation(
        &self,
        generation: &PublishedGeneration,
    ) -> Result<StructurallyVerifiedGeneration> {
        validate_supplied_manifest(&generation.manifest)?;

        self.validate_integrity(generation, false)
            .context("failed to validate generation integrity metadata")?;

        open_structurally_verified_generation(
            &self.store.index_path(&generation.path),
            &generation.manifest,
        )
    }

    pub fn open_seo_verified_published_generation(
        &self,
        generation: &PublishedGeneration,
    ) -> Result<SeoVerifiedGeneration> {
        validate_supplied_manifest(&generation.manifest)?;

        let sidecar = SeoFactsArtifact::read_manifest_checked(generation)?;
        self.validate_integrity(generation, true)
            .context("failed to validate generation integrity metadata")?;

        open_seo_verified_generation(
            &self.store.index_path(&generation.path),
            &generation.manifest,
            sidecar,
        )
        .context("failed to validate SEO-verified generation")
    }

    pub fn open_seo_verified_leased_generation(
        &self,
        generation: &LeasedPublishedGeneration,
    ) -> Result<SeoVerifiedGeneration> {
        self.open_seo_verified_published_generation(generation.published_generation())
    }

    /// Opens an index after validating its integrity attestation.
    ///
    /// This is the serving fast path: integrity metadata is written only after
    /// generation publication/repair has performed the expensive structural
    /// validation, so request and startup paths can avoid re-scanning every
    /// document on each process start.
    pub fn open_integrity_attested_published_index(
        &self,
        generation: &PublishedGeneration,
        seo_sidecar_required: bool,
    ) -> Result<SearchIndex> {
        validate_supplied_manifest(&generation.manifest)?;

        self.validate_integrity(generation, seo_sidecar_required)
            .context("failed to validate generation integrity metadata")?;

        let index_path = self.store.index_path(&generation.path);
        SearchIndex::open(&index_path)
            .with_context(|| format!("failed to open search index {index_path}"))
    }

    /// Opens an SEO-capable generation using its integrity attestation.
    pub fn open_integrity_attested_seo_generation(
        &self,
        generation: &PublishedGeneration,
    ) -> Result<SeoVerifiedGeneration> {
        validate_supplied_manifest(&generation.manifest)?;

        let sidecar = SeoFactsArtifact::read_manifest_checked(generation)?;
        let index = self.open_integrity_attested_published_index(generation, true)?;
        let scan = GenerationScan {
            document_count: generation.manifest.document_count,
            seo_sidecar: sidecar.sidecar().clone(),
        };

        Ok(SeoVerifiedGeneration {
            index,
            sidecar: sidecar.into_index_verified_after_matching_scan(),
            scan,
        })
    }

    /// Validates a published generation without acquiring a generation lease.
    ///
    /// Callers must already prevent concurrent deletion, for example by holding the
    /// update lock. Request and startup paths should use leased validation instead.
    pub fn validate_seo_verified_published_generation_unleased(
        &self,
        generation: &PublishedGeneration,
    ) -> Result<()> {
        self.open_integrity_attested_seo_generation(generation)
            .map(|_| ())
    }

    pub fn validate_seo_verified_leased_generation(
        &self,
        generation: &LeasedPublishedGeneration,
    ) -> Result<()> {
        self.open_integrity_attested_seo_generation(generation.published_generation())
            .map(|_| ())
    }

    pub fn require_seo_sidecar_file_present(&self, generation: &PublishedGeneration) -> Result<()> {
        validate_supplied_manifest(&generation.manifest)?;

        let path = SeoFactsArtifact::path(&generation.path);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("failed to read SEO sidecar metadata {path}"))?;

        if !metadata.is_file() {
            anyhow::bail!("SEO sidecar is not a file {path}");
        }

        Ok(())
    }

    fn validate_integrity(
        &self,
        generation: &PublishedGeneration,
        seo_sidecar_required: bool,
    ) -> Result<()> {
        let paths = GenerationIntegrityPaths {
            manifest_path: self.store.manifest_path(&generation.path),
            seo_sidecar_path: SeoFactsArtifact::path(&generation.path),
            index_path: self.store.index_path(&generation.path),
            integrity_path: self.store.integrity_path(&generation.path),
        };

        generation_integrity::validate_integrity(
            &generation.manifest.generation_id,
            &paths,
            seo_sidecar_required,
        )
    }
}

fn validate_supplied_manifest(manifest: &crate::manifest::IndexGenerationManifest) -> Result<()> {
    validate_index_schema_version(manifest)
        .context("failed to validate supplied index generation manifest")?;
    validate_generation_id(manifest)
        .context("failed to validate supplied index generation manifest id")?;
    validate_manifest_invariants(manifest)
        .context("failed to validate supplied index generation manifest invariants")
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::{OptionDoc, SearchDocument};
    use nixsearch_core::ingest::IngestContext;
    use nixsearch_core::target::RefRole;

    use crate::annotation::EntryAnnotationIndex;
    use crate::manifest::{IndexGenerationManifest, IndexTargetManifest};
    use crate::search::SearchIndex;
    use crate::seo::SeoSidecarAccumulator;
    use crate::seo_sidecar::{IndexVerifiedSeoFacts, SeoFactsArtifact};
    use crate::store::{IndexStore, PublishedGeneration};

    use super::GenerationValidator;

    const SOURCE_FIXTURES: &str = "fixtures";
    const REF_SMALL: &str = "small";

    fn utf8_path(path: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
    }

    fn store_for(tempdir: &TempDir) -> IndexStore {
        IndexStore::new(utf8_path(tempdir.path().to_path_buf()))
    }

    fn context() -> IngestContext {
        IngestContext {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            revision: None,
            repo: None,
        }
    }

    fn publish_one_option_generation(store: &IndexStore) -> PublishedGeneration {
        let generation = store.create_generation_path().unwrap();
        let document = SearchDocument::Option(OptionDoc::new(&context(), "programs.git.enable"));
        let index = SearchIndex::create_or_replace(store.index_path(&generation)).unwrap();
        let mut annotations = EntryAnnotationIndex::new();
        annotations.observe(&document);
        let mut writer = index.writer().unwrap();
        writer
            .add_document(&document, &annotations.annotation_for(&document))
            .unwrap();
        writer.commit().unwrap();

        let manifest = IndexGenerationManifest::with_generated_at(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count: 1,
                artifact_hash: Some("fixture-hash".to_owned()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        let mut accumulator = SeoSidecarAccumulator::new();
        accumulator.observe(&document);
        let sidecar = accumulator.into_sidecar_for_manifest(&manifest);
        let generation = PublishedGeneration {
            path: generation,
            manifest,
        };

        let verified_sidecar =
            IndexVerifiedSeoFacts::new(sidecar, &generation.manifest, &index).unwrap();
        SeoFactsArtifact::write_index_verified(store, &generation, &verified_sidecar).unwrap();
        store
            .write_manifest(&generation.path, &generation.manifest)
            .unwrap();
        store.write_integrity(&generation, true).unwrap();
        generation
    }

    #[test]
    fn structural_fast_path_recomputes_seo_sidecar() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = publish_one_option_generation(&store);

        let complete = GenerationValidator::new(store)
            .open_structurally_verified_published_generation(&generation)
            .unwrap();

        assert_eq!(complete.scan.document_count, 1);
        assert_eq!(complete.scan.seo_sidecar.refs.len(), 1);
        assert_eq!(complete.scan.seo_sidecar.entries.len(), 1);
        assert_eq!(
            complete.scan.seo_sidecar.entries[0].name,
            "programs.git.enable"
        );
    }
}
