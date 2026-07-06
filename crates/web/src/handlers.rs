use std::convert::Infallible;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use datastar::prelude::{ExecuteScript, PatchElements};
use futures_util::stream;

use nixsearch_index::search::{EntryFactsStatus, EntryLookupResult, SearchResult};
use nixsearch_service::{
    EntryRequest, RequestResolutionError, SearchRequest, ServedGenerationSnapshot, ServiceError,
    ServiceResult,
};

use crate::AppState;
use crate::DEFAULT_LIMIT;
use crate::entry::{AnnotatedEntryDocument, EntryData};
use crate::metadata::{self, PageHeadMetadataInput, PageMetadata};
use crate::opensearch;
use crate::origin::{PageUrls, page_urls, page_urls_for_public_uri, public_path_and_query};
use crate::reconciliation::RequestGeneration;
use crate::request::{
    PageRequest, PageState, PublicRoute, SourceFilter, normalized_query,
    page_request_from_public_uri, page_state, public_uri,
};
use crate::robots;
use crate::scripts::{
    datastar_script, datastar_script_fingerprint, navigation_script, navigation_script_fingerprint,
    style_css as style_css_asset, style_css_fingerprint,
};
use crate::sitemap::validate_sitemap_query;
use crate::sitemap_artifact::SitemapArtifactLookupError;
use crate::templates;
use crate::templates::layout::{
    InitialReturnMetadata, ResultsContent, generation_change_script, head_metadata_script,
    modal_patch_script, results_patch_script,
};
use crate::urls::close_url_for_state;

pub async fn health() -> &'static str {
    "ok"
}

pub async fn favicon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/x-icon")],
        include_bytes!("../favicon.ico"),
    )
}

pub async fn apple_touch_icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        include_bytes!("../apple-touch-icon.png"),
    )
}

pub async fn datastar_js(uri: Uri) -> Response {
    asset_response(
        &uri,
        "text/javascript; charset=utf-8",
        datastar_script(),
        datastar_script_fingerprint(),
    )
}

pub async fn navigation_js(uri: Uri) -> Response {
    asset_response(
        &uri,
        "text/javascript; charset=utf-8",
        navigation_script(),
        navigation_script_fingerprint(),
    )
}

pub async fn style_css(uri: Uri) -> Response {
    asset_response(
        &uri,
        "text/css; charset=utf-8",
        style_css_asset(),
        style_css_fingerprint(),
    )
}

fn asset_headers(content_type: &'static str) -> [(HeaderName, &'static str); 2] {
    [
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
    ]
}

fn asset_response(
    uri: &Uri,
    content_type: &'static str,
    body: &'static str,
    expected_fingerprint: &str,
) -> Response {
    if !asset_fingerprint_matches(uri.query(), expected_fingerprint) {
        return (
            StatusCode::NOT_FOUND,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            "not found",
        )
            .into_response();
    }

    (asset_headers(content_type), body).into_response()
}

fn asset_fingerprint_matches(raw_query: Option<&str>, expected_fingerprint: &str) -> bool {
    raw_query
        .and_then(|query| query.strip_prefix("v="))
        .is_some_and(|value| !value.contains('&') && value == expected_fingerprint)
}

pub async fn robots_txt(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let public_seo_enabled = state.config.public_seo_enabled();
    let urls = public_seo_enabled.then(|| page_urls(state.config.as_ref(), &headers, &uri));
    let body = robots::robots_txt_body(
        public_seo_enabled,
        urls.as_ref().map(|urls| urls.origin.as_str()),
    );

    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

pub async fn sitemap_xml(State(state): State<AppState>, _headers: HeaderMap, uri: Uri) -> Response {
    if !state.config.public_seo_enabled() {
        return sitemaps_not_found().await;
    }

    let raw_query = uri.query();
    if validate_sitemap_query(raw_query).is_err() {
        return sitemaps_not_found().await;
    }

    let Some(artifact) = state.sitemap_artifacts.current() else {
        return sitemap_unavailable();
    };

    let snapshot = state.search.snapshot();
    let served_generation_id = &snapshot.manifest().generation_id;
    if artifact.generation_id() != served_generation_id {
        tracing::warn!(
            artifact_generation_id = artifact.generation_id(),
            served_generation_id,
            "sitemap artifact generation does not match served search generation"
        );
        return sitemap_unavailable();
    }

    match artifact.file_for_query(raw_query) {
        Ok(file) => file.serve_response().await,
        Err(
            SitemapArtifactLookupError::MalformedQuery | SitemapArtifactLookupError::ShardNotFound,
        ) => sitemaps_not_found().await,
    }
}

pub async fn root_opensearch_xml(State(state): State<AppState>, uri: Uri) -> Response {
    if !state.config.public_seo_enabled() {
        return sitemaps_not_found().await;
    }

    let source = match opensearch_source_query(&uri) {
        Ok(source) => source,
        Err(()) => return sitemaps_not_found().await,
    };

    let Some(origin) = crate::origin::configured_public_origin(&state.config) else {
        return sitemaps_not_found().await;
    };

    if let Some(source) = source {
        let Some(body) = opensearch::source_opensearch_xml(&state.config, &origin, &source) else {
            return sitemaps_not_found().await;
        };

        return opensearch_response(body);
    }

    opensearch_response(opensearch::root_opensearch_xml(&origin))
}

fn opensearch_source_query(uri: &Uri) -> Result<Option<String>, ()> {
    let Some(raw_query) = uri.query() else {
        return Ok(None);
    };
    if raw_query.is_empty() {
        return Ok(None);
    }

    let mut source = None;
    for part in raw_query.split('&') {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        if key != "source" || source.is_some() {
            return Err(());
        }
        let value = percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| ())?;
        let value = value.trim();
        if value.is_empty() {
            return Err(());
        }
        source = Some(value.to_owned());
    }

    Ok(source)
}

