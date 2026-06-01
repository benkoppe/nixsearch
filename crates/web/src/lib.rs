use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::IndexStore;
use nixsearch_ops::generate;
use nixsearch_ops::lock;
use nixsearch_service::SearchService;

mod handlers;
mod maintenance;
mod render_docs;
mod request;
mod scripts;
mod templates;
mod urls;

const DEFAULT_LIMIT: usize = 50;
const DATASTAR_JS_URL: &str = "/-/assets/datastar.js";
const RECONCILE_EVENTS_URL: &str = "/-/state/events";
const RESULTS_SLICE_URL: &str = "/-/results/slice";

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    search: SearchService,
}

pub async fn serve(config: AppConfig) -> Result<()> {
    let generation = ensure_current_generation(&config).await?;

    let addr: SocketAddr =
        config.server.listen.parse().with_context(|| {
            format!("failed to parse listen address {:?}", config.server.listen)
        })?;

    log_startup_maintenance_state(&config, &generation);

    let config = Arc::new(config);
    let search =
        SearchService::from_generation(Arc::clone(&config), generation.path, generation.manifest)?;

    maintenance::spawn(Arc::clone(&config), search.clone());

    let state = AppState { config, search };

    let app = Router::new()
        .route("/-/health", get(handlers::health))
        .route(RECONCILE_EVENTS_URL, get(handlers::state_events))
        .route(RESULTS_SLICE_URL, get(handlers::results_slice))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/apple-touch-icon.png", get(handlers::apple_touch_icon))
        .route(DATASTAR_JS_URL, get(handlers::datastar_js))
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
            if let Err(error) = SearchService::validate_generation(&generation.path) {
                if !config.server.bootstrap {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to open current index generation {}; run `nixsearch update` first",
                            generation.path
                        )
                    });
                }

                tracing::warn!(
                    generation = %generation.path,
                    "current index generation cannot be opened; bootstrap will rebuild it: {error:#}"
                );
            } else {
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
        "current index requires bootstrap; building index generation"
    );

    let index_dir = config.data.index_dir.clone();
    let _lock = tokio::task::spawn_blocking(move || lock::acquire_update_lock(&index_dir))
        .await
        .context("failed to join maintenance lock task")??;

    match maintenance::read_current_generation(&index_store) {
        Ok(maintenance::CurrentGeneration::Found(generation)) => {
            if !maintenance::current_generation_missing_configured_targets(config, &generation) {
                match SearchService::validate_generation(&generation.path) {
                    Ok(()) => {
                        tracing::info!(
                            "current index was created by another process while waiting for lock"
                        );
                        return Ok(generation);
                    }
                    Err(error) => {
                        tracing::warn!(
                            generation = %generation.path,
                            "current index generation is still unopenable after acquiring lock; rebuilding it: {error:#}"
                        );
                    }
                }
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
        .context("failed to bootstrap current index")?;

    match maintenance::read_current_generation(&index_store)? {
        maintenance::CurrentGeneration::Found(generation) => {
            SearchService::validate_generation(&generation.path).with_context(|| {
                format!(
                    "bootstrap published index generation {} but it cannot be opened",
                    generation.path
                )
            })?;

            Ok(generation)
        }
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
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use nixsearch_config::app::AppConfig;
    use nixsearch_index::search::SearchIndex;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        assert_canonical_options_manifest_targets, publish_canonical_options_index,
        publish_fixture_options_index_for_refs,
    };
    use nixsearch_service::SearchService;
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, app_config, app_config_with_extra_fixture_source,
        multi_ref_app_config, utf8_path_buf,
    };
    use tempfile::tempdir;
    use tower::ServiceExt;

    use super::{
        AppState, RECONCILE_EVENTS_URL, RESULTS_SLICE_URL, ensure_current_generation, handlers,
    };

    fn test_app(config: AppConfig) -> Router {
        let config = Arc::new(config);
        let search = SearchService::open_current(Arc::clone(&config)).unwrap();

        Router::new()
            .route(RECONCILE_EVENTS_URL, get(handlers::state_events))
            .route(RESULTS_SLICE_URL, get(handlers::results_slice))
            .route("/", get(handlers::root_page))
            .route("/{source}", get(handlers::source_page))
            .route("/{source}/{*entry}", get(handlers::entry_page))
            .with_state(AppState { config, search })
    }

    async fn request_status(app: Router, uri: &str) -> StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn full_page_unknown_source_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(request_status(app, "/missing").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn full_page_unknown_ref_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref=missing").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn full_page_unknown_ref_set_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref_set=missing").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn full_page_configured_but_unserved_ref_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref=stable").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn full_page_default_served_ref_returns_200() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(request_status(app, "/fixtures").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn full_page_non_default_served_ref_returns_200() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref=stable").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn full_page_multi_ref_ref_set_without_explicit_ref_returns_400() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref_set=multi").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn full_page_multi_ref_ref_set_with_explicit_valid_ref_returns_200() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/fixtures?ref_set=multi&ref=stable").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn all_source_search_works_when_some_configured_refs_are_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_extra_fixture_source(&index_dir, "extra"));

        assert_eq!(request_status(app, "/?q=git").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn state_events_unknown_ref_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(
            request_status(app, "/-/state/events?url=%2Ffixtures%3Fref%3Dmissing").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn state_events_multi_ref_ref_set_without_explicit_ref_returns_400() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app, "/-/state/events?url=%2Ffixtures%3Fref_set%3Dmulti").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn results_slice_unknown_ref_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(
            request_status(
                app,
                "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref%3Dmissing&offset=0",
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn results_slice_multi_ref_ref_set_without_explicit_ref_returns_400() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(
                app,
                "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref_set%3Dmulti&offset=0",
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn full_page_state_events_and_results_slice_accept_valid_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config(&index_dir));

        assert_eq!(
            request_status(app.clone(), "/fixtures?ref=stable").await,
            StatusCode::OK
        );
        assert_eq!(
            request_status(
                app.clone(),
                "/-/state/events?url=%2Ffixtures%3Fref%3Dstable",
            )
            .await,
            StatusCode::OK
        );
        assert_eq!(
            request_status(
                app,
                "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref%3Dstable&offset=0",
            )
            .await,
            StatusCode::OK
        );
    }

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
    async fn ensure_current_generation_bootstraps_unopenable_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.current_manifest().unwrap();
        let broken = store.create_generation_path().unwrap();
        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();

        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());

        let generation = ensure_current_generation(&config).await.unwrap();

        assert_ne!(generation.path, broken);
        assert_canonical_options_manifest_targets(&generation.manifest);
        assert_eq!(store.current_path().unwrap(), generation.path);
        SearchIndex::open(&generation.path).unwrap();
    }

    #[tokio::test]
    async fn ensure_current_generation_errors_on_unopenable_generation_when_bootstrap_disabled() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.current_manifest().unwrap();
        let broken = store.create_generation_path().unwrap();
        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();
        let mut config = app_config(&index_dir);
        config.server.bootstrap = false;

        let error = ensure_current_generation(&config).await.unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("failed to open current index generation"));
        assert!(error.contains("run `nixsearch update` first"));
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
