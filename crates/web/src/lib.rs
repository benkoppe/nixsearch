use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use axum::routing::get;
use datastar::prelude::{ExecuteScript, PatchElements};
use futures_util::stream;
use html_escape::{encode_double_quoted_attribute, encode_text};
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use nix_search_config::AppConfig;
use nix_search_core::{
    CommonDoc, DocumentKind, License, Maintainer, SearchDocument, SourceLinkConfig,
    SourceLinkResolver,
};
use nix_search_index::{
    EntryLookup, EntryLookupResult, IndexStore, SearchHit, SearchIndex, SearchOptions, SearchScope,
};

const DEFAULT_LIMIT: usize = 20;
const RECONCILE_EVENTS_URL: &str = "/-/state/events";
const EMPTY_MODAL_HTML: &str = r#"<div id="entry-modal-container"></div>"#;

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    index_path: Arc<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PageQuery {
    q: Option<String>,

    #[serde(rename = "ref")]
    ref_id: Option<String>,

    kind: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PageRequest {
    source: Option<String>,
    entry: Option<String>,
    query: PageQuery,
}

#[derive(Debug, Clone, Deserialize)]
struct StateQuery {
    url: String,
}

pub async fn serve(config: AppConfig) -> Result<()> {
    let index_store = IndexStore::new(&config.data.index_dir);
    let index_path = index_store.current_path().with_context(|| {
        format!(
            "failed to locate current index in {}; run `nix-search update` first",
            config.data.index_dir.display()
        )
    })?;

    let addr: SocketAddr =
        config.server.listen.parse().with_context(|| {
            format!("failed to parse listen address {:?}", config.server.listen)
        })?;

    let state = AppState {
        config: Arc::new(config),
        index_path: Arc::new(index_path),
    };

    let app = Router::new()
        .route("/-/health", get(health))
        .route(RECONCILE_EVENTS_URL, get(state_events))
        .route("/", get(root_page))
        .route("/{source}", get(source_page))
        .route("/{source}/{*entry}", get(entry_page))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    tracing::info!("serving nix-search web UI at http://{addr}");

    axum::serve(listener, app)
        .await
        .context("web server failed")?;

    Ok(())
}

async fn health() -> &'static str {
    "ok"
}

async fn root_page(
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

async fn source_page(
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

async fn entry_page(
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

fn render_full_page_response(state: &AppState, request: PageRequest) -> Html<String> {
    let search_result = run_page_search(state, &request);
    let error_message = search_result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));

    let view = match (&search_result, &error_message) {
        (Ok(hits), _) => Ok(hits.as_slice()),
        (Err(_), Some(error)) => Err(error.as_str()),
        (Err(_), None) => unreachable!(),
    };

    Html(render_full_page(state, &request, view))
}

async fn state_events(
    State(state): State<AppState>,
    Query(query): Query<StateQuery>,
) -> impl IntoResponse {
    let request = match page_request_from_public_url(&query.url) {
        Ok(request) => request,
        Err(error) => {
            let html = render_error_results_html(&error);
            let event = PatchElements::new(html).write_as_axum_sse_event();
            let events: Vec<std::result::Result<Event, Infallible>> = vec![Ok(event)];

            return Sse::new(stream::iter(events));
        }
    };

    let search_result = run_page_search(&state, &request);

    let results_html = match &search_result {
        Ok(hits) => render_results_html(&request, hits, &state.config),
        Err(error) => render_error_results_html(&format!("{error:#}")),
    };

    let modal_html = render_modal_html(&state, &request);

    let events: Vec<std::result::Result<Event, Infallible>> = vec![
        Ok(PatchElements::new(results_html).write_as_axum_sse_event()),
        Ok(PatchElements::new(modal_html).write_as_axum_sse_event()),
        Ok(ExecuteScript::new(dialog_reconcile_script()).write_as_axum_sse_event()),
    ];

    Sse::new(stream::iter(events))
}

fn run_page_search(state: &AppState, request: &PageRequest) -> Result<Vec<SearchHit>> {
    let Some(q) = normalized_query(&request.query) else {
        return Ok(Vec::new());
    };

    let index = SearchIndex::open(&*state.index_path).with_context(|| {
        format!(
            "failed to open current search index {}",
            state.index_path.display()
        )
    })?;

    let scopes = state
        .config
        .resolve_search_scopes(
            request.source.as_deref().and_then(non_empty),
            request.query.ref_id.as_deref().and_then(non_empty),
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
            limit: DEFAULT_LIMIT,
            scopes,
        })
        .context("search failed")
}

