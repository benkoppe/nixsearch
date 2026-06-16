use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use time::OffsetDateTime;

use crate::manifest::{IndexGenerationManifest, validate_generation_id};
use crate::seo::SeoSidecar;

#[derive(Debug, Clone)]
pub struct IndexStore {
    root: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedGeneration {
    pub path: Utf8PathBuf,
    pub manifest: IndexGenerationManifest,
}

#[derive(Debug)]
pub struct GenerationLease {
    _file: File,
}

#[derive(Debug, Clone)]
pub struct LeasedPublishedGeneration {
    generation: PublishedGeneration,
    _lease: Arc<GenerationLease>,
}

impl LeasedPublishedGeneration {
    pub fn path(&self) -> &Utf8Path {
        &self.generation.path
    }

    pub fn manifest(&self) -> &IndexGenerationManifest {
        &self.generation.manifest
    }

    pub fn published_generation(&self) -> &PublishedGeneration {
        &self.generation
    }

    pub fn to_published_generation(&self) -> PublishedGeneration {
        self.generation.clone()
    }
}

impl IndexStore {
    pub fn new(root: impl AsRef<Utf8Path>) -> Self {
        let root = root.as_ref().to_owned();
        Self { root }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub fn generations_dir(&self) -> Utf8PathBuf {
        self.root.join("generations")
    }

    pub fn generation_locks_dir(&self) -> Utf8PathBuf {
        self.root.join("generation-locks")
    }

    pub fn generation_lock_path(&self, generation_name: &str) -> Utf8PathBuf {
        self.generation_locks_dir()
            .join(format!("{generation_name}.lock"))
    }

    pub fn try_acquire_existing_exclusive_generation_lock(
        &self,
        generation_name: &str,
    ) -> Result<Option<GenerationLease>> {
        validate_generation_name(generation_name)?;

        let lock_path = self.generation_lock_path(generation_name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open generation lease {}", lock_path.as_str()))?;

        match file.try_lock() {
            Ok(()) => Ok(Some(GenerationLease { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).with_context(|| {
                format!("failed to acquire exclusive generation lease {lock_path}")
            }),
        }
    }

    pub fn current_file(&self) -> Utf8PathBuf {
        self.root.join("CURRENT")
    }

    pub fn create_generation_path(&self) -> Result<Utf8PathBuf> {
        fs::create_dir_all(self.generations_dir()).with_context(|| {
            format!(
                "failed to create generations dir {}",
                self.generations_dir().as_str()
            )
        })?;

        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

        for attempt in 0..100 {
            let path = self.generations_dir().join(format!(
                "generation-{}-{timestamp}-{attempt}",
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
            self.generations_dir().as_str()
        )
    }

    // Atomically publishes generation_path as CURRENT. Update callers should still use the
    // update lock to avoid redundant concurrent generation work.
    pub fn publish(&self, generation_path: &Utf8Path) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create index root {}", self.root.as_str()))?;

        let generation_path = self.validate_generation_path(generation_path)?;

        // On Unix, rename atomically replaces CURRENT with this fully written file.
        let current_tmp = self.create_temp_file(
            &self.root,
            "CURRENT.tmp",
            generation_path.as_str().as_bytes(),
        )?;

        if let Err(error) = fs::rename(&current_tmp, self.current_file()) {
            let _ = fs::remove_file(&current_tmp);

            return Err(error).with_context(|| {
                format!(
                    "failed to publish current index {}",
                    self.current_file().as_str()
                )
            });
        }

        Self::sync_dir(&self.root)?;

        Ok(())
    }

    fn validate_generation_path(&self, generation_path: &Utf8Path) -> Result<Utf8PathBuf> {
        let generation_path = generation_path
            .canonicalize_utf8()
            .with_context(|| format!("failed to canonicalize {generation_path}"))?;

        let generations_dir = self
            .generations_dir()
            .canonicalize_utf8()
            .with_context(|| {
                format!(
                    "failed to canonicalize generations dir {}",
                    self.generations_dir()
                )
            })?;

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

    fn generation_lease_path(
        &self,
        generation_path: &Utf8Path,
    ) -> Result<(Utf8PathBuf, Utf8PathBuf)> {
        let generation_path = self.validate_generation_path(generation_path)?;
        let name = generation_path
            .file_name()
            .with_context(|| {
                format!(
                    "failed to derive generation lease name for {}",
                    generation_path.as_str()
                )
            })?
            .to_owned();

        let lock_path = self.generation_lock_path(&name);

        Ok((generation_path, lock_path))
    }

    fn open_generation_lease_file(&self, generation_path: &Utf8Path) -> Result<File> {
        let (_, lock_path) = self.generation_lease_path(generation_path)?;

        fs::create_dir_all(self.generation_locks_dir()).with_context(|| {
            format!(
                "failed to create generation locks dir {}",
                self.generation_locks_dir()
            )
        })?;

        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open generation lease {}", lock_path.as_str()))
    }

    pub fn acquire_shared_generation_lease(
        &self,
        generation_path: &Utf8Path,
    ) -> Result<GenerationLease> {
        let file = self.open_generation_lease_file(generation_path)?;

        file.lock_shared().with_context(|| {
            format!("failed to acquire shared generation lease for {generation_path}")
        })?;

        Ok(GenerationLease { _file: file })
    }

    pub fn try_acquire_exclusive_generation_lease(
        &self,
        generation_path: &Utf8Path,
    ) -> Result<Option<GenerationLease>> {
        let file = self.open_generation_lease_file(generation_path)?;

        match file.try_lock() {
            Ok(()) => Ok(Some(GenerationLease { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).with_context(|| {
                format!("failed to acquire exclusive generation lease for {generation_path}")
            }),
        }
    }

    pub fn lease_published_generation(
        &self,
        generation: PublishedGeneration,
    ) -> Result<LeasedPublishedGeneration> {
        validate_generation_id(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        let lease = Arc::new(self.acquire_shared_generation_lease(&generation.path)?);

        Ok(LeasedPublishedGeneration {
            generation,
            _lease: lease,
        })
    }

    fn create_temp_file(&self, dir: &Utf8Path, prefix: &str, bytes: &[u8]) -> Result<Utf8PathBuf> {
        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

        for attempt in 0..100 {
            let temp_path = dir.join(format!(
                "{prefix}.{}.{}.{}",
                std::process::id(),
                timestamp,
                attempt
            ));

            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", temp_path.as_str()));
                }
            };

            if let Err(error) = file.write_all(bytes) {
                let _ = fs::remove_file(&temp_path);

                return Err(error)
                    .with_context(|| format!("failed to write {}", temp_path.as_str()));
            }

            if let Err(error) = file.sync_all() {
                let _ = fs::remove_file(&temp_path);

                return Err(error)
                    .with_context(|| format!("failed to sync {}", temp_path.as_str()));
            }

            return Ok(temp_path);
        }

        anyhow::bail!(
            "failed to create unique temporary {prefix} file in {}",
            dir.as_str()
        )
    }

    fn sync_dir(path: &Utf8Path) -> Result<()> {
        fs::File::open(path)
            .with_context(|| format!("failed to open directory {}", path.as_str()))?
            .sync_all()
            .with_context(|| format!("failed to sync directory {}", path.as_str()))
    }

    pub fn current_path(&self) -> Result<Utf8PathBuf> {
        self.try_current_path()?.with_context(|| {
            format!(
                "failed to read current index file {}; run `nixsearch update` first",
                self.current_file().as_str()
            )
        })
    }

    pub fn try_current_path(&self) -> Result<Option<Utf8PathBuf>> {
        let raw = match fs::read_to_string(self.current_file()) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read current index file {}",
                        self.current_file().as_str()
                    )
                });
            }
        };

        let path = Utf8PathBuf::from(raw);

        if path.as_str().is_empty() {
            anyhow::bail!("current index file is empty");
        }

        self.validate_generation_path(&path).map(Some)
    }

    pub fn manifest_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join("index-manifest.json")
    }

    pub fn seo_sidecar_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join("seo-facts.json")
    }