fn opensearch_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, opensearch::OPENSEARCH_CONTENT_TYPE),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        body,
    )
        .into_response()
}

pub async fn sitemaps_not_found() -> Response {
    robots::add_noindex_header(
        (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "not found",
        )
            .into_response(),
    )
    .await
}

fn sitemap_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::RETRY_AFTER, "30"),
        ],
        "sitemap temporarily unavailable",
    )
        .into_response()
}

pub async fn public_page(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    match normalized_public_uri(&uri) {
        Ok(Some(target)) => {
            return (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, target)]).into_response();
        }
        Ok(None) => {}
        Err(error) => {
            let page_urls = page_urls(state.config.as_ref(), &headers, &uri);
            return render_parse_error_response(&state, page_urls, &error);
        }
    }

    let request = match page_request_from_public_uri(&uri) {
        Ok(request) => request,
        Err(error) => {
            let page_urls = page_urls(state.config.as_ref(), &headers, &uri);
            return render_parse_error_response(&state, page_urls, &error.to_string());
        }
    };

    render_full_page_response(
        &state,
        page_urls(state.config.as_ref(), &headers, &uri),
        request,
    )
}

fn normalized_public_uri(uri: &Uri) -> std::result::Result<Option<String>, String> {
    let path = uri.path();
    if path.starts_with("//") {
        return Err("public URL must not be protocol-relative".to_owned());
    }
    if path != "/" && path.contains("//") {
        return Err("path must not contain empty segments".to_owned());
    }

    let mut normalized_path = path.to_owned();
    if normalized_path != "/" {
        while normalized_path.ends_with('/') {
            normalized_path.pop();
        }
    }

    let normalized = match uri.query() {
        Some(query) => format!("{normalized_path}?{query}"),
        None => normalized_path,
    };
    let current = match uri.query() {
        Some(query) => format!("{}?{query}", uri.path()),
        None => uri.path().to_owned(),
    };

    if normalized == current {
        Ok(None)
    } else {
        Ok(Some(normalized))
    }
}

