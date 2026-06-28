use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactKind;
use crate::document::IndexedEntryKind;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum RefRole {
    #[default]
    Search,
    IndexOnly,
    ArtifactOnly,
}

impl RefRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::IndexOnly => "index-only",
            Self::ArtifactOnly => "artifact-only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetCapabilities {
    role: RefRole,
    artifact_kind: ArtifactKind,
}

impl TargetCapabilities {
    pub fn new(role: RefRole, artifact_kind: ArtifactKind) -> Self {
        Self {
            role,
            artifact_kind,
        }
    }

    pub fn role(self) -> RefRole {
        self.role
    }

    pub fn artifact_kind(self) -> ArtifactKind {
        self.artifact_kind
    }

    pub fn is_searchable(self) -> bool {
        self.role == RefRole::Search && self.artifact_kind.indexed_entry_kind().is_some()
    }

    pub fn is_artifact_only(self) -> bool {
        self.role == RefRole::ArtifactOnly
    }

    pub fn can_appear_in_ref_set(self) -> bool {
        self.is_searchable()
    }

    pub fn indexes_search_documents(self) -> bool {
        matches!(self.role, RefRole::Search | RefRole::IndexOnly)
            && self.artifact_kind.indexed_document_kind().is_some()
    }

    pub fn is_public_seo_candidate(self) -> bool {
        self.is_searchable()
    }

    pub fn indexed_entry_kind(self) -> Option<IndexedEntryKind> {
        self.indexes_search_documents()
            .then(|| self.artifact_kind.indexed_entry_kind())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use crate::artifact::ArtifactKind;
    use crate::document::IndexedEntryKind;
    use crate::target::{RefRole, TargetCapabilities};

    #[test]
    fn search_role_is_searchable_ref_set_eligible_indexing_and_public() {
        let capabilities = TargetCapabilities::new(RefRole::Search, ArtifactKind::OptionsJson);

        assert!(capabilities.is_searchable());
        assert!(capabilities.can_appear_in_ref_set());
        assert!(capabilities.indexes_search_documents());
        assert!(capabilities.is_public_seo_candidate());
        assert_eq!(
            capabilities.indexed_entry_kind(),
            Some(IndexedEntryKind::Option)
        );
    }

    #[test]
    fn index_only_role_indexes_but_is_hidden_from_search_ref_sets_and_seo() {
        let capabilities = TargetCapabilities::new(RefRole::IndexOnly, ArtifactKind::OptionsJson);

        assert!(!capabilities.is_searchable());
        assert!(!capabilities.can_appear_in_ref_set());
        assert!(capabilities.indexes_search_documents());
        assert!(!capabilities.is_public_seo_candidate());
        assert_eq!(
            capabilities.indexed_entry_kind(),
            Some(IndexedEntryKind::Option)
        );
    }

    #[test]
    fn artifact_only_role_has_no_search_index_capabilities() {
        let capabilities =
            TargetCapabilities::new(RefRole::ArtifactOnly, ArtifactKind::OptionsJson);

        assert!(capabilities.is_artifact_only());
        assert!(!capabilities.is_searchable());
        assert!(!capabilities.can_appear_in_ref_set());
        assert!(!capabilities.indexes_search_documents());
        assert!(!capabilities.is_public_seo_candidate());
        assert_eq!(capabilities.indexed_entry_kind(), None);
    }
}
