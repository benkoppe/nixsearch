use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::middleware;
use axum::routing::get;
use tower_http::trace::TraceLayer;

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::{IndexStore, PublishedGeneration};
use nixsearch_ops::targets::{TargetCoverage, TargetKey, target_coverage};
use nixsearch_ops::{generate, lock, seo};
use nixsearch_service::{SearchService, ServingGenerationPolicy};

mod entry;
mod handlers;
mod maintenance;
mod metadata;
mod opensearch;
mod origin;
mod reconciliation;
mod render_docs;
mod request;
mod robots;
mod scripts;
mod sitemap;
mod sitemap_artifact;
mod source_labels;
mod templates;
mod urls;

const DEFAULT_LIMIT: usize = 50;
const MAX_PAGE: usize = 1000;
const MAX_OFFSET: usize = (MAX_PAGE - 1) * DEFAULT_LIMIT;

const DATASTAR_JS_PATH: &str = "/-/assets/datastar.js";
const NAVIGATION_JS_PATH: &str = "/-/assets/navigation.js";
const RECONCILE_EVENTS_URL: &str = "/-/state/events";
const RESULTS_SLICE_URL: &str = "/-/results/slice";
const STYLE_CSS_PATH: &str = "/-/assets/style.css";

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    search: SearchService,
    sitemap_artifacts: sitemap_artifact::SitemapArtifacts,
}

pub async fn serve(config: AppConfig) -> Result<()> {
    config.validate()?;
    log_public_seo_state(&config);

    let startup_generation = prepare_startup_generation(&config).await?;

    let addr: SocketAddr =
        config.server.listen.parse().with_context(|| {
            format!("failed to parse listen address {:?}", config.server.listen)
        })?;

    let index_store = IndexStore::new(&config.data.index_dir);
    let leased_generation = index_store
        .lease_published_generation(startup_generation.generation)
        .context("failed to lease startup-accepted current index generation")?;

    let config = Arc::new(config);
    let search = SearchService::from_integrity_attested_leased_generation_with_policy(
        Arc::clone(&config),
        leased_generation,
        ServingGenerationPolicy::lazy_for_config(&config),
    )?;
    let generation = search.snapshot().to_published_generation();
    let sitemap_artifacts = sitemap_artifact::SitemapArtifacts::default();
    if config.public_seo_enabled() {
        let artifact =
            sitemap_artifact::ensure_current_sitemap_artifact(Arc::clone(&config), search.clone())
                .await
                .context("failed to prepare startup sitemap artifact")?;
        sitemap_artifacts.set_current(artifact);
    }

    log_startup_maintenance_state(&config, &generation);

    maintenance::spawn(
        Arc::clone(&config),
        search.clone(),
        sitemap_artifacts.clone(),
        startup_generation.cleanup_after_startup,
    );

    let state = AppState {
        config,
        search,
        sitemap_artifacts,
    };

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
    let internal_routes = Router::new()
        .route("/health", get(handlers::health))
        .route("/state/events", get(handlers::state_events))
        .route("/results/slice", get(handlers::results_slice))
        .route("/assets/datastar.js", get(handlers::datastar_js))
        .route("/assets/navigation.js", get(handlers::navigation_js))
        .route("/assets/style.css", get(handlers::style_css))
        .layer(middleware::map_response(robots::add_noindex_header));

    Router::new()
        .nest("/-", internal_routes)
        .route("/robots.txt", get(handlers::robots_txt))
        .route("/sitemap.xml", get(handlers::sitemap_xml))
        .route("/opensearch.xml", get(handlers::root_opensearch_xml))
        .route("/sitemaps", get(handlers::sitemaps_not_found))
        .route("/sitemaps/{*path}", get(handlers::sitemaps_not_found))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/apple-touch-icon.png", get(handlers::apple_touch_icon))
        .route("/", get(handlers::public_page))
        .route("/{*path}", get(handlers::public_page))
        .with_state(state)
}

struct StartupGeneration {
    generation: PublishedGeneration,
    cleanup_after_startup: bool,
}

#[cfg(test)]
async fn ensure_current_generation(config: &AppConfig) -> Result<PublishedGeneration> {
    Ok(prepare_startup_generation(config).await?.generation)
}