pub async fn state_events(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let query = match crate::request::state_events_query_from_uri(&uri) {
        Ok(query) => query,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                error.to_string(),
            )
                .into_response();
        }
    };

    let target_uri = query.url;
    let target_public_url = public_path_and_query(&target_uri);
    let page_urls = page_urls_for_public_uri(&state.config, &headers, &target_uri);
    let generation = RequestGeneration::reconcile(&state);
    let snapshot = generation.snapshot();

    if generation.client_generation_changed(query.generation_id.as_deref()) {
        return generation_change_response(
            &state,
            &target_uri,
            &target_public_url,
            &page_urls,
            snapshot,
        );
    }

    let request = match page_request_from_public_uri(&target_uri) {
        Ok(request) => request,
        Err(error) => {
            return sse_error_response(
                state.config.public_seo_enabled(),
                &page_urls,
                &error.to_string(),
                Some(&target_public_url),
            );
        }
    };

    let page_state = match resolve_page_state(&state, snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return sse_error_response(
                state.config.public_seo_enabled(),
                &page_urls,
                &error.to_string(),
                Some(&target_public_url),
            );
        }
    };

    let previous_request = query
        .previous_url
        .as_ref()
        .and_then(|uri| page_request_from_public_uri(uri).ok());
    let navigation =
        state_events_navigation(&state, snapshot, previous_request.as_ref(), &page_state);
    let has_entry_detail = page_state.detail.is_some();
    let direct_entry = request.is_direct_entry();

    let search_result = if navigation.needs_search_result(&page_state) {
        let offset = match search_offset(&request) {
            Ok(offset) => offset,
            Err(error) => {
                return sse_error_response(
                    state.config.public_seo_enabled(),
                    &page_urls,
                    &error,
                    Some(&target_public_url),
                );
            }
        };

        Some(run_search_with_snapshot(
            &state,
            snapshot,
            &page_state,
            offset,
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    if let Some(Err(ServiceError::Resolution(error))) = &search_result {
        return sse_error_response(
            state.config.public_seo_enabled(),
            &page_urls,
            &error.to_string(),
            Some(&target_public_url),
        );
    }

    let search_error = search_error_message(&search_result);
    let results_content = results_content_for_search(&search_result, search_error.as_deref());

    let context_results_html = if !direct_entry && navigation.patch_results {
        Some(match &search_result {
            Some(Ok(result)) => {
                templates::results::render(&page_state, &result.hits, result.total, &state.config)
                    .into_string()
            }
            Some(Err(error)) => {
                templates::results::render_status_error("Search failed", &format!("{error:#}"))
                    .into_string()
            }
            None => templates::home::render(&state, &request, &page_state, snapshot).into_string(),
        })
    } else {
        None
    };

    let entry = match load_entry_data_from_snapshot(
        &state,
        &page_state,
        has_entry_detail.then_some(snapshot),
    ) {
        Ok(entry) => entry,
        Err(error) => {
            return sse_entry_error_response(
                SseEntryErrorContext {
                    state: &state,
                    request: &request,
                    page_state: &page_state,
                    page_urls: &page_urls,
                    snapshot,
                    results_html: context_results_html,
                    results_content,
                    target_public_url: &target_public_url,
                },
                &error,
            );
        }
    };
    let results_html = if direct_entry {
        Some(templates::results::render_entry(&state.config, &page_state, &entry).into_string())
    } else {
        context_results_html
    };
    let empty_entry = EntryData::Empty;
    let modal_entry = if direct_entry { &empty_entry } else { &entry };
    let modal_html =
        templates::modal::render(&state.config, &page_state, modal_entry).into_string();

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();

    if let Some(results_html) = results_html {
        events.push(Ok(ExecuteScript::new(results_patch_script(
            &results_html,
            &target_public_url,
        ))
        .write_as_axum_sse_event()));
    }

    events.push(Ok(ExecuteScript::new(modal_patch_script(
        &modal_html,
        &target_public_url,
    ))
    .write_as_axum_sse_event()));

    if navigation.has_complete_metadata(&page_state, &search_result, &entry) {
        let metadata = metadata::page_head_metadata(PageHeadMetadataInput {
            state: &state,
            request: &request,
            page_state: &page_state,
            page_urls: &page_urls,
            snapshot,
            content: results_content.metadata_content(),
            entry: &entry,
        });

        events.push(Ok(ExecuteScript::new(head_metadata_script(
            &metadata,
            Some(&target_public_url),
        ))
        .write_as_axum_sse_event()));
    }

    Sse::new(stream::iter(events)).into_response()
}

struct GenerationChangeContent {
    results_html: String,
    modal_html: String,
    metadata: PageMetadata,
}

struct GenerationChangeError {
    message: String,
}

impl GenerationChangeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn generation_change_response(
    state: &AppState,
    target_uri: &Uri,
    target_public_url: &str,
    page_urls: &PageUrls,
    snapshot: &ServedGenerationSnapshot,
) -> Response {
    let content = match generation_change_content(state, target_uri, page_urls, snapshot) {
        Ok(content) => content,
        Err(error) => generation_change_error_content(state, page_urls, error),
    };

    generation_change_events_response(snapshot, content, target_public_url)
}

fn generation_change_content(
    state: &AppState,
    target_uri: &Uri,
    page_urls: &PageUrls,
    snapshot: &ServedGenerationSnapshot,
) -> Result<GenerationChangeContent, GenerationChangeError> {
    let request = page_request_from_public_uri(target_uri)
        .map_err(|error| GenerationChangeError::new(error.to_string()))?;

    let page_state = resolve_page_state(state, snapshot, &request)
        .map_err(|error| GenerationChangeError::new(error.to_string()))?;

    let search_result = if normalized_query(&request.query).is_some() {
        let offset = search_offset(&request).map_err(GenerationChangeError::new)?;

        Some(run_search_with_snapshot(
            state,
            snapshot,
            &page_state,
            offset,
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    if let Some(Err(ServiceError::Resolution(error))) = &search_result {
        return Err(GenerationChangeError::new(error.to_string()));
    }

    let search_error = search_error_message(&search_result);
    let search_results_content =
        results_content_for_search(&search_result, search_error.as_deref());
    let entry_snapshot = page_state.detail.is_some().then_some(snapshot);
    let entry = match load_entry_data_from_snapshot(state, &page_state, entry_snapshot) {
        Ok(entry) => entry,
        Err(error) => entry_data_for_load_error(&error),
    };

    let direct_entry = request.is_direct_entry();
    let results_content = if direct_entry {
        ResultsContent::DirectEntry(&entry)
    } else {
        search_results_content
    };
    let results_html = if direct_entry {
        templates::results::render_entry(&state.config, &page_state, &entry).into_string()
    } else {
        render_navigation_results_html(state, &request, &page_state, snapshot, &search_result)
    };
    let empty_entry = EntryData::Empty;
    let modal_entry = if direct_entry { &empty_entry } else { &entry };
    let modal_html =
        templates::modal::render(&state.config, &page_state, modal_entry).into_string();
    let metadata = metadata::page_head_metadata(PageHeadMetadataInput {
        state,
        request: &request,
        page_state: &page_state,
        page_urls,
        snapshot,
        content: results_content.metadata_content(),
        entry: &entry,
    });

    Ok(GenerationChangeContent {
        results_html,
        modal_html,
        metadata,
    })
}

fn render_navigation_results_html(
    state: &AppState,
    request: &PageRequest,
    page_state: &PageState,
    snapshot: &ServedGenerationSnapshot,
    search_result: &Option<ServiceResult<SearchResult>>,
) -> String {
    match search_result {
        Some(Ok(result)) => {
            templates::results::render(page_state, &result.hits, result.total, &state.config)
                .into_string()
        }
        Some(Err(error)) => {
            templates::results::render_status_error("Search failed", &format!("{error:#}"))
                .into_string()
        }
        None => templates::home::render(state, request, page_state, snapshot).into_string(),
    }
}

fn generation_change_error_content(
    state: &AppState,
    page_urls: &PageUrls,
    error: GenerationChangeError,
) -> GenerationChangeContent {
    let message = error.message;
    let results_html =
        templates::results::render_status_error("Request failed", &message).into_string();
    let modal_html = templates::modal::render_empty_container().into_string();
    let metadata = metadata::noindex_head_metadata(
        state.config.public_seo_enabled(),
        page_urls,
        "Request failed",
        &message,
    );

    GenerationChangeContent {
        results_html,
        modal_html,
        metadata,
    }
}

fn generation_change_events_response(
    snapshot: &ServedGenerationSnapshot,
    content: GenerationChangeContent,
    target_public_url: &str,
) -> Response {
    let event = ExecuteScript::new(generation_change_script(
        &snapshot.manifest().generation_id,
        &content.results_html,
        &content.modal_html,
        &content.metadata,
        target_public_url,
    ))
    .write_as_axum_sse_event();

    Sse::new(stream::iter([Ok::<Event, Infallible>(event)])).into_response()
}

struct StateEventsNavigation {
    patch_results: bool,
}

impl StateEventsNavigation {
    fn needs_search_result(&self, next_state: &PageState) -> bool {
        next_state.q.is_some() && (self.patch_results || next_state.detail.is_none())
    }

    fn has_complete_metadata(
        &self,
        next_state: &PageState,
        search_result: &Option<ServiceResult<SearchResult>>,
        entry: &EntryData,
    ) -> bool {
        if next_state.q.is_some()
            && next_state.detail.is_none()
            && search_result.is_none()
            && matches!(entry, EntryData::Empty)
        {
            return false;
        }

        true
    }
}

fn state_events_navigation(
    state: &AppState,
    snapshot: &ServedGenerationSnapshot,
    previous_request: Option<&PageRequest>,
    next_state: &PageState,
) -> StateEventsNavigation {
    let Some(previous_request) = previous_request else {
        return StateEventsNavigation {
            patch_results: true,
        };
    };

    match resolve_page_state(state, snapshot, previous_request) {
        Ok(previous_state) => {
            let patch_results = previous_state.q != next_state.q
                || previous_state.page != next_state.page
                || previous_state.source_filter != next_state.source_filter
                || previous_state.source_ref != next_state.source_ref
                || previous_state.active_ref_set() != next_state.active_ref_set();

            StateEventsNavigation { patch_results }
        }
        Err(_) => StateEventsNavigation {
            patch_results: true,
        },
    }
}

fn search_offset(request: &PageRequest) -> std::result::Result<usize, String> {
    let page = request.query.page.unwrap_or(1);
    page.checked_sub(1)
        .and_then(|page_index| page_index.checked_mul(DEFAULT_LIMIT))
        .ok_or_else(|| "page offset overflow".to_owned())
}

fn search_error_message(search_result: &Option<ServiceResult<SearchResult>>) -> Option<String> {
    match search_result {
        Some(Err(error)) => Some(format!("{error:#}")),
        _ => None,
    }
}

fn results_content_for_search<'a>(
    search_result: &'a Option<ServiceResult<SearchResult>>,
    search_error: Option<&'a str>,
) -> ResultsContent<'a> {
    match search_result {
        Some(Ok(result)) => ResultsContent::SearchResults(result),
        Some(Err(_)) => ResultsContent::Error {
            title: "Search failed",
            message: search_error.unwrap_or("search failed"),
        },
        None => ResultsContent::Home,
    }
}

pub async fn results_slice(
    State(state): State<AppState>,
    _headers: HeaderMap,
    uri: Uri,
) -> Response {
    let query = match crate::request::slice_query_from_uri(&uri) {
        Ok(query) => query,
        Err(error) => {
            return json_error_response(StatusCode::BAD_REQUEST, &error.to_string());
        }
    };

    let generation = RequestGeneration::reconcile(&state);
    let snapshot = generation.snapshot();

    if generation.client_generation_changed(query.generation_id.as_deref()) {
        return generation.stale_json_response();
    }

    let uri = query.url;
    let request = match page_request_from_public_uri(&uri) {
        Ok(request) => request,
        Err(error) => {
            return json_error_response(StatusCode::BAD_REQUEST, &error.to_string());
        }
    };

    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if normalized_query(&request.query).is_none() {
        return json_error_response(StatusCode::BAD_REQUEST, "result slice requires q");
    }

    let page_state = match resolve_page_state(&state, snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return json_error_response(status_for_resolution_error(&error), &error.to_string());
        }
    };
    let search_result =
        run_search_with_snapshot(&state, snapshot, &page_state, query.offset, limit);

    match search_result {
        Ok(result) => {
            let count = result.hits.len();
            let end_offset = match query.offset.checked_add(count) {
                Some(end_offset) => end_offset,
                None => {
                    return json_error_response(
                        StatusCode::BAD_REQUEST,
                        "result slice offset overflow",
                    );
                }
            };
            let rows_html = templates::results::render_rows_only(
                &request,
                &result.hits,
                &state.config,
                query.offset,
            );
            Json(serde_json::json!({
                "rows": rows_html,
                "total": result.total,
                "offset": query.offset,
                "limit": limit,
                "count": count,
                "endOffset": end_offset,
            }))
            .into_response()
        }
        Err(error) => json_error_response(status_for_service_error(&error), &format!("{error:#}")),
    }
}

fn render_full_page_response(
    state: &AppState,
    page_urls: PageUrls,
    request: PageRequest,
) -> Response {
    let generation = RequestGeneration::reconcile(state);
    let snapshot = generation.snapshot();

    let page_state = match resolve_page_state(state, snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return render_full_page_error_response(state, page_urls, snapshot, &request, &error);
        }
    };

    let needs_search = normalized_query(&request.query).is_some();
    let needs_entry = page_state.detail.is_some();

    let search_result = if needs_search {
        let offset = match search_offset(&request) {
            Ok(offset) => offset,
            Err(error) => return render_parse_error_response(state, page_urls, &error),
        };

        Some(run_search_with_snapshot(
            state,
            snapshot,
            &page_state,
            offset,
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    let search_error = search_error_message(&search_result);
    let search_results_content =
        results_content_for_search(&search_result, search_error.as_deref());

    let entry =
        match load_entry_data_from_snapshot(state, &page_state, needs_entry.then_some(snapshot)) {
            Ok(entry) => entry,
            Err(error) => {
                return render_full_page_with_entry_error_response(
                    state,
                    page_urls,
                    snapshot,
                    &request,
                    &page_state,
                    &search_result,
                    &error,
                );
            }
        };

    let direct_entry = request.is_direct_entry();
    let results_content = if direct_entry {
        ResultsContent::DirectEntry(&entry)
    } else {
        search_results_content
    };

    let initial_return_metadata = if direct_entry {
        None
    } else {
        initial_return_metadata(state, &page_urls, snapshot, &page_state, results_content)
    };

    let markup = templates::layout::render_full_page(templates::layout::FullPageRender {
        state,
        request: &request,
        page_state: &page_state,
        page_urls: &page_urls,
        served_generation: snapshot,
        results_content,
        entry: &entry,
        initial_return_metadata: initial_return_metadata.as_ref(),
    });

    let mut response = Html(markup.into_string()).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    response
}

fn render_full_page_error_response(
    state: &AppState,
    page_urls: PageUrls,
    snapshot: &ServedGenerationSnapshot,
    request: &PageRequest,
    error: &RequestResolutionError,
) -> Response {
    let page_state = page_state(&state.config, request);
    let message = error.to_string();

    let markup = templates::layout::render_full_page(templates::layout::FullPageRender {
        state,
        request,
        page_state: &page_state,
        page_urls: &page_urls,
        served_generation: snapshot,
        results_content: ResultsContent::Error {
            title: "Page unavailable",
            message: &message,
        },
        entry: &EntryData::Empty,
        initial_return_metadata: None,
    });

    (
        status_for_resolution_error(error),
        Html(markup.into_string()),
    )
        .into_response()
}

fn render_parse_error_response(state: &AppState, page_urls: PageUrls, message: &str) -> Response {
    let snapshot = state.search.snapshot();
    let request = PageRequest::default();
    let page_state = page_state(&state.config, &request);

    let markup = templates::layout::render_full_page(templates::layout::FullPageRender {
        state,
        request: &request,
        page_state: &page_state,
        page_urls: &page_urls,
        served_generation: &snapshot,
        results_content: ResultsContent::Error {
            title: "Bad request",
            message,
        },
        entry: &EntryData::Empty,
        initial_return_metadata: None,
    });

    (StatusCode::BAD_REQUEST, Html(markup.into_string())).into_response()
}

#[derive(Debug)]
enum EntryLoadError {
    NotFound { entry: String },
    IndexUnavailable,
    Lookup(ServiceError),
}

impl EntryLoadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::IndexUnavailable => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Lookup(error) => status_for_service_error(error),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::NotFound { entry } => format!("Entry {entry:?} was not found."),
            Self::IndexUnavailable => "search index was not opened".to_owned(),
            Self::Lookup(error) => format!("{error:#}"),
        }
    }
}

fn render_full_page_with_entry_error_response(
    state: &AppState,
    page_urls: PageUrls,
    snapshot: &ServedGenerationSnapshot,
    request: &PageRequest,
    page_state: &PageState,
    search_result: &Option<ServiceResult<SearchResult>>,
    error: &EntryLoadError,
) -> Response {
    let search_error = search_error_message(search_result);
    let search_results_content = results_content_for_search(search_result, search_error.as_deref());
    let entry = entry_data_for_load_error(error);

    let direct_entry = request.is_direct_entry();
    let results_content = if direct_entry {
        ResultsContent::DirectEntry(&entry)
    } else {
        search_results_content
    };

    let initial_return_metadata = if direct_entry {
        None
    } else {
        initial_return_metadata(state, &page_urls, snapshot, page_state, results_content)
    };

    let markup = templates::layout::render_full_page(templates::layout::FullPageRender {
        state,
        request,
        page_state,
        page_urls: &page_urls,
        served_generation: snapshot,
        results_content,
        entry: &entry,
        initial_return_metadata: initial_return_metadata.as_ref(),
    });

    (error.status(), Html(markup.into_string())).into_response()
}

fn entry_data_for_load_error(error: &EntryLoadError) -> EntryData {
    match error {
        EntryLoadError::NotFound { entry } => EntryData::NotFound {
            entry: entry.clone(),
        },
        EntryLoadError::IndexUnavailable | EntryLoadError::Lookup(_) => {
            EntryData::Error(error.message())
        }
    }
}

fn initial_return_metadata(
    state: &AppState,
    page_urls: &PageUrls,
    snapshot: &ServedGenerationSnapshot,
    page_state: &PageState,
    results_content: ResultsContent<'_>,
) -> Option<InitialReturnMetadata> {
    page_state.detail.as_ref()?;

    let close_url = close_url_for_state(&state.config, page_state);
    let close_uri = public_uri(&close_url).ok()?;
    let close_request = page_request_from_public_uri(&close_uri).ok()?;
    let close_state = resolve_page_state(state, snapshot, &close_request).ok()?;
    let close_path = public_path_and_query(&close_uri);
    let close_page_urls = PageUrls {
        current_url: page_urls.absolute_url(&close_path),
        image_url: page_urls.image_url.clone(),
        origin: page_urls.origin.clone(),
    };

    let metadata = metadata::page_head_metadata(PageHeadMetadataInput {
        state,
        request: &close_request,
        page_state: &close_state,
        page_urls: &close_page_urls,
        snapshot,
        content: results_content.metadata_content(),
        entry: &EntryData::Empty,
    });

    Some(InitialReturnMetadata {
        metadata,
        url: close_path,
    })
}

fn resolve_page_state(
    state: &AppState,
    snapshot: &ServedGenerationSnapshot,
    request: &PageRequest,
) -> std::result::Result<PageState, RequestResolutionError> {
    let page_state = page_state(&state.config, request);
    validate_page_request(state, snapshot, request, &page_state)?;
    Ok(page_state)
}

fn validate_page_request(
    state: &AppState,
    snapshot: &ServedGenerationSnapshot,
    request: &PageRequest,
    page_state: &PageState,
) -> std::result::Result<(), RequestResolutionError> {
    let raw_ref = request.query.ref_id.as_deref();
    let raw_ref_set = request.query.ref_set.as_deref();

    match &page_state.source_filter {
        SourceFilter::All => {
            let all_source_ref = if matches!(&request.route, PublicRoute::Entry { .. }) {
                None
            } else {
                raw_ref
            };

            state
                .search
                .search_scopes_for_snapshot(snapshot, None, all_source_ref, raw_ref_set)?;
        }
        SourceFilter::Named(source) => {
            state.search.search_scopes_for_snapshot(
                snapshot,
                Some(source),
                raw_ref,
                raw_ref_set,
            )?;
        }
    }

    if let PublicRoute::Entry { source, .. } = &request.route {
        let entry_ref_set = if page_state.source_filter == SourceFilter::All {
            page_state.active_ref_set()
        } else {
            raw_ref_set
        };

        state
            .search
            .search_scopes_for_snapshot(snapshot, Some(source), raw_ref, entry_ref_set)?;
    }

    Ok(())
}

fn status_for_service_error(error: &ServiceError) -> StatusCode {
    match error {
        ServiceError::Resolution(error) => status_for_resolution_error(error),
        ServiceError::Search(_) | ServiceError::EntryLookup(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn status_for_resolution_error(error: &RequestResolutionError) -> StatusCode {
    match error {
        RequestResolutionError::RefRequiresSource
        | RequestResolutionError::AmbiguousRefSetSource { .. }
        | RequestResolutionError::InvalidRefForRefSet { .. } => StatusCode::BAD_REQUEST,
        RequestResolutionError::UnknownSource { .. }
        | RequestResolutionError::UnknownRef { .. }
        | RequestResolutionError::UnknownRefSet { .. }
        | RequestResolutionError::UnservedRef { .. }
        | RequestResolutionError::MissingDefaultRef { .. }
        | RequestResolutionError::NoServedSearchScopes => StatusCode::NOT_FOUND,
    }
}

fn sse_error_response(
    public_seo_enabled: bool,
    page_urls: &PageUrls,
    error: &str,
    target_public_url: Option<&str>,
) -> Response {
    let html = templates::results::render_status_error("Request failed", error).into_string();
    let metadata =
        metadata::noindex_head_metadata(public_seo_enabled, page_urls, "Request failed", error);

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();

    if let Some(target_public_url) = target_public_url {
        events.push(Ok(ExecuteScript::new(results_patch_script(
            &html,
            target_public_url,
        ))
        .write_as_axum_sse_event()));
        events.push(Ok(ExecuteScript::new(head_metadata_script(
            &metadata,
            Some(target_public_url),
        ))
        .write_as_axum_sse_event()));
    } else {
        events.push(Ok(PatchElements::new(html).write_as_axum_sse_event()));
    }

    Sse::new(stream::iter(events)).into_response()
}

struct SseEntryErrorContext<'a> {
    state: &'a AppState,
    request: &'a PageRequest,
    page_state: &'a PageState,
    page_urls: &'a PageUrls,
    snapshot: &'a ServedGenerationSnapshot,
    results_html: Option<String>,
    results_content: ResultsContent<'a>,
    target_public_url: &'a str,
}

fn sse_entry_error_response(context: SseEntryErrorContext<'_>, error: &EntryLoadError) -> Response {
    let entry = entry_data_for_load_error(error);
    let direct_entry = context.request.is_direct_entry();
    let empty_entry = EntryData::Empty;
    let modal_entry = if direct_entry { &empty_entry } else { &entry };
    let modal_html =
        templates::modal::render(&context.state.config, context.page_state, modal_entry)
            .into_string();
    let metadata = metadata::page_head_metadata(PageHeadMetadataInput {
        state: context.state,
        request: context.request,
        page_state: context.page_state,
        page_urls: context.page_urls,
        snapshot: context.snapshot,
        content: context.results_content.metadata_content(),
        entry: &entry,
    });

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();

    if direct_entry {
        let results_html =
            templates::results::render_entry(&context.state.config, context.page_state, &entry)
                .into_string();
        events.push(Ok(ExecuteScript::new(results_patch_script(
            &results_html,
            context.target_public_url,
        ))
        .write_as_axum_sse_event()));
    } else if let Some(results_html) = context.results_html {
        events.push(Ok(ExecuteScript::new(results_patch_script(
            &results_html,
            context.target_public_url,
        ))
        .write_as_axum_sse_event()));
    }

    events.push(Ok(ExecuteScript::new(modal_patch_script(
        &modal_html,
        context.target_public_url,
    ))
    .write_as_axum_sse_event()));
    events.push(Ok(ExecuteScript::new(head_metadata_script(
        &metadata,
        Some(context.target_public_url),
    ))
    .write_as_axum_sse_event()));

    Sse::new(stream::iter(events)).into_response()
}

fn json_error_response(status: StatusCode, error: &str) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": error
        })),
    )
        .into_response()
}

