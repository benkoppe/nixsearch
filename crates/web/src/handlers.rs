use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::Response;
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use datastar::prelude::{ExecuteScript, PatchElements};
use futures_util::stream;
use serde::Deserialize;
use url::Url;

use nixsearch_index::search::{EntryLookupResult, SearchResult};
use nixsearch_service::{
    EntryRequest, RequestResolutionError, SearchRequest, ServedGenerationSnapshot, ServiceError,
    ServiceResult,
};

use crate::AppState;
use crate::DEFAULT_LIMIT;
use crate::request::{
    PageQuery, PageRequest, PageState, SourceFilter, decode_path_value, non_empty,
    normalized_query, page_request_from_public_url, page_state, parse_document_kind,
};
use crate::scripts::{datastar_script, dialog_reconcile_script};
use crate::templates::layout::ResultsContent;
use crate::templates::{self, layout::PageUrls, modal::EntryData};

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
        page_urls(&state, &headers, &uri),
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
        page_urls(&state, &headers, &uri),
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
        page_urls(&state, &headers, &uri),
        PageRequest {
            source: Some(source),
            entry: Some(entry),
            query,
        },
    )
}

pub async fn state_events(
    State(state): State<AppState>,
    Query(query): Query<StateQuery>,
) -> Response {
    let request = match page_request_from_public_url(&query.url) {
        Ok(request) => request,
        Err(error) => {
            return sse_error_response(&error);
        }
    };

    let snapshot = state.search.snapshot();
    let page_state = match resolve_page_state(&state, &snapshot, &request) {
        Ok(page_state) => page_state,
        Err(error) => {
            return sse_error_response(&error.to_string());
        }
    };

    let patch_results =
        should_patch_results(&state, &snapshot, query.previous_url.as_deref(), &request);
    let needs_entry = page_state.detail.is_some();

    let results_html = if patch_results {
        if page_state.q.is_none() {
            Some(templates::home::render(&state, &request, &page_state, &snapshot).into_string())
        } else {
            let search_result =
                run_search_with_snapshot(&state, &snapshot, &page_state, 0, DEFAULT_LIMIT);

            Some(match &search_result {
                Ok(result) => templates::results::render(
                    &page_state,
                    &result.hits,
                    result.total,
                    &state.config,
                )
                .into_string(),
                Err(ServiceError::Resolution(error)) => {
                    return sse_error_response(&error.to_string());
                }
                Err(error) => {
                    templates::results::render_error("Search failed", &format!("{error:#}"))
                        .into_string()
                }
            })
        }
    } else {
        None
    };

    let entry = match load_entry_data_from_snapshot(
        &state,
        &page_state,
        needs_entry.then_some(&snapshot),
    ) {
        Ok(entry) => entry,
        Err(error) => {
            return sse_entry_error_response(&state, &page_state, results_html, &error);
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

    Sse::new(stream::iter(events)).into_response()
}

fn should_patch_results(
    state: &AppState,
    snapshot: &ServedGenerationSnapshot,
    previous_url: Option<&str>,
    request: &PageRequest,
) -> bool {
    let Some(previous_url) = previous_url.and_then(non_empty) else {
        return true;
    };

    match page_request_from_public_url(previous_url) {
        Ok(previous_request) => {
            let previous_state = match resolve_page_state(state, snapshot, &previous_request) {
                Ok(previous_state) => previous_state,
                Err(_) => return true,
            };
            let next_state = match resolve_page_state(state, snapshot, request) {
                Ok(next_state) => next_state,
                Err(_) => return true,
            };

            previous_state.q != next_state.q
                || previous_state.source_filter != next_state.source_filter
                || previous_state.source_ref != next_state.source_ref
                || previous_state.active_ref_set() != next_state.active_ref_set()
        }
        Err(_) => true,
    }
}

pub async fn results_slice(
    State(state): State<AppState>,
    Query(query): Query<SliceQuery>,
) -> Response {
    let request = match page_request_from_public_url(&query.url) {
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

    let page = request.query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * DEFAULT_LIMIT;
    let needs_search = normalized_query(&request.query).is_some();
    let needs_entry = page_state.detail.is_some();

    let search_result = if needs_search {
        Some(run_search_with_snapshot(
            state,
            &snapshot,
            &page_state,
            offset,
            DEFAULT_LIMIT,
        ))
    } else {
        None
    };

    let search_error = match &search_result {
        Some(Err(error)) => Some(format!("{error:#}")),
        _ => None,
    };

    let results_content = match &search_result {
        Some(Ok(result)) => ResultsContent::SearchResults(result),
        Some(Err(_)) => ResultsContent::Error {
            title: "Search failed",
            message: search_error.as_deref().unwrap_or("search failed"),
        },
        None => ResultsContent::Home,
    };

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

    let markup = templates::layout::render_full_page(
        state,
        &request,
        &page_state,
        &page_urls,
        &snapshot,
        results_content,
        &entry,
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
    let search_error = match search_result {
        Some(Err(error)) => Some(format!("{error:#}")),
        _ => None,
    };
    let results_content = match search_result {
        Some(Ok(result)) => ResultsContent::SearchResults(result),
        Some(Err(_)) => ResultsContent::Error {
            title: "Search failed",
            message: search_error.as_deref().unwrap_or("search failed"),
        },
        None => ResultsContent::Home,
    };
    let entry = entry_data_for_load_error(error);

    let markup = templates::layout::render_full_page(
        state,
        request,
        page_state,
        &page_urls,
        snapshot,
        results_content,
        &entry,
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

fn sse_error_response(error: &str) -> Response {
    let html = templates::results::render_error("Request failed", error).into_string();
    let event = PatchElements::new(html).write_as_axum_sse_event();
    let events: Vec<std::result::Result<Event, Infallible>> = vec![Ok(event)];

    Sse::new(stream::iter(events)).into_response()
}

fn sse_entry_error_response(
    state: &AppState,
    page_state: &PageState,
    results_html: Option<String>,
    error: &EntryLoadError,
) -> Response {
    let entry = entry_data_for_load_error(error);
    let modal_html = templates::modal::render(&state.config, page_state, &entry).into_string();

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

fn page_urls(state: &AppState, headers: &HeaderMap, uri: &Uri) -> PageUrls {
    let path = uri.path();
    let query = uri.query();

    if let Some(public_url) = state.config.server.public_url.as_deref()
        && let Ok(base) = Url::parse(public_url)
    {
        return page_urls_from_base(base, path, query);
    }

    page_urls_from_headers(headers, path, query)
}

fn page_urls_from_base(mut base: Url, path: &str, query: Option<&str>) -> PageUrls {
    let base_path = base.path().trim_end_matches('/').to_owned();
    let path = if path == "/" {
        if base_path.is_empty() {
            "/".to_owned()
        } else {
            format!("{base_path}/")
        }
    } else {
        format!("{base_path}{path}")
    };

    base.set_path(&path);
    base.set_query(query);
    base.set_fragment(None);

    let current_url = base.to_string();

    base.set_path(&format!("{base_path}/apple-touch-icon.png"));
    base.set_query(None);

    PageUrls {
        current_url,
        image_url: base.to_string(),
    }
}

fn page_urls_from_headers(headers: &HeaderMap, path: &str, query: Option<&str>) -> PageUrls {
    let forwarded = forwarded_proto_host(headers);
    let proto = forwarded
        .as_ref()
        .and_then(|(proto, _)| proto.as_deref())
        .or_else(|| first_header_value(headers, "x-forwarded-proto"))
        .unwrap_or("http");
    let host = forwarded
        .as_ref()
        .and_then(|(_, host)| host.as_deref())
        .or_else(|| first_header_value(headers, header::HOST.as_str()))
        .unwrap_or("localhost");
    let origin = format!("{proto}://{host}");
    let path_and_query = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };

    PageUrls {
        current_url: format!("{origin}{path_and_query}"),
        image_url: format!("{origin}/apple-touch-icon.png"),
    }
}

fn forwarded_proto_host(headers: &HeaderMap) -> Option<(Option<String>, Option<String>)> {
    let header = first_header_value(headers, "forwarded")?;
    let first = header.split(',').next().unwrap_or(header);
    let mut proto = None;
    let mut host = None;

    for part in first.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "proto" => proto = Some(value.to_owned()),
            "host" => host = Some(value.to_owned()),
            _ => {}
        }
    }

    (proto.is_some() || host.is_some()).then_some((proto, host))
}

fn first_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .and_then(non_empty)
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
    use axum::http::{HeaderMap, HeaderValue};
    use url::Url;

    use super::{page_urls_from_base, page_urls_from_headers};

    #[test]
    fn page_urls_use_public_url_origin() {
        let urls = page_urls_from_base(
            Url::parse("https://search.example.com/").unwrap(),
            "/nixpkgs/git",
            Some("q=git"),
        );

        assert_eq!(
            urls.current_url,
            "https://search.example.com/nixpkgs/git?q=git"
        );
        assert_eq!(
            urls.image_url,
            "https://search.example.com/apple-touch-icon.png"
        );
    }

    #[test]
    fn page_urls_fall_back_to_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            HeaderValue::from_static("for=127.0.0.1;proto=https;host=nixsearch.example.com"),
        );
        let urls = page_urls_from_headers(&headers, "/", None);

        assert_eq!(urls.current_url, "https://nixsearch.example.com/");
        assert_eq!(
            urls.image_url,
            "https://nixsearch.example.com/apple-touch-icon.png"
        );
    }

    #[test]
    fn page_urls_fall_back_to_host_and_proto_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:3000"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let urls = page_urls_from_headers(&headers, "/nixpkgs", Some("q=git"));

        assert_eq!(urls.current_url, "https://localhost:3000/nixpkgs?q=git");
        assert_eq!(
            urls.image_url,
            "https://localhost:3000/apple-touch-icon.png"
        );
    }
}