async fn prepare_startup_generation(config: &AppConfig) -> Result<StartupGeneration> {
    let index_store = IndexStore::new(&config.data.index_dir);

    let mut startup_assessment = assess_current_generation_for_startup(config, &index_store);

    if config.public_seo_enabled()
        && matches!(
            startup_assessment,
            Ok(StartupGenerationAssessment::SeoSidecarUnavailable { .. })
        )
    {
        match repair_current_generation(config).await {
            Ok(()) => {
                startup_assessment = assess_current_generation_for_startup(config, &index_store);
            }
            Err(error) => {
                tracing::warn!(
                    "failed to repair current generation before startup validation: {error:#}"
                );
            }
        }
    }

    match startup_assessment {
        Ok(StartupGenerationAssessment::Ready(current)) => {
            if current.missing_targets().is_empty() {
                return Ok(StartupGeneration {
                    generation: current.generation,
                    cleanup_after_startup: false,
                });
            }

            if current.serves_default_scope() {
                tracing::warn!(
                    missing = %format_target_keys(current.missing_targets()),
                    "current index is missing configured targets but still serves a default search scope; startup will continue"
                );

                return Ok(StartupGeneration {
                    generation: current.generation,
                    cleanup_after_startup: false,
                });
            }

            if !config.server.bootstrap {
                return Ok(StartupGeneration {
                    generation: current.generation,
                    cleanup_after_startup: false,
                });
            }

            tracing::info!(
                missing = %format_target_keys(current.missing_targets()),
                "current index is missing configured targets needed for default search; bootstrap enabled, rebuilding index"
            );
        }
        Ok(
            StartupGenerationAssessment::StructurallyInvalid { generation, error }
            | StartupGenerationAssessment::SeoSidecarUnavailable { generation, error },
        ) => {
            if !config.server.bootstrap {
                return Err(error).with_context(|| {
                    format!(
                        "failed to validate current index generation {}; run `nixsearch update` first",
                        generation.path
                    )
                });
            }

            tracing::warn!(
                generation = %generation.path,
                "current index generation is not startup-ready; bootstrap will rebuild it: {error:#}"
            );
        }
        Ok(StartupGenerationAssessment::Missing) => {}
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

    match assess_current_generation_for_startup(config, &index_store) {
        Ok(StartupGenerationAssessment::Ready(current)) => {
            if current.missing_targets().is_empty() || current.serves_default_scope() {
                tracing::info!(
                    "current index was created by another process while waiting for lock"
                );
                return Ok(StartupGeneration {
                    generation: current.generation,
                    cleanup_after_startup: false,
                });
            }

            tracing::warn!(
                generation = %current.generation.path,
                missing = %format_target_keys(current.missing_targets()),
                "current index still does not serve a default search scope after acquiring lock; rebuilding"
            );
        }
        Ok(
            StartupGenerationAssessment::StructurallyInvalid { generation, error }
            | StartupGenerationAssessment::SeoSidecarUnavailable { generation, error },
        ) => {
            tracing::warn!(
                generation = %generation.path,
                "current index generation is still invalid after acquiring lock; rebuilding it: {error:#}"
            );
        }
        Ok(StartupGenerationAssessment::Missing) => {}
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

    match assess_current_generation_for_startup(config, &index_store)? {
        StartupGenerationAssessment::Ready(current) => {
            drop(update_lock);

            Ok(StartupGeneration {
                generation: current.generation,
                cleanup_after_startup: true,
            })
        }
        StartupGenerationAssessment::StructurallyInvalid { generation, error }
        | StartupGenerationAssessment::SeoSidecarUnavailable { generation, error } => Err(error)
            .with_context(|| {
                format!(
                    "bootstrap published index generation {} but it is not startup-ready",
                    generation.path
                )
            }),
        StartupGenerationAssessment::Missing => {
            bail!("bootstrap completed without publishing a current index")
        }
    }
}

async fn repair_current_generation(config: &AppConfig) -> Result<()> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let update_lock = lock::acquire_update_lock(&config.data.index_dir)?;
        match seo::repair_current_seo_sidecar_under_lock(&config, &update_lock)? {
            seo::SeoSidecarRepairOutcome::AlreadySeoVerified { generation }
            | seo::SeoSidecarRepairOutcome::Repaired { generation } => {
                tracing::info!(generation = %generation.path, "current generation is SEO-verified");
            }
            seo::SeoSidecarRepairOutcome::MissingCurrent => {
                tracing::debug!(
                    "skipped SEO sidecar repair because there is no current generation"
                );
            }
            seo::SeoSidecarRepairOutcome::SupersededBeforeRepair
            | seo::SeoSidecarRepairOutcome::SupersededAfterRepair => {
                tracing::info!("skipped SEO sidecar repair because current generation changed");
            }
            seo::SeoSidecarRepairOutcome::Unrepairable { generation, error }
            | seo::SeoSidecarRepairOutcome::RepairFailed { generation, error } => {
                anyhow::bail!(
                    "failed to repair SEO sidecar for {}: {error}",
                    generation.path
                );
            }
        }

        Ok(())
    })
    .await
    .context("failed to join SEO sidecar repair task")?
}

struct StartupAcceptedGeneration {
    generation: PublishedGeneration,
    coverage: TargetCoverage,
}

impl StartupAcceptedGeneration {
    fn missing_targets(&self) -> &std::collections::BTreeSet<TargetKey> {
        &self.coverage.missing_configured_targets
    }

    fn serves_default_scope(&self) -> bool {
        self.coverage.serves_default_scope
    }
}

enum StartupGenerationAssessment {
    Missing,
    Ready(StartupAcceptedGeneration),
    StructurallyInvalid {
        generation: PublishedGeneration,
        error: anyhow::Error,
    },
    SeoSidecarUnavailable {
        generation: PublishedGeneration,
        error: anyhow::Error,
    },
}

fn assess_current_generation_for_startup(
    config: &AppConfig,
    index_store: &IndexStore,
) -> Result<StartupGenerationAssessment> {
    let Some(generation) = index_store.try_current_leased_generation()? else {
        return Ok(StartupGenerationAssessment::Missing);
    };
    let published = generation.to_published_generation();

    if let Err(error) = SearchService::verify_leased_generation_structural(config, &generation) {
        return Ok(StartupGenerationAssessment::StructurallyInvalid {
            generation: published,
            error,
        });
    }

    if config.public_seo_enabled()
        && let Err(error) =
            SearchService::verify_leased_generation_seo_sidecar_present(config, &generation)
    {
        return Ok(StartupGenerationAssessment::SeoSidecarUnavailable {
            generation: published,
            error,
        });
    }

    let coverage = target_coverage(config, generation.manifest())?;

    Ok(StartupGenerationAssessment::Ready(
        StartupAcceptedGeneration {
            generation: published,
            coverage,
        },
    ))
}

