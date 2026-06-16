use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::{IndexStore, PublishedGeneration};
use nixsearch_ops::targets::{TargetKey, default_search_target_keys};
use nixsearch_ops::{cleanup, generate, lock};
use nixsearch_service::SearchService;

mod entry;
mod handlers;
mod maintenance;
mod origin;
mod render_docs;
mod request;
mod scripts;
mod templates;
mod urls;

const DEFAULT_LIMIT: usize = 50;
const MAX_PAGE: usize = 1000;
const MAX_OFFSET: usize = (MAX_PAGE - 1) * DEFAULT_LIMIT;

const DATASTAR_JS_URL: &str = "/-/assets/datastar.js";
const RECONCILE_EVENTS_URL: &str = "/-/state/events";
const RESULTS_SLICE_URL: &str = "/-/results/slice";

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    search: SearchService,
}

pub async fn serve(config: AppConfig) -> Result<()> {
    ensure_current_generation(&config).await?;

    let addr: SocketAddr =
        config.server.listen.parse().with_context(|| {
            format!("failed to parse listen address {:?}", config.server.listen)
        })?;

    let config = Arc::new(config);
    let search = SearchService::open_current(Arc::clone(&config))?;
    let generation = search.snapshot().to_published_generation();

    log_startup_maintenance_state(&config, &generation);

    maintenance::spawn(Arc::clone(&config), search.clone());

    let state = AppState { config, search };

    let app = app_router(state).layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    tracing::info!("serving nixsearch web UI at http://{addr}");

    axum::serve(listener, app)
        .await
        .context("web server failed")?;

    Ok(())
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/-/health", get(handlers::health))
        .route(RECONCILE_EVENTS_URL, get(handlers::state_events))
        .route(RESULTS_SLICE_URL, get(handlers::results_slice))
        .route("/robots.txt", get(handlers::robots_txt))
        .route("/sitemap.xml", get(handlers::sitemap_xml))
        .route("/sitemaps", get(handlers::sitemaps_not_found))
        .route("/sitemaps/{*path}", get(handlers::sitemaps_not_found))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/apple-touch-icon.png", get(handlers::apple_touch_icon))
        .route(DATASTAR_JS_URL, get(handlers::datastar_js))
        .route("/", get(handlers::public_page))
        .route("/{*path}", get(handlers::public_page))
        .with_state(state)
}

