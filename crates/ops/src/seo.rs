use anyhow::{Context, Result};

use nixsearch_config::app::AppConfig;
use nixsearch_index::generation::open_structurally_verified_generation;
use nixsearch_index::generation_validator::GenerationValidator;
use nixsearch_index::seo_sidecar::{ManifestCheckedSeoFacts, SeoFactsArtifact};
use nixsearch_index::store::{IndexStore, PublishedGeneration};

use crate::lock::{UpdateLock, update_lock_path};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeoSidecarRepairOutcome {
    AlreadySeoVerified {
        generation: PublishedGeneration,
    },
    Repaired {
        generation: PublishedGeneration,
    },
    MissingCurrent,
    SupersededBeforeRepair,
    SupersededAfterRepair,
    Unrepairable {
        generation: PublishedGeneration,
        error: String,
    },
    RepairFailed {
        generation: PublishedGeneration,
        error: String,
    },
}

pub fn repair_current_seo_sidecar_under_lock(
    config: &AppConfig,
    update_lock: &UpdateLock,
) -> Result<SeoSidecarRepairOutcome> {
    let expected_lock_path = update_lock_path(&config.data.index_dir);
    if update_lock.path() != expected_lock_path {
        anyhow::bail!(
            "maintenance lock {} does not protect index directory {}",
            update_lock.path(),
            config.data.index_dir
        );
    }

    let index_store = IndexStore::new(&config.data.index_dir);
    let validator = GenerationValidator::new(index_store.clone());
    let Some(candidate) = index_store.try_current_generation_metadata()? else {
        return Ok(SeoSidecarRepairOutcome::MissingCurrent);
    };

    if validator
        .validate_seo_verified_published_generation_unleased(&candidate)
        .is_ok()
    {
        return Ok(SeoSidecarRepairOutcome::AlreadySeoVerified {
            generation: candidate,
        });
    }

    let structural = match open_structurally_verified_generation(
        &index_store.index_path(&candidate.path),
        &candidate.manifest,
    ) {
        Ok(structural) => structural,
        Err(error) => {
            return Ok(SeoSidecarRepairOutcome::Unrepairable {
                generation: candidate,
                error: format!("{error:#}"),
            });
        }
    };

    if !published_generation_is_current(&index_store, &candidate)? {
        return Ok(SeoSidecarRepairOutcome::SupersededBeforeRepair);
    }

    let sidecar =
        match ManifestCheckedSeoFacts::new(structural.scan.seo_sidecar, &candidate.manifest) {
            Ok(sidecar) => sidecar,
            Err(error) => {
                return Ok(SeoSidecarRepairOutcome::RepairFailed {
                    generation: candidate,
                    error: format!("{error:#}"),
                });
            }
        };
    if let Err(error) = SeoFactsArtifact::write_manifest_checked_without_index_validation(
        &index_store,
        &candidate,
        &sidecar,
    ) {
        return Ok(SeoSidecarRepairOutcome::RepairFailed {
            generation: candidate,
            error: format!("{error:#}"),
        });
    }

    if let Err(error) = index_store.write_integrity(&candidate, true) {
        return Ok(SeoSidecarRepairOutcome::RepairFailed {
            generation: candidate,
            error: format!("{error:#}"),
        });
    }

    if !published_generation_is_current(&index_store, &candidate)? {
        return Ok(SeoSidecarRepairOutcome::SupersededAfterRepair);
    }

    validator
        .validate_seo_verified_published_generation_unleased(&candidate)
        .context("repaired SEO sidecar did not validate")?;

    Ok(SeoSidecarRepairOutcome::Repaired {
        generation: candidate,
    })
}

fn published_generation_is_current(
    index_store: &IndexStore,
    generation: &PublishedGeneration,
) -> Result<bool> {
    let Some(current) = index_store.try_current_generation_metadata()? else {
        return Ok(false);
    };

    Ok(current == *generation)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use nixsearch_index::generation_validator::GenerationValidator;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::publish_canonical_index;
    use nixsearch_test_support::{app_config_with_public_url, utf8_path_buf};

    use crate::lock::acquire_update_lock;
    use crate::seo::{SeoSidecarRepairOutcome, repair_current_seo_sidecar_under_lock};

    #[test]
    fn repair_current_seo_sidecar_rejects_lock_for_different_index_dir() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let other_index_dir = utf8_path_buf(tempdir.path().join("other-indexes"));
        let config = app_config_with_public_url(&index_dir);
        let update_lock = acquire_update_lock(&other_index_dir).unwrap();

        let error = repair_current_seo_sidecar_under_lock(&config, &update_lock).unwrap_err();

        assert!(format!("{error:#}").contains("does not protect index directory"));
    }

    #[test]
    fn repair_current_seo_sidecar_rewrites_integrity_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let path = publish_canonical_index(&index_dir);
        let config = app_config_with_public_url(&index_dir);
        let store = IndexStore::new(&index_dir);
        let generation = store.try_current_generation_metadata().unwrap().unwrap();

        std::fs::remove_file(SeoFactsArtifact::path(&path)).unwrap();
        std::fs::remove_file(store.integrity_path(&path)).unwrap();

        let update_lock = acquire_update_lock(&index_dir).unwrap();
        let outcome = repair_current_seo_sidecar_under_lock(&config, &update_lock).unwrap();

        assert!(matches!(outcome, SeoSidecarRepairOutcome::Repaired { .. }));
        assert!(SeoFactsArtifact::path(&path).exists());
        assert!(store.integrity_path(&path).exists());
        GenerationValidator::new(store)
            .validate_seo_verified_published_generation_unleased(&generation)
            .unwrap();
    }
}
