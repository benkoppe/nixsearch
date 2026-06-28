mod current;
mod integrity;
mod layout;
mod leases;
mod manifest;
use camino::{Utf8Path, Utf8PathBuf};

use crate::manifest::IndexGenerationManifest;

use self::layout::GenerationLayout;
pub use self::leases::{GenerationLease, LeasedPublishedGeneration};

#[derive(Debug, Clone)]
pub struct IndexStore {
    layout: GenerationLayout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedGeneration {
    pub path: Utf8PathBuf,
    pub manifest: IndexGenerationManifest,
}

impl IndexStore {
    pub fn new(root: impl AsRef<Utf8Path>) -> Self {
        Self {
            layout: GenerationLayout::new(root),
        }
    }

    pub fn root(&self) -> &Utf8Path {
        self.layout.root()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::thread;

    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::target::RefRole;

    use crate::manifest::{
        IndexGenerationManifest, IndexTargetManifest, canonical_generation_id,
        refresh_generation_id,
    };
    use crate::store::IndexStore;

    const SOURCE_FIXTURES: &str = "fixtures";
    const REF_SMALL: &str = "small";

    fn utf8_path(path: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
    }

    fn store_for(tempdir: &TempDir) -> IndexStore {
        IndexStore::new(utf8_path(tempdir.path().to_path_buf()))
    }

    fn options_manifest(document_count: usize) -> IndexGenerationManifest {
        IndexGenerationManifest::new(
            document_count,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                target_role: RefRole::Search,
                indexes_search_documents: true,
                document_count,
                artifact_hash: None,
                revision: None,
            }],
        )
        .unwrap()
    }

    fn old_schema_manifest(document_count: usize) -> IndexGenerationManifest {
        let mut manifest = options_manifest(document_count);
        manifest.schema_version = 2;
        refresh_generation_id(&mut manifest).unwrap();
        manifest
    }

    fn write_raw_manifest(
        store: &IndexStore,
        generation: &Utf8PathBuf,
        manifest: &IndexGenerationManifest,
    ) {
        fs::write(
            store.manifest_path(generation),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
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
    fn shared_generation_lease_revalidates_after_waiting_for_exclusive_lock() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let exclusive = store
            .try_acquire_exclusive_generation_lease(&generation)
            .unwrap()
            .unwrap();
        let waiter_store = store.clone();
        let waiter_generation = generation.clone();
        let (started_tx, started_rx) = mpsc::channel();

        let waiter = thread::spawn(move || {
            waiter_store.acquire_shared_generation_lease_with_hook(&waiter_generation, || {
                started_tx.send(()).unwrap();
            })
        });

        started_rx.recv().unwrap();
        fs::remove_dir_all(&generation).unwrap();
        drop(exclusive);

        let error = waiter.join().unwrap().unwrap_err();

        assert!(format!("{error:#}").contains("failed to canonicalize"));
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
                target_role: RefRole::Search,
                indexes_search_documents: true,
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
    fn index_store_read_manifest_rejects_unsupported_schema_version() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let manifest = old_schema_manifest(1);

        write_raw_manifest(&store, &generation, &manifest);

        let error = store.read_manifest(&generation).unwrap_err();

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
    }

    #[test]
    fn index_store_current_manifest_rejects_unsupported_schema_version() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let manifest = old_schema_manifest(1);

        write_raw_manifest(&store, &generation, &manifest);
        store.publish(&generation).unwrap();

        let error = store.current_manifest().unwrap_err();

        assert!(format!("{error:#}").contains("unsupported index schema version 2"));
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
        let manifest = options_manifest(1);

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
        let old_manifest = options_manifest(1);
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = options_manifest(2);

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
        let manifest = options_manifest(1);

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
        let old_manifest = options_manifest(1);
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = options_manifest(2);

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
            .try_current_leased_generation_with(
                |store, path| {
                    if first_attempt {
                        first_attempt = false;
                        assert_eq!(path, old_canonical.as_path());
                        store.publish(&new_generation).unwrap();
                        fs::remove_dir_all(&old_generation).unwrap();
                        anyhow::bail!("simulated stale current generation deletion");
                    }

                    store.acquire_shared_generation_lease(path)
                },
                |_, _| Ok(()),
            )
            .unwrap()
            .unwrap();

        assert_eq!(loaded.path(), new_canonical.as_path());
        assert_eq!(loaded.manifest().document_count, 2);
        assert!(!old_generation.exists());
    }

    #[test]
    fn index_store_retries_current_leased_generation_when_manifest_read_fails_after_current_changes()
     {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let old_generation = store.create_generation_path().unwrap();
        let old_manifest = options_manifest(1);
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = options_manifest(2);

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
            .try_current_leased_generation_with(
                |store, path| {
                    let lease = store.acquire_shared_generation_lease(path)?;

                    if first_attempt {
                        first_attempt = false;
                        assert_eq!(path, old_canonical.as_path());
                        store.publish(&new_generation).unwrap();
                        fs::remove_file(store.manifest_path(path)).unwrap();
                    }

                    Ok(lease)
                },
                |_, _| Ok(()),
            )
            .unwrap()
            .unwrap();

        assert_eq!(loaded.path(), new_canonical.as_path());
        assert_eq!(loaded.manifest().document_count, 2);
    }

    #[test]
    fn index_store_retries_current_leased_generation_when_current_changes_after_manifest_read() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let old_generation = store.create_generation_path().unwrap();
        let old_manifest = options_manifest(1);
        let new_generation = store.create_generation_path().unwrap();
        let new_manifest = options_manifest(2);

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
            .try_current_leased_generation_with(
                IndexStore::acquire_shared_generation_lease,
                |store, path| {
                    if first_attempt {
                        first_attempt = false;
                        assert_eq!(path, old_canonical.as_path());
                        store.publish(&new_generation).unwrap();
                    }

                    Ok(())
                },
            )
            .unwrap()
            .unwrap();

        assert_eq!(loaded.path(), new_canonical.as_path());
        assert_eq!(loaded.manifest().document_count, 2);
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
            "schema_version": 5,
            "generated_at": "1970-01-01T00:00:00Z",
            "document_count": 10,
            "targets": [
                {
                    "source": SOURCE_FIXTURES,
                    "ref_id": REF_SMALL,
                    "artifact_kind": "options-json",
                    "target_role": "search",
                    "indexes_search_documents": true,
                    "document_count": 10,
                    "artifact_hash": "abc123",
                    "revision": null
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
                target_role: RefRole::Search,
                indexes_search_documents: true,
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
                target_role: RefRole::Search,
                indexes_search_documents: true,
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
    fn index_store_write_manifest_rejects_artifact_only_document_counts() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let manifest = IndexGenerationManifest::with_generated_at(
            1,
            vec![IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::FlakeInfoJson,
                target_role: RefRole::ArtifactOnly,
                indexes_search_documents: false,
                document_count: 1,
                artifact_hash: None,
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        let error = store.write_manifest(&generation, &manifest).unwrap_err();

        assert!(format!("{error:#}").contains("non-indexing"));
    }

    #[test]
    fn index_store_read_manifest_rejects_mismatched_generation_id() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        let invalid = serde_json::json!({
            "schema_version": 5,
            "generated_at": "1970-01-01T00:00:00Z",
            "generation_id": "sha256:wrong",
            "document_count": 10,
            "targets": [
                {
                    "source": SOURCE_FIXTURES,
                    "ref_id": REF_SMALL,
                    "artifact_kind": "options-json",
                    "target_role": "search",
                    "indexes_search_documents": true,
                    "document_count": 10,
                    "artifact_hash": "abc123",
                    "revision": null
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