fn normalized_query(query: &PageQuery) -> Option<&str> {
    query.q.as_deref().and_then(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();

    if value.is_empty() { None } else { Some(value) }
}

fn render_full_page(
    state: &AppState,
    request: &PageRequest,
    search_result: std::result::Result<&[SearchHit], &str>,
) -> String {
    let q = request.query.q.as_deref().unwrap_or("");
    let ref_id = request.query.ref_id.as_deref().unwrap_or("");

    let results_html = match search_result {
        Ok(hits) if normalized_query(&request.query).is_some() => {
            render_results_html(request, hits, &state.config)
        }
        Ok(_) => render_empty_results_html(),
        Err(error) => render_error_results_html(error),
    };

    let modal_html = render_modal_html(state, request);

    let form_action = request
        .source
        .as_deref()
        .map(source_path)
        .unwrap_or_else(|| "/".to_owned());

    format!(
        r#"<!doctype html>
   <html lang="en">
   <head>
     <meta charset="utf-8">
     <meta name="viewport" content="width=device-width, initial-scale=1">
     <title>Nix Search</title>
     <script type="module"
 src="https://cdn.jsdelivr.net/gh/starfederation/datastar@main/bundles/datastar.js"></script>
     <style>{css}</style>
     <noscript>
       <style>
         dialog#entry-modal {{
           display: block;
         }}
       </style>
     </noscript>
   </head>
   <body
     data-on:nix-search-reconcile__window="@get('{reconcile_url}?url=' + encodeURIComponent(location.pathname +
 location.search))"
   >
     <main>
       <h1>Nix Search</h1>
       <p class="subtitle">Search indexed Nix packages and options.</p>

       <form class="search" action="{form_action}" method="get">
         <label>
           Query
           <input
             type="search"
             name="q"
             value="{q_attr}"
             placeholder="git, programs.git.enable, services.nginx..."
             autocomplete="off"
             autofocus
             data-nix-search-input="q"
           >
         </label>

         <div class="filters">
           <label>
             Ref
             <input
               name="ref"
               value="{ref_attr}"
               placeholder="optional"
               data-nix-search-input="ref"
             >
           </label>
         </div>

         <button type="submit">Search</button>
       </form>

       {results_html}
       {modal_html}
     </main>

     <script>{nav_script}</script>
   </body>
   </html>"#,
        css = page_css(),
        reconcile_url = RECONCILE_EVENTS_URL,
        form_action = encode_double_quoted_attribute(&form_action),
        q_attr = encode_double_quoted_attribute(q),
        ref_attr = encode_double_quoted_attribute(ref_id),
        nav_script = navigation_script(),
    )
}

fn render_results_html(request: &PageRequest, hits: &[SearchHit], config: &AppConfig) -> String {
    let Some(q) = normalized_query(&request.query) else {
        return render_empty_results_html();
    };

    if hits.is_empty() {
        return format!(
            r#"<div id="results" class="status">No results for <strong>{}</strong>.</div>"#,
            encode_text(q)
        );
    }

    let mut html = format!(
        r#"<div id="results" class="results" aria-live="polite"><div class="status">{} result{} for
 <strong>{}</strong>.</div>"#,
        hits.len(),
        if hits.len() == 1 { "" } else { "s" },
        encode_text(q),
    );

    for hit in hits {
        html.push_str(&render_hit(request, hit, config));
    }

    html.push_str("</div>");
    html
}

fn render_empty_results_html() -> String {
    r#"<div id="results" class="status">Enter a search query.</div>"#.to_owned()
}

fn render_error_results_html(error: &str) -> String {
    format!(
        r#"<div id="results" class="error"><strong>Search failed:</strong> {}</div>"#,
        encode_text(error)
    )
}

fn render_hit(request: &PageRequest, hit: &SearchHit, config: &AppConfig) -> String {
    let common = hit.document.common();
    let summary = summary_for_document(&hit.document);
    let source_link = first_source_link(&hit.document, config);

    let entry_href = entry_url_for(
        &common.source,
        &common.name,
        Some(common.kind.as_str()),
        &PageQuery {
            q: request.query.q.clone(),
            ref_id: Some(common.ref_id.clone()),
            kind: Some(common.kind.as_str().to_owned()),
        },
    );

    let mut html = format!(
        r#"<article class="result">
     <h2>
       <a href="{href}">
         <code>{name}</code>
       </a>
     </h2>
     <div class="meta">{kind} · {source}/{ref_id} · score {score:.3}</div>"#,
        href = encode_double_quoted_attribute(&entry_href),
        name = encode_text(&common.name),
        kind = encode_text(common.kind.as_str()),
        source = encode_text(&common.source),
        ref_id = encode_text(&common.ref_id),
        score = hit.score,
    );

    if let Some(summary) = summary {
        html.push_str(&format!(
            r#"
     <p class="summary">{}</p>"#,
            encode_text(summary)
        ));
    }

    if let Some(source_link) = source_link {
        html.push_str(&format!(
            r#"
     <a href="{href}" rel="noreferrer">Source</a>"#,
            href = encode_double_quoted_attribute(&source_link),
        ));
    }

    html.push_str("\n</article>");
    html
}

