use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use time::OffsetDateTime;

use nixsearch_config::AppConfig;
use nixsearch_index::{IndexGenerationManifest, IndexStore};
use nixsearch_ops::generate;
use nixsearch_ops::lock;
use nixsearch_ops::targets::all_targets;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MANIFEST_ERROR_RETRY: Duration = Duration::from_secs(60);
const MIN_LOCK_BUSY_RETRY: Duration = Duration::from_secs(60);
const MAX_LOCK_BUSY_RETRY: Duration = Duration::from_secs(10 * 60);
const MIN_FAILURE_RETRY: Duration = Duration::from_secs(60);
const MAX_FAILURE_RETRY: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
pub(crate) struct PublishedGeneration {
    pub path: Utf8PathBuf,
    pub manifest: IndexGenerationManifest,
}

#[derive(Debug, Clone)]
pub(crate) enum CurrentGeneration {
    Missing,
    Found(PublishedGeneration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaintenanceOutcome {
    Completed,
    LockBusy,
    Failed,
}

pub(crate) fn spawn(
    config: Arc<AppConfig>,
    index_path: Arc<RwLock<Utf8PathBuf>>,
    generated_at: Arc<RwLock<OffsetDateTime>>,
) {
    let interval = config
        .server
        .schedule
        .parse_interval()
        .expect("schedule interval already validated");

    tokio::spawn(async move {
        run_loop(config, index_path, generated_at, interval).await;
    });
}

async fn run_loop(
    config: Arc<AppConfig>,
    index_path: Arc<RwLock<Utf8PathBuf>>,
    generated_at: Arc<RwLock<OffsetDateTime>>,
    interval: Duration,
) {
    let index_store = IndexStore::new(&config.data.index_dir);
    let regeneration_enabled = config.server.schedule.enabled && has_configured_targets(&config);

    loop {
        let generation = match read_current_generation(&index_store) {
            Ok(CurrentGeneration::Found(generation)) => generation,
            Ok(CurrentGeneration::Missing) => {
                tracing::warn!("current index disappeared during maintenance loop");

                if !regeneration_enabled {
                    tokio::time::sleep(MANIFEST_ERROR_RETRY.min(RECONCILE_INTERVAL)).await;
                    continue;
                }

                let outcome = run_scheduled_regeneration(&config, interval).await;
                sleep_after_regeneration_outcome(outcome, interval).await;

                continue;
            }
            Err(error) => {
                tracing::warn!("failed to read current index generation: {error:#}");

                if regeneration_enabled {
                    let outcome = run_scheduled_regeneration(&config, interval).await;
                    sleep_after_regeneration_outcome(outcome, interval).await;
                    continue;
                }

                tokio::time::sleep(MANIFEST_ERROR_RETRY.min(RECONCILE_INTERVAL)).await;
                continue;
            }
        };

        reconcile_served_generation(&index_path, &generation.path);
        reconcile_generated_at(&generated_at, generation.manifest.generated_at);

        if !regeneration_enabled {
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

async fn sleep_after_regeneration_outcome(outcome: MaintenanceOutcome, interval: Duration) {
    match outcome {
        MaintenanceOutcome::Completed => {
            // The next loop iteration will reconcile against the just-published generation.
        }
        MaintenanceOutcome::LockBusy => {
            tracing::info!("scheduled regeneration skipped; maintenance lock is held");
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
    match current_generation_is_due(&index_store, interval, OffsetDateTime::now_utc()) {
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

    let start = Instant::now();

    let result = generate::regenerate_all(config).await;

    drop(update_lock);

    match result {
        Ok(_) => {
            tracing::info!(
                elapsed_secs = start.elapsed().as_secs_f64(),
                "scheduled regeneration completed"
            );
            MaintenanceOutcome::Completed
        }
        Err(error) => {
            tracing::error!("scheduled regeneration failed: {error:#}");
            MaintenanceOutcome::Failed
        }
    }
}

pub(crate) fn current_generation_is_due(
    index_store: &IndexStore,
    interval: Duration,
    now: OffsetDateTime,
) -> Result<bool> {
    match read_current_generation(index_store) {
        Ok(CurrentGeneration::Found(generation)) => {
            let Some(next_due) = next_due(generation.manifest.generated_at, interval) else {
                bail!("failed to compute next scheduled regeneration time")
            };

            Ok(now >= next_due)
        }
        Ok(CurrentGeneration::Missing) => Ok(true),
        Err(error) => {
            tracing::warn!("treating unreadable current index generation as due: {error:#}");
            Ok(true)
        }
    }
}

pub(crate) fn read_current_generation(index_store: &IndexStore) -> Result<CurrentGeneration> {
    let Some(path) = index_store.try_current_path()? else {
        return Ok(CurrentGeneration::Missing);
    };

    let manifest = index_store.read_manifest(&path)?;

    Ok(CurrentGeneration::Found(PublishedGeneration {
        path,
        manifest,
    }))
}

pub(crate) fn reconcile_served_generation(
    index_path: &Arc<RwLock<Utf8PathBuf>>,
    published_path: &Utf8Path,
) {
    let mut served_path = index_path.write().expect("index path lock poisoned");

    if served_path.as_path() != published_path {
        tracing::info!(
            old = %served_path,
                new = %published_path,
                "detected published index generation change"
        );

        *served_path = published_path.to_owned();
    }
}

pub(crate) fn reconcile_generated_at(
    generated_at: &Arc<RwLock<OffsetDateTime>>,
    manifest_generated_at: OffsetDateTime,
) {
    let mut current = generated_at.write().expect("generated_at lock poisoned");

    if *current != manifest_generated_at {
        *current = manifest_generated_at;
    }
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
    use std::fs;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use camino::Utf8PathBuf;
    use nixsearch_index::IndexStore;
    use nixsearch_index_test_support::{
        assert_canonical_manifest_targets, publish_canonical_index,
        publish_canonical_index_with_generated_at,
    };
    use nixsearch_test_support::utf8_path_buf;
    use tempfile::tempdir;
    use time::Duration as TimeDuration;

    use super::{
        CurrentGeneration, clamp_duration, current_generation_is_due, duration_until, next_due,
        read_current_generation, reconcile_served_generation,
    };

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
    fn reconcile_updates_changed_path() {
        let path = Arc::new(RwLock::new(Utf8PathBuf::from("/old")));

        reconcile_served_generation(&path, camino::Utf8Path::new("/new"));

        assert_eq!(*path.read().unwrap(), Utf8PathBuf::from("/new"));
    }

    #[test]
    fn reconcile_keeps_current_path() {
        let path = Arc::new(RwLock::new(Utf8PathBuf::from("/current")));

        reconcile_served_generation(&path, camino::Utf8Path::new("/current"));

        assert_eq!(*path.read().unwrap(), Utf8PathBuf::from("/current"));
    }

    #[test]
    fn read_current_generation_returns_missing_when_current_absent() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);

        let generation = read_current_generation(&store).unwrap();

        assert!(matches!(generation, CurrentGeneration::Missing));
    }

    #[test]
    fn read_current_generation_loads_manifest() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let published_path = publish_canonical_index(&index_dir);
        let store = IndexStore::new(&index_dir);

        let generation = read_current_generation(&store).unwrap();

        let CurrentGeneration::Found(generation) = generation else {
            panic!("expected published generation");
        };
        assert_eq!(generation.path, published_path);
        assert_canonical_manifest_targets(&generation.manifest);
    }

    #[test]
    fn read_current_generation_errors_on_empty_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(store.current_file(), "").unwrap();

        let error = read_current_generation(&store).unwrap_err();

        assert!(format!("{error:#}").contains("current index file is empty"));
    }

    #[test]
    fn current_generation_is_due_returns_true_for_stale_generation() {
        let tempdir = tempdir().unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2);
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        publish_canonical_index_with_generated_at(&index_dir, now - TimeDuration::hours(2));
        let store = IndexStore::new(&index_dir);

        let due = current_generation_is_due(&store, Duration::from_secs(60 * 60), now).unwrap();

        assert!(due);
    }

    #[test]
    fn current_generation_is_due_returns_false_for_fresh_generation() {
        let tempdir = tempdir().unwrap();
        let now = time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2);
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        publish_canonical_index_with_generated_at(&index_dir, now);
        let store = IndexStore::new(&index_dir);

        let due = current_generation_is_due(&store, Duration::from_secs(60 * 60), now).unwrap();

        assert!(!due);
    }

    #[test]
    fn current_generation_is_due_returns_true_when_current_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);

        let due = current_generation_is_due(
            &store,
            Duration::from_secs(60 * 60),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert!(due);
    }

    #[test]
    fn current_generation_is_due_returns_true_for_invalid_current() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().to_path_buf());
        let store = IndexStore::new(&index_dir);
        fs::create_dir_all(&index_dir).unwrap();
        let missing = store.generations_dir().join("missing");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let due = current_generation_is_due(
            &store,
            Duration::from_secs(60 * 60),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();

        assert!(due);
    }
}