fn load_entry_data_from_snapshot(
    state: &AppState,
    page_state: &PageState,
    snapshot: Option<&ServedGenerationSnapshot>,
) -> std::result::Result<EntryData, EntryLoadError> {
    let Some(detail) = page_state.detail.as_ref() else {
        return Ok(EntryData::Empty);
    };
    let Some(snapshot) = snapshot else {
        return Err(EntryLoadError::IndexUnavailable);
    };
    let lookup_ref = detail
        .ref_id
        .as_deref()
        .or(page_state.source_ref.as_deref())
        .or_else(|| {
            page_state.active_ref_set().and_then(|ref_set| {
                state
                    .config
                    .first_ref_for_ref_set_source(ref_set, &detail.source)
            })
        });

    let entry_request = EntryRequest {
        source: detail.source.clone(),
        ref_id: lookup_ref.map(ToOwned::to_owned),
        name: detail.entry.clone(),
    };

    let facts = state
        .search
        .entry_facts_with_snapshot(snapshot, entry_request.clone())
        .map_err(EntryLoadError::Lookup)?;

    match facts.status() {
        EntryFactsStatus::NotFound => Err(EntryLoadError::NotFound {
            entry: detail.entry.clone(),
        }),
        EntryFactsStatus::Unique => {
            let representative = facts
                .representative
                .as_ref()
                .ok_or(EntryLoadError::IndexUnavailable)?;

            Ok(EntryData::Found(AnnotatedEntryDocument::from_facts(
                representative.document.clone(),
                &facts,
            )))
        }
        EntryFactsStatus::Ambiguous => {
            match state
                .search
                .find_entry_with_facts_with_snapshot(snapshot, entry_request, &facts)
            {
                Ok(EntryLookupResult::Ambiguous(documents)) => Ok(EntryData::Ambiguous(
                    documents
                        .into_iter()
                        .map(|document| AnnotatedEntryDocument::from_facts(document, &facts))
                        .collect(),
                )),
                Ok(EntryLookupResult::Found(document)) => Ok(EntryData::Found(
                    AnnotatedEntryDocument::from_facts(*document, &facts),
                )),
                Ok(EntryLookupResult::NotFound) => Err(EntryLoadError::NotFound {
                    entry: detail.entry.clone(),
                }),
                Err(error) => Err(EntryLoadError::Lookup(error)),
            }
        }
    }
}

