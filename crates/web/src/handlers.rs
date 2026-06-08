use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::Response;
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use datastar::prelude::{ExecuteScript, PatchElements};
use futures_util::stream;
use serde::Deserialize;

use nixsearch_index::search::{EntryLookupResult, SearchResult};
use nixsearch_service::{
    EntryRequest, RequestResolutionError, SearchRequest, ServedGenerationSnapshot, ServiceError,
    ServiceResult,
};

use crate::AppState;
use crate::DEFAULT_LIMIT;
use crate::origin::{
    PageUrls, page_urls, page_urls_for_public_uri, public_path_and_query, public_uri_for_request,
};
use crate::request::{
    PageQuery, PageRequest, PageState, SourceFilter, decode_path_value, non_empty,
    normalized_query, page_request_from_public_uri, page_state, parse_document_kind, public_uri,
};
use crate::scripts::{datastar_script, dialog_reconcile_script};
use crate::templates::layout::{PageMetadata, ResultsContent, head_metadata_script};
use crate::templates::{self, modal::EntryData};
use crate::urls::close_url_for_state;

#[derive(Debug, Clone, Deserialize)]
pub struct StateQuery {
    url: String,
    previous_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SliceQuery {
    url: String,
    offset: usize,
    limit: Option<usize>,
}

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

pub async fn datastar_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        datastar_script(),
    )
}

pub async fn root_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    render_full_page_response(
        &state,
        page_urls(state.config.as_ref(), &headers, &uri),
        PageRequest {
            source: None,
            entry: None,
            query,
        },
    )
}

pub async fn source_page(
    State(state): State<AppState>,
    Path(source): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    render_full_page_response(
        &state,
        page_urls(state.config.as_ref(), &headers, &uri),
        PageRequest {
            source: Some(source),
            entry: None,
            query,
        },
    )
}

