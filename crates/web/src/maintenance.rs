use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use time::OffsetDateTime;

use nixsearch_config::app::AppConfig;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::store::{IndexStore, PublishedGeneration};
use nixsearch_ops::targets::{TargetKey, all_targets, missing_configured_target_keys};
use nixsearch_ops::{cleanup, generate, lock, seo};
use nixsearch_service::{ReconcileReport, SearchService};

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MANIFEST_ERROR_RETRY: Duration = Duration::from_secs(60);
const MIN_LOCK_BUSY_RETRY: Duration = Duration::from_secs(60);
const MAX_LOCK_BUSY_RETRY: Duration = Duration::from_secs(10 * 60);
const MIN_FAILURE_RETRY: Duration = Duration::from_secs(60);
const MAX_FAILURE_RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenanceOutcome {
    Completed,
    LockBusy,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegenerationModes {
    recovery_enabled: bool,
    scheduled_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidCurrentAction {
    RecoveryRegeneration,
    ScheduledRegeneration,
    Retry,
}

enum CurrentGenerationStatus {
    Missing,
    MissingConfiguredTargets {
        generation: PublishedGeneration,
    },
    Invalid {
        generation: PublishedGeneration,
        error: anyhow::Error,
    },
    Valid {
        generation: PublishedGeneration,
    },
}

pub(crate) fn spawn(config: Arc<AppConfig>, search: SearchService) {
    let interval = config
        .server
        .schedule
        .parse_interval()
        .expect("schedule interval already validated");

    tokio::spawn(async move {
        run_loop(config, search, interval).await;
    });
}

async fn run_loop(config: Arc<AppConfig>, search: SearchService, interval: Duration) {
    let modes = regeneration_modes(&config);

    loop {
        let (generation, reloaded) = match search.reconcile_current_generation() {
            Ok(ReconcileReport::Superseded) => {
                tracing::debug!("published index generation changed during reconciliation");

                tokio::time::sleep(Duration::ZERO).await;
                continue;
            }
            Ok(ReconcileReport::Reloaded { generation }) => {
                tracing::info!(
                    generation = %generation.path,
                    "detected published index generation change"
                );

                (generation, true)
            }
            Ok(ReconcileReport::Unchanged { generation }) => (generation, false),
            Err(error) => {
                tracing::error!(
                    "failed to switch to published index generation; continuing to serve previous generation: {error:#}"
                );

                handle_invalid_current_generation(&config, modes, interval).await;
                continue;
            }
        };

        if reloaded {
            run_cleanup_after_reload(&config).await;
        }

        if !modes.scheduled_enabled && !modes.recovery_enabled {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            continue;
        }

        if current_generation_missing_configured_targets(&config, &generation) {
            let outcome = if modes.recovery_enabled {
                run_recovery_regeneration(&config).await
            } else {
                run_scheduled_regeneration(&config, interval).await
            };
            sleep_after_regeneration_outcome(outcome, interval).await;
            continue;
        }

        if !modes.scheduled_enabled {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            continue;
        }

        let Some(next_due) = next_due(generation.manifest.generated_at, interval) else {
            tracing::error!("failed to compute next scheduled regeneration time");
            tokio::time::sleep(MANIFEST_ERROR_RETRY.min(RECONCILE_INTERVAL)).await;
            continue;
        };

        let now = OffsetDateTime::now_utc();

        if now < next_due {
            tokio::time::sleep(duration_until(next_due, now).min(RECONCILE_INTERVAL)).await;
            continue;
        }

        let outcome = run_scheduled_regeneration(&config, interval).await;
        sleep_after_regeneration_outcome(outcome, interval).await;
    }
}

fn regeneration_modes(config: &AppConfig) -> RegenerationModes {
    let has_targets = has_configured_targets(config);

    RegenerationModes {
        recovery_enabled: config.server.bootstrap && has_targets,
        scheduled_enabled: config.server.schedule.enabled && has_targets,
    }
}

fn invalid_current_action(modes: RegenerationModes) -> InvalidCurrentAction {
    if modes.recovery_enabled {
        InvalidCurrentAction::RecoveryRegeneration
    } else if modes.scheduled_enabled {
        InvalidCurrentAction::ScheduledRegeneration
    } else {
        InvalidCurrentAction::Retry
    }
}

async fn handle_invalid_current_generation(
    config: &AppConfig,
    modes: RegenerationModes,
    interval: Duration,
) {
    if config.public_seo_enabled() {
        let repair = run_seo_sidecar_repair(config).await;
        match repair {
            MaintenanceOutcome::Completed => return,
            MaintenanceOutcome::LockBusy => {
                sleep_after_regeneration_outcome(repair, interval).await;
                return;
            }
            MaintenanceOutcome::Failed => {}
        }
    }

    match invalid_current_action(modes) {
        InvalidCurrentAction::RecoveryRegeneration => {
            tracing::info!("published index generation is invalid; running recovery regeneration");
            let outcome = run_recovery_regeneration(config).await;
            sleep_after_regeneration_outcome(outcome, interval).await;
        }
        InvalidCurrentAction::ScheduledRegeneration => {
            tracing::info!("published index generation is invalid; running scheduled regeneration");
            let outcome = run_scheduled_regeneration(config, interval).await;
            sleep_after_regeneration_outcome(outcome, interval).await;
        }
        InvalidCurrentAction::Retry => {
            tokio::time::sleep(MANIFEST_ERROR_RETRY.min(RECONCILE_INTERVAL)).await;
        }
    }
}

async fn run_seo_sidecar_repair(config: &AppConfig) -> MaintenanceOutcome {
    let update_lock = match lock::try_acquire_update_lock(&config.data.index_dir) {
        Ok(Some(update_lock)) => update_lock,
        Ok(None) => return MaintenanceOutcome::LockBusy,
        Err(error) => {
            tracing::error!("failed to acquire maintenance lock for SEO sidecar repair: {error:#}");
            return MaintenanceOutcome::Failed;
        }
    };

    match seo::repair_current_seo_sidecar_under_lock(config, &update_lock) {
        Ok(
            seo::SeoSidecarRepairOutcome::AlreadySeoVerified { .. }
            | seo::SeoSidecarRepairOutcome::Repaired { .. }
            | seo::SeoSidecarRepairOutcome::SupersededBeforeRepair
            | seo::SeoSidecarRepairOutcome::SupersededAfterRepair,
        ) => MaintenanceOutcome::Completed,
        Ok(seo::SeoSidecarRepairOutcome::MissingCurrent) => MaintenanceOutcome::Failed,
        Ok(seo::SeoSidecarRepairOutcome::Unrepairable { generation, error }) => {
            tracing::warn!(generation = %generation.path, "current SEO sidecar is not repairable: {error}");
            MaintenanceOutcome::Failed
        }
        Ok(seo::SeoSidecarRepairOutcome::RepairFailed { generation, error }) => {
            tracing::warn!(generation = %generation.path, "failed to repair current SEO sidecar: {error}");
            MaintenanceOutcome::Failed
        }
        Err(error) => {
            tracing::warn!("failed to repair current SEO sidecar: {error:#}");
            MaintenanceOutcome::Failed
        }
    }
}

async fn sleep_after_regeneration_outcome(outcome: MaintenanceOutcome, interval: Duration) {
    match outcome {
        MaintenanceOutcome::Completed => {
            // The next loop iteration will reconcile against the just-published generation.
        }
        MaintenanceOutcome::LockBusy => {
            tracing::info!("index regeneration skipped; maintenance lock is held");
            let delay = clamp_duration(interval, MIN_LOCK_BUSY_RETRY, MAX_LOCK_BUSY_RETRY)
                .min(RECONCILE_INTERVAL);
            tokio::time::sleep(delay).await;
        }
        MaintenanceOutcome::Failed => {
            let delay = clamp_duration(interval, MIN_FAILURE_RETRY, MAX_FAILURE_RETRY)
                .min(RECONCILE_INTERVAL);
            tokio::time::sleep(delay).await;
        }
    }
}

async fn run_scheduled_regeneration(config: &AppConfig, interval: Duration) -> MaintenanceOutcome {
    let update_lock = match lock::try_acquire_update_lock(&config.data.index_dir) {
        Ok(Some(update_lock)) => update_lock,
        Ok(None) => return MaintenanceOutcome::LockBusy,
        Err(error) => {
            tracing::error!("failed to acquire maintenance lock: {error:#}");
            return MaintenanceOutcome::Failed;
        }
    };

    let index_store = IndexStore::new(&config.data.index_dir);
    match current_generation_needs_regeneration(
        config,
        &index_store,
        interval,
        OffsetDateTime::now_utc(),
    ) {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                "scheduled regeneration skipped; current index was refreshed before lock acquisition"
            );
            return MaintenanceOutcome::Completed;
        }
        Err(error) => {
            tracing::error!("failed to verify scheduled regeneration due state: {error:#}");
            return MaintenanceOutcome::Failed;
        }
    }

    run_locked_regeneration(config, update_lock).await
}

