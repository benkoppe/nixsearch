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
use nixsearch_ops::targets::{TargetKey, default_search_target_keys};
use nixsearch_service::SearchService;

mod handlers;
mod maintenance;
mod origin;
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

                if generation_serves_default_scope(config, &generation)? {
                    tracing::warn!(
                        missing = %format_target_keys(&missing),
                        "current index is missing configured targets but still serves a default search scope; startup will continue"
                    );

                    return Ok(generation);
                }

                if !config.server.bootstrap {
                    return Ok(generation);
                }

                tracing::info!(
                    missing = %format_target_keys(&missing),
                    "current index is missing configured targets needed for default search; bootstrap enabled, rebuilding index"
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
            match SearchService::validate_generation(&generation.path) {
                Ok(()) => {
                    let missing =
                        maintenance::missing_configured_targets(config, &generation.manifest);

                    if missing.is_empty() || generation_serves_default_scope(config, &generation)? {
                        tracing::info!(
                            "current index was created by another process while waiting for lock"
                        );
                        return Ok(generation);
                    }

                    tracing::warn!(
                        generation = %generation.path,
                        missing = %format_target_keys(&missing),
                        "current index still does not serve a default search scope after acquiring lock; rebuilding"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        generation = %generation.path,
                        "current index generation is still unopenable after acquiring lock; rebuilding it: {error:#}"
                    );
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

    let bootstrap = generate::bootstrap_all_tolerant(config)
        .await
        .context("failed to bootstrap current index")?;

    if bootstrap.is_degraded() {
        tracing::warn!(
            failed = %format_target_keys(&bootstrap.failed_refresh_targets),
            skipped = %format_target_keys(&bootstrap.skipped_targets),
            "bootstrap published a degraded index generation"
        );
    }

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

fn generation_serves_default_scope(
    config: &AppConfig,
    generation: &maintenance::PublishedGeneration,
) -> Result<bool> {
    let default_targets = default_search_target_keys(config)?;

    if default_targets.is_empty() {
        return Ok(false);
    }

    Ok(generation
        .manifest
        .targets
        .iter()
        .map(TargetKey::from)
        .any(|target| default_targets.contains(&target)))
}

fn format_target_keys<'a>(targets: impl IntoIterator<Item = &'a TargetKey>) -> String {
    targets
        .into_iter()
        .map(|target| format!("{}/{}", target.source, target.ref_id))
        .collect::<Vec<_>>()
        .join(", ")
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
    use axum::body::{Body, to_bytes};
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

    async fn request_body(app: Router, uri: &str) -> (StatusCode, String) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn app_config_with_public_url(index_dir: impl AsRef<camino::Utf8Path>) -> AppConfig {
        let mut config = app_config(index_dir);
        config.server.public_url = Some("https://search.example.com/".to_owned());
        config
    }

    fn multi_ref_app_config_with_public_url(index_dir: impl AsRef<camino::Utf8Path>) -> AppConfig {
        let mut config = multi_ref_app_config(index_dir);
        config.server.public_url = Some("https://search.example.com/".to_owned());
        config
    }

    fn assert_has_canonical(body: &str, expected: &str) {
        let tag = format!(r#"<link rel="canonical" href="{expected}">"#);
        assert!(body.contains(&tag), "missing canonical tag {tag:?}");
    }

    fn assert_no_canonical(body: &str) {
        assert!(
            !body.contains(r#"rel="canonical""#),
            "unexpected canonical tag in body"
        );
    }

    fn assert_has_robots(body: &str) {
        assert!(
            body.contains(r#"<meta name="robots" content="noindex,follow">"#),
            "missing noindex robots tag"
        );
    }

    fn assert_no_robots(body: &str) {
        assert!(
            !body.contains(r#"name="robots""#),
            "unexpected robots tag in body"
        );
    }

    fn assert_og_url(body: &str, expected: &str) {
        let tag = format!(r#"<meta property="og:url" content="{expected}">"#);
        assert!(body.contains(&tag), "missing og:url tag {tag:?}");
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
    async fn state_events_unknown_ref_patches_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) =
            request_body(app, "/-/state/events?url=%2Ffixtures%3Fref%3Dmissing").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Request failed"));
        assert!(body.contains("unknown ref"));
    }

    #[tokio::test]
    async fn state_events_multi_ref_ref_set_without_explicit_ref_patches_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));
        let (status, body) =
            request_body(app, "/-/state/events?url=%2Ffixtures%3Fref_set%3Dmulti").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Request failed"));
        assert!(body.contains("explicit ref is required"));
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
    async fn unknown_source_error_page_keeps_recovery_controls() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_extra_fixture_source(&index_dir, "extra"));
        let (status, body) = request_body(app, "/missing?q=git").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/missing\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("Page unavailable"));
        assert!(body.contains("unknown source"));
        assert!(!body.contains("data-nixsearch-source=\"\" data-active"));
        assert!(!body.contains("data-nixsearch-source=\"fixtures\" data-active"));
        assert!(!body.contains("data-nixsearch-source=\"extra\" data-active"));
        assert!(!body.contains("style=\"--logo-accent:"));
        assert!(!body.contains("style=\"--search-focus-color:"));
        assert!(!body.contains("style=\"--source-color:"));
    }

    #[tokio::test]
    async fn unknown_ref_error_page_keeps_known_source_controls() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git&ref=missing").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/fixtures\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("Page unavailable"));
        assert!(body.contains("unknown ref"));
        assert!(!body.contains("value=\"missing\""));
        assert!(!body.contains("checked data-nixsearch-input=\"ref\""));
    }

    #[tokio::test]
    async fn unknown_ref_set_error_page_keeps_ref_set_unselected() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));
        let (status, body) = request_body(app, "/?q=git&ref_set=missing").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("Page unavailable"));
        assert!(body.contains("unknown ref set"));
        assert!(!body.contains("checked data-nixsearch-input=\"ref\""));
    }

    #[tokio::test]
    async fn ambiguous_ref_set_error_page_keeps_recovery_controls() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git&ref_set=multi").await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/fixtures\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("Page unavailable"));
        assert!(body.contains("explicit ref is required"));
        assert!(!body.contains("checked data-nixsearch-input=\"ref\""));
    }

    #[tokio::test]
    async fn missing_entry_page_returns_404_with_modal_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.missing.enable?q=git&ref=small").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/fixtures\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("entry-modal"));
        assert!(body.contains("Entry not found"));
        assert!(body.contains("programs.missing.enable"));
        assert!(body.contains("Close"));
    }

    #[tokio::test]
    async fn missing_entry_page_preserves_all_source_modal_recovery_context() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.missing.enable?q=git&source=all").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("search-form"));
        assert!(body.contains("action=\"/\""));
        assert!(body.contains("value=\"git\""));
        assert!(body.contains("entry-modal"));
        assert!(body.contains("Entry not found"));
        assert!(body.contains("programs.missing.enable"));
        assert!(body.contains("Close"));
    }

    #[tokio::test]
    async fn state_events_missing_entry_returns_404_with_modal_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=%2Ffixtures%2Fprograms.missing.enable",
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Entry not found"));
        assert!(body.contains("programs.missing.enable"));
    }

    #[tokio::test]
    async fn home_emits_self_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/");
        assert_no_robots(&body);
        assert_og_url(&body, "https://search.example.com/");
    }

    #[tokio::test]
    async fn source_default_ref_emits_clean_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn source_explicit_default_ref_canonicalizes_cleanly() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?ref=small").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn entry_default_ref_emits_clean_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.git.enable").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn entry_explicit_default_ref_canonicalizes_cleanly() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.git.enable?ref=small").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn search_pages_emit_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/?q=git").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
        assert_og_url(&body, "https://search.example.com/?q=git");
    }

    #[tokio::test]
    async fn paginated_search_pages_emit_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/?q=git&page=2").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn contextual_entry_url_canonicalizes_to_clean_entry_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.git.enable?q=git&page=2&source=all").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn non_indexed_ref_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?ref=stable").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn error_pages_omit_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/missing").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn missing_entry_pages_omit_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.missing.enable").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_no_canonical(&body);
        assert_has_robots(&body);
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

    #[tokio::test]
    async fn source_kind_query_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?kind=option").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn all_ref_set_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/?ref_set=single").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn source_ref_set_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?ref_set=single").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn entry_ref_set_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.small.git.enable?ref_set=single").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn state_events_emits_canonical_head_metadata_for_source_page() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/-/state/events?url=%2Ffixtures").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":"https://search.example.com/fixtures""#));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_accepts_absolute_public_url_for_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=https%3A%2F%2Fsearch.example.com%2Ffixtures",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":"https://search.example.com/fixtures""#));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_modal_navigation_updates_entry_head_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=%2Ffixtures%2Fprograms.git.enable%3Fq%3Dgit%26source%3Dall&previous_url=%2F%3Fq%3Dgit",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-modal"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(
            r#""canonicalUrl":"https://search.example.com/fixtures/programs.git.enable""#
        ));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_modal_close_skips_search_head_description() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=%2F%3Fq%3Dgit&previous_url=%2Ffixtures%2Fprograms.git.enable%3Fq%3Dgit%26source%3Dall",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-modal-container"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
        assert!(!body.contains(" results for git"));
    }

    #[tokio::test]
    async fn state_events_emits_noindex_head_metadata_for_search_page() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/-/state/events?url=%2F%3Fq%3Dgit").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
    }

    #[tokio::test]
    async fn state_events_page_only_navigation_keeps_search_head_description() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=%2F%3Fq%3Dgit%26page%3D2&previous_url=%2F%3Fq%3Dgit",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(" results for git"));
        assert!(!body.contains(r#""description":"Search the Nix ecosystem""#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
    }

    #[tokio::test]
    async fn state_events_emits_noindex_head_metadata_for_ref_set_page() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/-/state/events?url=%2Ffixtures%3Fref_set%3Dsingle").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
    }
}
