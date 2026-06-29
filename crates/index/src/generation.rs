use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use camino::Utf8Path;

use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::SearchDocument;
use nixsearch_core::target::TargetCapabilities;

use crate::annotation::EntryAnnotationIndex;
use crate::manifest::{
    IndexGenerationManifest, validate_generation_id, validate_index_schema_version,
};
use crate::search::{IndexedSearchDocument, SearchIndex};
use crate::seo::SeoSidecar;
use crate::seo_sidecar::{IndexVerifiedSeoFacts, ManifestCheckedSeoFacts, SeoFactsArtifact};

pub struct StructurallyVerifiedGeneration {
    pub index: SearchIndex,
    pub scan: GenerationScan,
}

pub struct SeoVerifiedGeneration {
    pub index: SearchIndex,
    pub sidecar: IndexVerifiedSeoFacts,
    pub scan: GenerationScan,
}

#[derive(Debug, Clone)]
pub struct GenerationScan {
    pub document_count: usize,
    pub seo_sidecar: SeoSidecar,
}

pub fn open_structurally_verified_generation(
    path: &Utf8Path,
    manifest: &IndexGenerationManifest,
) -> Result<StructurallyVerifiedGeneration> {
    validate_index_schema_version(manifest).context("failed to validate index schema version")?;
    validate_manifest_invariants(manifest)?;
    validate_generation_id(manifest).context("failed to validate index generation manifest id")?;

    let index =
        SearchIndex::open(path).with_context(|| format!("failed to open search index {path}"))?;
    let scan = scan_generation(&index, manifest)?;

    Ok(StructurallyVerifiedGeneration { index, scan })
}

pub fn open_seo_verified_generation(
    path: &Utf8Path,
    manifest: &IndexGenerationManifest,
    sidecar: ManifestCheckedSeoFacts,
) -> Result<SeoVerifiedGeneration> {
    let verified = open_structurally_verified_generation(path, manifest)?;

    if sidecar.sidecar() != &verified.scan.seo_sidecar {
        bail!("SEO sidecar facts do not match indexed documents");
    }

    Ok(SeoVerifiedGeneration {
        index: verified.index,
        sidecar: sidecar.into_index_verified_after_matching_scan(),
        scan: verified.scan,
    })
}

pub fn validate_manifest_invariants(manifest: &IndexGenerationManifest) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut target_sum = 0usize;

    for target in &manifest.targets {
        if !seen.insert((
            target.source.as_str(),
            target.ref_id.as_str(),
            target.artifact_kind,
        )) {
            bail!(
                "duplicate index generation target {}/{}/{}",
                target.source,
                target.ref_id,
                target.artifact_kind.as_str()
            );
        }

        let expected_indexes_search_documents =
            TargetCapabilities::new(target.target_role, target.artifact_kind)
                .indexes_search_documents();
        if target.indexes_search_documents != expected_indexes_search_documents {
            bail!(
                "index generation target {}/{}/{} role {} has inconsistent indexes_search_documents {} (expected {})",
                target.source,
                target.ref_id,
                target.artifact_kind.as_str(),
                target.target_role.as_str(),
                target.indexes_search_documents,
                expected_indexes_search_documents
            );
        }

        if !target.indexes_search_documents && target.document_count != 0 {
            bail!(
                "non-indexing index generation target {}/{}/{} role {} cannot report indexed document_count {}",
                target.source,
                target.ref_id,
                target.artifact_kind.as_str(),
                target.target_role.as_str(),
                target.document_count
            );
        }

        target_sum = target_sum
            .checked_add(target.document_count)
            .context("index generation target document counts overflowed")?;
    }

    if manifest.document_count != target_sum {
        bail!(
            "index generation document_count mismatch: manifest {}, target sum {}",
            manifest.document_count,
            target_sum
        );
    }

    Ok(())
}

fn scan_generation(
    index: &SearchIndex,
    manifest: &IndexGenerationManifest,
) -> Result<GenerationScan> {
    let documents = index.scan_indexed_documents()?;
    if documents.len() != manifest.document_count {
        bail!(
            "indexed document count mismatch: index {}, manifest {}",
            documents.len(),
            manifest.document_count
        );
    }

    let expected = manifest_target_counts(manifest);
    let mut actual = BTreeMap::<TargetCountKey, usize>::new();
    let mut annotations = EntryAnnotationIndex::new();

    for document in &documents {
        annotations.observe(&document.document);
    }

    for indexed in &documents {
        validate_annotation(indexed, &annotations)?;
        let key = target_key_for_document(&indexed.document)?;
        if !expected.contains_key(&key) {
            let common = indexed.document.common();
            bail!(
                "indexed document {}/{}/{} has no matching manifest target {}",
                common.source,
                common.ref_id,
                common.name,
                key.artifact_kind.as_str()
            );
        }

        *actual.entry(key).or_default() += 1;
    }

    for (key, expected_count) in expected {
        let actual_count = actual.remove(&key).unwrap_or(0);
        if actual_count != expected_count {
            bail!(
                "manifest target count mismatch for {}/{}/{}: index {}, manifest {}",
                key.source,
                key.ref_id,
                key.artifact_kind.as_str(),
                actual_count,
                expected_count
            );
        }
    }

    if let Some((key, count)) = actual.into_iter().next() {
        bail!(
            "indexed documents for unmanifested target {}/{}/{}: {}",
            key.source,
            key.ref_id,
            key.artifact_kind.as_str(),
            count
        );
    }

    Ok(GenerationScan {
        document_count: documents.len(),
        seo_sidecar: SeoFactsArtifact::derive_from_documents(
            manifest,
            documents.iter().map(|indexed| &indexed.document),
        ),
    })
}

