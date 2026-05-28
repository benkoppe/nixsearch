use nixsearch_core::{ArtifactKind, IngestContext};
use nixsearch_store::{ArtifactMetadata, ArtifactRef};

#[derive(Debug, Clone)]
pub struct ProduceRequest {
    pub source: String,
    pub ref_id: String,
}

impl ProduceRequest {
    pub fn artifact_ref(&self, kind: ArtifactKind) -> ArtifactRef {
        ArtifactRef::latest(self.source.clone(), self.ref_id.clone(), kind)
    }

    pub fn ingest_context(&self, revision: Option<String>) -> IngestContext {
        IngestContext {
            source: self.source.clone(),
            ref_id: self.ref_id.clone(),
            revision,
            repo: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProducedArtifact {
    pub artifact_ref: ArtifactRef,
    pub metadata: ArtifactMetadata,
}