fn log_public_seo_state(config: &AppConfig) {
    if let Some(public_url) = config.server.public_url.as_deref() {
        tracing::info!(public_url, "public SEO enabled");
    } else {
        tracing::warn!(
            "public SEO disabled because server.public_url is unset; pages will emit noindex and sitemap.xml will return 404"
        );
    }
}

fn format_target_keys<'a>(targets: impl IntoIterator<Item = &'a TargetKey>) -> String {
    targets
        .into_iter()
        .map(ToString::to_string)
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
    use nixsearch_config::source::SourceKind;
    use nixsearch_core::artifact::ArtifactKind;
    use nixsearch_core::document::SearchDocument;
    use nixsearch_index::search::SearchIndex;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::{
        assert_canonical_options_manifest_targets, index_target, options_target,
        publish_canonical_options_index, publish_canonical_options_index_with_generated_at,
        publish_documents_with_manifest_targets, publish_fixture_options_index_for_refs,
    };
    use nixsearch_service::{SearchService, ServingGenerationPolicy};
    use nixsearch_test_support::{
        REF_SMALL, REF_STABLE, SOURCE_FIXTURES, TEST_PUBLIC_ORIGIN, app_config,
        app_config_with_extra_fixture_source, app_config_with_public_url, ingest_context_for,
        multi_ref_app_config, multi_ref_app_config_with_public_url, option_doc_for,
        package_doc_for, utf8_path_buf,
    };
    use tempfile::{TempDir, tempdir};
    use tower::ServiceExt;

    use crate::app_router;

    use super::{AppState, ensure_current_generation};

    fn test_app(config: AppConfig) -> Router {
        let config = Arc::new(config);
        let search = SearchService::open_current(Arc::clone(&config)).unwrap();
        let sitemap_artifacts = crate::sitemap_artifact::SitemapArtifacts::default();
        if config.public_seo_enabled() {
            sitemap_artifacts.set_current(
                crate::sitemap_artifact::ensure_current_sitemap_artifact_blocking(
                    Arc::clone(&config),
                    search.clone(),
                )
                .unwrap(),
            );
        }

        app_router(AppState {
            config,
            search,
            sitemap_artifacts,
        })
    }

    struct TestResponse {
        status: StatusCode,
        cache_control: Option<String>,
        content_type: String,
        location: Option<String>,
        x_robots_tag: Option<String>,
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

    async fn request_status_and_location(app: Router, uri: &str) -> (StatusCode, Option<String>) {
        let response = request_test_response(app, uri).await;

        (response.status, response.location)
    }

    async fn request_x_robots_tag(app: Router, uri: &str) -> Option<String> {
        request_test_response(app, uri).await.x_robots_tag
    }

    async fn request_test_response(app: Router, uri: &str) -> TestResponse {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let cache_control = response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let x_robots_tag = response
            .headers()
            .get("x-robots-tag")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();

        TestResponse {
            status,
            cache_control,
            content_type,
            location,
            x_robots_tag,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn request_sitemap(app: Router) -> String {
        let (status, content_type, body) = request_content_type_and_body(app, "/sitemap.xml").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/xml"));
        assert!(body.contains(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#));

        body
    }

    fn current_generation_id(index_dir: impl AsRef<camino::Utf8Path>) -> String {
        let store = IndexStore::new(index_dir.as_ref());
        let path = store.current_path().unwrap();
        store.read_manifest(&path).unwrap().generation_id
    }

    fn bootstrap_config(index_dir: impl AsRef<camino::Utf8Path>, tempdir: &TempDir) -> AppConfig {
        let mut config = app_config(index_dir);
        config.data.artifact_url = format!("file://{}", tempdir.path().join("artifacts").display());
        config
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
        let app = test_app(app_config_with_public_url(&index_dir));

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

    fn publish_internal_and_hidden_options_index(index_dir: &camino::Utf8Path) {
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
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                SearchDocument::Option(internal),
                SearchDocument::Option(hidden),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 2)],
        );
    }

    fn publish_public_internal_and_hidden_options_index(index_dir: &camino::Utf8Path) {
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
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![
                option_doc_for(&context, "public.entry", "Public option."),
                SearchDocument::Option(internal),
                SearchDocument::Option(hidden),
            ],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 3)],
        );
    }

    fn publish_opensearch_entry_options_index(index_dir: &camino::Utf8Path) {
        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);

        publish_documents_with_manifest_targets(
            index_dir,
            time::OffsetDateTime::now_utc(),
            vec![option_doc_for(
                &context,
                "opensearch.xml",
                "OpenSearch option.",
            )],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
        );
    }

    fn remove_current_seo_sidecar(index_dir: &camino::Utf8Path) {
        let store = IndexStore::new(index_dir);
        let current = store.current_path().unwrap();

        fs::remove_file(SeoFactsArtifact::path(&current)).unwrap();
    }

    fn corrupt_current_seo_sidecar(index_dir: &camino::Utf8Path) {
        let store = IndexStore::new(index_dir);
        let current = store.current_path().unwrap();

        fs::write(SeoFactsArtifact::path(&current), b"{ not valid json").unwrap();
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
        let tag = format!(
            r#"<meta name="robots" content="{}">"#,
            crate::robots::ROBOTS_NOINDEX_FOLLOW
        );
        assert!(body.contains(&tag), "missing noindex robots tag {tag:?}");
    }

    fn assert_no_robots(body: &str) {
        assert!(
            !body.contains(r#"name="robots""#),
            "unexpected robots tag in body"
        );
    }

    fn sitemap_loc_for_path(path: &str) -> String {
        let url = format!("{TEST_PUBLIC_ORIGIN}{path}");
        format!("<loc>{}</loc>", html_escape::encode_text(&url))
    }

    fn assert_sitemap_has_path(body: &str, path: &str) {
        let loc = sitemap_loc_for_path(path);
        assert!(body.contains(&loc), "missing sitemap loc {loc:?}");
    }

    fn assert_sitemap_missing_path(body: &str, path: &str) {
        let loc = sitemap_loc_for_path(path);
        assert!(!body.contains(&loc), "unexpected sitemap loc {loc:?}");
    }

    fn assert_sitemap_has_no_query_kind(body: &str) {
        assert!(
            !body.contains("?kind="),
            "unexpected sitemap kind query in {body}"
        );
    }

    fn assert_sitemap_home_only(body: &str) {
        assert_sitemap_has_path(body, "/");
        assert_eq!(
            body.matches("<loc>").count(),
            1,
            "expected home-only sitemap, got {body}"
        );
    }

    fn assert_og_url(body: &str, expected: &str) {
        let tag = format!(r#"<meta property="og:url" content="{expected}">"#);
        assert!(body.contains(&tag), "missing og:url tag {tag:?}");
    }

    fn assert_no_open_graph(body: &str) {
        assert!(
            !body.contains(r#"property="og:"#),
            "unexpected Open Graph tag in body"
        );
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

        let app = test_app(app_config_with_public_url(&index_dir));

        assert_eq!(request_status(app, "/missing").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn full_page_unknown_ref_returns_404() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

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
    async fn full_page_reconciles_published_generation_before_rendering() {
        let fixture = reconciled_generation_fixture();

        let (status, body) = request_body(fixture.app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!(
            r#""generationId":"{}""#,
            fixture.new_generation_id
        )));
        assert!(!body.contains(&format!(
            r#""generationId":"{}""#,
            fixture.old_generation_id
        )));
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

        let app = test_app(app_config_with_public_url(&index_dir));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/robots.txt").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("User-agent: *"));
        assert!(body.contains("Sitemap: https://search.example.com/sitemap.xml"));

        let (status, content_type, body) =
            request_content_type_and_body(app.clone(), "/sitemap.xml").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/xml"));
        assert!(body.contains(r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">"#));
        assert!(body.contains("<loc>https://search.example.com/</loc>"));

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
    async fn opensearch_routes_use_configured_public_origin() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        let response = request_test_response(app.clone(), "/opensearch.xml").await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(
            response
                .content_type
                .starts_with("application/opensearchdescription+xml")
        );
        assert_eq!(
            response.cache_control.as_deref(),
            Some("public, max-age=604800")
        );
        assert!(response.body.contains("<ShortName>nixsearch</ShortName>"));
        assert!(
            response
                .body
                .contains(r#"template="https://search.example.com/?q={searchTerms}""#)
        );
        assert!(
            response
                .body
                .contains("<SearchForm>https://search.example.com/</SearchForm>")
        );

        let response = request_test_response(app, "/opensearch.xml?source=fixtures").await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains("<ShortName>fixtures</ShortName>"));
        assert!(
            response
                .body
                .contains(r#"template="https://search.example.com/fixtures?q={searchTerms}""#)
        );
    }

    #[tokio::test]
    async fn source_opensearch_path_remains_available_for_entries() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_opensearch_entry_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/opensearch.xml").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("opensearch.xml"));
        assert_has_canonical(&body, "https://search.example.com/fixtures/opensearch.xml");
        assert!(!body.contains("OpenSearchDescription"));
    }

    #[tokio::test]
    async fn opensearch_not_found_responses_emit_x_robots_noindex_header() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let public_app = test_app(app_config_with_public_url(&index_dir));
        assert_eq!(
            request_x_robots_tag(public_app, "/opensearch.xml?source=missing")
                .await
                .as_deref(),
            Some(crate::robots::X_ROBOTS_TAG_NOINDEX_NOFOLLOW)
        );

        let private_app = test_app(app_config(&index_dir));
        assert_eq!(
            request_x_robots_tag(private_app, "/opensearch.xml")
                .await
                .as_deref(),
            Some(crate::robots::X_ROBOTS_TAG_NOINDEX_NOFOLLOW)
        );
    }

    #[tokio::test]
    async fn internal_endpoints_emit_x_robots_noindex_header() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let expected = Some(crate::robots::X_ROBOTS_TAG_NOINDEX_NOFOLLOW);
        let result_slice_uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0",
            &generation_id,
        );

        for uri in [
            "/-/health",
            "/-/state/events?url=%2F",
            "/-/assets/datastar.js",
            "/-/assets/navigation.js",
            "/-/assets/style.css",
            result_slice_uri.as_str(),
        ] {
            assert_eq!(
                request_x_robots_tag(app.clone(), uri).await.as_deref(),
                expected,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn static_assets_are_served_with_long_lived_cache_headers() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for (uri, expected_content_type, expected_body) in [
            (
                crate::scripts::datastar_script_url(),
                "text/javascript",
                "datastar",
            ),
            (
                crate::scripts::navigation_script_url(),
                "text/javascript",
                "nixsearch-reconcile",
            ),
            (crate::scripts::style_css_url(), "text/css", ":root"),
        ] {
            let response = request_test_response(app.clone(), uri).await;

            assert_eq!(response.status, StatusCode::OK, "{uri}");
            assert!(
                response.content_type.starts_with(expected_content_type),
                "{uri}: {}",
                response.content_type
            );
            assert_eq!(
                response.cache_control.as_deref(),
                Some("public, max-age=31536000, immutable"),
                "{uri}"
            );
            assert!(response.body.contains(expected_body), "{uri}");
        }
    }

    #[tokio::test]
    async fn static_assets_require_matching_fingerprints_for_immutable_cache() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for uri in ["/-/assets/style.css", "/-/assets/style.css?v=wrong"] {
            let response = request_test_response(app.clone(), uri).await;

            assert_eq!(response.status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(response.cache_control.as_deref(), Some("no-store"), "{uri}");
            assert!(!response.body.contains(":root"), "{uri}");
        }
    }

    #[tokio::test]
    async fn full_page_uses_external_css_and_navigation_assets() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let response = request_test_response(app, "/").await;

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.cache_control.as_deref(),
            Some("public, max-age=300")
        );
        assert!(response.body.contains(&format!(
            r#"<link rel="stylesheet" href="{}">"#,
            crate::scripts::style_css_url()
        )));
        assert!(response.body.contains(
            r#"<link rel="search" type="application/opensearchdescription+xml" title="nixsearch" href="/opensearch.xml">"#
        ));
        assert!(response.body.contains(
            r#"<link rel="search" type="application/opensearchdescription+xml" title="nixsearch Fixtures" href="/opensearch.xml?source=fixtures">"#
        ));
        assert!(response.body.contains(&format!(
            r#"<script type="module" src="{}"></script>"#,
            crate::scripts::datastar_script_url()
        )));
        assert!(response.body.contains(&format!(
            r#"<script src="{}"></script>"#,
            crate::scripts::navigation_script_url()
        )));
        assert!(!response.body.contains("const RECONCILE_EVENT"));
        assert!(!response.body.contains("color-scheme: dark"));
    }

    #[tokio::test]
    async fn public_pages_and_sitemap_do_not_emit_x_robots_header() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for uri in ["/", "/sitemap.xml", "/opensearch.xml"] {
            assert_eq!(request_x_robots_tag(app.clone(), uri).await, None, "{uri}");
        }
    }

    #[tokio::test]
    async fn sitemap_not_found_responses_emit_x_robots_noindex_header() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let public_app = test_app(app_config_with_public_url(&index_dir));
        for uri in ["/sitemaps", "/sitemap.xml?foo=bar"] {
            assert_eq!(
                request_x_robots_tag(public_app.clone(), uri)
                    .await
                    .as_deref(),
                Some(crate::robots::X_ROBOTS_TAG_NOINDEX_NOFOLLOW),
                "{uri}"
            );
        }

        let private_app = test_app(app_config(&index_dir));
        assert_eq!(
            request_x_robots_tag(private_app, "/sitemap.xml")
                .await
                .as_deref(),
            Some(crate::robots::X_ROBOTS_TAG_NOINDEX_NOFOLLOW)
        );
    }

    #[tokio::test]
    async fn sitemap_ignores_request_derived_origin() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
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

        assert!(body.contains("https://search.example.com/"));
        assert!(!body.contains("example.com&amp;x=&lt;tag&gt;"));
        assert!(!body.contains("http://example.com&x=<tag>/"));
    }

    #[tokio::test]
    async fn sitemap_includes_home_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_has_path(&body, "/");
    }

    #[tokio::test]
    async fn sitemap_urlset_omits_generation_lastmod() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        );

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert!(!body.contains("<lastmod>"));
    }

    #[tokio::test]
    async fn sitemap_returns_503_when_artifact_generation_is_stale() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let old_search = SearchService::open_current(Arc::clone(&config)).unwrap();
        let sitemap_artifacts = crate::sitemap_artifact::SitemapArtifacts::default();
        sitemap_artifacts.set_current(
            crate::sitemap_artifact::ensure_current_sitemap_artifact_blocking(
                Arc::clone(&config),
                old_search,
            )
            .unwrap(),
        );

        let context = ingest_context_for(SOURCE_FIXTURES, REF_SMALL);
        publish_documents_with_manifest_targets(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + time::Duration::hours(2),
            vec![option_doc_for(
                &context,
                "programs.ripgrep.enable",
                "Ripgrep option.",
            )],
            vec![options_target(SOURCE_FIXTURES, REF_SMALL, 1)],
        );
        let search = SearchService::open_current(Arc::clone(&config)).unwrap();

        let app = app_router(AppState {
            config,
            search,
            sitemap_artifacts,
        });
        let response = request_test_response(app, "/sitemap.xml").await;

        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.body, "sitemap temporarily unavailable");
    }

    #[tokio::test]
    async fn sitemap_includes_clean_candidate_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_has_path(&body, "/fixtures/programs.git.enable");
    }

    #[tokio::test]
    async fn sitemap_ignores_stale_cross_kind_candidates() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_has_path(&body, "/fixtures/git");
        assert_sitemap_has_no_query_kind(&body);
    }

    #[tokio::test]
    async fn sitemap_excludes_hidden_and_internal_only_options() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_internal_and_hidden_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_missing_path(&body, "/fixtures/internal.entry");
        assert_sitemap_missing_path(&body, "/fixtures/hidden.entry");
        assert_sitemap_home_only(&body);
    }

    #[tokio::test]
    async fn sitemap_excludes_same_kind_duplicates() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_duplicate_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_home_only(&body);
    }

    #[tokio::test]
    async fn sitemap_excludes_non_default_refs() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_has_path(&body, "/fixtures/programs.small.git.enable");
        assert_sitemap_missing_path(&body, "/fixtures/programs.stable.git.enable");
        assert!(
            !body.contains("ref="),
            "sitemap must not include ref params"
        );
    }

    #[tokio::test]
    async fn sitemap_excludes_app_and_service_sources() {
        for source_kind in [SourceKind::Apps, SourceKind::Services] {
            let tempdir = tempdir().unwrap();
            let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
            publish_canonical_options_index(&index_dir);

            let mut config = app_config_with_public_url(&index_dir);
            config
                .sources
                .get_mut(SOURCE_FIXTURES)
                .expect("fixture source exists")
                .kind = source_kind;
            let app = test_app(config);
            let body = request_sitemap(app).await;

            assert_sitemap_home_only(&body);
        }
    }

    #[tokio::test]
    async fn sitemap_with_verified_seo_facts_renders_candidates_immediately() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let body = request_sitemap(app).await;

        assert_sitemap_has_path(&body, "/fixtures");
        assert_sitemap_has_path(&body, "/fixtures/programs.git.enable");
    }

    #[tokio::test]
    async fn sitemap_with_missing_sidecar_is_rejected_by_app_startup() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        remove_current_seo_sidecar(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();
        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
    }

    #[tokio::test]
    async fn sitemap_shard_query_returns_404_when_sitemap_fits_one_document() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        assert_eq!(
            request_status(app, "/sitemap.xml?shard=00001").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn sitemap_rejects_noncanonical_queries() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for uri in [
            "/sitemap.xml?foo=bar",
            "/sitemap.xml?shard=1",
            "/sitemap.xml?shard=%30%30%30%30%31",
            "/sitemap.xml?shard=00001&shard=00002",
        ] {
            assert_eq!(
                request_status(app.clone(), uri).await,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn robots_txt_references_sitemap_xml() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, content_type, body) = request_content_type_and_body(app, "/robots.txt").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("Sitemap: https://search.example.com/sitemap.xml"));
    }

    #[tokio::test]
    async fn robots_txt_disallows_without_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, content_type, body) = request_content_type_and_body(app, "/robots.txt").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/plain"));
        assert_eq!(body, "User-agent: *\nDisallow: /\n");
        assert!(!body.contains("Sitemap:"));
    }

    #[tokio::test]
    async fn sitemap_returns_404_without_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));

        assert_eq!(
            request_status(app, "/sitemap.xml").await,
            StatusCode::NOT_FOUND
        );
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
            request_status(app.clone(), "/?kind=option").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request_status(app.clone(), "/fixtures/").await,
            StatusCode::PERMANENT_REDIRECT
        );
        assert_eq!(
            request_status(app, "/?q=git&page=1001").await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn public_page_preserves_query_when_redirecting_trailing_slash() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, location) = request_status_and_location(app, "/fixtures/?q=git").await;

        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(location.as_deref(), Some("/fixtures?q=git"));
    }

    #[tokio::test]
    async fn public_page_rejects_tracking_query_params() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/?q=git&utm_source=x").await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown query parameter"));
        assert!(body.contains("utm_source"));
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
        assert!(body.contains("unknown query parameter"));
        assert!(body.contains("kind"));

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
        assert!(body.contains("unknown query parameter"));
        assert!(body.contains("kind"));
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
    async fn state_events_missing_entry_returns_200_with_modal_error() {
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

        assert_eq!(status, StatusCode::OK);
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
    async fn home_without_public_url_emits_no_public_seo_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
        assert_no_open_graph(&body);
    }

    #[tokio::test]
    async fn home_count_ignores_stale_cross_kind_manifest_targets() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<strong>1</strong> packages and options"));
        assert!(!body.contains("<strong>2</strong> packages and options"));
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
        assert!(body.contains(r#""openGraph":null"#));
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
        assert_og_url(&body, "https://search.example.com/fixtures");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn source_without_public_url_emits_no_public_seo_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/fixtures").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
        assert_no_open_graph(&body);
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
    async fn verified_source_default_ref_emits_canonical_immediately() {
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
    async fn source_default_ref_without_sidecar_is_rejected_by_app_startup() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        remove_current_seo_sidecar(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();
        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
    }

    #[tokio::test]
    async fn source_default_ref_without_indexable_entries_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_internal_and_hidden_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
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
        assert_og_url(
            &body,
            "https://search.example.com/fixtures/programs.git.enable",
        );
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn direct_entry_without_public_url_emits_no_public_seo_metadata() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app, "/fixtures/programs.git.enable").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert_no_canonical(&body);
        assert_has_robots(&body);
        assert_no_open_graph(&body);
    }

    #[tokio::test]
    async fn direct_entry_page_without_sidecar_is_rejected_by_app_startup() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        remove_current_seo_sidecar(&index_dir);

        let config = Arc::new(app_config_with_public_url(&index_dir));
        let error = SearchService::open_current(config).unwrap_err();
        assert!(format!("{error:#}").contains("failed to read SEO sidecar"));
    }

    #[tokio::test]
    async fn direct_entry_non_default_ref_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) =
            request_body(app, "/fixtures/programs.stable.git.enable?ref=stable").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn direct_entry_app_and_service_sources_emit_noindex_without_canonical() {
        for source_kind in [SourceKind::Apps, SourceKind::Services] {
            let tempdir = tempdir().unwrap();
            let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
            publish_canonical_options_index(&index_dir);

            let mut config = app_config_with_public_url(&index_dir);
            config
                .sources
                .get_mut(SOURCE_FIXTURES)
                .expect("fixture source exists")
                .kind = source_kind;
            let app = test_app(config);
            let (status, body) = request_body(app, "/fixtures/programs.git.enable").await;

            assert_eq!(status, StatusCode::OK);
            assert!(body.contains("entry-page"));
            assert_no_canonical(&body);
            assert_has_robots(&body);
        }
    }

    #[tokio::test]
    async fn direct_entry_identifying_params_still_render_in_results() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        for url in ["/fixtures/programs.git.enable?ref=small"] {
            let (status, body) = request_body(app.clone(), url).await;

            assert_eq!(status, StatusCode::OK, "{url}");
            assert!(body.contains("entry-page"), "{url}");
            assert!(body.contains("programs.git.enable"), "{url}");
            assert_h1_count(&body, 1);
            assert_empty_modal_container(&body);
        }
    }

    #[tokio::test]
    async fn direct_cross_kind_entry_page_uses_configured_kind() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/git").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert!(body.contains("Git option."));
        assert!(!body.contains("Multiple entries found"));
        assert_h1_count(&body, 1);
        assert_empty_modal_container(&body);
        assert_has_canonical(&body, "https://search.example.com/fixtures/git");
        assert_no_robots(&body);
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
        assert_og_url(
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
    async fn cross_kind_stale_entries_do_not_force_kind_canonicals() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));

        let (status, body) = request_body(app, "/fixtures/git").await;
        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures/git");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn same_kind_duplicate_entry_emits_noindex() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_duplicate_option_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures/duplicate.entry").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn result_links_ignore_stale_cross_kind_ambiguity() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_ambiguous_package_option_search_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config(&index_dir));
        let (status, body) = request_body(app.clone(), "/?q=git").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"href="/fixtures/git?q=git&amp;source=all""#));
        assert!(!body.contains("?kind="));

        let uri = with_generation(
            "/-/results/slice?url=%2F%3Fq%3Dgit&offset=0",
            &generation_id,
        );
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/fixtures/git?q=git&amp;source=all"));
        assert!(!body.contains("?kind="));
    }

    #[tokio::test]
    async fn internal_and_hidden_entry_pages_render_but_emit_noindex() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_internal_and_hidden_options_index(&index_dir);

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
    async fn all_source_search_pages_emit_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/?q=git").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
        assert_no_open_graph(&body);
    }

    #[tokio::test]
    async fn clean_named_source_search_page_emits_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures?q=git");
        assert_og_url(&body, "https://search.example.com/fixtures?q=git");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn named_source_search_page_canonicalizes_simple_query_spelling() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=%20Git%20").await;

        assert_eq!(status, StatusCode::OK);
        assert_has_canonical(&body, "https://search.example.com/fixtures?q=git");
        assert_og_url(&body, "https://search.example.com/fixtures?q=git");
        assert_no_robots(&body);
    }

    #[tokio::test]
    async fn empty_named_source_search_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=definitelynotindexed").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn search_page_matching_only_hidden_entries_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_public_internal_and_hidden_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=hidden").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
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
    async fn paginated_named_source_search_pages_emit_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git&page=2").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn contextual_entry_url_canonicalizes_to_clean_entry() {
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
    async fn non_default_ref_search_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git&ref=stable").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn ref_set_search_page_emits_noindex_without_canonical() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_fixture_options_index_for_refs(&index_dir, &[REF_SMALL, REF_STABLE]);

        let app = test_app(multi_ref_app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?q=git&ref_set=single").await;

        assert_eq!(status, StatusCode::OK);
        assert_no_canonical(&body);
        assert_has_robots(&body);
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
        let config = bootstrap_config(&index_dir, &tempdir);

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
        let config = bootstrap_config(&index_dir, &tempdir);
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
        let config = bootstrap_config(&index_dir, &tempdir);
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
    async fn ensure_current_generation_bootstraps_invalid_generation() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let manifest = store.current_manifest().unwrap();
        let broken = store.create_generation_path().unwrap();
        store.write_manifest(&broken, &manifest).unwrap();
        store.publish(&broken).unwrap();

        let config = bootstrap_config(&index_dir, &tempdir);

        let generation = ensure_current_generation(&config).await.unwrap();

        assert_ne!(generation.path, broken);
        assert_canonical_options_manifest_targets(&generation.manifest);
        assert_eq!(store.current_path().unwrap(), generation.path);
        SearchIndex::open(store.index_path(&generation.path)).unwrap();
    }

    #[tokio::test]
    async fn startup_defers_seo_sidecar_parse_until_seo_route() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        corrupt_current_seo_sidecar(&index_dir);
        let store = IndexStore::new(&index_dir);
        let config = app_config_with_public_url(&index_dir);

        let generation = ensure_current_generation(&config).await.unwrap();
        let generation_path = generation.path.clone();
        let leased_generation = store.lease_published_generation(generation).unwrap();
        let config = Arc::new(config);
        let search = SearchService::from_leased_generation_with_policy(
            Arc::clone(&config),
            leased_generation,
            ServingGenerationPolicy::lazy_for_config(&config),
        )
        .unwrap();

        assert_eq!(store.current_path().unwrap(), generation_path);
        let snapshot = search.snapshot();
        assert!(search.sitemap_candidates(&snapshot).is_err());
    }

    #[tokio::test]
    async fn direct_entry_with_unverified_lazy_sidecar_emits_noindex() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        corrupt_current_seo_sidecar(&index_dir);
        let store = IndexStore::new(&index_dir);
        let config = Arc::new(app_config_with_public_url(&index_dir));

        let generation = ensure_current_generation(&config).await.unwrap();
        let leased_generation = store.lease_published_generation(generation).unwrap();
        let search = SearchService::from_leased_generation_with_policy(
            Arc::clone(&config),
            leased_generation,
            ServingGenerationPolicy::lazy_for_config(&config),
        )
        .unwrap();
        let app = app_router(AppState {
            config,
            search,
            sitemap_artifacts: crate::sitemap_artifact::SitemapArtifacts::default(),
        });

        let (status, body) = request_body(app, "/fixtures/programs.git.enable").await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("entry-page"));
        assert_no_canonical(&body);
        assert_has_robots(&body);
    }

    #[tokio::test]
    async fn ensure_current_generation_keeps_generation_with_missing_seo_sidecar() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        let sidecar_path = SeoFactsArtifact::path(&published_path);
        fs::remove_file(&sidecar_path).unwrap();

        let config = bootstrap_config(&index_dir, &tempdir);

        let generation = ensure_current_generation(&config).await.unwrap();

        assert_eq!(generation.path, published_path);
        assert_canonical_options_manifest_targets(&generation.manifest);
        assert_eq!(store.current_path().unwrap(), generation.path);
        let search = SearchService::open_current(Arc::new(config)).unwrap();
        let snapshot = search.snapshot();
        assert!(search.sitemap_candidates(&snapshot).is_err());
        assert!(!sidecar_path.exists());
    }

    #[tokio::test]
    async fn ensure_current_generation_errors_on_invalid_generation_when_bootstrap_disabled() {
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
        assert!(error.contains("failed to validate current index generation"));
        assert!(error.contains("run `nixsearch update` first"));
    }

    #[tokio::test]
    async fn ensure_current_generation_allows_missing_seo_sidecar_when_bootstrap_disabled() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let store = IndexStore::new(&index_dir);
        fs::remove_file(SeoFactsArtifact::path(&published_path)).unwrap();
        let mut config = app_config(&index_dir);
        config.server.bootstrap = false;

        let generation = ensure_current_generation(&config).await.unwrap();

        assert_eq!(generation.path, published_path);
        assert_eq!(store.current_path().unwrap(), generation.path);
    }

    #[tokio::test]
    async fn ensure_current_generation_rebuilds_when_default_scope_is_incomplete() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        let published_path = publish_canonical_options_index(&index_dir);
        let mut config = bootstrap_config(&index_dir, &tempdir);
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
    async fn source_kind_query_returns_bad_request() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, body) = request_body(app, "/fixtures?kind=option").await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown query parameter"));
        assert!(body.contains("kind"));
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
    async fn state_events_rejects_matching_absolute_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation(
            "/-/state/events?url=https%3A%2F%2Fsearch.example.com%2Ffixtures",
            &generation_id,
        );
        let (status, content_type, body) = request_content_type_and_body(app, &uri).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("endpoint URL must be a path-and-query URL"));
    }

    #[tokio::test]
    async fn state_events_rejects_foreign_absolute_public_url() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let (status, content_type, body) = request_content_type_and_body(
            app,
            "/-/state/events?url=https%3A%2F%2Fevil.example%2Ffixtures",
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(content_type.starts_with("text/plain"));
        assert!(body.contains("endpoint URL must be a path-and-query URL"));
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
    async fn state_events_emits_noindex_head_metadata_for_all_source_search_page() {
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
    async fn state_events_emits_canonical_head_metadata_for_named_source_search_page() {
        let tempdir = tempdir().unwrap();
        let index_dir = utf8_path_buf(tempdir.path().join("indexes"));
        publish_canonical_options_index(&index_dir);
        let generation_id = current_generation_id(&index_dir);

        let app = test_app(app_config_with_public_url(&index_dir));
        let uri = with_generation("/-/state/events?url=%2Ffixtures%3Fq%3Dgit", &generation_id);
        let (status, body) = request_body(app, &uri).await;

        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("nixsearchApplyHeadMetadata"));
        assert!(body.contains(r#""canonicalUrl":"https://search.example.com/fixtures?q=git""#));
        assert!(body.contains(r#""robots":null"#));
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