fn manifest_target_counts(manifest: &IndexGenerationManifest) -> BTreeMap<TargetCountKey, usize> {
    manifest
        .targets
        .iter()
        .map(|target| {
            (
                TargetCountKey {
                    source: target.source.clone(),
                    ref_id: target.ref_id.clone(),
                    artifact_kind: target.artifact_kind,
                },
                target.document_count,
            )
        })
        .collect()
}

fn target_key_for_document(document: &SearchDocument) -> Result<TargetCountKey> {
    let common = document.common();
    let variant_kind = document.variant_kind();
    let artifact_kind =
        ArtifactKind::for_indexed_document_kind(&variant_kind).with_context(|| {
            format!(
                "indexed document {}/{}/{} has unsupported document kind {}",
                common.source,
                common.ref_id,
                common.name,
                variant_kind.as_str()
            )
        })?;

    Ok(TargetCountKey {
        source: common.source.clone(),
        ref_id: common.ref_id.clone(),
        artifact_kind,
    })
}

fn validate_annotation(
    indexed: &IndexedSearchDocument,
    annotations: &EntryAnnotationIndex,
) -> Result<()> {
    let expected = annotations.annotation_for(&indexed.document);
    if indexed.annotation != expected {
        let common = indexed.document.common();
        bail!(
            "stored entry annotation mismatch for {}/{}/{}",
            common.source,
            common.ref_id,
            common.name
        );
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TargetCountKey {
    source: String,
    ref_id: String,
    artifact_kind: ArtifactKind,
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::target::{RefRole, TargetCapabilities};

    use crate::manifest::{IndexGenerationManifest, IndexTargetManifest, refresh_generation_id};

    use super::{open_structurally_verified_generation, validate_manifest_invariants};

    const SOURCE: &str = "fixtures";
    const REF: &str = "small";

    #[test]
    fn manifest_invariants_reject_nonzero_artifact_only_targets() {
        let manifest = IndexGenerationManifest::with_generated_at(
            2,
            vec![target(ArtifactKind::FlakeInfoJson, 2)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let error = validate_manifest_invariants(&manifest).unwrap_err();

        assert!(format!("{error:#}").contains("non-indexing"));
    }

    #[test]
    fn manifest_invariants_accept_zero_count_artifact_only_targets() {
        let manifest = IndexGenerationManifest::with_generated_at(
            0,
            vec![target(ArtifactKind::FlakeInfoJson, 0)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        validate_manifest_invariants(&manifest).unwrap();
    }

    #[test]
    fn manifest_invariants_still_reject_document_count_mismatch() {
        let manifest = IndexGenerationManifest::with_generated_at(
            1,
            vec![target(ArtifactKind::OptionsJson, 2)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let error = validate_manifest_invariants(&manifest).unwrap_err();

        assert!(format!("{error:#}").contains("document_count mismatch"));
    }

    #[test]
    fn structurally_opening_generation_rejects_unsupported_schema_before_index_scan() {
        let tempdir = tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(tempdir.path().to_path_buf()).unwrap();
        let mut manifest = IndexGenerationManifest::with_generated_at(
            1,
            vec![target(ArtifactKind::OptionsJson, 1)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        manifest.schema_version = 2;
        refresh_generation_id(&mut manifest).unwrap();

        let error = match open_structurally_verified_generation(&path, &manifest) {
            Ok(_) => panic!("expected unsupported schema error"),
            Err(error) => error,
        };

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
    }

    fn target(artifact_kind: ArtifactKind, document_count: usize) -> IndexTargetManifest {
        let target_role = RefRole::default_for_artifact_kind(artifact_kind);

        IndexTargetManifest {
            source: SOURCE.to_owned(),
            ref_id: REF.to_owned(),
            artifact_kind,
            target_role,
            indexes_search_documents: TargetCapabilities::new(target_role, artifact_kind)
                .indexes_search_documents(),
            document_count,
            artifact_hash: None,
            revision: None,
        }
    }
}