pub async fn entry_page(
    State(state): State<AppState>,
    Path((source, entry)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    let entry = decode_path_value(&entry).unwrap_or(entry);

    render_full_page_response(
        &state,
        page_urls(state.config.as_ref(), &headers, &uri),
        PageRequest {
            source: Some(source),
            entry: Some(entry),
            query,
        },
    )
}

pub async fn state_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StateQuery>,
) -> Response {
    let target_uri = match public_uri_for_request(&state.config, &headers, &query.url) {
        Ok(uri) => uri,
        Err(error) => {
            let page_urls = page_urls(&state.config, &headers, &Uri::from_static("/"));
            return sse_error_response(&page_urls, &error, None);
        }
    };
    let target_public_url = public_path_and_query(&target_uri);
    let page_urls = page_urls_for_public_uri(&state.config, &headers, &target_uri);

    let request = match page_request_from_public_uri(&target_uri) {
        Ok(request) => request,
        Err(error) => {
            return sse_error_response(&page_urls, &error, Some(&target_public_url));
        }
    };

    let snapshot = state.search.snapshot();
    let page_state = match resolve_page_state(&state, &snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return sse_error_response(&page_urls, &error.to_string(), Some(&target_public_url));
        }
    };

    let previous_request = query
        .previous_url
        .as_deref()
        .and_then(non_empty)
        .and_then(|url| public_uri_for_request(&state.config, &headers, url).ok())
        .and_then(|uri| page_request_from_public_uri(&uri).ok());
    let navigation =
        state_events_navigation(&state, &snapshot, previous_request.as_ref(), &page_state);
    let has_entry_detail = page_state.detail.is_some();

    let search_result = if navigation.needs_search_result(&page_state) {
        Some(run_search_with_snapshot(
            &state,
            &snapshot,
            &page_state,
            search_offset(&request),
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    if let Some(Err(ServiceError::Resolution(error))) = &search_result {
        return sse_error_response(&page_urls, &error.to_string(), Some(&target_public_url));
    }

    let search_error = search_error_message(&search_result);
    let results_content = results_content_for_search(&search_result, search_error.as_deref());

    let results_html = if navigation.patch_results {
        Some(match &search_result {
            Some(Ok(result)) => {
                templates::results::render(&page_state, &result.hits, result.total, &state.config)
                    .into_string()
            }
            Some(Err(error)) => {
                templates::results::render_error("Search failed", &format!("{error:#}"))
                    .into_string()
            }
            None => templates::home::render(&state, &request, &page_state, &snapshot).into_string(),
        })
    } else {
        None
    };

    let entry = match load_entry_data_from_snapshot(
        &state,
        &page_state,
        has_entry_detail.then_some(&snapshot),
    ) {
        Ok(entry) => entry,
        Err(error) => {
            return sse_entry_error_response(
                SseEntryErrorContext {
                    state: &state,
                    request: &request,
                    page_state: &page_state,
                    page_urls: &page_urls,
                    snapshot: &snapshot,
                    results_html,
                    results_content,
                    target_public_url: &target_public_url,
                },
                &error,
            );
        }
    };
    let modal_html = templates::modal::render(&state.config, &page_state, &entry).into_string();

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();

    if let Some(results_html) = results_html {
        events.push(Ok(
            PatchElements::new(results_html).write_as_axum_sse_event()
        ));
    }

    events.push(Ok(PatchElements::new(modal_html).write_as_axum_sse_event()));
    events.push(Ok(
        ExecuteScript::new(dialog_reconcile_script()).write_as_axum_sse_event()
    ));

    if navigation.has_complete_metadata(&page_state, &search_result, &entry) {
        let metadata = templates::layout::page_head_metadata(
            &state,
            &request,
            &page_state,
            &page_urls,
            &snapshot,
            results_content,
            &entry,
        );

        events.push(Ok(ExecuteScript::new(head_metadata_script(
            &metadata,
            Some(&target_public_url),
        ))
        .write_as_axum_sse_event()));
    }

    Sse::new(stream::iter(events)).into_response()
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

fn search_offset(request: &PageRequest) -> usize {
    let page = request.query.page.unwrap_or(1).max(1);
    (page - 1) * DEFAULT_LIMIT
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
    headers: HeaderMap,
    Query(query): Query<SliceQuery>,
) -> Response {
    let uri = match public_uri_for_request(&state.config, &headers, &query.url) {
        Ok(uri) => uri,
        Err(error) => {
            return json_error_response(StatusCode::BAD_REQUEST, &error);
        }
    };
    let request = match page_request_from_public_uri(&uri) {
        Ok(request) => request,
        Err(error) => {
            return json_error_response(StatusCode::BAD_REQUEST, &error);
        }
    };

    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, DEFAULT_LIMIT * 4);
    let search_result = run_search(&state, &request, query.offset, limit);

    match search_result {
        Ok(result) => {
            let count = result.hits.len();
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
                "endOffset": query.offset + count,
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
    let snapshot = state.search.snapshot();

    let page_state = match resolve_page_state(state, &snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return render_full_page_error_response(state, page_urls, &snapshot, &request, &error);
        }
    };

    let needs_search = normalized_query(&request.query).is_some();
    let needs_entry = page_state.detail.is_some();

    let search_result = if needs_search {
        Some(run_search_with_snapshot(
            state,
            &snapshot,
            &page_state,
            search_offset(&request),
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    let search_error = search_error_message(&search_result);
    let results_content = results_content_for_search(&search_result, search_error.as_deref());

    let entry =
        match load_entry_data_from_snapshot(state, &page_state, needs_entry.then_some(&snapshot)) {
            Ok(entry) => entry,
            Err(error) => {
                return render_full_page_with_entry_error_response(
                    state,
                    page_urls,
                    &snapshot,
                    &request,
                    &page_state,
                    &search_result,
                    &error,
                );
            }
        };

    let initial_return_metadata =
        initial_return_metadata(state, &page_urls, &snapshot, &page_state, results_content);

    let markup = templates::layout::render_full_page(
        state,
        &request,
        &page_state,
        &page_urls,
        &snapshot,
        results_content,
        &entry,
        initial_return_metadata.as_ref(),
    );

    Html(markup.into_string()).into_response()
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

    let markup = templates::layout::render_full_page(
        state,
        request,
        &page_state,
        &page_urls,
        snapshot,
        ResultsContent::Error {
            title: "Page unavailable",
            message: &message,
        },
        &EntryData::Empty,
        None,
    );

    (
        status_for_resolution_error(error),
        Html(markup.into_string()),
    )
        .into_response()
}

#[derive(Debug)]
enum EntryLoadError {
    NotFound { entry: String },
    InvalidKind(String),
    IndexUnavailable,
    Lookup(ServiceError),
}

impl EntryLoadError {
    fn status(&self) -> StatusCode {
        match self {
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::InvalidKind(_) => StatusCode::BAD_REQUEST,
            Self::IndexUnavailable => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Lookup(error) => status_for_service_error(error),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::NotFound { entry } => format!("Entry {entry:?} was not found."),
            Self::InvalidKind(error) => error.clone(),
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
    let results_content = results_content_for_search(search_result, search_error.as_deref());
    let entry = entry_data_for_load_error(error);

    let initial_return_metadata =
        initial_return_metadata(state, &page_urls, snapshot, page_state, results_content);

    let markup = templates::layout::render_full_page(
        state,
        request,
        page_state,
        &page_urls,
        snapshot,
        results_content,
        &entry,
        initial_return_metadata.as_ref(),
    );

    (error.status(), Html(markup.into_string())).into_response()
}

fn entry_data_for_load_error(error: &EntryLoadError) -> EntryData {
    match error {
        EntryLoadError::NotFound { entry } => EntryData::NotFound {
            entry: entry.clone(),
        },
        EntryLoadError::InvalidKind(_)
        | EntryLoadError::IndexUnavailable
        | EntryLoadError::Lookup(_) => EntryData::Error(error.message()),
    }
}

fn initial_return_metadata(
    state: &AppState,
    page_urls: &PageUrls,
    snapshot: &ServedGenerationSnapshot,
    page_state: &PageState,
    results_content: ResultsContent<'_>,
) -> Option<PageMetadata> {
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

    Some(templates::layout::page_head_metadata(
        state,
        &close_request,
        &close_state,
        &close_page_urls,
        snapshot,
        results_content,
        &EntryData::Empty,
    ))
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
            let all_source_ref = if request.entry.is_some() {
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

    if request.entry.is_some()
        && let Some(source) = request.source.as_deref()
    {
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
    page_urls: &PageUrls,
    error: &str,
    target_public_url: Option<&str>,
) -> Response {
    let html = templates::results::render_error("Request failed", error).into_string();
    let metadata = templates::layout::noindex_head_metadata(page_urls, "Request failed", error);

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();
    events.push(Ok(PatchElements::new(html).write_as_axum_sse_event()));

    if let Some(target_public_url) = target_public_url {
        events.push(Ok(ExecuteScript::new(head_metadata_script(
            &metadata,
            Some(target_public_url),
        ))
        .write_as_axum_sse_event()));
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
    let modal_html =
        templates::modal::render(&context.state.config, context.page_state, &entry).into_string();
    let metadata = templates::layout::page_head_metadata(
        context.state,
        context.request,
        context.page_state,
        context.page_urls,
        context.snapshot,
        context.results_content,
        &entry,
    );

    let mut events: Vec<std::result::Result<Event, Infallible>> = Vec::new();

    if let Some(results_html) = context.results_html {
        events.push(Ok(
            PatchElements::new(results_html).write_as_axum_sse_event()
        ));
    }

    events.push(Ok(PatchElements::new(modal_html).write_as_axum_sse_event()));
    events.push(Ok(
        ExecuteScript::new(dialog_reconcile_script()).write_as_axum_sse_event()
    ));
    events.push(Ok(ExecuteScript::new(head_metadata_script(
        &metadata,
        Some(context.target_public_url),
    ))
    .write_as_axum_sse_event()));

    (error.status(), Sse::new(stream::iter(events))).into_response()
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

    let kind = parse_document_kind(detail.kind.as_deref()).map_err(EntryLoadError::InvalidKind)?;

    match state.search.find_entry_with_snapshot(
        snapshot,
        EntryRequest {
            source: detail.source.clone(),
            ref_id: lookup_ref.map(ToOwned::to_owned),
            name: detail.entry.clone(),
            kind,
        },
    ) {
        Ok(EntryLookupResult::Found(document)) => Ok(EntryData::Found(document)),
        Ok(EntryLookupResult::NotFound) => Err(EntryLoadError::NotFound {
            entry: detail.entry.clone(),
        }),
        Ok(EntryLookupResult::Ambiguous(documents)) => Ok(EntryData::Ambiguous(documents)),
        Err(error) => Err(EntryLoadError::Lookup(error)),
    }
}

fn run_search(
    state: &AppState,
    request: &PageRequest,
    offset: usize,
    limit: usize,
) -> ServiceResult<SearchResult> {
    let snapshot = state.search.snapshot();
    let page_state = resolve_page_state(state, &snapshot, request)?;

    if page_state.q.is_none() {
        return Ok(empty_search_result());
    };

    run_search_with_snapshot(state, &snapshot, &page_state, offset, limit)
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
                kind: None,
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
