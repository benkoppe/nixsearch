use std::fs;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use time::OffsetDateTime;

use super::IndexStore;

pub(super) const CURRENT_TEMP_PREFIX: &str = "CURRENT.tmp";
pub(super) const MANIFEST_TEMP_PREFIX: &str = "index-manifest.json.tmp";
pub(super) const SEO_SIDECAR_TEMP_PREFIX: &str = "seo-facts.json.tmp";

const CURRENT_FILE: &str = "CURRENT";
const GENERATIONS_DIR: &str = "generations";
const GENERATION_LOCKS_DIR: &str = "generation-locks";
const GENERATION_NAME_PREFIX: &str = "generation-";
const INDEX_DIR: &str = "index";
const INTEGRITY_FILE: &str = "integrity.json";
const MANIFEST_FILE: &str = "index-manifest.json";
const SEO_SIDECAR_FILE: &str = "seo-facts.json";

#[derive(Debug, Clone)]
pub(super) struct GenerationLayout {
    root: Utf8PathBuf,
}

impl GenerationLayout {
    pub(super) fn new(root: impl AsRef<Utf8Path>) -> Self {
        Self {
            root: root.as_ref().to_owned(),
        }
    }

    pub(super) fn root(&self) -> &Utf8Path {
        &self.root
    }

    fn generations_dir(&self) -> Utf8PathBuf {
        self.root.join(GENERATIONS_DIR)
    }

    fn generation_locks_dir(&self) -> Utf8PathBuf {
        self.root.join(GENERATION_LOCKS_DIR)
    }

    fn current_file(&self) -> Utf8PathBuf {
        self.root.join(CURRENT_FILE)
    }
}

impl IndexStore {
    pub fn generations_dir(&self) -> Utf8PathBuf {
        self.layout.generations_dir()
    }

    pub fn generation_locks_dir(&self) -> Utf8PathBuf {
        self.layout.generation_locks_dir()
    }

    pub fn generation_lock_path(&self, generation_name: &str) -> Utf8PathBuf {
        self.generation_locks_dir()
            .join(format!("{generation_name}.lock"))
    }

    pub fn current_file(&self) -> Utf8PathBuf {
        self.layout.current_file()
    }

    pub fn manifest_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join(MANIFEST_FILE)
    }

    pub fn seo_sidecar_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join(SEO_SIDECAR_FILE)
    }

    pub fn integrity_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join(INTEGRITY_FILE)
    }

    pub fn index_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join(INDEX_DIR)
    }

    pub fn create_generation_path(&self) -> Result<Utf8PathBuf> {
        let generations_dir = self.generations_dir();
        fs::create_dir_all(&generations_dir).with_context(|| {
            format!(
                "failed to create generations dir {}",
                generations_dir.as_str()
            )
        })?;

        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

        for attempt in 0..100 {
            let path = generations_dir.join(format!(
                "{GENERATION_NAME_PREFIX}{}-{timestamp}-{attempt}",
                std::process::id()
            ));

            match fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create generation {}", path.as_str()));
                }
            }
        }

        anyhow::bail!(
            "failed to create unique generation directory in {}",
            generations_dir.as_str()
        )
    }

    pub(super) fn validate_generation_path(
        &self,
        generation_path: &Utf8Path,
    ) -> Result<Utf8PathBuf> {
        let generation_path = generation_path
            .canonicalize_utf8()
            .with_context(|| format!("failed to canonicalize {generation_path}"))?;

        let generations_dir = self.generations_dir();
        let generations_dir = generations_dir
            .canonicalize_utf8()
            .with_context(|| format!("failed to canonicalize generations dir {generations_dir}"))?;

        if !generation_path.starts_with(&generations_dir) {
            anyhow::bail!(
                "generation {} is outside generations dir {}",
                generation_path.as_str(),
                generations_dir.as_str()
            );
        }

        if generation_path.parent() != Some(generations_dir.as_path()) {
            anyhow::bail!(
                "generation {} is not a direct child of generations dir {}",
                generation_path.as_str(),
                generations_dir.as_str()
            );
        }

        if !generation_path.is_dir() {
            anyhow::bail!("generation {} is not a directory", generation_path.as_str());
        }

        Ok(generation_path)
    }
}

pub(super) fn validate_generation_name(generation_name: &str) -> Result<()> {
    if generation_name.starts_with(GENERATION_NAME_PREFIX)
        && !generation_name.contains('/')
        && !generation_name.contains('\\')
    {
        return Ok(());
    }

    anyhow::bail!("invalid generation name {generation_name:?}")
}
