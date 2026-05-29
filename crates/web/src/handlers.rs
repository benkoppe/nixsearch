use std::convert::Infallible;

use anyhow::{Context, Result};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use datastar::prelude::{ExecuteScript, PatchElements};
use futures_util::stream;
use serde::Deserialize;

use nixsearch_index::search::{SearchIndex, SearchOptions, SearchResult, SearchScope};

use crate::AppState;
use crate::DEFAULT_LIMIT;
use crate::request::{
    LinkOrigin, PageQuery, PageRequest, decode_path_value, non_empty, normalized_query,
    page_request_from_public_url, results_context,
};
use crate::scripts::dialog_reconcile_script;
use crate::templates;

#[derive(Debug, Clone, Deserialize)]
pub struct StateQuery {
    url: String,
    previous_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MoreQuery {
    url: String,
    offset: usize,
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

pub async fn root_page(
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    render_full_page_response(
        &state,
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
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    render_full_page_response(
        &state,
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
    Query(query): Query<PageQuery>,
) -> impl IntoResponse {
    let entry = decode_path_value(&entry).unwrap_or(entry);

    render_full_page_response(
        &state,
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
) -> impl IntoResponse {
    let request = match page_request_from_public_url(&query.url) {
        Ok(request) => request,
        Err(error) => {
            let html = templates::results::render_error(&error).into_string();
            let event = PatchElements::new(html).write_as_axum_sse_event();
            let events: Vec<std::result::Result<Event, Infallible>> = vec![Ok(event)];
            return Sse::new(stream::iter(events));
        }
    };

    let patch_results = should_patch_results(query.previous_url.as_deref(), &request);

    let results_html = if patch_results {
        if normalized_query(&request.query).is_none() {
            Some(templates::home::render(&state, &request).into_string())
        } else {
            let search_result = run_search(&state, &request, 0, DEFAULT_LIMIT);

            Some(match &search_result {
                Ok(result) => {
                    templates::results::render(&request, &result.hits, result.total, &state.config)
                        .into_string()
                }
                Err(error) => templates::results::render_error(&format!("{error:#}")).into_string(),
            })
        }
    } else {
        None
    };

    let modal_html = templates::modal::render(&state, &request).into_string();

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

    Sse::new(stream::iter(events))
}

fn should_patch_results(previous_url: Option<&str>, request: &PageRequest) -> bool {
    let Some(previous_url) = previous_url.and_then(non_empty) else {
        return true;
    };

    match page_request_from_public_url(previous_url) {
        Ok(previous_request) => results_context(&previous_request) != results_context(request),
        Err(_) => true,
    }
}

pub async fn more_results(
    State(state): State<AppState>,
    Query(query): Query<MoreQuery>,
) -> impl IntoResponse {
    let request = match page_request_from_public_url(&query.url) {
        Ok(request) => request,
        Err(error) => {
            return Json(serde_json::json!({
                "error": error
            }));
        }
    };

    let search_result = run_search(&state, &request, query.offset, DEFAULT_LIMIT);

    match search_result {
        Ok(result) => {
            let rows_html = templates::results::render_rows_only(
                &request,
                &result.hits,
                &state.config,
                query.offset,
            );
            Json(serde_json::json!({
                "rows": rows_html,
                "total": result.total
            }))
        }
        Err(error) => Json(serde_json::json!({
            "error": format!("{error:#}")
        })),
    }
}

fn render_full_page_response(state: &AppState, request: PageRequest) -> Html<String> {
    let page = request.query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * DEFAULT_LIMIT;
    let search_result = run_search(state, &request, offset, DEFAULT_LIMIT);
    let error_message = search_result.as_ref().err().map(|e| format!("{e:#}"));

    let view = match (&search_result, &error_message) {
        (Ok(result), _) => Ok(result),
        (Err(_), Some(error)) => Err(error.as_str()),
        (Err(_), None) => unreachable!(),
    };

    let markup = templates::layout::render_full_page(state, &request, view);
    Html(markup.into_string())
}

fn run_search(
    state: &AppState,
    request: &PageRequest,
    offset: usize,
    limit: usize,
) -> Result<SearchResult> {
    let Some(q) = normalized_query(&request.query) else {
        return Ok(SearchResult {
            hits: Vec::new(),
            total: 0,
        });
    };

    let index_path = state
        .index_path
        .read()
        .expect("index path lock poisoned")
        .clone();

    let index = SearchIndex::open(&index_path)
        .with_context(|| format!("failed to open current search index {}", index_path))?;

    // source=all overrides path-based source filter
    let effective_source = match request.query.source {
        Some(LinkOrigin::All) => None,
        _ => request.source.as_deref().and_then(non_empty),
    };

    let scopes = state
        .config
        .resolve_search_scopes(
            effective_source,
            if effective_source.is_some() {
                request.query.ref_id.as_deref().and_then(non_empty)
            } else {
                None
            },
            if effective_source.is_none() {
                request.query.ref_set.as_deref().and_then(non_empty)
            } else {
                None
            },
        )
        .context("failed to resolve search scope")?
        .into_iter()
        .map(|scope| SearchScope {
            source: scope.source,
            ref_id: scope.ref_id,
        })
        .collect();

    index
        .search(SearchOptions {
            query: q.to_owned(),
            limit,
            offset,
            scopes,
        })
        .context("search failed")
}