fn render_modal_html(state: &AppState, request: &PageRequest) -> String {
    let Some(source) = request.source.as_deref() else {
        return EMPTY_MODAL_HTML.to_owned();
    };

    let Some(entry) = request.entry.as_deref() else {
        return EMPTY_MODAL_HTML.to_owned();
    };

    let ref_id = match resolve_entry_ref(&state.config, source, request.query.ref_id.as_deref()) {
        Ok(ref_id) => ref_id,
        Err(error) => return render_entry_error_modal(request, &format!("{error:#}")),
    };

    let kind = match parse_document_kind(request.query.kind.as_deref()) {
        Ok(kind) => kind,
        Err(error) => return render_entry_error_modal(request, &error),
    };

    let index = match SearchIndex::open(&*state.index_path) {
        Ok(index) => index,
        Err(error) => return render_entry_error_modal(request, &format!("{error:#}")),
    };

    let lookup = EntryLookup {
        source: source.to_owned(),
        ref_id,
        name: entry.to_owned(),
        kind,
    };

    match index.find_entry(lookup) {
        Ok(EntryLookupResult::Found(document)) => {
            render_entry_modal(request, &document, &state.config)
        }
        Ok(EntryLookupResult::NotFound) => render_entry_error_modal(request, "Entry not found."),
        Ok(EntryLookupResult::Ambiguous(documents)) => {
            render_ambiguous_entry_modal(request, &documents)
        }
        Err(error) => render_entry_error_modal(request, &format!("{error:#}")),
    }
}

fn render_entry_modal(
    request: &PageRequest,
    document: &SearchDocument,
    config: &AppConfig,
) -> String {
    let common = document.common();
    let close_href = close_url_for(request);

    format!(
        r#"<div id="entry-modal-container">
     <dialog id="entry-modal">
       <article class="entry">
         <header>
           <div>
             <h2><code>{name}</code></h2>
             <div class="meta">{kind} · {source}/{ref_id}{revision}</div>
           </div>
           <a href="{close_href}" data-role="entry-close">Close</a>
         </header>
         {detail}
       </article>
     </dialog>
   </div>"#,
        name = encode_text(&common.name),
        kind = encode_text(common.kind.as_str()),
        source = encode_text(&common.source),
        ref_id = encode_text(&common.ref_id),
        revision = common
            .revision
            .as_deref()
            .map(|revision| format!(" · {}", encode_text(revision)))
            .unwrap_or_default(),
        close_href = encode_double_quoted_attribute(&close_href),
        detail = render_entry_detail(document, config),
    )
}

fn render_entry_error_modal(request: &PageRequest, message: &str) -> String {
    let close_href = close_url_for(request);

    format!(
        r#"<div id="entry-modal-container">
     <dialog id="entry-modal">
       <article class="entry">
         <header>
           <h2>Entry</h2>
           <a href="{close_href}" data-role="entry-close">Close</a>
         </header>
         <div class="error">{message}</div>
       </article>
     </dialog>
   </div>"#,
        close_href = encode_double_quoted_attribute(&close_href),
        message = encode_text(message),
    )
}

fn render_ambiguous_entry_modal(request: &PageRequest, documents: &[SearchDocument]) -> String {
    let close_href = close_url_for(request);

    let mut list = String::new();

    for document in documents {
        let common = document.common();
        let href = entry_url_for(
            &common.source,
            &common.name,
            Some(common.kind.as_str()),
            &PageQuery {
                q: request.query.q.clone(),
                ref_id: Some(common.ref_id.clone()),
                kind: Some(common.kind.as_str().to_owned()),
            },
        );

        list.push_str(&format!(
            r#"<li><a href="{href}">{kind} · {source}/{ref_id}</a></li>"#,
            href = encode_double_quoted_attribute(&href),
            kind = encode_text(common.kind.as_str()),
            source = encode_text(&common.source),
            ref_id = encode_text(&common.ref_id),
        ));
    }

    format!(
        r#"<div id="entry-modal-container">
     <dialog id="entry-modal">
       <article class="entry">
         <header>
           <h2>Multiple entries found</h2>
           <a href="{close_href}" data-role="entry-close">Close</a>
         </header>
         <p>Multiple entries have this name. Choose one:</p>
         <ul>{list}</ul>
       </article>
     </dialog>
   </div>"#,
        close_href = encode_double_quoted_attribute(&close_href),
    )
}