fn empty_search_result() -> SearchResult {
    SearchResult {
        hits: Vec::new(),
        total: 0,
    }
}

fn run_search_with_snapshot(
    state: &AppState,
    snapshot: &ServedGenerationSnapshot,
    page_state: &PageState,
    offset: usize,
    limit: usize,
) -> ServiceResult<SearchResult> {
    let Some(q) = page_state.q.as_deref() else {
        return Ok(empty_search_result());
    };

    state.search.search_with_snapshot(
        snapshot,
        search_request_for_page_state(page_state, q, offset, limit),
    )
}

fn search_request_for_page_state(
    page_state: &PageState,
    query: &str,
    offset: usize,
    limit: usize,
) -> SearchRequest {
    let (source, ref_id, ref_set) = match &page_state.source_filter {
        SourceFilter::All => (
            None,
            None,
            page_state.active_ref_set().map(ToOwned::to_owned),
        ),
        SourceFilter::Named(source) => (Some(source.clone()), page_state.source_ref.clone(), None),
    };

    SearchRequest {
        query: query.to_owned(),
        source,
        ref_id,
        ref_set,
        offset,
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{DetailState, RefScope};

    fn page_state(q: Option<&str>, has_entry_detail: bool) -> PageState {
        PageState {
            q: q.map(ToOwned::to_owned),
            page: None,
            source_filter: SourceFilter::All,
            ref_scope: RefScope::Ref,
            source_ref: None,
            detail: has_entry_detail.then(|| DetailState {
                source: "fixtures".to_owned(),
                entry: "programs.git.enable".to_owned(),
                ref_id: None,
            }),
        }
    }

    fn navigation(patch_results: bool) -> StateEventsNavigation {
        StateEventsNavigation { patch_results }
    }

    #[test]
    fn state_events_search_needed_when_results_are_patched() {
        let navigation = navigation(true);

        assert!(navigation.needs_search_result(&page_state(Some("git"), true)));
        assert!(navigation.needs_search_result(&page_state(Some("git"), false)));
    }

    #[test]
    fn state_events_search_needed_for_query_metadata_without_entry() {
        assert!(navigation(false).needs_search_result(&page_state(Some("git"), false)));
    }

    #[test]
    fn state_events_search_skipped_for_modal_only_entry_navigation() {
        assert!(!navigation(false).needs_search_result(&page_state(Some("git"), true)));
    }

    #[test]
    fn state_events_search_needed_for_modal_close_metadata() {
        assert!(navigation(false).needs_search_result(&page_state(Some("git"), false)));
    }

    #[test]
    fn state_events_search_skipped_without_query() {
        assert!(!navigation(true).needs_search_result(&page_state(None, false)));
        assert!(!navigation(false).needs_search_result(&page_state(None, true)));
    }
}