async fn run_recovery_regeneration(config: &AppConfig) -> MaintenanceOutcome {
    let update_lock = match lock::try_acquire_update_lock(&config.data.index_dir) {
        Ok(Some(update_lock)) => update_lock,
        Ok(None) => return MaintenanceOutcome::LockBusy,
        Err(error) => {
            tracing::error!("failed to acquire maintenance lock: {error:#}");
            return MaintenanceOutcome::Failed;
        }
    };

    let index_store = IndexStore::new(&config.data.index_dir);
    match current_generation_status(config, &index_store) {
        Ok(CurrentGenerationStatus::Valid { generation }) => {
            tracing::info!(
                generation = %generation.path,
                "recovery regeneration skipped; current index generation is valid"
            );
            return MaintenanceOutcome::Completed;
        }
        Ok(CurrentGenerationStatus::MissingConfiguredTargets { generation }) => {
            tracing::warn!(
                generation = %generation.path,
                "current index remains missing configured targets after lock acquisition; rebuilding"
            );
        }
        Ok(CurrentGenerationStatus::Invalid { generation, error }) => {
            tracing::warn!(
                generation = %generation.path,
                "current index generation remains invalid after lock acquisition; rebuilding: {error:#}"
            );
        }
        Ok(CurrentGenerationStatus::Missing) => {
            tracing::warn!("current index remains missing after lock acquisition; rebuilding");
        }
        Err(error) => {
            tracing::warn!(
                "current index generation remains unreadable after lock acquisition; rebuilding: {error:#}"
            );
        }
    }

    run_locked_regeneration(config, update_lock).await
}