fn render_entry_detail(document: &SearchDocument, config: &AppConfig) -> String {
    match document {
        SearchDocument::Option(option) => {
            let mut html = String::new();

            if let Some(description) = &option.description {
                html.push_str(&section(
                    "Description",
                    &format!("<p>{}</p>", encode_text(description)),
                ));
            }

            if let Some(option_type) = &option.option_type {
                html.push_str(&field("Type", option_type));
            }

            if let Some(default) = &option.default {
                html.push_str(&json_section("Default", default));
            }

            if let Some(example) = &option.example {
                html.push_str(&json_section("Example", example));
            }

            if let Some(related_packages) = &option.related_packages {
                html.push_str(&section(
                    "Related packages",
                    &format!("<p>{}</p>", encode_text(related_packages)),
                ));
            }

            let flags = [
                ("Read only", option.read_only),
                ("Internal", option.internal),
                ("Visible", option.visible),
            ]
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| format!("{name}: {value}")))
            .collect::<Vec<_>>();

            if !flags.is_empty() {
                html.push_str(&section(
                    "Flags",
                    &format!(
                        "<ul>{}</ul>",
                        flags
                            .iter()
                            .map(|flag| format!("<li>{}</li>", encode_text(flag)))
                            .collect::<String>()
                    ),
                ));
            }

            if !option.declarations.is_empty() {
                let resolver =
                    source_link_config_for_document(config, &option.common).map(|config| {
                        SourceLinkResolver::new(config, option.common.revision.as_deref())
                    });

                let mut items = String::new();

                for declaration in &option.declarations {
                    let label = encode_text(&declaration.name);

                    if let Some(url) = resolver
                        .as_ref()
                        .and_then(|resolver| resolver.resolve_declaration(declaration))
                    {
                        items.push_str(&format!(
                            r#"<li><a href="{href}" rel="noreferrer">{label}</a></li>"#,
                            href = encode_double_quoted_attribute(&url),
                        ));
                    } else {
                        items.push_str(&format!("<li>{label}</li>"));
                    }
                }

                html.push_str(&section("Declarations", &format!("<ul>{items}</ul>")));
            }

            html
        }

        SearchDocument::Package(package) => {
            let mut html = String::new();

            let mut summary = Vec::new();

            if let Some(pname) = &package.pname {
                summary.push(format!("pname: {}", encode_text(pname)));
            }

            if let Some(version) = &package.version {
                summary.push(format!("version: {}", encode_text(version)));
            }

            if let Some(main_program) = &package.main_program {
                summary.push(format!("main program: {}", encode_text(main_program)));
            }

            if let Some(broken) = package.broken {
                summary.push(format!("broken: {broken}"));
            }

            if !summary.is_empty() {
                html.push_str(&section(
                    "Package",
                    &format!(
                        "<ul>{}</ul>",
                        summary
                            .iter()
                            .map(|item| format!("<li>{item}</li>"))
                            .collect::<String>()
                    ),
                ));
            }

            if let Some(description) = &package.description {
                html.push_str(&section(
                    "Description",
                    &format!("<p>{}</p>", encode_text(description)),
                ));
            }

            if let Some(long_description) = &package.long_description {
                html.push_str(&section(
                    "Long description",
                    &format!("<p>{}</p>", encode_text(long_description)),
                ));
            }

            if !package.homepages.is_empty() {
                html.push_str(&string_links_section("Homepages", &package.homepages));
            }

            if !package.platforms.is_empty() {
                html.push_str(&strings_section("Platforms", &package.platforms));
            }

            if !package.licenses.is_empty() {
                html.push_str(&licenses_section(&package.licenses));
            }

            if !package.maintainers.is_empty() {
                html.push_str(&maintainers_section(&package.maintainers));
            }

            if let Some(position) = &package.position {
                let resolver =
                    source_link_config_for_document(config, &package.common).map(|config| {
                        SourceLinkResolver::new(config, package.common.revision.as_deref())
                    });

                if let Some(url) = resolver
                    .as_ref()
                    .and_then(|resolver| resolver.resolve_package_position(position))
                {
                    html.push_str(&section(
                        "Source",
                        &format!(
                            r#"<p><a href="{href}" rel="noreferrer">{label}</a></p>"#,
                            href = encode_double_quoted_attribute(&url),
                            label = encode_text(position),
                        ),
                    ));
                } else {
                    html.push_str(&field("Source", position));
                }
            }

            html
        }
    }
}

fn section(title: &str, body: &str) -> String {
    format!(
        r#"<section class="entry-section"><h3>{}</h3>{}</section>"#,
        encode_text(title),
        body
    )
}

