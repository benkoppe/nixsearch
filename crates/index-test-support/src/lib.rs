//! Test helpers that require `nixsearch-index`.
//!
//! Kept separate from `nixsearch-test-support` so `nixsearch-index` can use
//! the base test fixtures in its own tests without creating a dependency cycle.

use camino::{Utf8Path, Utf8PathBuf};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::SearchDocument;
use nixsearch_core::target::{RefRole, TargetCapabilities};
use nixsearch_index::annotation::EntryAnnotationIndex;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::search::SearchIndex;
use nixsearch_index::seo::SeoSidecar;
use nixsearch_index::seo_sidecar::SeoFactsArtifact;
use nixsearch_index::store::{IndexStore, PublishedGeneration};
use nixsearch_test_support::{
    REF_SMALL, SOURCE_FIXTURES, canonical_documents, ingest_context_for, option_doc_for,
};

pub fn publish_canonical_index(index_root: &Utf8Path) -> Utf8PathBuf {
    publish_canonical_mixed_index(index_root)
}

pub fn publish_canonical_index_with_generated_at(
    index_root: &Utf8Path,
    generated_at: time::OffsetDateTime,
) -> Utf8PathBuf {
    publish_canonical_mixed_index_with_generated_at(index_root, generated_at)
}

pub fn publish_canonical_options_index(index_root: &Utf8Path) -> Utf8PathBuf {
    publish_canonical_options_index_with_generated_at(index_root, time::OffsetDateTime::now_utc())
}

pub fn publish_canonical_options_index_with_generated_at(
    index_root: &Utf8Path,
    generated_at: time::OffsetDateTime,
) -> Utf8PathBuf {
    let documents = canonical_documents()
        .into_iter()
        .filter(|document| matches!(document, SearchDocument::Option(_)))
        .collect::<Vec<_>>();
    publish_documents_with_manifest_targets(
        index_root,
        generated_at,
        documents,
        vec![IndexTargetManifest {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            artifact_kind: ArtifactKind::OptionsJson,
            target_role: RefRole::Search,
            indexes_search_documents: true,
            document_count: 4,
            artifact_hash: Some("fixture-options-hash".to_owned()),
            revision: Some("fixture-revision".to_owned()),
        }],
    )
}

pub fn publish_canonical_mixed_index(index_root: &Utf8Path) -> Utf8PathBuf {
    publish_canonical_mixed_index_with_generated_at(index_root, time::OffsetDateTime::now_utc())
}

pub fn publish_canonical_mixed_index_with_generated_at(
    index_root: &Utf8Path,
    generated_at: time::OffsetDateTime,
) -> Utf8PathBuf {
    let documents = canonical_documents();
    let options_count = documents
        .iter()
        .filter(|document| matches!(document, SearchDocument::Option(_)))
        .count();
    let packages_count = documents
        .iter()
        .filter(|document| matches!(document, SearchDocument::Package(_)))
        .count();

    publish_documents_with_manifest_targets(
        index_root,
        generated_at,
        documents,
        vec![
            IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count: options_count,
                artifact_hash: Some("fixture-options-hash".to_owned()),
                revision: Some("fixture-revision".to_owned()),
            },
            IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::PackagesJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count: packages_count,
                artifact_hash: Some("fixture-packages-hash".to_owned()),
                revision: Some("fixture-revision".to_owned()),
            },
        ],
    )
}

pub fn publish_fixture_options_index_for_refs(
    index_root: &Utf8Path,
    ref_ids: &[&str],
) -> Utf8PathBuf {
    let documents = ref_ids
        .iter()
        .map(|ref_id| {
            option_doc_for(
                &ingest_context_for(SOURCE_FIXTURES, ref_id),
                &format!("programs.{ref_id}.git.enable"),
                &format!("Fixture option for {ref_id}."),
            )
        })
        .collect::<Vec<_>>();
    let targets = ref_ids
        .iter()
        .map(|ref_id| options_target(SOURCE_FIXTURES, ref_id, 1))
        .collect::<Vec<_>>();

    publish_documents_with_manifest_targets(
        index_root,
        time::OffsetDateTime::now_utc(),
        documents,
        targets,
    )
}

