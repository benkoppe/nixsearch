use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::target::{RefRole, TargetCapabilities};

use crate::schema::INDEX_SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexGenerationManifest {
    pub schema_version: u32,

    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,

    pub generation_id: String,

    pub document_count: usize,
    pub targets: Vec<IndexTargetManifest>,
}

impl IndexGenerationManifest {
    pub fn new(document_count: usize, targets: Vec<IndexTargetManifest>) -> Result<Self> {
        Self::with_generated_at(document_count, targets, OffsetDateTime::now_utc())
    }

    pub fn with_generated_at(
        document_count: usize,
        targets: Vec<IndexTargetManifest>,
        generated_at: OffsetDateTime,
    ) -> Result<Self> {
        let mut manifest = Self {
            schema_version: INDEX_SCHEMA_VERSION,
            generated_at,
            generation_id: String::new(),
            document_count,
            targets,
        };

        validate_target_content_identity(&manifest)?;
        refresh_generation_id(&mut manifest)?;

        Ok(manifest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexTargetManifest {
    pub source: String,
    pub ref_id: String,
    pub artifact_kind: ArtifactKind,
    pub target_role: RefRole,
    pub indexes_search_documents: bool,
    pub document_count: usize,
    pub artifact_hash: Option<String>,
    pub revision: Option<String>,
}

impl IndexTargetManifest {
    pub fn new(
        source: impl Into<String>,
        ref_id: impl Into<String>,
        artifact_kind: ArtifactKind,
        target_role: RefRole,
        document_count: usize,
    ) -> Self {
        Self {
            source: source.into(),
            ref_id: ref_id.into(),
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

pub fn canonical_generation_id(manifest: &IndexGenerationManifest) -> Result<String> {
    let bytes = canonical_manifest_bytes(manifest)?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);

    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

pub fn refresh_generation_id(manifest: &mut IndexGenerationManifest) -> Result<()> {
    manifest.generation_id = canonical_generation_id(manifest)?;
    Ok(())
}

pub fn validate_generation_id(manifest: &IndexGenerationManifest) -> Result<()> {
    validate_target_content_identity(manifest)?;

    let computed = canonical_generation_id(manifest)?;

    if manifest.generation_id != computed {
        bail!(
            "index generation manifest generation_id mismatch: stored {:?}, computed {:?}",
            manifest.generation_id,
            computed
        );
    }

    Ok(())
}

pub fn validate_target_content_identity(manifest: &IndexGenerationManifest) -> Result<()> {
    for target in &manifest.targets {
        if target
            .artifact_hash
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            && target
                .revision
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            bail!(
                "index generation target {}/{}/{} has no content identity (artifact_hash or revision required)",
                target.source,
                target.ref_id,
                target.artifact_kind.as_str()
            );
        }
    }

    Ok(())
}

pub fn validate_index_schema_version(manifest: &IndexGenerationManifest) -> Result<()> {
    if manifest.schema_version != INDEX_SCHEMA_VERSION {
        bail!(
            "unsupported index schema version {} (current {})",
            manifest.schema_version,
            INDEX_SCHEMA_VERSION
        );
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct CanonicalGenerationManifest {
    generation_id_version: u32,
    schema_version: u32,
    document_count: usize,
    targets: Vec<CanonicalTargetManifest>,
}

#[derive(serde::Serialize)]
struct CanonicalTargetManifest {
    source: String,
    ref_id: String,
    artifact_kind: &'static str,
    target_role: &'static str,
    indexes_search_documents: bool,
    document_count: usize,
    artifact_hash: Option<String>,
    revision: Option<String>,
}

fn canonical_manifest_bytes(manifest: &IndexGenerationManifest) -> Result<Vec<u8>> {
    let mut targets = manifest
        .targets
        .iter()
        .map(|target| CanonicalTargetManifest {
            source: target.source.clone(),
            ref_id: target.ref_id.clone(),
            artifact_kind: target.artifact_kind.as_str(),
            target_role: target.target_role.as_str(),
            indexes_search_documents: target.indexes_search_documents,
            document_count: target.document_count,
            artifact_hash: target.artifact_hash.clone(),
            revision: target.revision.clone(),
        })
        .collect::<Vec<_>>();

    targets.sort_by(|left, right| {
        (
            left.source.as_str(),
            left.ref_id.as_str(),
            left.artifact_kind,
            left.target_role,
            left.indexes_search_documents,
            left.document_count,
            left.artifact_hash.is_some(),
            left.artifact_hash.as_deref().unwrap_or(""),
            left.revision.is_some(),
            left.revision.as_deref().unwrap_or(""),
        )
            .cmp(&(
                right.source.as_str(),
                right.ref_id.as_str(),
                right.artifact_kind,
                right.target_role,
                right.indexes_search_documents,
                right.document_count,
                right.artifact_hash.is_some(),
                right.artifact_hash.as_deref().unwrap_or(""),
                right.revision.is_some(),
                right.revision.as_deref().unwrap_or(""),
            ))
    });

    let canonical = CanonicalGenerationManifest {
        generation_id_version: 1,
        schema_version: manifest.schema_version,
        document_count: manifest.document_count,
        targets,
    };

    serde_json::to_vec(&canonical)
        .context("failed to serialize canonical index generation manifest")
}

#[cfg(test)]
mod tests {
    use nixsearch_core::target::RefRole;

    use super::*;

    const SOURCE_FIXTURES: &str = "fixtures";
    const REF_SMALL: &str = "small";

    fn target(
        artifact_kind: ArtifactKind,
        document_count: usize,
        artifact_hash: Option<&str>,
        revision: Option<&str>,
    ) -> IndexTargetManifest {
        IndexTargetManifest {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: REF_SMALL.to_owned(),
            artifact_kind,
            target_role: RefRole::Search,
            indexes_search_documents: true,
            document_count,
            artifact_hash: artifact_hash.map(str::to_owned),
            revision: revision.map(str::to_owned),
        }
    }

    fn golden_manifest_with_reversed_targets() -> IndexGenerationManifest {
        IndexGenerationManifest::with_generated_at(
            3,
            vec![
                target(ArtifactKind::PackagesJson, 2, Some("bbb"), None),
                target(ArtifactKind::OptionsJson, 1, Some("aaa"), Some("rev1")),
            ],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap()
    }

    #[test]
    fn canonical_generation_id_matches_golden_hash() {
        let manifest = golden_manifest_with_reversed_targets();

        let id = canonical_generation_id(&manifest).unwrap();

        assert_eq!(
            id,
            "sha256:b9215702301e9abfe572e2cc35add074a1a62e23df370f34da54f43e96449bc1"
        );
    }

    #[test]
    fn canonical_generation_id_ignores_generated_at() {
        let first = golden_manifest_with_reversed_targets();
        let second = IndexGenerationManifest::with_generated_at(
            3,
            first.targets.clone(),
            OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        )
        .unwrap();

        assert_eq!(
            canonical_generation_id(&first).unwrap(),
            canonical_generation_id(&second).unwrap()
        );
    }

    #[test]
    fn canonical_generation_id_sorts_targets_stably() {
        let reversed = golden_manifest_with_reversed_targets();
        let sorted = IndexGenerationManifest::with_generated_at(
            3,
            vec![
                target(ArtifactKind::OptionsJson, 1, Some("aaa"), Some("rev1")),
                target(ArtifactKind::PackagesJson, 2, Some("bbb"), None),
            ],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert_eq!(
            canonical_generation_id(&reversed).unwrap(),
            canonical_generation_id(&sorted).unwrap()
        );
    }

    #[test]
    fn manifest_rejects_target_without_content_identity() {
        let error = IndexGenerationManifest::with_generated_at(
            1,
            vec![target(ArtifactKind::OptionsJson, 1, None, None)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("has no content identity"));
    }

    #[test]
    fn canonical_manifest_uses_expected_compact_json() {
        let manifest = IndexGenerationManifest::with_generated_at(
            1,
            vec![target(ArtifactKind::OptionsJson, 1, Some("aaa"), None)],
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let json = String::from_utf8(canonical_manifest_bytes(&manifest).unwrap()).unwrap();

        assert_eq!(
            json,
            r#"{"generation_id_version":1,"schema_version":5,"document_count":1,"targets":[{"source":"fixtures","ref_id":"small","artifact_kind":"options-json","target_role":"search","indexes_search_documents":true,"document_count":1,"artifact_hash":"aaa","revision":null}]}"#
        );
    }

    #[test]
    fn refresh_generation_id_overwrites_existing_id() {
        let mut manifest = golden_manifest_with_reversed_targets();
        manifest.generation_id = "sha256:wrong".to_owned();

        refresh_generation_id(&mut manifest).unwrap();

        assert_eq!(
            manifest.generation_id,
            "sha256:b9215702301e9abfe572e2cc35add074a1a62e23df370f34da54f43e96449bc1"
        );
    }

    #[test]
    fn validate_generation_id_rejects_mismatch() {
        let mut manifest = golden_manifest_with_reversed_targets();
        manifest.generation_id = "sha256:wrong".to_owned();

        let error = validate_generation_id(&manifest).unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }
}