fn field(name: &str, value: &str) -> String {
    section(name, &format!("<p>{}</p>", encode_text(value)))
}

fn json_section(name: &str, value: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());

    section(name, &format!("<pre>{}</pre>", encode_text(&pretty)))
}

fn strings_section(name: &str, values: &[String]) -> String {
    section(
        name,
        &format!(
            "<ul>{}</ul>",
            values
                .iter()
                .map(|value| format!("<li>{}</li>", encode_text(value)))
                .collect::<String>()
        ),
    )
}

fn string_links_section(name: &str, values: &[String]) -> String {
    section(
        name,
        &format!(
            "<ul>{}</ul>",
            values
                .iter()
                .map(|value| {
                    if value.starts_with("http://") || value.starts_with("https://") {
                        format!(
                            r#"<li><a href="{href}" rel="noreferrer">{label}</a></li>"#,
                            href = encode_double_quoted_attribute(value),
                            label = encode_text(value),
                        )
                    } else {
                        format!("<li>{}</li>", encode_text(value))
                    }
                })
                .collect::<String>()
        ),
    )
}

fn licenses_section(licenses: &[License]) -> String {
    section(
        "Licenses",
        &format!(
            "<ul>{}</ul>",
            licenses
                .iter()
                .map(|license| {
                    let label = license
                        .spdx_id
                        .as_deref()
                        .or(license.name.as_deref())
                        .or(license.full_name.as_deref())
                        .unwrap_or("unknown");

                    if let Some(url) = &license.url {
                        format!(
                            r#"<li><a href="{href}" rel="noreferrer">{label}</a></li>"#,
                            href = encode_double_quoted_attribute(url),
                            label = encode_text(label),
                        )
                    } else {
                        format!("<li>{}</li>", encode_text(label))
                    }
                })
                .collect::<String>()
        ),
    )
}

fn maintainers_section(maintainers: &[Maintainer]) -> String {
    section(
        "Maintainers",
        &format!(
            "<ul>{}</ul>",
            maintainers
                .iter()
                .map(|maintainer| {
                    let label = maintainer
                        .name
                        .as_deref()
                        .or(maintainer.github.as_deref())
                        .or(maintainer.email.as_deref())
                        .unwrap_or("unknown");

                    format!("<li>{}</li>", encode_text(label))
                })
                .collect::<String>()
        ),
    )
}

fn summary_for_document(document: &SearchDocument) -> Option<&str> {
    match document {
        SearchDocument::Option(option) => {
            option.description.as_deref().and_then(first_non_empty_line)
        }
        SearchDocument::Package(package) => package
            .description
            .as_deref()
            .and_then(first_non_empty_line),
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn first_source_link(document: &SearchDocument, config: &AppConfig) -> Option<String> {
    let common = document.common();
    let source_links = source_link_config_for_document(config, common)?;
    let resolver = SourceLinkResolver::new(source_links, common.revision.as_deref());

    match document {
        SearchDocument::Option(option) => option
            .declarations
            .iter()
            .find_map(|declaration| resolver.resolve_declaration(declaration)),
        SearchDocument::Package(package) => package
            .position
            .as_deref()
            .and_then(|position| resolver.resolve_package_position(position)),
    }
}

fn source_link_config_for_document<'a>(
    config: &'a AppConfig,
    common: &CommonDoc,
) -> Option<&'a SourceLinkConfig> {
    let source = config.sources.get(&common.source)?;

    let ref_config = source
        .refs
        .iter()
        .find(|ref_config| ref_config.id == common.ref_id)?;

    ref_config.source_links.as_ref()
}

fn resolve_entry_ref(config: &AppConfig, source_id: &str, ref_id: Option<&str>) -> Result<String> {
    if let Some(ref_id) = ref_id.and_then(non_empty) {
        return Ok(ref_id.to_owned());
    }

    let source = config
        .sources
        .get(source_id)
        .with_context(|| format!("unknown source {source_id:?}"))?;

    source
        .default_ref
        .clone()
        .with_context(|| format!("source {source_id:?} has no default ref"))
}

fn parse_document_kind(value: Option<&str>) -> std::result::Result<Option<DocumentKind>, String> {
    match value.and_then(non_empty) {
        None => Ok(None),
        Some("option") => Ok(Some(DocumentKind::Option)),
        Some("package") => Ok(Some(DocumentKind::Package)),
        Some("app") => Ok(Some(DocumentKind::App)),
        Some("service") => Ok(Some(DocumentKind::Service)),
        Some(other) => Err(format!("unknown entry kind {other:?}")),
    }
}

fn source_path(source: &str) -> String {
    format!("/{}", encode_path(source))
}

