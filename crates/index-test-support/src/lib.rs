//! Test helpers that require `nixsearch-index`.
//!
//! Kept separate from `nixsearch-test-support` so `nixsearch-index` can use
//! the base test fixtures in its own tests without creating a dependency cycle.

use camino::{Utf8Path, Utf8PathBuf};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::SearchDocument;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_index::search::SearchIndex;
use nixsearch_index::store::IndexStore;
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
                document_count: options_count,
                artifact_hash: Some("fixture-options-hash".to_owned()),
                revision: Some("fixture-revision".to_owned()),
            },
            IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::PackagesJson,
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

    let index = SearchIndex::create_or_replace(&generation).unwrap();
    let mut writer = index.writer().unwrap();

    for doc in &documents {
        writer.add_document(doc).unwrap();
    }

    writer.commit().unwrap();

    let manifest =
        IndexGenerationManifest::with_generated_at(documents.len(), targets, generated_at);

    store.write_manifest(&generation, &manifest).unwrap();
    store.publish(&generation).unwrap();
    store.current_path().unwrap()
}

pub fn index_target(
    source: &str,
    ref_id: &str,
    artifact_kind: ArtifactKind,
    document_count: usize,
) -> IndexTargetManifest {
    IndexTargetManifest {
        source: source.to_owned(),
        ref_id: ref_id.to_owned(),
        artifact_kind,
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
