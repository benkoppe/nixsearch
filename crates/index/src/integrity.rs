use std::fs;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};

use crate::atomic_file;

const INTEGRITY_SCHEMA_VERSION: u32 = 1;
const INTEGRITY_TEMP_PREFIX: &str = "integrity.json.tmp";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GenerationIntegrity {
    schema_version: u32,
    manifest_generation_id: String,
    manifest_hash: String,
    seo_sidecar_hash: Option<String>,
    index_fingerprint: String,
}

pub(crate) struct GenerationIntegrityPaths<'a> {
    pub(crate) manifest_path: &'a Utf8Path,
    pub(crate) seo_sidecar_path: &'a Utf8Path,
    pub(crate) index_path: &'a Utf8Path,
    pub(crate) integrity_path: &'a Utf8Path,
}

pub(crate) fn write_integrity(
    generation_path: &Utf8Path,
    manifest_generation_id: &str,
    paths: &GenerationIntegrityPaths<'_>,
    seo_sidecar_required: bool,
) -> Result<()> {
    let integrity = build_integrity(manifest_generation_id, paths, seo_sidecar_required)?;
    let bytes = serde_json::to_vec_pretty(&integrity)
        .context("failed to serialize index generation integrity metadata")?;
    let temp_path = atomic_file::create_temp_file(generation_path, INTEGRITY_TEMP_PREFIX, &bytes)?;

    if let Err(error) = fs::rename(&temp_path, paths.integrity_path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "failed to write integrity metadata {}",
                paths.integrity_path
            )
        });
    }

    atomic_file::sync_file(paths.integrity_path)?;
    atomic_file::sync_dir(generation_path)?;

    Ok(())
}

pub(crate) fn build_integrity(
    manifest_generation_id: &str,
    paths: &GenerationIntegrityPaths<'_>,
    seo_sidecar_required: bool,
) -> Result<GenerationIntegrity> {
    let seo_sidecar_hash = match hash_file_if_present(paths.seo_sidecar_path)? {
        Some(hash) => Some(hash),
        None if seo_sidecar_required => {
            anyhow::bail!("SEO sidecar is required before writing generation integrity")
        }
        None => None,
    };

    Ok(GenerationIntegrity {
        schema_version: INTEGRITY_SCHEMA_VERSION,
        manifest_generation_id: manifest_generation_id.to_owned(),
        manifest_hash: hash_file(paths.manifest_path)?,
        seo_sidecar_hash,
        index_fingerprint: index_fingerprint(paths.index_path)?,
    })
}

pub(crate) fn read_integrity(integrity_path: &Utf8Path) -> Result<GenerationIntegrity> {
    let bytes = fs::read(integrity_path)
        .with_context(|| format!("failed to read integrity metadata {integrity_path}"))?;
    let integrity: GenerationIntegrity = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse integrity metadata {integrity_path}"))?;

    if integrity.schema_version != INTEGRITY_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported integrity schema version {} (current {})",
            integrity.schema_version,
            INTEGRITY_SCHEMA_VERSION
        );
    }

    Ok(integrity)
}

pub(crate) fn validate_integrity(
    manifest_generation_id: &str,
    paths: &GenerationIntegrityPaths<'_>,
    seo_sidecar_required: bool,
) -> Result<()> {
    let expected = build_integrity(manifest_generation_id, paths, seo_sidecar_required)?;
    let actual = read_integrity(paths.integrity_path)?;

    if actual != expected {
        anyhow::bail!("index generation integrity metadata does not match generation files");
    }

    Ok(())
}

fn index_fingerprint(index_path: &Utf8Path) -> Result<String> {
    let mut entries = Vec::new();
    collect_index_files(index_path, index_path, &mut entries)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    for (relative, size, hash) in entries {
        hasher.update(relative.as_bytes());
        hasher.update(b"\0");
        hasher.update(size.to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }

    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn hash_file(path: &Utf8Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read file {path}"))?;
    Ok(hash_bytes(&bytes))
}

fn hash_file_if_present(path: &Utf8Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(hash_bytes(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read file {path}")),
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn collect_index_files(
    root: &Utf8Path,
    path: &Utf8Path,
    entries: &mut Vec<(String, u64, String)>,
) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read index dir {path}"))? {
        let entry = entry.with_context(|| format!("failed to read index dir entry in {path}"))?;
        let entry_path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|path| anyhow::anyhow!("index path is not valid UTF-8: {}", path.display()))?;
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to stat index file {entry_path}"))?;

        if metadata.is_dir() {
            collect_index_files(root, &entry_path, entries)?;
            continue;
        }

        if !metadata.is_file() {
            anyhow::bail!("unexpected non-regular index path {entry_path}");
        }

        let relative = entry_path
            .strip_prefix(root)
            .with_context(|| format!("failed to relativize index path {entry_path}"))?
            .as_str()
            .to_owned();
        entries.push((relative, metadata.len(), hash_file(&entry_path)?));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::GenerationIntegrity;

    #[test]
    fn generation_integrity_serializes_existing_json_shape() {
        let integrity = GenerationIntegrity {
            schema_version: 1,
            manifest_generation_id: "sha256:manifest-id".to_owned(),
            manifest_hash: "sha256:manifest".to_owned(),
            seo_sidecar_hash: None,
            index_fingerprint: "sha256:index".to_owned(),
        };

        let json = String::from_utf8(serde_json::to_vec_pretty(&integrity).unwrap()).unwrap();

        assert_eq!(
            json,
            r#"{
  "schema_version": 1,
  "manifest_generation_id": "sha256:manifest-id",
  "manifest_hash": "sha256:manifest",
  "seo_sidecar_hash": null,
  "index_fingerprint": "sha256:index"
}"#
        );
    }
}
