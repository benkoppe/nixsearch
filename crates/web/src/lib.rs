use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Sse, sse::Event};
use axum::routing::get;
use datastar::{axum::ReadSignals, prelude::PatchElements};
use futures_util::stream;
use html_escape::{encode_double_quoted_attribute, encode_text};
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use nix_search_config::AppConfig;
use nix_search_core::{CommonDoc, SearchDocument, SourceLinkConfig, SourceLinkResolver};
use nix_search_index::{IndexStore, SearchHit, SearchIndex, SearchOptions, SearchScope};

const DEFAULT_LIMIT: usize = 20;

#[derive(Debug, Clone)]
struct AppState {
    config: Arc<AppConfig>,
    index_path: Arc<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    source: Option<String>,
    #[serde(rename = "ref")]
    ref_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SearchSignals {
    q: Option<String>,
    source: Option<String>,
    #[serde(rename = "ref")]
    ref_id: Option<String>,
}

impl From<SearchSignals> for SearchQuery {
    fn from(value: SearchSignals) -> Self {
        Self {
            q: value.q,
            source: value.source,
            ref_id: value.ref_id,
        }
    }
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
        .route("/", get(index_page))
        .route("/search", get(search_page))
        .route("/search/events", get(search_events))
        .route("/-/health", get(health))
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

async fn index_page(State(state): State<AppState>) -> impl IntoResponse {
    Html(render_page(&SearchQuery::default(), None, &state.config))
}

async fn search_page(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    match run_search(&state, &query) {
        Ok(hits) => Html(render_page(&query, Some(Ok(&hits)), &state.config)),
        Err(error) => Html(render_page(
            &query,
            Some(Err(&format!("{error:#}"))),
            &state.config,
        )),
    }
}

async fn search_events(
    State(state): State<AppState>,
    ReadSignals(signals): ReadSignals<SearchSignals>,
) -> impl IntoResponse {
    let query = SearchQuery::from(signals);

    let results_html = match run_search(&state, &query) {
        Ok(hits) => render_results_container(&query, &hits, &state.config),
        Err(error) => render_error_container(&format!("{error:#}")),
    };

    let event = PatchElements::new(results_html).write_as_axum_sse_event();

    Sse::new(stream::once(async move { Ok::<Event, Infallible>(event) }))
}

fn run_search(state: &AppState, query: &SearchQuery) -> Result<Vec<SearchHit>> {
    let Some(q) = normalized_query(query) else {
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
            query.source.as_deref().and_then(non_empty),
            query.ref_id.as_deref().and_then(non_empty),
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

fn normalized_query(query: &SearchQuery) -> Option<&str> {
    query.q.as_deref().and_then(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();

    if value.is_empty() { None } else { Some(value) }
}

fn render_page(
    query: &SearchQuery,
    results: Option<std::result::Result<&[SearchHit], &str>>,
    config: &AppConfig,
) -> String {
    let q = query.q.as_deref().unwrap_or("");
    let source = query.source.as_deref().unwrap_or("");
    let ref_id = query.ref_id.as_deref().unwrap_or("");

    let results_html = match results {
        Some(Ok(hits)) => render_results_container(query, hits, config),
        Some(Err(error)) => render_error_container(error),
        None => render_empty_results_container(query),
    };

    format!(
        r#"<!doctype html>
   <html lang="en">
   <head>
     <meta charset="utf-8">
     <meta name="viewport" content="width=device-width, initial-scale=1">
     <title>Nix Search</title>
     <script type="module"
 src="https://cdn.jsdelivr.net/gh/starfederation/datastar@main/bundles/datastar.js"></script>
     <style>
       :root {{
         color-scheme: light dark;
         --bg: #0f172a;
         --panel: #111827;
         --text: #e5e7eb;
         --muted: #9ca3af;
         --accent: #38bdf8;
         --border: #374151;
       }}

       body {{
         margin: 0;
         font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
         background: var(--bg);
         color: var(--text);
       }}

       main {{
         max-width: 960px;
         margin: 0 auto;
         padding: 2rem 1rem 4rem;
       }}

       h1 {{
         margin-bottom: 0.25rem;
         font-size: 2rem;
       }}

       .subtitle {{
         color: var(--muted);
         margin-top: 0;
         margin-bottom: 2rem;
       }}

       form {{
         display: grid;
         gap: 0.75rem;
         background: var(--panel);
         border: 1px solid var(--border);
         border-radius: 0.75rem;
         padding: 1rem;
         margin-bottom: 1.25rem;
       }}

       .filters {{
         display: grid;
         gap: 0.75rem;
         grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
       }}

       label {{
         display: grid;
         gap: 0.25rem;
         color: var(--muted);
         font-size: 0.875rem;
       }}

       input {{
         box-sizing: border-box;
         width: 100%;
         border: 1px solid var(--border);
         border-radius: 0.5rem;
         background: #030712;
         color: var(--text);
         padding: 0.7rem 0.8rem;
         font: inherit;
       }}

       input[type="search"] {{
         font-size: 1.1rem;
       }}

       button {{
         justify-self: start;
         border: 0;
         border-radius: 0.5rem;
         background: var(--accent);
         color: #082f49;
         font-weight: 700;
         padding: 0.65rem 1rem;
         cursor: pointer;
       }}

       .status, .error {{
         border: 1px solid var(--border);
         border-radius: 0.75rem;
         padding: 1rem;
         color: var(--muted);
         background: var(--panel);
       }}

       .error {{
         color: #fecaca;
         border-color: #7f1d1d;
       }}

       .results {{
         display: grid;
         gap: 0.75rem;
       }}

       .result {{
         border: 1px solid var(--border);
         border-radius: 0.75rem;
         padding: 1rem;
         background: var(--panel);
       }}

       .result h2 {{
         margin: 0 0 0.35rem;
         font-size: 1.15rem;
       }}

       .result h2 code {{
         color: var(--text);
       }}

       .meta {{
         color: var(--muted);
         font-size: 0.875rem;
         margin-bottom: 0.5rem;
       }}

       .summary {{
         margin: 0.5rem 0;
       }}

       a {{
         color: var(--accent);
       }}
     </style>
   </head>
   <body data-signals-q="{q_attr}" data-signals-source="{source_attr}" data-signals-ref="{ref_attr}">
     <main>
       <h1>Nix Search</h1>
       <p class="subtitle">Search indexed Nix packages and options.</p>

       <form action="/search" method="get">
         <label>
           Query
           <input
             type="search"
             name="q"
             value="{q_attr}"
             placeholder="git, programs.git.enable, services.nginx..."
             autocomplete="off"
             autofocus
             data-bind-q
             data-on-input__debounce.300ms="@get('/search/events')"
           >
         </label>

         <div class="filters">
           <label>
             Source
             <input
               name="source"
               value="{source_attr}"
               placeholder="optional"
               data-bind-source
               data-on-input__debounce.300ms="@get('/search/events')"
             >
           </label>

           <label>
             Ref
             <input
               name="ref"
               value="{ref_attr}"
               placeholder="optional"
               data-bind-ref
               data-on-input__debounce.300ms="@get('/search/events')"
             >
           </label>
         </div>

         <button type="submit">Search</button>
       </form>

       {results_html}
     </main>
   </body>
   </html>"#,
        q_attr = encode_double_quoted_attribute(q),
        source_attr = encode_double_quoted_attribute(source),
        ref_attr = encode_double_quoted_attribute(ref_id),
    )
}

fn render_results_container(query: &SearchQuery, hits: &[SearchHit], config: &AppConfig) -> String {
    let Some(q) = normalized_query(query) else {
        return render_empty_results_container(query);
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
        html.push_str(&render_hit(hit, config));
    }

    html.push_str("</div>");
    html
}

fn render_empty_results_container(_query: &SearchQuery) -> String {
    r#"<div id="results" class="status">Enter a search query.</div>"#.to_owned()
}

fn render_error_container(error: &str) -> String {
    format!(
        r#"<div id="results" class="error"><strong>Search failed:</strong> {}</div>"#,
        encode_text(error)
    )
}

fn render_hit(hit: &SearchHit, config: &AppConfig) -> String {
    let common = hit.document.common();
    let summary = summary_for_document(&hit.document);
    let source_link = first_source_link(&hit.document, config);

    let mut html = format!(
        r#"<article class="result">
     <h2><code>{name}</code></h2>
     <div class="meta">{kind} · {source}/{ref_id} · score {score:.3}</div>"#,
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

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use nix_search_config::AppConfig;
    use nix_search_core::{
        CommonDoc, Declaration, DocumentKind, NameParts, OptionDoc, SearchDocument,
    };

    use super::{SearchQuery, first_source_link, render_page};

    #[test]
    fn escapes_query_in_search_input() {
        let config = test_config();
        let query = SearchQuery {
            q: Some(r#"<script>alert("x")</script>"#.to_owned()),
            ..SearchQuery::default()
        };

        let html = render_page(&query, None, &config);

        assert!(!html.contains(r#"<script>alert("x")</script>"#));
        assert!(html.contains("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;"));
    }

    #[test]
    fn renders_empty_results_message() {
        let config = test_config();
        let html = render_page(&SearchQuery::default(), None, &config);

        assert!(html.contains("Enter a search query."));
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

    fn test_config() -> AppConfig {
        nix_search_test_support::app_config("./data/indexes")
    }

    #[allow(dead_code)]
    fn test_common() -> CommonDoc {
        CommonDoc {
            id: "fixtures/small/option/programs.fixture.enable".to_owned(),
            source: "fixtures".to_owned(),
            ref_id: "small".to_owned(),
            kind: DocumentKind::Option,
            name: "programs.fixture.enable".to_owned(),
            name_parts: NameParts::from_dotted("programs.fixture.enable"),
            revision: Some("abc123".to_owned()),
            repo: None,
            imported_at: OffsetDateTime::now_utc(),
        }
    }
}