pub fn publish_documents_with_manifest_targets(
    index_root: &Utf8Path,
    generated_at: time::OffsetDateTime,
    documents: Vec<SearchDocument>,
    targets: Vec<IndexTargetManifest>,
) -> Utf8PathBuf {
    let store = IndexStore::new(index_root);
    let generation = store.create_generation_path().unwrap();

    let index = SearchIndex::create_or_replace(store.index_path(&generation)).unwrap();
    let mut writer = index.writer().unwrap();
    let mut annotations = EntryAnnotationIndex::new();

    for doc in &documents {
        annotations.observe(doc);
    }

    for doc in &documents {
        writer
            .add_document(doc, &annotations.annotation_for(doc))
            .unwrap();
    }

    writer.commit().unwrap();

    let manifest =
        IndexGenerationManifest::with_generated_at(documents.len(), targets, generated_at).unwrap();

    let published_generation = PublishedGeneration {
        path: generation.clone(),
        manifest: manifest.clone(),
    };

    SeoFactsArtifact::write_derived_index_verified(&store, &published_generation).unwrap();
    store.write_manifest(&generation, &manifest).unwrap();
    store.write_integrity(&published_generation, true).unwrap();
    store.publish(&generation).unwrap();
    store.current_path().unwrap()
}

/// Writes a sidecar directly, bypassing production validation.
///
/// Use only in tests that intentionally simulate corrupt on-disk sidecars.
pub fn write_raw_seo_sidecar(
    _store: &IndexStore,
    generation: &PublishedGeneration,
    sidecar: &SeoSidecar,
) {
    std::fs::write(
        SeoFactsArtifact::path(&generation.path),
        serde_json::to_vec_pretty(sidecar).unwrap(),
    )
    .unwrap();
}

/// Writes a manifest directly, bypassing production validation.
///
/// Use only in tests that intentionally simulate corrupt on-disk manifests. Call
/// `refresh_generation_id` before using this helper when the test needs a valid
/// generation id.
pub fn write_raw_manifest(
    store: &IndexStore,
    generation: &PublishedGeneration,
    manifest: &IndexGenerationManifest,
) {
    std::fs::write(
        store.manifest_path(&generation.path),
        serde_json::to_vec_pretty(manifest).unwrap(),
    )
    .unwrap();
}

pub fn index_target(
    source: &str,
    ref_id: &str,
    artifact_kind: ArtifactKind,
    document_count: usize,
) -> IndexTargetManifest {
    let target_role = RefRole::default_for_artifact_kind(artifact_kind);

    IndexTargetManifest {
        source: source.to_owned(),
        ref_id: ref_id.to_owned(),
        artifact_kind,
        target_role,
        indexes_search_documents: TargetCapabilities::new(target_role, artifact_kind)
            .indexes_search_documents(),
        document_count,
        artifact_hash: None,
        revision: None,
    }
}

pub fn options_target(source: &str, ref_id: &str, document_count: usize) -> IndexTargetManifest {
    index_target(source, ref_id, ArtifactKind::OptionsJson, document_count)
}

pub fn assert_canonical_options_manifest_targets(manifest: &IndexGenerationManifest) {
    assert_eq!(manifest.targets.len(), 1);

    let options = &manifest.targets[0];
    assert_eq!(options.artifact_kind, ArtifactKind::OptionsJson);
    assert_eq!(options.source, SOURCE_FIXTURES);
    assert_eq!(options.ref_id, REF_SMALL);
    assert_eq!(options.document_count, 4);
}

pub fn assert_canonical_manifest_targets(manifest: &IndexGenerationManifest) {
    assert_eq!(manifest.targets.len(), 2);

    let options = manifest
        .targets
        .iter()
        .find(|target| target.artifact_kind == ArtifactKind::OptionsJson)
        .expect("expected options target");
    assert_eq!(options.source, SOURCE_FIXTURES);
    assert_eq!(options.ref_id, REF_SMALL);
    assert_eq!(options.document_count, 4);

    let packages = manifest
        .targets
        .iter()
        .find(|target| target.artifact_kind == ArtifactKind::PackagesJson)
        .expect("expected packages target");
    assert_eq!(packages.source, SOURCE_FIXTURES);
    assert_eq!(packages.ref_id, REF_SMALL);
    assert_eq!(packages.document_count, 3);
}
