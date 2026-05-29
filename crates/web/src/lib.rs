use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::routing::get;
use camino::Utf8PathBuf;
use time::OffsetDateTime;
use tower_http::trace::TraceLayer;

use nixsearch_config::app::AppConfig;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::store::IndexStore;
use nixsearch_ops::generate;
use nixsearch_ops::lock;

mod handlers;
mod maintenance;
mod render_docs;
mod request;
mod scripts;
mod templates;
mod urls;

const DEFAULT_LIMIT: usize = 50;
const RECONCILE_EVENTS_URL: &str = "/-/state/events";
const MORE_RESULTS_URL: &str = "/-/more";

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    index_path: Arc<RwLock<Utf8PathBuf>>,
    generated_at: Arc<RwLock<OffsetDateTime>>,
    manifest: Arc<RwLock<IndexGenerationManifest>>,
}

pub async fn serve(config: AppConfig) -> Result<()> {
    let generation = ensure_current_generation(&config).await?;

    let addr: SocketAddr =
        config.server.listen.parse().with_context(|| {
            format!("failed to parse listen address {:?}", config.server.listen)
        })?;

    log_startup_maintenance_state(&config, &generation);

    let config = Arc::new(config);
    let index_path = Arc::new(RwLock::new(generation.path));
    let generated_at = Arc::new(RwLock::new(generation.manifest.generated_at));
    let manifest = Arc::new(RwLock::new(generation.manifest));

    maintenance::spawn(
        Arc::clone(&config),
        Arc::clone(&index_path),
        Arc::clone(&generated_at),
        Arc::clone(&manifest),
    );

    let state = AppState {
        config,
        index_path,
        generated_at,
        manifest,
    };

    let app = Router::new()
        .route("/-/health", get(handlers::health))
        .route(RECONCILE_EVENTS_URL, get(handlers::state_events))
        .route(MORE_RESULTS_URL, get(handlers::more_results))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/apple-touch-icon.png", get(handlers::apple_touch_icon))
        .route("/", get(handlers::root_page))
        .route("/{source}", get(handlers::source_page))
        .route("/{source}/{*entry}", get(handlers::entry_page))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    tracing::info!("serving nixsearch web UI at http://{addr}");

    axum::serve(listener, app)
        .await
        .context("web server failed")?;

    Ok(())
}

async fn ensure_current_generation(config: &AppConfig) -> Result<maintenance::PublishedGeneration> {
    let index_store = IndexStore::new(&config.data.index_dir);

    match maintenance::read_current_generation(&index_store) {
        Ok(maintenance::CurrentGeneration::Found(generation)) => {
            let missing = maintenance::missing_configured_targets(config, &generation.manifest);

            if missing.is_empty() {
                return Ok(generation);
            }

            if !config.server.bootstrap {
                return Ok(generation);
            }

            let missing = missing
                .iter()
                .map(|target| format!("{}/{}", target.source, target.ref_id))
                .collect::<Vec<_>>()
                .join(", ");

            tracing::info!(
                missing = %missing,
                "current index is missing configured targets; bootstrap enabled, rebuilding index"
            );
        }
        Ok(maintenance::CurrentGeneration::Missing) => {}
        Err(error) => {
            if !config.server.bootstrap {
                return Err(error).context(
                    "failed to read current index generation; run `nixsearch update` first",
                );
            }

            tracing::warn!(
                "failed to read current index generation; bootstrap will rebuild it: {error:#}"
            );
        }
    }

    if !config.server.bootstrap {
        bail!(
            "failed to locate current index in {}; run `nixsearch update` first",
            config.data.index_dir
        );
    }

    if !maintenance::has_configured_targets(config) {
        bail!("cannot bootstrap missing index: no configured refs to index");
    }

    tracing::info!(
        index_dir = %config.data.index_dir,
        "no current index found; bootstrap enabled, building initial index"
    );

    let index_dir = config.data.index_dir.clone();
    let _lock = tokio::task::spawn_blocking(move || lock::acquire_update_lock(&index_dir))
        .await
        .context("failed to join maintenance lock task")??;

    match maintenance::read_current_generation(&index_store) {
        Ok(maintenance::CurrentGeneration::Found(generation)) => {
            if !maintenance::current_generation_missing_configured_targets(config, &generation) {
                tracing::info!(
                    "current index was created by another process while waiting for lock"
                );
                return Ok(generation);
            }
        }
        Ok(maintenance::CurrentGeneration::Missing) => {}
        Err(error) => {
            tracing::warn!(
                "current index generation is still unreadable after acquiring lock; rebuilding it: {error:#}"
            );
        }
    }

    generate::regenerate_all(config)
        .await
        .context("failed to bootstrap missing index")?;

    match maintenance::read_current_generation(&index_store)? {
        maintenance::CurrentGeneration::Found(generation) => Ok(generation),
        maintenance::CurrentGeneration::Missing => {
            bail!("bootstrap completed without publishing a current index")
        }
    }
}