async fn run_locked_regeneration(
    config: &AppConfig,
    update_lock: lock::UpdateLock,
) -> MaintenanceOutcome {
    let start = Instant::now();

    let result = generate::regenerate_all(config).await;

    drop(update_lock);

    match result {
        Ok(_) => {
            tracing::info!(
                elapsed_secs = start.elapsed().as_secs_f64(),
                "index regeneration completed"
            );
            MaintenanceOutcome::Completed
        }
        Err(error) => {
            tracing::error!("index regeneration failed: {error:#}");
            MaintenanceOutcome::Failed
        }
    }
}

async fn run_cleanup_after_reload(config: &AppConfig) {
    let update_lock = match lock::try_acquire_update_lock(&config.data.index_dir) {
        Ok(Some(update_lock)) => update_lock,
        Ok(None) => {
            tracing::info!("index cleanup skipped; maintenance lock is held");
            return;
        }
        Err(error) => {
            tracing::warn!("failed to acquire maintenance lock for cleanup: {error:#}");
            return;
        }
    };

    match cleanup::cleanup_under_lock(config, &update_lock).await {
        Ok(report) => cleanup::log_report(&report),
        Err(error) => tracing::warn!("index cleanup failed: {error:#}"),
    }

    drop(update_lock);
}

pub(crate) fn current_generation_needs_regeneration(
    config: &AppConfig,
    index_store: &IndexStore,
    interval: Duration,
    now: OffsetDateTime,
) -> Result<bool> {
    match current_generation_status(config, index_store) {
        Ok(CurrentGenerationStatus::Missing) => Ok(true),
        Ok(CurrentGenerationStatus::MissingConfiguredTargets { .. }) => Ok(true),
        Ok(CurrentGenerationStatus::Invalid { generation, error }) => {
            tracing::warn!(
                generation = %generation.path,
                "treating invalid current index generation as needing regeneration: {error:#}"
            );
            Ok(true)
        }
        Ok(CurrentGenerationStatus::Valid { generation }) => {
            let Some(next_due) = next_due(generation.manifest.generated_at, interval) else {
                bail!("failed to compute next scheduled regeneration time")
            };

            Ok(now >= next_due)
        }
        Err(error) => {
            tracing::warn!(
                "treating unreadable current index generation as needing regeneration: {error:#}"
            );
            Ok(true)
        }
    }
}