fn search_url_for(source: Option<&str>, query: &PageQuery) -> String {
    let path = source.map(source_path).unwrap_or_else(|| "/".to_owned());

    let qs = query_string([("q", query.q.as_deref()), ("ref", query.ref_id.as_deref())]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

fn entry_url_for(source: &str, entry: &str, kind: Option<&str>, query: &PageQuery) -> String {
    let path = format!("{}/{}", source_path(source), encode_path(entry));

    let qs = query_string([
        ("q", query.q.as_deref()),
        ("ref", query.ref_id.as_deref()),
        ("kind", kind.or(query.kind.as_deref())),
    ]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

fn close_url_for(request: &PageRequest) -> String {
    search_url_for(request.source.as_deref(), &request.query)
}

fn query_string<const N: usize>(pairs: [(&str, Option<&str>); N]) -> String {
    pairs
        .into_iter()
        .filter_map(|(key, value)| {
            let value = value.and_then(non_empty)?;

            Some(format!("{}={}", encode_query(key), encode_query(value)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_path(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn decode_path_value(value: &str) -> Option<String> {
    urlencoding::decode(value)
        .ok()
        .map(|value| value.into_owned())
}

fn page_request_from_public_url(raw_url: &str) -> std::result::Result<PageRequest, String> {
    let (raw_path, raw_query) = raw_url
        .split_once('?')
        .map_or((raw_url, ""), |(path, query)| (path, query));

    let path_parts = raw_path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    let source = path_parts
        .first()
        .map(|value| decode_path_value(value).unwrap_or_else(|| (*value).to_owned()));

    let entry = if path_parts.len() >= 2 {
        let raw_entry = path_parts[1..].join("/");
        Some(decode_path_value(&raw_entry).unwrap_or(raw_entry))
    } else {
        None
    };

    let mut q = None;
    let mut ref_id = None;
    let mut kind = None;

    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "q" => q = Some(value.into_owned()),
            "ref" => ref_id = Some(value.into_owned()),
            "kind" => kind = Some(value.into_owned()),
            _ => {}
        }
    }

    Ok(PageRequest {
        source,
        entry,
        query: PageQuery { q, ref_id, kind },
    })
}

fn dialog_reconcile_script() -> &'static str {
    r#"
   (() => {
     const dialog = document.getElementById("entry-modal");

     if (dialog) {
       if (!dialog.open) dialog.showModal();
     } else {
       document.querySelectorAll("dialog[open]").forEach((d) => d.close());
     }
   })();
   "#
}

fn navigation_script() -> &'static str {
    r#"
   (() => {
     const RECONCILE_EVENT = "nix-search-reconcile";

     function paramsFromInputs() {
       const params = new URLSearchParams();
       document.querySelectorAll("[data-nix-search-input]").forEach((el) => {
         const name = el.getAttribute("data-nix-search-input");
         const value = el.value.trim();
         if (value) params.set(name, value);
       });
       return params;
     }

     function currentSourcePath() {
       const parts = window.location.pathname.split("/").filter(Boolean);
       return parts.length > 0 ? "/" + parts[0] : "/";
     }

     function buildSearchUrlFromInputs() {
       const params = paramsFromInputs();
       const path = currentSourcePath();
       const qs = params.toString();
       return qs ? path + "?" + qs : path;
     }

     function navigate(url, { push = true } = {}) {
       const next = new URL(url, window.location.href);
       const target = next.pathname + next.search;
       const current = window.location.pathname + window.location.search;

       if (push && current !== target) {
         history.pushState(null, "", target);
       }

       window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
     }

     function syncInputsFromUrl() {
       const params = new URLSearchParams(window.location.search);
       document.querySelectorAll("[data-nix-search-input]").forEach((el) => {
         const name = el.getAttribute("data-nix-search-input");
         el.value = params.get(name) || "";
       });
     }

     document.addEventListener("click", (evt) => {
       if (evt.defaultPrevented) return;
       if (evt.button !== 0) return;
       if (evt.metaKey || evt.ctrlKey || evt.shiftKey || evt.altKey) return;

       const link = evt.target.closest("a[href]");
       if (!link) return;
       if (link.target === "_blank") return;
       if (link.hasAttribute("download")) return;

       const url = new URL(link.href, window.location.href);
       if (url.origin !== window.location.origin) return;
       if (link.rel && link.rel.includes("external")) return;

       evt.preventDefault();
       navigate(url.toString());
     });

     let debounce;
     document.addEventListener("input", (evt) => {
       const el = evt.target;
       if (!el.matches || !el.matches("[data-nix-search-input]")) return;
       clearTimeout(debounce);
       debounce = setTimeout(() => {
         navigate(buildSearchUrlFromInputs());
       }, 300);
     });

     document.addEventListener("submit", (evt) => {
       const form = evt.target;
       if (!(form instanceof HTMLFormElement)) return;
       if (form.method && form.method.toLowerCase() !== "get") return;

       evt.preventDefault();
       navigate(buildSearchUrlFromInputs());
     });

     window.addEventListener("popstate", () => {
       syncInputsFromUrl();
       window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
     });

     window.nixSearchNavigate = navigate;

     // Open the modal once on initial full-page load.
     (() => {
       const dialog = document.getElementById("entry-modal");
       if (dialog && !dialog.open) dialog.showModal();
     })();
   })();
   "#
}

fn page_css() -> &'static str {
    r#"
   :root {
     color-scheme: light dark;
     --bg: #0f172a;
     --panel: #111827;
     --text: #e5e7eb;
     --muted: #9ca3af;
     --accent: #38bdf8;
     --border: #374151;
     --danger: #fecaca;
   }

   body {
     margin: 0;
     font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
     background: var(--bg);
     color: var(--text);
   }

   main {
     max-width: 960px;
     margin: 0 auto;
     padding: 2rem 1rem 4rem;
   }

   h1 {
     margin-bottom: 0.25rem;
     font-size: 2rem;
   }

   .subtitle {
     color: var(--muted);
     margin-top: 0;
     margin-bottom: 2rem;
   }

   form.search {
     display: grid;
     gap: 0.75rem;
     background: var(--panel);
     border: 1px solid var(--border);
     border-radius: 0.75rem;
     padding: 1rem;
     margin-bottom: 1.25rem;
   }

   .filters {
     display: grid;
     gap: 0.75rem;
     grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
   }

   label {
     display: grid;
     gap: 0.25rem;
     color: var(--muted);
     font-size: 0.875rem;
   }

   input {
     box-sizing: border-box;
     width: 100%;
     border: 1px solid var(--border);
     border-radius: 0.5rem;
     background: #030712;
     color: var(--text);
     padding: 0.7rem 0.8rem;
     font: inherit;
   }

   input[type="search"] {
     font-size: 1.1rem;
   }

   button {
     border: 0;
     border-radius: 0.5rem;
     background: var(--accent);
     color: #082f49;
     font-weight: 700;
     padding: 0.65rem 1rem;
     cursor: pointer;
   }

   .status, .error {
     border: 1px solid var(--border);
     border-radius: 0.75rem;
     padding: 1rem;
     color: var(--muted);
     background: var(--panel);
   }

   .error {
     color: var(--danger);
     border-color: #7f1d1d;
   }

   .results {
     display: grid;
     gap: 0.75rem;
   }

   .result {
     border: 1px solid var(--border);
     border-radius: 0.75rem;
     padding: 1rem;
     background: var(--panel);
   }

   .result h2 {
     margin: 0 0 0.35rem;
     font-size: 1.15rem;
   }

   .result h2 code {
     color: var(--text);
   }

   .meta {
     color: var(--muted);
     font-size: 0.875rem;
     margin-bottom: 0.5rem;
   }

   .summary {
     margin: 0.5rem 0;
   }

   a {
     color: var(--accent);
   }

   dialog {
     position: fixed;
     inset: 50% auto auto 50%;
     transform: translate(-50%, -50%);
     width: min(900px, calc(100vw - 2rem));
     max-height: calc(100vh - 2rem);
     overflow: auto;
     border: 1px solid var(--border);
     border-radius: 1rem;
     background: var(--panel);
     color: var(--text);
     padding: 0;
   }

   dialog::backdrop {
     background: rgb(0 0 0 / 0.65);
   }

   .entry {
     padding: 1.25rem;
   }

   .entry header {
     display: flex;
     justify-content: space-between;
     align-items: start;
     gap: 1rem;
     border-bottom: 1px solid var(--border);
     padding-bottom: 1rem;
     margin-bottom: 1rem;
   }

   .entry h2 {
     margin: 0;
     font-size: 1.35rem;
   }

   .entry-section {
     margin-top: 1rem;
   }

   .entry-section h3 {
     margin-bottom: 0.35rem;
   }

   pre {
     overflow: auto;
     background: #030712;
     border: 1px solid var(--border);
     border-radius: 0.5rem;
     padding: 0.75rem;
   }

   ul {
     padding-left: 1.25rem;
   }
   "#
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use nix_search_config::AppConfig;
    use nix_search_core::{Declaration, OptionDoc, SearchDocument};

    use super::{
        AppState, PageQuery, PageRequest, close_url_for, dialog_reconcile_script, entry_url_for,
        first_source_link, page_request_from_public_url, render_full_page, search_url_for,
    };

    #[test]
    fn escapes_query_in_search_input() {
        let state = test_state();
        let request = test_request(PageQuery {
            q: Some(r#"<script>alert("x")</script>"#.to_owned()),
            ..PageQuery::default()
        });

        let html = render_full_page(&state, &request, Ok(&[]));

        assert!(!html.contains(r#"<script>alert("x")</script>"#));
        assert!(html.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"));
    }

    #[test]
    fn renders_empty_results_message() {
        let state = test_state();
        let request = test_request(PageQuery::default());

        let html = render_full_page(&state, &request, Ok(&[]));

        assert!(html.contains("Enter a search query."));
    }

    #[test]
    fn renders_reconcile_attribute_and_navigation_script() {
        let state = test_state();
        let request = test_request(PageQuery::default());

        let html = render_full_page(&state, &request, Ok(&[]));

        assert!(html.contains("data-on:nix-search-reconcile__window"));
        assert!(html.contains("/-/state/events"));
        assert!(html.contains("window.nixSearchNavigate"));
        assert!(html.contains("data-nix-search-input=\"q\""));
        assert!(html.contains("data-nix-search-input=\"ref\""));
    }

    #[test]
    fn resolves_source_link_when_available() {
        let config = test_config();
        let mut option = OptionDoc::new(
            &nix_search_core::IngestContext {
                source: "fixtures".into(),
                ref_id: "small".into(),
                revision: Some("abc123".into()),
                repo: None,
            },
            "programs.fixture.enable",
        );

        option.declarations.push(Declaration {
            name: "module.nix:4".into(),
            url: None,
        });

        let document = SearchDocument::Option(option);

        assert_eq!(
            first_source_link(&document, &config).as_deref(),
            Some("https://github.com/example/repo/blob/abc123/module.nix#L4")
        );
    }

    #[test]
    fn search_url_for_root_with_query() {
        let url = search_url_for(
            None,
            &PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        );

        assert_eq!(url, "/?q=git");
    }

    #[test]
    fn search_url_for_source_with_query_and_ref() {
        let url = search_url_for(
            Some("fixtures"),
            &PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                ..PageQuery::default()
            },
        );

        assert_eq!(url, "/fixtures?q=git&ref=small");
    }

    #[test]
    fn entry_url_for_includes_kind() {
        let url = entry_url_for(
            "fixtures",
            "programs.git.enable",
            Some("option"),
            &PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                ..PageQuery::default()
            },
        );

        assert_eq!(
            url,
            "/fixtures/programs.git.enable?q=git&ref=small&kind=option"
        );
    }

    #[test]
    fn close_url_for_strips_entry_segment() {
        let request = PageRequest {
            source: Some("fixtures".to_owned()),
            entry: Some("programs.git.enable".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                kind: Some("option".to_owned()),
            },
        };

        assert_eq!(close_url_for(&request), "/fixtures?q=git&ref=small");
    }

    #[test]
    fn close_url_for_returns_root_when_no_source() {
        let request = PageRequest {
            source: None,
            entry: None,
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        };

        assert_eq!(close_url_for(&request), "/?q=git");
    }

    #[test]
    fn parses_root_public_url() {
        let request = page_request_from_public_url("/").unwrap();

        assert_eq!(request.source, None);
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q, None);
    }

    #[test]
    fn parses_source_search_public_url() {
        let request = page_request_from_public_url("/fixtures?q=git&ref=small").unwrap();

        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry, None);
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
    }

    #[test]
    fn parses_entry_public_url() {
        let request = page_request_from_public_url(
            "/fixtures/programs.git.enable?q=git&ref=small&kind=option",
        )
        .unwrap();

        assert_eq!(request.source.as_deref(), Some("fixtures"));
        assert_eq!(request.entry.as_deref(), Some("programs.git.enable"));
        assert_eq!(request.query.q.as_deref(), Some("git"));
        assert_eq!(request.query.ref_id.as_deref(), Some("small"));
        assert_eq!(request.query.kind.as_deref(), Some("option"));
    }

    #[test]
    fn dialog_reconcile_script_handles_open_and_close() {
        let script = dialog_reconcile_script();

        assert!(script.contains("showModal()"));
        assert!(script.contains("dialog[open]"));
        assert!(script.contains(".close()"));
    }

    fn test_state() -> AppState {
        AppState {
            config: Arc::new(test_config()),
            index_path: Arc::new(PathBuf::from("./data/indexes/missing-test-index")),
        }
    }

    fn test_request(query: PageQuery) -> PageRequest {
        PageRequest {
            source: None,
            entry: None,
            query,
        }
    }

    fn test_config() -> AppConfig {
        nix_search_test_support::app_config("./data/indexes")
    }
}