fn log_startup_maintenance_state(
    config: &AppConfig,
    generation: &maintenance::PublishedGeneration,
) {
    tracing::info!("background index reconciliation enabled");
    tracing::info!(
        enabled = config.server.bootstrap,
        "server bootstrap setting"
    );

    if config.server.schedule.enabled {
        if maintenance::has_configured_targets(config) {
            let interval = config
                .server
                .schedule
                .parse_interval()
                .expect("schedule interval already validated");

            if let Some(next_due) =
                maintenance::next_due(generation.manifest.generated_at, interval)
            {
                tracing::info!(
                    interval = %config.server.schedule.interval,
                    generated_at = %generation.manifest.generated_at,
                    next_due = %next_due,
                    "scheduled regeneration enabled"
                );
            } else {
                tracing::error!(
                    interval = %config.server.schedule.interval,
                    generated_at = %generation.manifest.generated_at,
                    "scheduled regeneration enabled but next due time could not be computed"
                );
            }
        } else {
            tracing::warn!(
                "scheduled regeneration enabled but no refs are configured; reconciliation will continue"
            );
        }
    } else {
        tracing::info!("scheduled regeneration disabled");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        assert_canonical_options_manifest_targets, publish_canonical_options_index,
    };
    use nixsearch_test_support::{REF_SMALL, SOURCE_FIXTURES, app_config, utf8_path_buf};
    use tempfile::tempdir;

    use super::ensure_current_generation;

    #[tokio::test]
    async fn ensure_current_generation_returns_existing_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let config = app_config(&index_dir);

        let generation = ensure_current_generation(&config).await.unwrap();

        assert_eq!(generation.path, published_path);
        assert_canonical_options_manifest_targets(&generation.manifest);
    }

    #[tokio::test]
    async fn ensure_current_generation_errors_when_bootstrap_disabled() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.server.bootstrap = false;

        let error = ensure_current_generation(&config).await.unwrap_err();

        assert!(format!("{error:#}").contains("run `nixsearch update` first"));
    }

    #[tokio::test]
    async fn ensure_current_generation_errors_when_no_refs_are_configured() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.sources.clear();

        let error = ensure_current_generation(&config).await.unwrap_err();

        assert!(format!("{error:#}").contains("no configured refs"));
    }

    #[tokio::test]
    async fn ensure_current_generation_bootstraps_missing_index() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());

        let generation = ensure_current_generation(&config).await.unwrap();

        assert!(generation.path.exists());
        assert!(generation.manifest.document_count > 0);
        assert_eq!(generation.manifest.targets.len(), 1);
        let target = &generation.manifest.targets[0];
        assert_eq!(target.source, SOURCE_FIXTURES);
        assert_eq!(target.ref_id, REF_SMALL);

        let store = IndexStore::new(&index_dir);
        assert_eq!(store.current_path().unwrap(), generation.path);
    }

    #[tokio::test]
    async fn ensure_current_generation_bootstraps_missing_current_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());
        let store = IndexStore::new(&index_dir);
        store.create_generation_path().unwrap();
        let missing = store.generations_dir().join("missing-generation");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let generation = ensure_current_generation(&config).await.unwrap();

        assert!(generation.path.exists());
        assert_canonical_options_manifest_targets(&generation.manifest);
        assert_eq!(store.current_path().unwrap(), generation.path);
    }

    #[tokio::test]
    async fn ensure_current_generation_bootstraps_generation_with_missing_manifest() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());
        let store = IndexStore::new(&index_dir);
        let generation_without_manifest = store.create_generation_path().unwrap();
        store.publish(&generation_without_manifest).unwrap();

        let generation = ensure_current_generation(&config).await.unwrap();

        assert!(generation.path.exists());
        assert_ne!(generation.path, generation_without_manifest);
        assert_canonical_options_manifest_targets(&generation.manifest);
        assert_eq!(store.current_path().unwrap(), generation.path);
    }

    #[tokio::test]
    async fn ensure_current_generation_bootstraps_existing_generation_with_missing_target() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());
        let extra_source = config.sources[SOURCE_FIXTURES].clone();
        config.sources.insert("extra".to_owned(), extra_source);

        let generation = ensure_current_generation(&config).await.unwrap();

        assert!(generation.path.exists());
        assert_ne!(generation.path, published_path);
        assert_eq!(generation.manifest.targets.len(), 2);
        assert!(
            generation
                .manifest
                .targets
                .iter()
                .any(|target| target.source == SOURCE_FIXTURES && target.ref_id == REF_SMALL)
        );
        assert!(
            generation
                .manifest
                .targets
                .iter()
                .any(|target| target.source == "extra" && target.ref_id == REF_SMALL)
        );

        let store = IndexStore::new(&index_dir);
        assert_eq!(store.current_path().unwrap(), generation.path);
    }

    #[tokio::test]
    async fn ensure_current_generation_errors_on_invalid_current_when_bootstrap_disabled() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let mut config = app_config(&index_dir);
        config.server.bootstrap = false;
        let store = IndexStore::new(&index_dir);
        store.create_generation_path().unwrap();
        let missing = store.generations_dir().join("missing-generation");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let error = ensure_current_generation(&config).await.unwrap_err();

        assert!(format!("{error:#}").contains("run `nixsearch update` first"));
    }
}
