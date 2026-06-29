use std::fs;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};

use nixsearch_core::document::SearchDocument;

use crate::atomic_file;
use crate::manifest::{
    IndexGenerationManifest, validate_generation_id, validate_index_schema_version,
};
use crate::search::SearchIndex;
use crate::seo::{SeoSidecar, SeoSidecarAccumulator};
use crate::store::{IndexStore, PublishedGeneration};

const SEO_SIDECAR_FILE: &str = "seo-facts.json";
const SEO_SIDECAR_TEMP_PREFIX: &str = "seo-facts.json.tmp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestCheckedSeoFacts {
    sidecar: SeoSidecar,
}

impl ManifestCheckedSeoFacts {
    pub fn new(sidecar: SeoSidecar, manifest: &IndexGenerationManifest) -> Result<Self> {
        validate_supplied_manifest(manifest)?;
        sidecar.validate_for_manifest(manifest)?;

        Ok(Self { sidecar })
    }

    pub fn sidecar(&self) -> &SeoSidecar {
        &self.sidecar
    }

    pub fn into_sidecar(self) -> SeoSidecar {
        self.sidecar
    }

    #[cfg(test)]
    pub(crate) fn from_manifest_assumed_checked(sidecar: SeoSidecar) -> Self {
        Self { sidecar }
    }

