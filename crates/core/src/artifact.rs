use serde::{Deserialize, Serialize};

use crate::document::{DocumentKind, IndexedEntryKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    OptionsJson,
    PackagesJson,
    FlakeInfoJson,
}

impl ArtifactKind {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::OptionsJson => "options.json",
            Self::PackagesJson => "packages.json",
            Self::FlakeInfoJson => "flake-info.json",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptionsJson => "options-json",
            Self::PackagesJson => "packages-json",
            Self::FlakeInfoJson => "flake-info-json",
        }
    }

    pub fn indexed_document_kind(self) -> Option<DocumentKind> {
        match self {
            Self::OptionsJson => Some(DocumentKind::Option),
            Self::PackagesJson => Some(DocumentKind::Package),
            Self::FlakeInfoJson => None,
        }
    }

    pub fn indexed_entry_kind(self) -> Option<IndexedEntryKind> {
        match self {
            Self::OptionsJson => Some(IndexedEntryKind::Option),
            Self::PackagesJson => Some(IndexedEntryKind::Package),
            Self::FlakeInfoJson => None,
        }
    }

    pub fn for_indexed_document_kind(kind: &DocumentKind) -> Option<Self> {
        match kind {
            DocumentKind::Option => Some(Self::OptionsJson),
            DocumentKind::Package => Some(Self::PackagesJson),
            DocumentKind::App | DocumentKind::Service => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::document::{DocumentKind, IndexedEntryKind};

    use super::ArtifactKind;

    #[test]
    fn artifact_kind_maps_to_indexed_document_kind() {
        assert_eq!(
            ArtifactKind::OptionsJson.indexed_document_kind(),
            Some(DocumentKind::Option)
        );
        assert_eq!(
            ArtifactKind::PackagesJson.indexed_document_kind(),
            Some(DocumentKind::Package)
        );
        assert_eq!(ArtifactKind::FlakeInfoJson.indexed_document_kind(), None);
    }

    #[test]
    fn artifact_kind_maps_to_indexed_entry_kind() {
        assert_eq!(
            ArtifactKind::OptionsJson.indexed_entry_kind(),
            Some(IndexedEntryKind::Option)
        );
        assert_eq!(
            ArtifactKind::PackagesJson.indexed_entry_kind(),
            Some(IndexedEntryKind::Package)
        );
        assert_eq!(ArtifactKind::FlakeInfoJson.indexed_entry_kind(), None);
    }

    #[test]
    fn indexed_document_kind_maps_to_artifact_kind() {
        assert_eq!(
            ArtifactKind::for_indexed_document_kind(&DocumentKind::Option),
            Some(ArtifactKind::OptionsJson)
        );
        assert_eq!(
            ArtifactKind::for_indexed_document_kind(&DocumentKind::Package),
            Some(ArtifactKind::PackagesJson)
        );
        assert_eq!(
            ArtifactKind::for_indexed_document_kind(&DocumentKind::App),
            None
        );
        assert_eq!(
            ArtifactKind::for_indexed_document_kind(&DocumentKind::Service),
            None
        );
    }
}