    pub fn write_manifest(
        &self,
        generation_path: &Utf8Path,
        manifest: &IndexGenerationManifest,
    ) -> Result<()> {
        let generation_path = self.validate_generation_path(generation_path)?;
        let path = self.manifest_path(&generation_path);

        validate_generation_id(manifest)
            .context("failed to validate supplied index generation manifest")?;

        let bytes = serde_json::to_vec_pretty(manifest)
            .context("failed to serialize index generation manifest")?;

        let temp_path =
            self.create_temp_file(&generation_path, "index-manifest.json.tmp", &bytes)?;

        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);

            return Err(error)
                .with_context(|| format!("failed to write index metadata {}", path.as_str()));
        }

        Self::sync_dir(&generation_path)?;

        Ok(())
    }

    pub fn read_manifest(&self, generation_path: &Utf8Path) -> Result<IndexGenerationManifest> {
        let path = self.manifest_path(generation_path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read index metadata {}", path.as_str()))?;

        let manifest: IndexGenerationManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse index metadata {}", path.as_str()))?;

        validate_generation_id(&manifest)
            .with_context(|| format!("failed to validate index metadata {}", path.as_str()))?;

        Ok(manifest)
    }

    pub fn write_seo_sidecar(
        &self,
        generation: &PublishedGeneration,
        sidecar: &SeoSidecar,
    ) -> Result<()> {
        let generation_path = self.validate_generation_path(&generation.path)?;
        let path = self.seo_sidecar_path(&generation_path);

        validate_generation_id(&generation.manifest)
            .context("failed to validate supplied index generation manifest")?;

        sidecar
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        let bytes =
            serde_json::to_vec_pretty(sidecar).context("failed to serialize SEO sidecar")?;

        let temp_path = self.create_temp_file(&generation_path, "seo-facts.json.tmp", &bytes)?;

        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);

            return Err(error)
                .with_context(|| format!("failed to write SEO sidecar {}", path.as_str()));
        }

        Self::sync_dir(&generation_path)?;

        Ok(())
    }

    pub fn read_seo_sidecar(&self, generation: &PublishedGeneration) -> Result<SeoSidecar> {
        let path = self.seo_sidecar_path(&generation.path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read SEO sidecar {}", path.as_str()))?;

        let sidecar: SeoSidecar = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse SEO sidecar {}", path.as_str()))?;

        sidecar
            .validate_for_manifest(&generation.manifest)
            .with_context(|| format!("failed to validate SEO sidecar {}", path.as_str()))?;

        Ok(sidecar)
    }

    pub fn current_manifest(&self) -> Result<IndexGenerationManifest> {
        let current = self.current_path()?;
        self.read_manifest(&current)
    }

    pub fn try_current_manifest(&self) -> Result<Option<IndexGenerationManifest>> {
        let Some(current) = self.try_current_path()? else {
            return Ok(None);
        };

        self.read_manifest(&current).map(Some)
    }

    pub fn try_current_generation_metadata(&self) -> Result<Option<PublishedGeneration>> {
        self.try_current_generation_metadata_with(|_, _| Ok(()))
    }

    fn try_current_generation_metadata_with(
        &self,
        mut after_manifest_read: impl FnMut(&IndexStore, &Utf8Path) -> Result<()>,
    ) -> Result<Option<PublishedGeneration>> {
        for _ in 0..100 {
            let Some(path) = self.try_current_path()? else {
                return Ok(None);
            };

            let manifest = match self.read_manifest(&path) {
                Ok(manifest) => manifest,
                Err(error) => match self.try_current_path()? {
                    Some(current) if current != path => continue,
                    Some(_) => return Err(error),
                    None => return Ok(None),
                },
            };

            after_manifest_read(self, &path)?;

            match self.try_current_path()? {
                Some(current) if current == path => {
                    return Ok(Some(PublishedGeneration { path, manifest }));
                }
                Some(_) => continue,
                None => return Ok(None),
            }
        }

        anyhow::bail!("failed to read stable current generation metadata")
    }

    pub fn current_leased_generation(&self) -> Result<LeasedPublishedGeneration> {
        self.try_current_leased_generation()?.with_context(|| {
            format!(
                "failed to read current index file {}; run `nixsearch update` first",
                self.current_file().as_str()
            )
        })
    }

    pub fn try_current_leased_generation(&self) -> Result<Option<LeasedPublishedGeneration>> {
        self.try_current_leased_generation_with(IndexStore::acquire_shared_generation_lease)
    }

    fn try_current_leased_generation_with(
        &self,
        mut acquire_shared_generation_lease: impl FnMut(
            &IndexStore,
            &Utf8Path,
        ) -> Result<GenerationLease>,
    ) -> Result<Option<LeasedPublishedGeneration>> {
        for _ in 0..100 {
            let Some(path) = self.try_current_path()? else {
                return Ok(None);
            };

            let lease = match acquire_shared_generation_lease(self, &path) {
                Ok(lease) => Arc::new(lease),
                Err(error) => match self.try_current_path()? {
                    Some(current) if current != path => continue,
                    Some(_) => return Err(error),
                    None => return Ok(None),
                },
            };

            match self.try_current_path()? {
                Some(current) if current == path => {
                    let manifest = self.read_manifest(&path)?;
                    return Ok(Some(LeasedPublishedGeneration {
                        generation: PublishedGeneration { path, manifest },
                        _lease: lease,
                    }));
                }
                Some(_) => continue,
                None => return Ok(None),
            }
        }

        anyhow::bail!("failed to acquire a stable current generation lease")
    }
}

