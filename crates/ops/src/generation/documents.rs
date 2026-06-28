use anyhow::Result;

use nixsearch_core::document::SearchDocument;
use nixsearch_index::annotation::EntryAnnotationIndex;
use nixsearch_index::manifest::{IndexGenerationManifest, IndexTargetManifest};
use nixsearch_store::ArtifactStore;

use crate::consume::consume_target;
use crate::spool::{DocumentSpool, DocumentSpoolWriter};
use crate::targets::TargetKey;

use super::production::ProducedTarget;

pub(crate) struct SpooledDocumentSetBuilder {
    spool: DocumentSpool,
    writer: DocumentSpoolWriter,
    annotations: EntryAnnotationIndex,
    total_documents: usize,
    manifest_targets: Vec<IndexTargetManifest>,
    successful_targets: Vec<TargetKey>,
}

impl SpooledDocumentSetBuilder {
    pub(crate) fn create() -> Result<Self> {
        let spool = DocumentSpool::create()?;
        let writer = spool.writer()?;

        Ok(Self {
            spool,
            writer,
            annotations: EntryAnnotationIndex::new(),
            total_documents: 0,
            manifest_targets: Vec::new(),
            successful_targets: Vec::new(),
        })
    }

    pub(crate) fn successful_targets(&self) -> &[TargetKey] {
        &self.successful_targets
    }

    fn append_target_documents(
        &mut self,
        produced_target: &ProducedTarget,
        documents: &[SearchDocument],
    ) -> Result<()> {
        for document in documents {
            self.annotations.observe(document);
            self.writer.push(document)?;
        }

        self.total_documents += documents.len();
        self.successful_targets.push(produced_target.key.clone());
        self.manifest_targets.push(IndexTargetManifest {
            source: produced_target.target.source_id.clone(),
            ref_id: produced_target.target.ref_config.id.clone(),
            artifact_kind: produced_target.produced.artifact_ref.kind,
            target_role: produced_target.target.ref_config.role,
            indexes_search_documents: produced_target.target.indexes_search_documents(),
            document_count: documents.len(),
            artifact_hash: Some(produced_target.verified_metadata.content_hash.clone()),
            revision: produced_target.verified_metadata.revision.clone(),
        });

        tracing::info!(
            "{} {} documents: {}/{}",
            produced_target.status.as_str(),
            documents.len(),
            produced_target.target.source_id,
            produced_target.target.ref_config.id
        );

        Ok(())
    }

    pub(crate) fn finish(self) -> Result<SpooledDocumentSet> {
        self.writer.finish()?;

        Ok(SpooledDocumentSet {
            spool: self.spool,
            annotations: self.annotations,
            total_documents: self.total_documents,
            manifest_targets: self.manifest_targets,
            successful_targets: self.successful_targets,
        })
    }
}

pub(crate) struct SpooledDocumentSet {
    pub(crate) spool: DocumentSpool,
    pub(crate) annotations: EntryAnnotationIndex,
    pub(crate) total_documents: usize,
    pub(crate) manifest_targets: Vec<IndexTargetManifest>,
    pub(crate) successful_targets: Vec<TargetKey>,
}

pub(crate) async fn consume_and_spool_target(
    artifact_store: &ArtifactStore,
    produced_target: &ProducedTarget,
    builder: &mut SpooledDocumentSetBuilder,
) -> Result<()> {
    let documents = if produced_target.target.indexes_search_documents() {
        consume_target(
            artifact_store,
            &produced_target.target,
            &produced_target.produced,
        )
        .await?
    } else {
        Vec::new()
    };

    builder.append_target_documents(produced_target, &documents)
}

pub(crate) fn build_generation_manifest(
    documents: &SpooledDocumentSet,
) -> Result<IndexGenerationManifest> {
    IndexGenerationManifest::new(
        documents.total_documents,
        documents.manifest_targets.clone(),
    )
}