async fn ensure_current_generation(config: &AppConfig) -> Result<PublishedGeneration> {
    let index_store = IndexStore::new(&config.data.index_dir);

    match index_store.try_current_generation_metadata() {
        Ok(Some(generation)) => {
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
        Ok(None) => {}
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
    let update_lock = tokio::task::spawn_blocking(move || lock::acquire_update_lock(&index_dir))
        .await
        .context("failed to join maintenance lock task")??;

    match index_store.try_current_generation_metadata() {
        Ok(Some(generation)) => match SearchService::validate_generation(&generation.path) {
            Ok(()) => {
                let missing = maintenance::missing_configured_targets(config, &generation.manifest);

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
        },
        Ok(None) => {}
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

    match index_store.try_current_generation_metadata()? {
        Some(generation) => {
            SearchService::validate_generation(&generation.path).with_context(|| {
                format!(
                    "bootstrap published index generation {} but it cannot be opened",
                    generation.path
                )
            })?;

            let report = cleanup::cleanup_under_lock(config, &update_lock).await?;
            cleanup::log_report(&report);

            Ok(generation)
        }
        None => {
            bail!("bootstrap completed without publishing a current index")
        }
    }
}

fn generation_serves_default_scope(
    config: &AppConfig,
    generation: &PublishedGeneration,
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

fn log_startup_maintenance_state(config: &AppConfig, generation: &PublishedGeneration) {
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
    use nixsearch_config::app::AppConfig;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::SearchDocument;
    use nixsearch_index::search::SearchIndex;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        assert_canonical_options_manifest_targets, index_target, options_target,
        publish_canonical_options_index, publish_documents_with_manifest_targets,
        publish_fixture_options_index_for_refs,
    };
    use nixsearch_service::SearchService;
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, app_config, app_config_with_extra_fixture_source,
        ingest_context_for, multi_ref_app_config, option_doc_for, package_doc_for, utf8_path_buf,
    };
    use tempfile::{TempDir, tempdir};
    use tower::ServiceExt;

    use crate::app_router;

    use super::{AppState, ensure_current_generation};

    fn test_app(config: AppConfig) -> Router {
        let config = Arc::new(config);
        let search = SearchService::open_current(Arc::clone(&config)).unwrap();

        app_router(AppState { config, search })
    }

    struct TestResponse {
        status: StatusCode,
        content_type: String,
        body: String,
    }

    async fn request_status(app: Router, uri: &str) -> StatusCode {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    async fn request_body(app: Router, uri: &str) -> (StatusCode, String) {
        let response = request_test_response(app, uri).await;

        (response.status, response.body)
    }

    async fn request_content_type_and_body(app: Router, uri: &str) -> (StatusCode, String, String) {
        let response = request_test_response(app, uri).await;

        (response.status, response.content_type, response.body)
    }

    async fn request_test_response(app: Router, uri: &str) -> TestResponse {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        TestResponse {
            status,
            content_type,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    fn current_generation_id(index_dir: impl AsRef<camino::Utf8Path>) -> String {
        let store = IndexStore::new(index_dir.as_ref());
        let path = store.current_path().unwrap();
        store.read_manifest(&path).unwrap().generation_id
    }

    struct ReconciledGenerationFixture {
        _tempdir: TempDir,
        app: Router,
        old_generation_id: String,
        new_generation_id: String,
    }

    fn reconciled_generation_fixture() -> ReconciledGenerationFixture {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let old_generation_id = current_generation_id(&index_dir);
        let app = test_app(app_config(&index_dir));

        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            vec![option_doc_for(
                &context,
                "programs.ripgrep.enable",
                "Ripgrep option.",
            )],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
        );
        let new_generation_id = current_generation_id(&index_dir);

        ReconciledGenerationFixture {
            _tempdir: tempdir,
            app,
            old_generation_id,
            new_generation_id,
        }
    }

    fn with_generation(uri: &str, generation_id: &str) -> String {
        let separator = if uri.contains('?') { '&' } else { '?' };
        format!(
            "{uri}{separator}generation_id={}",
            urlencoding::encode(generation_id)
        )
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

    fn publish_ambiguous_package_option_index(index_dir: &camino::Utf8Path) {
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                option_doc_for(&context, "git", "Git option."),
                package_doc_for(&context, "git", "Git package."),
            ],
            vec![
                options_target(SOURCE_FIXTURES, REF_SMALL, 1),
                index_target(SOURCE_FIXTURES, REF_SMALL, ArtifactKind::PackagesJson, 1),
            ],
        );
    }

    fn publish_ambiguous_package_option_search_index(index_dir: &camino::Utf8Path) {
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                option_doc_for(&context, "git", "Git option."),
                package_doc_for(&context, "git", "Git package."),
                package_doc_for(&context, "ripgrep", "Ripgrep package."),
            ],
            vec![
                options_target(SOURCE_FIXTURES, REF_SMALL, 1),
                index_target(SOURCE_FIXTURES, REF_SMALL, ArtifactKind::PackagesJson, 2),
            ],
        );
    }

    fn publish_duplicate_option_index(index_dir: &camino::Utf8Path) {
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                option_doc_for(&context, "duplicate.entry", "First duplicate option."),
                option_doc_for(&context, "duplicate.entry", "Second duplicate option."),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 2)],
        );
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

    fn assert_h1_count(body: &str, expected: usize) {
        let count = body.matches("<h1").count();
        assert_eq!(count, expected, "unexpected h1 count in body");
    }

    fn assert_empty_modal_container(body: &str) {
        let marker = r#"<div id="entry-modal-container">"#;
        let start = body.find(marker).expect("missing modal container");
        let after = &body[start + marker.len()..];
        let end = after.find("</div>").expect("missing modal container close");

        assert!(
            after[..end].trim().is_empty(),
            "expected empty modal container"
        );
        assert!(
            !body.contains(r#"<dialog id="entry-modal""#),
            "expected no populated entry modal"
        );
    }

    fn assert_populated_modal(body: &str) {
        assert!(
            body.contains(r#"<div id="entry-modal-container">"#),
            "expected modal container"
        );
        assert!(
            body.contains(r#"<dialog id="entry-modal""#),
            "expected populated entry modal"
        );
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
    async fn full_page_exposes_generation_state() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"id="generation-state""#));
        assert!(body.contains(&format!(r#""generationId":"{generation_id}""#)));
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
    async fn reserved_routes_take_precedence_over_public_pages() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/robots.txt").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("User-agent: *"));
        assert!(body.contains("Sitemap: http://localhost/sitemap.xml"));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/sitemap.xml").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/xml"));
        assert!(body.contains(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#));
        assert!(body.contains("<loc>http://localhost/</loc>"));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/sitemaps").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(content_type.starts_with("text/plain"));
        assert_eq!(body, "not found");

        let (status, _, body) =
            request_content_type_and_body(app.clone(), "/sitemaps/shard.xml").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "not found");

        assert_eq!(
            request_status(app.clone(), "/favicon.ico").await,
            StatusCode::OK
        );
        assert_eq!(
            request_status(app, "/apple-touch-icon.png").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn sitemap_escapes_request_derived_origin() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/sitemap.xml")
                    .header("x-forwarded-host", "example.com&x=<tag>")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(body.contains("http://example.com&amp;x=&lt;tag&gt;/"));
        assert!(!body.contains("http://example.com&x=<tag>/"));
    }

    #[tokio::test]
    async fn full_page_invalid_public_query_returns_400() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(
            request_status(app.clone(), "/?q=git&q=git").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request_status(app.clone(), "/?kind=app").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request_status(app.clone(), "/fixtures/").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request_status(app, "/?q=git&page=1001").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn endpoint_outer_param_guards_return_expected_errors() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/-/state/events?url=%2F&url=%2Ffixtures")
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("duplicate url"));

        let (status, content_type, body) = request_content_type_and_body(
            app.clone(),
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=-1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(content_type.starts_with("application/json"));
        assert!(body.contains("offset"));
    }

    #[tokio::test]
    async fn state_events_accepts_datastar_transport_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=%2F%3Fq%3Dh&previous_url=%2F&datastar=%7B%7D",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("unknown query parameter"));
    }

    #[tokio::test]
    async fn endpoint_inner_public_request_guards_are_endpoint_specific() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));

        let (status, body) =
            request_body(app.clone(), "/-/state/events?url=%2F%3Fkind%3Dapp").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Request failed"));
        assert!(body.contains("kind app"));

        let uri = with_generation("/-/results/slice?url=%2Ffixtures&offset=0", &generation_id);
        let (status, body) = request_body(app.clone(), &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("requires q"));

        let uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit%26kind%3Dservice&offset=0",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("kind app and service"));
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
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let uri = with_generation(
            "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref%3Dmissing&offset=0",
            &generation_id,
        );

        assert_eq!(request_status(app, &uri).await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn results_slice_multi_ref_ref_set_without_explicit_ref_returns_400() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(multi_ref_app_config(&index_dir));
        let uri = with_generation(
            "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref_set%3Dmulti&offset=0",
            &generation_id,
        );

        assert_eq!(request_status(app, &uri).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn results_slice_missing_generation_returns_stale_generation_409() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0").await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains(r#""error":"stale_generation""#));
        assert!(body.contains(r#""reload":true"#));
        assert!(body.contains(&format!(r#""generationId":"{generation_id}""#)));
    }

    #[tokio::test]
    async fn results_slice_mismatched_generation_returns_stale_generation_409() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0&generation_id=sha256%3Astale",
        )
        .await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains(r#""error":"stale_generation""#));
    }

    #[tokio::test]
    async fn results_slice_matching_generation_returns_rows() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""rows":"#));
        assert!(!body.contains("stale_generation"));
    }

    #[tokio::test]
    async fn results_slice_reconciles_new_published_generation_before_generation_check() {
        let fixture = reconciled_generation_fixture();

        let uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0",
            &fixture.old_generation_id,
        );
        let (status, body) = request_body(fixture.app, &uri).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains(r#""error":"stale_generation""#));
        assert!(body.contains(&format!(
            r#""generationId":"{}""#,
            fixture.new_generation_id
        )));
    }

    #[tokio::test]
    async fn state_events_reconciles_new_published_generation_before_generation_check() {
        let fixture = reconciled_generation_fixture();

        let uri = with_generation(
            "/-/state/events?url=%2F%3Fq%3Dgit",
            &fixture.old_generation_id,
        );
        let (status, body) = request_body(fixture.app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&fixture.new_generation_id));
    }

    #[tokio::test]
    async fn full_page_state_events_and_results_slice_accept_valid_ref() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);
        let generation_id = current_generation_id(&index_dir);

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
        let uri = with_generation(
            "/-/results/slice?url=%2Ffixtures%3Fq%3Dgit%26ref%3Dstable&offset=0",
            &generation_id,
        );
        assert_eq!(request_status(app, &uri).await, StatusCode::OK);
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
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2Ffixtures%2Fprograms.missing.enable",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

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
        assert!(!body.contains(r#"<script id="initial-history-metadata""#));
    }

    #[tokio::test]
    async fn contextual_entry_page_seeds_return_head_metadata_for_modal_close() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.git.enable?q=git&source=all").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_og_url(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert!(body.contains(r#"<script id="initial-history-metadata" type="application/json">"#));
        assert!(body.contains(r#""returnHeadMetadata":{"#));
        assert!(body.contains(r#""returnHeadMetadataUrl":"/?q=git""#));
        assert!(body.contains(r#""url":"https://search.example.com/?q=git""#));
        assert!(body.contains(" results for git"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
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
    async fn direct_entry_page_renders_entry_in_results_with_empty_modal() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.git.enable").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert!(body.contains("programs.git.enable"));
        assert!(body.contains("Description"));
        assert_h1_count(&body, 1);
        assert_empty_modal_container(&body);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn direct_entry_identifying_params_still_render_in_results() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for url in [
            "/fixtures/programs.git.enable?ref=small",
            "/fixtures/programs.git.enable?kind=option",
        ] {
            let (status, body) = request_body(app.clone(), url).await;

            assert_eq!(status, StatusCode::OK, "{url}");
            assert!(body.contains("entry-page"), "{url}");
            assert!(body.contains("programs.git.enable"), "{url}");
            assert_h1_count(&body, 1);
            assert_empty_modal_container(&body);
        }
    }

    #[tokio::test]
    async fn direct_ambiguous_entry_page_renders_in_results_with_empty_modal() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/git").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert!(body.contains("Multiple entries found"));
        assert_h1_count(&body, 1);
        assert_empty_modal_container(&body);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn direct_missing_entry_page_renders_in_results_with_empty_modal() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.missing.enable").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("entry-page"));
        assert!(body.contains("Entry not found"));
        assert!(body.contains("programs.missing.enable"));
        assert_h1_count(&body, 1);
        assert_empty_modal_container(&body);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn contextual_entry_page_keeps_results_context_and_populated_modal() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.git.enable?q=git&source=all").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("result"));
        assert!(body.contains("for "));
        assert_populated_modal(&body);
        assert_h1_count(&body, 1);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn contextual_missing_entry_page_keeps_modal_error() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.missing.enable?q=git&source=all").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("result"));
        assert_populated_modal(&body);
        assert!(body.contains("Entry not found"));
        assert_h1_count(&body, 1);
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
    async fn redundant_kind_on_unambiguous_entry_canonicalizes_cleanly() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.git.enable?kind=option").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn ambiguous_package_option_entries_canonicalize_with_kind() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        let (status, body) = request_body(app.clone(), "/fixtures/git?kind=package").await;
        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(
            &body,
            "https://search.example.com/fixtures/git?kind=package",
        );
        assert_no_robots(&body);

        let (status, body) = request_body(app.clone(), "/fixtures/git?kind=option").await;
        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures/git?kind=option");
        assert_no_robots(&body);

        let (status, body) = request_body(app, "/fixtures/git").await;
        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn same_kind_duplicate_entry_emits_noindex() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_duplicate_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/duplicate.entry?kind=option").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn result_links_use_kind_only_for_ambiguous_entry_urls() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_search_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app.clone(), "/?q=git&kind=option").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"href="/fixtures/git?q=git&amp;kind=package&amp;source=all""#));
        assert!(body.contains(r#"href="/fixtures/git?q=git&amp;kind=option&amp;source=all""#));
        assert!(!body.contains(r#"/fixtures/ripgrep?q=git&amp;kind=option"#));

        let uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit%26kind%3Doption&offset=0",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/fixtures/git?q=git&amp;kind=package&amp;source=all"));
        assert!(body.contains("/fixtures/git?q=git&amp;kind=option&amp;source=all"));
        assert!(!body.contains("/fixtures/ripgrep?q=git&amp;kind=option"));
    }

    #[tokio::test]
    async fn internal_and_hidden_entry_pages_render_but_emit_noindex() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        let mut internal = match option_doc_for(&context, "internal.entry", "Internal option.") {
            SearchDocument::Option(option) => option,
            SearchDocument::Package(_) => unreachable!(),
        };
        internal.internal = Some(true);

        let mut hidden = match option_doc_for(&context, "hidden.entry", "Hidden option.") {
            SearchDocument::Option(option) => option,
            SearchDocument::Package(_) => unreachable!(),
        };
        hidden.visible = Some(false);

        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                SearchDocument::Option(internal),
                SearchDocument::Option(hidden),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 2)],
        );

        let app = test_app(app_config_with_public_url(&index_dir));

        for entry in ["internal.entry", "hidden.entry"] {
            let (status, body) = request_body(app.clone(), &format!("/fixtures/{entry}")).await;

            assert_eq!(status, StatusCode::OK);
            assert!(body.contains("entry-page"));
            assert_empty_modal_container(&body);
            assert_h1_count(&body, 1);
            assert!(body.contains(entry));
            assert_no_canonical(&body);
            assert_has_robots(&body);
        }
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
    async fn ensure_current_generation_keeps_existing_generation_when_default_scope_is_served() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let mut config = app_config(&index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());
        let extra_source = config.sources[SOURCE_FIXTURES].clone();
        config.sources.insert("extra".to_owned(), extra_source);

        let generation = ensure_current_generation(&config).await.unwrap();

        assert!(generation.path.exists());
        assert_eq!(generation.path, published_path);
        assert_eq!(generation.manifest.targets.len(), 1);
        assert!(
            generation
                .manifest
                .targets
                .iter()
                .any(|target| target.source == SOURCE_FIXTURES && target.ref_id == REF_SMALL)
        );
        assert!(
            !generation
                .manifest
                .targets
                .iter()
                .any(|target| target.source == "extra" && target.ref_id == REF_SMALL)
        );

        let store = IndexStore::new(&index_dir);
        assert_eq!(store.current_path().unwrap(), published_path);
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
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation("/-/state/events?url=%2Ffixtures", &generation_id);
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":"https://search.example.com/fixtures""#));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_missing_generation_returns_ordered_generation_change_sse() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/-/state/events?url=%2F%3Fq%3Dgit").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyGenerationChange"));
        assert!(body.contains(&generation_id));
        assert!(body.contains(r#""targetUrl":"/?q=git""#));
        assert!(body.contains(r#""generationId":"#));
        assert!(body.contains(r#""generationStateHtml":"#));
        assert!(body.contains(r#""resultsHtml":"#));
        assert!(body.contains(r#""modalHtml":"#));
        assert!(body.contains(r#""metadata":"#));
        assert!(!body.contains("window.nixsearchBeginGenerationChange"));
        assert!(!body.contains("window.nixsearchFinishGenerationChange"));
    }

    #[tokio::test]
    async fn state_events_generation_change_does_not_emit_unguarded_result_patch() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/-/state/events?url=%2F%3Fq%3Dgit").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyGenerationChange"));
        assert!(!body.contains("nixsearchApplyResultsPatch"));
    }

    #[tokio::test]
    async fn state_events_matching_generation_uses_target_guarded_results_patch() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2F%3Fq%3Dgit&previous_url=%2F",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyResultsPatch"));
        assert!(body.contains(r#""/?q=git""#));
        assert!(!body.contains("nixsearchApplyGenerationChange"));
    }

    #[tokio::test]
    async fn state_events_matching_generation_uses_normal_protocol() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation("/-/state/events?url=%2F%3Fq%3Dgit", &generation_id);
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("window.nixsearchBeginGenerationChange"));
        assert!(!body.contains("window.nixsearchFinishGenerationChange"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
    }

    #[tokio::test]
    async fn state_events_accepts_absolute_public_url_for_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=https%3A%2F%2Fsearch.example.com%2Ffixtures",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":"https://search.example.com/fixtures""#));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_rejects_foreign_absolute_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(
            app,
            "/-/state/events?url=https%3A%2F%2Fevil.example%2Ffixtures",
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("does not match expected origin"));
        assert!(!body.contains("nixsearchApplyHeadMetadata"));
    }

    #[tokio::test]
    async fn results_slice_rejects_foreign_absolute_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/results/slice?url=https%3A%2F%2Fevil.example%2F%3Fq%3Dgit&offset=0",
            &generation_id,
        );

        assert_eq!(request_status(app, &uri).await, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn state_events_modal_navigation_updates_entry_head_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2Ffixtures%2Fprograms.git.enable%3Fq%3Dgit%26source%3Dall&previous_url=%2F%3Fq%3Dgit",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-modal"));
        assert!(body.contains("nixsearchApplyModalPatch"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(
            r#""canonicalUrl":"https://search.example.com/fixtures/programs.git.enable""#
        ));
        assert!(body.contains(r#""robots":null"#));
    }

    #[tokio::test]
    async fn state_events_direct_entry_navigation_patches_results_and_clears_modal() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2Ffixtures%2Fprograms.git.enable&previous_url=%2F%3Fq%3Dgit",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert!(body.contains("programs.git.enable"));
        assert!(body.contains(r#"<div id=\"entry-modal-container\"></div>"#));
        assert!(body.contains("nixsearchApplyModalPatch"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(
            r#""canonicalUrl":"https://search.example.com/fixtures/programs.git.enable""#
        ));
    }

    #[tokio::test]
    async fn state_events_modal_close_emits_complete_head_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2F%3Fq%3Dgit&previous_url=%2Ffixtures%2Fprograms.git.enable%3Fq%3Dgit%26source%3Dall",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-modal-container"));
        assert!(body.contains("nixsearchApplyModalPatch"));
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(" results for git"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
    }

    #[tokio::test]
    async fn state_events_emits_noindex_head_metadata_for_search_page() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation("/-/state/events?url=%2F%3Fq%3Dgit", &generation_id);
        let (status, body) = request_body(app, &uri).await;

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
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2F%3Fq%3Dgit%26page%3D2&previous_url=%2F%3Fq%3Dgit",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

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
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=%2Ffixtures%3Fref_set%3Dsingle",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":null"#));
        assert!(body.contains(r#""robots":"noindex,follow""#));
    }
}