fn current_generation_status(
    config: &AppConfig,
    index_store: &IndexStore,
) -> Result<CurrentGenerationStatus> {
    let Some(generation) = index_store.try_current_leased_generation()? else {
        return Ok(CurrentGenerationStatus::Missing);
    };
    let published = generation.to_published_generation();

    if !missing_configured_targets(config, generation.manifest()).is_empty() {
        return Ok(CurrentGenerationStatus::MissingConfiguredTargets {
            generation: published,
        });
    }

    if let Err(error) = SearchService::verify_leased_generation_structural(config, &generation) {
        return Ok(CurrentGenerationStatus::Invalid {
            generation: published,
            error,
        });
    }

    if config.public_seo_enabled()
        && let Err(error) = SearchService::verify_leased_generation_seo(config, &generation)
    {
        return Ok(CurrentGenerationStatus::Invalid {
            generation: published,
            error,
        });
    }

    Ok(CurrentGenerationStatus::Valid {
        generation: published,
    })
}

pub(crate) fn current_generation_missing_configured_targets(
    config: &AppConfig,
    generation: &PublishedGeneration,
) -> bool {
    !missing_configured_targets(config, &generation.manifest).is_empty()
}

pub(crate) fn missing_configured_targets(
    config: &AppConfig,
    manifest: &IndexGenerationManifest,
) -> BTreeSet<TargetKey> {
    missing_configured_target_keys(config, manifest)
}

pub(crate) fn has_configured_targets(config: &AppConfig) -> bool {
    !all_targets(config).is_empty()
}

pub(crate) fn next_due(generated_at: OffsetDateTime, interval: Duration) -> Option<OffsetDateTime> {
    let interval = time::Duration::try_from(interval).ok()?;
    generated_at.checked_add(interval)
}

pub(crate) fn duration_until(next: OffsetDateTime, now: OffsetDateTime) -> Duration {
    if next <= now {
        return Duration::ZERO;
    }

    (next - now).try_into().unwrap_or(Duration::ZERO)
}