fn validate_generation_name(generation_name: &str) -> Result<()> {
    if generation_name.starts_with("generation-")
        && !generation_name.contains('/')
        && !generation_name.contains('\\')
    {
        return Ok(());
    }

    anyhow::bail!("invalid generation name {generation_name:?}")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_core::artifact::ArtifactKind;

    use crate::manifest::{IndexGenerationManifest, IndexTargetManifest, canonical_generation_id};
    use crate::store::{IndexStore, PublishedGeneration};

    const SOURCE_FIXTURES: &str = "fixtures";
    const REF_SMALL: &str = "small";

    fn utf8_path(path: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
    }

    fn store_for(tempdir: &TempDir) -> IndexStore {
        IndexStore::new(utf8_path(tempdir.path().to_path_buf()))
    }

    #[test]
    fn index_store_publishes_current_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let generation = store.create_generation_path().unwrap();
        assert!(generation.exists());

        store.publish(&generation).unwrap();

        let current = store.current_path().unwrap();

        assert_eq!(current, generation.canonicalize_utf8().unwrap());
    }

    #[test]
    fn index_store_creates_distinct_generation_paths() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let first = store.create_generation_path().unwrap();
        let second = store.create_generation_path().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn index_store_publish_updates_current_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let first = store.create_generation_path().unwrap();
        let second = store.create_generation_path().unwrap();

        store.publish(&first).unwrap();
        store.publish(&second).unwrap();

        assert_eq!(
            store.current_path().unwrap(),
            second.canonicalize_utf8().unwrap()
        );
    }

    #[test]
    fn index_store_publish_removes_temporary_current_file_on_success() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        store.publish(&generation).unwrap();

        let temporary_current_files = fs::read_dir(tempdir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("CURRENT.tmp.")
            })
            .collect::<Vec<_>>();

        assert!(temporary_current_files.is_empty());
    }

    #[test]
    fn index_store_publish_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();

        let error = store.publish(&external_generation).unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_publish_rejects_generations_dir_itself() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();

        let error = store.publish(&store.generations_dir()).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_publish_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();

        let error = store.publish(&nested).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_publish_rejects_file_under_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let file = store.generations_dir().join("generation-file");
        fs::write(&file, b"not a directory").unwrap();

        let error = store.publish(&file).unwrap_err();

        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn shared_generation_lease_blocks_exclusive_try_acquire() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let lease = store.acquire_shared_generation_lease(&generation).unwrap();

        assert!(
            store
                .try_acquire_exclusive_generation_lease(&generation)
                .unwrap()
                .is_none()
        );

        drop(lease);

        assert!(
            store
                .try_acquire_exclusive_generation_lease(&generation)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn existing_generation_lock_exclusive_try_acquire_respects_shared_lock() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let generation_name = generation.file_name().unwrap();
        let lease = store.acquire_shared_generation_lease(&generation).unwrap();

        assert!(
            store
                .try_acquire_existing_exclusive_generation_lock(generation_name)
                .unwrap()
                .is_none()
        );

        drop(lease);

        assert!(
            store
                .try_acquire_existing_exclusive_generation_lock(generation_name)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn existing_generation_lock_exclusive_try_acquire_does_not_create_lock() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let error = store
            .try_acquire_existing_exclusive_generation_lock("generation-missing")
            .unwrap_err();

        assert!(format!("{error:#}").contains("failed to open generation lease"));
        assert!(!store.generation_lock_path("generation-missing").exists());
    }

    #[test]
    fn existing_generation_lock_exclusive_try_acquire_rejects_invalid_names() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let error = store
            .try_acquire_existing_exclusive_generation_lock("../generation-bad")
            .unwrap_err();

        assert!(format!("{error:#}").contains("invalid generation name"));
    }

    #[test]
    fn shared_generation_lease_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();

        let error = store
            .acquire_shared_generation_lease(&external_generation)
            .unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn shared_generation_lease_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();

        let error = store.acquire_shared_generation_lease(&nested).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_current_path_preserves_whitespace() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::create_dir_all(store.generations_dir()).unwrap();
        let path = store
            .generations_dir()
            .join("generation with trailing space ");
        fs::create_dir(&path).unwrap();
        fs::write(store.current_file(), path.as_str().as_bytes()).unwrap();

        assert_eq!(
            store.current_path().unwrap(),
            path.canonicalize_utf8().unwrap()
        );
    }

    #[test]
    fn index_store_current_path_rejects_non_utf8_current_file() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::write(store.current_file(), b"\xff").unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("stream did not contain valid UTF-8"));
    }

    #[test]
    fn index_store_current_path_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();
        fs::write(
            store.current_file(),
            external_generation.as_str().as_bytes(),
        )
        .unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_current_path_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(store.current_file(), nested.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_current_path_rejects_missing_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let missing = store.generations_dir().join("missing-generation");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("failed to canonicalize"));
    }

    #[test]
    fn index_store_current_path_rejects_file_under_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let file = store.generations_dir().join("generation-file");
        fs::write(&file, b"not a directory").unwrap();
        fs::write(store.current_file(), file.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn index_store_try_current_path_returns_none_when_current_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        assert_eq!(store.try_current_path().unwrap(), None);
    }

    #[test]
    fn index_store_current_path_errors_with_update_hint_when_current_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("run `nixsearch update` first"));
    }

    #[test]
    fn index_store_current_path_errors_when_current_is_empty() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::write(store.current_file(), b"").unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("current index file is empty"));
    }

    #[test]
    fn index_store_writes_and_reads_generation_manifest() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let generation = store.create_generation_path().unwrap();

        let manifest = IndexGenerationManifest::with_generated_at(
            10,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 10,
                artifact_hash: Some("abc123".into()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        store.write_manifest(&generation, &manifest).unwrap();
        store.publish(&generation).unwrap();

        let loaded = store.current_manifest().unwrap();

        assert_eq!(loaded.document_count, 10);
        assert_eq!(loaded.targets.len(), 1);
        assert_eq!(loaded.targets[0].source, SOURCE_FIXTURES);
        assert_eq!(loaded.targets[0].ref_id, REF_SMALL);

        let temporary_manifest_files = fs::read_dir(&generation)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("index-manifest.json.tmp.")
            })
            .collect::<Vec<_>>();

        assert!(temporary_manifest_files.is_empty());
    }

    #[test]
    fn index_store_write_manifest_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();
        let manifest = IndexGenerationManifest::new(0, Vec::new()).unwrap();

        let error = store
            .write_manifest(&external_generation, &manifest)
            .unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_write_manifest_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();
        let manifest = IndexGenerationManifest::new(0, Vec::new()).unwrap();

        let error = store.write_manifest(&nested, &manifest).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_returns_none_when_current_manifest_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let manifest = store.try_current_manifest().unwrap();

        assert!(manifest.is_none());
    }

    #[test]
    fn index_store_loads_current_published_generation_metadata() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let manifest = IndexGenerationManifest::new(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 1,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();

        store.write_manifest(&generation, &manifest).unwrap();
        store.publish(&generation).unwrap();

        let loaded = store.try_current_generation_metadata().unwrap().unwrap();

        assert_eq!(loaded.path, generation.canonicalize_utf8().unwrap());
        assert_eq!(loaded.manifest.document_count, 1);
    }

    #[test]
    fn index_store_retries_current_generation_metadata_when_current_changes_after_manifest_read() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let old_generation = store.create_generation_path().unwrap();
        let old_manifest = IndexGenerationManifest::new(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 1,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = IndexGenerationManifest::new(
            2,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 2,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();

        store
            .write_manifest(&old_generation, &old_manifest)
            .unwrap();
        store.publish(&old_generation).unwrap();
        store
            .write_manifest(&new_generation, &new_manifest)
            .unwrap();

        let old_canonical = old_generation.canonicalize_utf8().unwrap();
        let new_canonical = new_generation.canonicalize_utf8().unwrap();
        let mut first_attempt = true;

        let loaded = store
            .try_current_generation_metadata_with(|store, path| {
                if first_attempt {
                    first_attempt = false;
                    assert_eq!(path, old_canonical.as_path());
                    store.publish(&new_generation).unwrap();
                }

                Ok(())
            })
            .unwrap()
            .unwrap();

        assert_eq!(loaded.path, new_canonical);
        assert_eq!(loaded.manifest.document_count, 2);
    }

    #[test]
    fn index_store_loads_current_leased_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let manifest = IndexGenerationManifest::new(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 1,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();

        store.write_manifest(&generation, &manifest).unwrap();
        store.publish(&generation).unwrap();

        let leased = store.current_leased_generation().unwrap();

        assert_eq!(leased.path(), generation.canonicalize_utf8().unwrap());
        assert_eq!(leased.manifest().document_count, 1);
        assert!(
            store
                .try_acquire_exclusive_generation_lease(leased.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn index_store_retries_current_leased_generation_when_observed_current_is_deleted_before_lease()
    {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let old_generation = store.create_generation_path().unwrap();
        let old_manifest = IndexGenerationManifest::new(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 1,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = IndexGenerationManifest::new(
            2,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 2,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap();

        store
            .write_manifest(&old_generation, &old_manifest)
            .unwrap();
        store.publish(&old_generation).unwrap();
        store
            .write_manifest(&new_generation, &new_manifest)
            .unwrap();

        let old_canonical = old_generation.canonicalize_utf8().unwrap();
        let new_canonical = new_generation.canonicalize_utf8().unwrap();
        let mut first_attempt = true;

        let loaded = store
            .try_current_leased_generation_with(|store, path| {
                if first_attempt {
                    first_attempt = false;
                    assert_eq!(path, old_canonical.as_path());
                    store.publish(&new_generation).unwrap();
                    fs::remove_dir_all(&old_generation).unwrap();
                    anyhow::bail!("simulated stale current generation deletion");
                }

                store.acquire_shared_generation_lease(path)
            })
            .unwrap()
            .unwrap();

        assert_eq!(loaded.path(), new_canonical.as_path());
        assert_eq!(loaded.manifest().document_count, 2);
        assert!(!old_generation.exists());
    }

    #[test]
    fn index_store_reports_missing_current_leased_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        assert!(store.try_current_generation_metadata().unwrap().is_none());
        assert!(store.try_current_leased_generation().unwrap().is_none());
        assert!(
            format!("{:#}", store.current_leased_generation().unwrap_err())
                .contains("run `nixsearch update` first")
        );
    }

    #[test]
    fn index_store_read_manifest_rejects_missing_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let missing_id = serde_json::json!({
            "schema_version": 1,
            "generated_at": "1970-01-01T00:00:00Z",
            "document_count": 10,
            "targets": [
                {
                    "source": SOURCE_FIXTURES,
                    "ref_id": REF_SMALL,
                    "artifact_kind": "options-json",
                    "document_count": 10,
                    "artifact_hash": "abc123"
                }
            ]
        });

        fs::write(
            store.manifest_path(&generation),
            serde_json::to_vec_pretty(&missing_id).unwrap(),
        )
        .unwrap();

        let error = store.read_manifest(&generation).unwrap_err();

        assert!(format!("{error:#}").contains("missing field `generation_id`"));
    }

    #[test]
    fn index_store_write_manifest_stores_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let manifest = IndexGenerationManifest::with_generated_at(
            10,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 10,
                artifact_hash: Some("abc123".into()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        store.write_manifest(&generation, &manifest).unwrap();

        let raw: serde_json::Value =
            serde_json::from_slice(&fs::read(store.manifest_path(&generation)).unwrap()).unwrap();
        let loaded = store.read_manifest(&generation).unwrap();
        let stored_id = raw["generation_id"].as_str().unwrap();

        assert!(stored_id.starts_with("sha256:"));
        assert_eq!(stored_id, canonical_generation_id(&loaded).unwrap());
    }

    #[test]
    fn index_store_write_manifest_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let mut manifest = IndexGenerationManifest::with_generated_at(
            10,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 10,
                artifact_hash: Some("abc123".into()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        manifest.generation_id = "sha256:wrong".to_owned();

        let error = store.write_manifest(&generation, &manifest).unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn index_store_write_seo_sidecar_rejects_mismatched_manifest_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let mut manifest = IndexGenerationManifest::with_generated_at(
            0,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 0,
                artifact_hash: None,
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let sidecar = crate::seo::SeoSidecar {
            schema_version: crate::seo::SEO_SIDECAR_SCHEMA_VERSION,
            generation_id: manifest.generation_id.clone(),
            refs: vec![crate::seo::SeoRefFacts {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                total_supported_indexed_count: 0,
                package_supported_count: 0,
                option_supported_count: 0,
                package_eligible_count: 0,
                option_eligible_count: 0,
            }],
            entries: Vec::new(),
        };

        manifest.generation_id = "sha256:wrong".to_owned();

        let error = store
            .write_seo_sidecar(
                &PublishedGeneration {
                    path: generation,
                    manifest,
                },
                &sidecar,
            )
            .unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }

    #[test]
    fn index_store_read_manifest_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let invalid = serde_json::json!({
            "schema_version": 1,
            "generated_at": "1970-01-01T00:00:00Z",
            "generation_id": "sha256:wrong",
            "document_count": 10,
            "targets": [
                {
                    "source": SOURCE_FIXTURES,
                    "ref_id": REF_SMALL,
                    "artifact_kind": "options-json",
                    "document_count": 10,
                    "artifact_hash": "abc123"
                }
            ]
        });

        fs::write(
            store.manifest_path(&generation),
            serde_json::to_vec_pretty(&invalid).unwrap(),
        )
        .unwrap();

        let error = store.read_manifest(&generation).unwrap_err();

        assert!(format!("{error:#}").contains("generation_id mismatch"));
    }
}