    pub(crate) fn into_index_verified_after_matching_scan(self) -> IndexVerifiedSeoFacts {
        IndexVerifiedSeoFacts {
            sidecar: self.sidecar,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexVerifiedSeoFacts {
    sidecar: SeoSidecar,
}

impl IndexVerifiedSeoFacts {
    pub fn new(
        sidecar: SeoSidecar,
        manifest: &IndexGenerationManifest,
        index: &SearchIndex,
    ) -> Result<Self> {
        validate_supplied_manifest(manifest)?;
        sidecar.validate_for_index(manifest, index)?;

        Ok(Self { sidecar })
    }

    pub fn sidecar(&self) -> &SeoSidecar {
        &self.sidecar
    }

    pub fn into_sidecar(self) -> SeoSidecar {
        self.sidecar
    }

    pub(crate) fn from_index_derived_facts(sidecar: SeoSidecar) -> Self {
        Self { sidecar }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SeoFactsArtifact;

impl SeoFactsArtifact {
    pub fn path(generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join(SEO_SIDECAR_FILE)
    }

    pub(crate) fn derive_from_index(
        manifest: &IndexGenerationManifest,
        index: &SearchIndex,
    ) -> Result<SeoSidecar> {
        Ok(SeoSidecarAccumulator::from_index(index)?.into_sidecar_for_manifest(manifest))
    }

    pub(crate) fn derive_from_documents<'a>(
        manifest: &IndexGenerationManifest,
        documents: impl IntoIterator<Item = &'a SearchDocument>,
    ) -> SeoSidecar {
        let mut accumulator = SeoSidecarAccumulator::new();

        for document in documents {
            accumulator.observe(document);
        }

        accumulator.into_sidecar_for_manifest(manifest)
    }

    pub fn write_index_verified(
        store: &IndexStore,
        generation: &PublishedGeneration,
        sidecar: &IndexVerifiedSeoFacts,
    ) -> Result<()> {
        let generation_path = store.validate_generation_path(&generation.path)?;
        let path = Self::path(&generation_path);

        validate_supplied_manifest(&generation.manifest)?;

        let index_path = store.index_path(&generation_path);
        let index = SearchIndex::open(&index_path)
            .with_context(|| format!("failed to open search index {index_path}"))?;

        sidecar
            .sidecar()
            .validate_for_index(&generation.manifest, &index)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Self::write_serialized(&generation_path, &path, sidecar.sidecar())
    }

    pub fn write_derived_index_verified(
        store: &IndexStore,
        generation: &PublishedGeneration,
    ) -> Result<IndexVerifiedSeoFacts> {
        let generation_path = store.validate_generation_path(&generation.path)?;
        let path = Self::path(&generation_path);

        validate_supplied_manifest(&generation.manifest)?;

        let index_path = store.index_path(&generation_path);
        let index = SearchIndex::open(&index_path)
            .with_context(|| format!("failed to open search index {index_path}"))?;
        let sidecar = Self::derive_from_index(&generation.manifest, &index)?;

        sidecar
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Self::write_serialized(&generation_path, &path, &sidecar)?;

        Ok(IndexVerifiedSeoFacts::from_index_derived_facts(sidecar))
    }

    pub fn write_manifest_checked_without_index_validation(
        store: &IndexStore,
        generation: &PublishedGeneration,
        sidecar: &ManifestCheckedSeoFacts,
    ) -> Result<()> {
        let generation_path = store.validate_generation_path(&generation.path)?;
        let path = Self::path(&generation_path);

        validate_supplied_manifest(&generation.manifest)?;
        sidecar
            .sidecar()
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Self::write_serialized(&generation_path, &path, sidecar.sidecar())
    }

    fn write_serialized(
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

    pub fn read_manifest_checked(
        generation: &PublishedGeneration,
    ) -> Result<ManifestCheckedSeoFacts> {
        validate_generation_id(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;
        validate_index_schema_version(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        let path = Self::path(&generation.path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read SEO sidecar {}", path.as_str()))?;

        let sidecar: SeoSidecar = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse SEO sidecar {}", path.as_str()))?;

        let sidecar = ManifestCheckedSeoFacts::new(sidecar, &generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Ok(sidecar)
    }

    pub fn read_index_verified(
        generation: &PublishedGeneration,
        index: &SearchIndex,
    ) -> Result<IndexVerifiedSeoFacts> {
        let sidecar = Self::read_manifest_checked(generation)?.into_sidecar();

        IndexVerifiedSeoFacts::new(sidecar, &generation.manifest, index).with_context(|| {
            format!(
                "failed to validate SEO sidecar {} against indexed documents",
                Self::path(&generation.path).as_str()
            )
        })
    }
}

fn validate_supplied_manifest(manifest: &IndexGenerationManifest) -> Result<()> {
    validate_generation_id(manifest)
        .context("failed to validate supplied index generation manifest")?;
    validate_index_schema_version(manifest)
        .context("failed to validate supplied index generation manifest")?;

    Ok(())
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
    use crate::manifest::{IndexGenerationManifest, IndexTargetManifest, refresh_generation_id};
    use crate::search::SearchIndex;
    use crate::seo::{SEO_SIDECAR_SCHEMA_VERSION, SeoRefFacts, SeoSidecar, SeoSidecarAccumulator};
    use crate::store::{IndexStore, PublishedGeneration};

    use super::{IndexVerifiedSeoFacts, ManifestCheckedSeoFacts, SeoFactsArtifact};

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

    fn options_manifest(document_count: usize) -> IndexGenerationManifest {
        IndexGenerationManifest::new(
            document_count,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count,
                artifact_hash: Some("fixture-hash".to_owned()),
                revision: None,
            }],
        )
        .unwrap()
    }

    fn old_schema_manifest(document_count: usize) -> IndexGenerationManifest {
        let mut manifest = options_manifest(document_count);
        manifest.schema_version = 2;
        refresh_generation_id(&mut manifest).unwrap();
        manifest
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
        let generation = PublishedGeneration {
            path: generation,
            manifest,
        };

        SeoFactsArtifact::write_derived_index_verified(store, &generation).unwrap();
        generation
    }

    #[test]
    fn read_rejects_unsupported_manifest_schema_before_file_read() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = PublishedGeneration {
            path: store.create_generation_path().unwrap(),
            manifest: old_schema_manifest(1),
        };

        let error = SeoFactsArtifact::read_manifest_checked(&generation).unwrap_err();

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
    }

    #[test]
    fn write_rejects_unsupported_manifest_schema_before_index_open() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = PublishedGeneration {
            path: store.create_generation_path().unwrap(),
            manifest: old_schema_manifest(0),
        };
        let sidecar = SeoSidecarAccumulator::new().into_sidecar_for_manifest(&generation.manifest);
        let sidecar = IndexVerifiedSeoFacts::from_index_derived_facts(sidecar);

        let error =
            SeoFactsArtifact::write_index_verified(&store, &generation, &sidecar).unwrap_err();

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
    }

    #[test]
    fn write_without_index_validation_rejects_unsupported_manifest_schema_before_write() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = PublishedGeneration {
            path: store.create_generation_path().unwrap(),
            manifest: old_schema_manifest(0),
        };
        let sidecar = SeoSidecarAccumulator::new().into_sidecar_for_manifest(&generation.manifest);
        let sidecar = ManifestCheckedSeoFacts::from_manifest_assumed_checked(sidecar);

        let error = SeoFactsArtifact::write_manifest_checked_without_index_validation(
            &store,
            &generation,
            &sidecar,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
    }

    #[test]
    fn write_rejects_mismatched_manifest_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let mut manifest = IndexGenerationManifest::with_generated_at(
            0,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count: 0,
                artifact_hash: Some("fixture-hash".to_owned()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let sidecar = SeoSidecar {
            schema_version: SEO_SIDECAR_SCHEMA_VERSION,
            generation_id: manifest.generation_id.clone(),
            refs: vec![SeoRefFacts {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                total_supported_indexed_count: 0,
                package_supported_count: 0,
                option_supported_count: 0,
                package_eligible_count: 0,
                option_eligible_count: 0,
            }],
            entries: Vec::new(),
        };

        manifest.generation_id = "sha256:wrong".to_owned();

        let generation = PublishedGeneration {
            path: generation,
            manifest,
        };
        let sidecar = ManifestCheckedSeoFacts::from_manifest_assumed_checked(sidecar);
        let error = SeoFactsArtifact::write_manifest_checked_without_index_validation(
            &store,
            &generation,
            &sidecar,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn write_rejects_forged_entry_name() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = publish_one_option_generation(&store);
        let mut sidecar = SeoFactsArtifact::read_manifest_checked(&generation)
            .unwrap()
            .into_sidecar();

        sidecar.entries[0].name = "not-real".to_owned();

        let error = IndexVerifiedSeoFacts::new(
            sidecar,
            &generation.manifest,
            &SearchIndex::open(store.index_path(&generation.path)).unwrap(),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("entry facts do not match indexed documents"));
    }
}