pub(crate) fn clamp_duration(value: Duration, min: Duration, max: Duration) -> Duration {
    value.max(min).min(max)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::{fs, path::PathBuf};

    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::target::RefRole;
    use nixsearch_index::search::SearchIndex;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        publish_canonical_index, publish_canonical_index_with_generated_at,
    };
    use nixsearch_ops::targets::TargetKey;
    use nixsearch_test_support::{
        REF_SMALL, SOURCE_FIXTURES, app_config, app_config_with_public_url, utf8_path_buf,
    };
    use tempfile::tempdir;
    use time::Duration as TimeDuration;

    use super::{
        InvalidCurrentAction, MaintenanceOutcome, RegenerationModes, clamp_duration,
        current_generation_needs_regeneration, duration_until, invalid_current_action,
        missing_configured_targets, next_due, regeneration_modes, run_recovery_regeneration,
    };

    #[test]
    fn regeneration_modes_enable_recovery_without_scheduling() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let config = app_config_with_public_url(&index_dir);

        let modes = regeneration_modes(&config);

        assert!(modes.recovery_enabled);
        assert!(!modes.scheduled_enabled);
    }

    #[test]
    fn regeneration_modes_keep_bootstrap_disabled_as_recovery_opt_out() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.server.bootstrap = false;
        config.server.schedule.enabled = true;

        let modes = regeneration_modes(&config);

        assert!(!modes.recovery_enabled);
        assert!(modes.scheduled_enabled);
    }

    #[test]
    fn invalid_current_action_prefers_recovery_when_enabled() {
        let modes = RegenerationModes {
            recovery_enabled: true,
            scheduled_enabled: true,
        };

        assert_eq!(
            invalid_current_action(modes),
            InvalidCurrentAction::RecoveryRegeneration
        );
    }

    #[test]
    fn invalid_current_action_uses_scheduled_regeneration_without_recovery() {
        let modes = RegenerationModes {
            recovery_enabled: false,
            scheduled_enabled: true,
        };

        assert_eq!(
            invalid_current_action(modes),
            InvalidCurrentAction::ScheduledRegeneration
        );
    }

    #[test]
    fn invalid_current_action_retries_when_regeneration_is_disabled() {
        let modes = RegenerationModes {
            recovery_enabled: false,
            scheduled_enabled: false,
        };

        assert_eq!(invalid_current_action(modes), InvalidCurrentAction::Retry);
    }

    #[test]
    fn next_due_adds_interval() {
        let generated_at = time::OffsetDateTime::UNIX_EPOCH;
        let next = next_due(generated_at, std::time::Duration::from_secs(60)).unwrap();

        assert_eq!(next, generated_at + TimeDuration::seconds(60));
    }

    #[test]
    fn duration_until_floors_past_times_to_zero() {
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::seconds(60);
        let next = time::OffsetDateTime::UNIX_EPOCH;

        assert_eq!(duration_until(next, now), std::time::Duration::ZERO);
    }

    #[test]
    fn clamp_duration_applies_bounds() {
        assert_eq!(
            clamp_duration(
                std::time::Duration::from_secs(5),
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(20)
            ),
            std::time::Duration::from_secs(10)
        );

        assert_eq!(
            clamp_duration(
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(10),
                std::time::Duration::from_secs(20)
            ),
            std::time::Duration::from_secs(20)
        );
    }

    #[test]
    fn current_generation_needs_regeneration_returns_true_for_stale_generation() {
        let tempdir = tempdir().unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2);
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        publish_canonical_index_with_generated_at(&index_dir, now - TimeDuration::hours(2));
        let store = IndexStore::new(&index_dir);

        let config = app_config(&index_dir);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            now,
        )
        .unwrap();

        assert!(needs_regeneration);
    }

    #[test]
    fn current_generation_needs_regeneration_returns_false_for_fresh_generation() {
        let tempdir = tempdir().unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2);
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        publish_canonical_index_with_generated_at(&index_dir, now);
        let store = IndexStore::new(&index_dir);

        let config = app_config(&index_dir);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            now,
        )
        .unwrap();

        assert!(!needs_regeneration);
    }

    #[tokio::test]
    async fn recovery_regeneration_rebuilds_fresh_invalid_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let store = IndexStore::new(&index_dir);
        let mut manifest = store.current_manifest().unwrap();
        let generated_at = time::OffsetDateTime::now_utc();
        manifest.generated_at = generated_at;
        let broken = store.create_generation_path().unwrap();
        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();

        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", index_dir.join("artifacts"));
        let interval = Duration::from_secs(60 * 60);
        let needs_regeneration =
            current_generation_needs_regeneration(&config, &store, interval, generated_at).unwrap();

        assert!(needs_regeneration);
        assert!(SearchIndex::open(&broken).is_err());

        let outcome = run_recovery_regeneration(&config).await;

        assert_eq!(outcome, MaintenanceOutcome::Completed);
        let current = store.current_path().unwrap();
        assert_ne!(current, broken);
        SearchIndex::open(store.index_path(&current)).unwrap();
    }

    #[tokio::test]
    async fn recovery_regeneration_skips_when_current_was_repaired_before_lock_check() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let store = IndexStore::new(&index_dir);
        let manifest = store.current_manifest().unwrap();
        let broken = store.create_generation_path().unwrap();
        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();
        assert!(SearchIndex::open(&broken).is_err());

        let repaired = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", index_dir.join("artifacts"));

        let outcome = run_recovery_regeneration(&config).await;

        assert_eq!(outcome, MaintenanceOutcome::Completed);
        let current = store.current_path().unwrap();
        assert_eq!(current, repaired);
        SearchIndex::open(store.index_path(&current)).unwrap();
    }

    #[tokio::test]
    async fn recovery_regeneration_rebuilds_missing_targets_when_schedule_disabled() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);

        let store = IndexStore::new(&index_dir);
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", index_dir.join("artifacts"));
        config.server.schedule.enabled = false;
        let extra_source = config.sources[SOURCE_FIXTURES].clone();
        config.sources.insert("extra".to_owned(), extra_source);

        let outcome = run_recovery_regeneration(&config).await;

        assert_eq!(outcome, MaintenanceOutcome::Completed);
        let manifest = store.current_manifest().unwrap();
        assert!(
            manifest
                .targets
                .iter()
                .any(|target| target.source == SOURCE_FIXTURES && target.ref_id == REF_SMALL)
        );
        assert!(
            manifest
                .targets
                .iter()
                .any(|target| target.source == "extra" && target.ref_id == REF_SMALL)
        );
    }

    #[test]
    fn current_generation_needs_regeneration_returns_true_when_current_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);

        let config = app_config(&index_dir);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert!(needs_regeneration);
    }

    #[test]
    fn current_generation_needs_regeneration_returns_true_for_invalid_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);
        fs::create_dir_all(&index_dir).unwrap();
        let missing = store.generations_dir().join("missing");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let config = app_config(&index_dir);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert!(needs_regeneration);
    }

    #[test]
    fn current_generation_needs_regeneration_returns_true_when_seo_sidecar_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        fs::remove_file(SeoFactsArtifact::path(&published_path)).unwrap();

        let config = app_config_with_public_url(&index_dir);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert!(needs_regeneration);
    }

    #[test]
    fn current_generation_needs_regeneration_returns_true_when_configured_target_missing() {
        let tempdir = tempdir().unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2);
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index_with_generated_at(&index_dir, now);
        let store = IndexStore::new(&index_dir);
        let mut config = app_config(&index_dir);
        let extra_source = config.sources["fixtures"].clone();
        config.sources.insert("extra".to_owned(), extra_source);

        let needs_regeneration = current_generation_needs_regeneration(
            &config,
            &store,
            Duration::from_secs(60 * 60),
            now,
        )
        .unwrap();

        assert!(needs_regeneration);
    }

    #[test]
    fn missing_configured_targets_reports_source_ref_with_stale_artifact_kind() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.current_manifest().unwrap();
        let mut config = app_config(&index_dir);
        let ref_config = &mut config
            .sources
            .get_mut(SOURCE_FIXTURES)
            .expect("fixture source exists")
            .refs[0];
        ref_config.role = RefRole::ArtifactOnly;
        ref_config.producer = ProducerConfig::ExistingFile {
            path: PathBuf::from("unused.json"),
            artifact: ArtifactKind::FlakeInfoJson,
        };

        let missing = missing_configured_targets(&config, &manifest);

        assert_eq!(
            missing,
            [TargetKey::new(
                SOURCE_FIXTURES,
                REF_SMALL,
                ArtifactKind::FlakeInfoJson,
                RefRole::ArtifactOnly,
            )]
            .into()
        );
    }
}
