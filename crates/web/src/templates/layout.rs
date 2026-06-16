use std::fmt::Write;

use maud::{DOCTYPE, Escaper, Markup, PreEscaped, html};
use serde::Serialize;

use nixsearch_config::app::AppConfig;
use nixsearch_config::server::{AnalyticsScriptConfig, ScriptAttributeValue};
use nixsearch_config::source::SourceKind;
use nixsearch_index::search::SearchResult;
use nixsearch_service::ServedGenerationSnapshot;

use crate::AppState;
use crate::DATASTAR_JS_URL;
use crate::RECONCILE_EVENTS_URL;
use crate::entry::EntryData;
use crate::origin::PageUrls;
use crate::request::{PageRequest, PageState, SourceFilter, non_empty, normalized_query};
use crate::scripts::navigation_script;
use crate::urls::{
    canonical_entry_path_for_document, canonical_home_path, canonical_source_path, source_path,
};

use super::footer;
use super::home;
use super::modal;
use super::results;
use super::search;
use super::source_tag;

static CSS: &str = include_str!("../../style.css");
const DEFAULT_DESCRIPTION: &str = "Search the Nix ecosystem";
const ROBOTS_NOINDEX_FOLLOW: &str = "noindex,follow";

#[derive(Clone, Copy)]
pub enum ResultsContent<'a> {
    Home,
    SearchResults(&'a SearchResult),
    DirectEntry(&'a EntryData),
    Error { title: &'a str, message: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageMetadata {
    title: String,
    description: String,
    url: String,
    image_url: String,
    canonical_url: Option<String>,
    robots: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitialReturnMetadata {
    pub metadata: PageMetadata,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct IndexMetadata {
    canonical_url: Option<String>,
    robots: Option<&'static str>,
}

pub fn render_full_page(
    state: &AppState,
    request: &PageRequest,
    page_state: &crate::request::PageState,
    page_urls: &PageUrls,
    served_generation: &ServedGenerationSnapshot,
    results_content: ResultsContent<'_>,
    entry: &EntryData,
    initial_return_metadata: Option<&InitialReturnMetadata>,
) -> Markup {
    let q = request.query.q.as_deref().unwrap_or("");
    let source_filter = &page_state.source_filter;

    let results_markup = match results_content {
        ResultsContent::Home => home::render(state, request, page_state, served_generation),
        ResultsContent::SearchResults(result) => {
            results::render(page_state, &result.hits, result.total, &state.config)
        }
        ResultsContent::DirectEntry(entry) => {
            results::render_entry(&state.config, page_state, entry)
        }
        ResultsContent::Error { title, message } => results::render_page_error(title, message),
    };

    let empty_entry = EntryData::Empty;
    let modal_entry = if matches!(results_content, ResultsContent::DirectEntry(_)) {
        &empty_entry
    } else {
        entry
    };
    let modal_markup = modal::render(&state.config, page_state, modal_entry);
    let source_metadata = source_metadata_json(&state.config);
    let generation_state = generation_state_json(&served_generation.manifest().generation_id);
    let initial_history_metadata = initial_return_metadata.map(initial_history_metadata_json);

    let metadata = page_head_metadata(
        state,
        request,
        page_state,
        page_urls,
        served_generation,
        results_content,
        entry,
    );

    let form_action = match source_filter {
        SourceFilter::All => "/".to_owned(),
        SourceFilter::Named(source) => source_path(source),
    };
    let logo_style = match source_filter {
        SourceFilter::All => None,
        SourceFilter::Named(source) => state.config.sources.contains_key(source).then(|| {
            format!(
                "--logo-accent: {};",
                source_tag::color_for_source(&state.config, source)
            )
        }),
    };

    let reconcile_attr = format!(
        "@get('{RECONCILE_EVENTS_URL}?url=' + encodeURIComponent(location.pathname + location.search) + '&previous_url=' + encodeURIComponent(window.nixsearchPreviousUrl || '') + '&generation_id=' + encodeURIComponent(window.nixsearchGenerationId ? window.nixsearchGenerationId() : ''))"
    );

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (&metadata.title) }
                meta name="description" content=(&metadata.description);
                @if let Some(canonical_url) = &metadata.canonical_url {
                    link rel="canonical" href=(canonical_url);
                }
                @if let Some(robots) = metadata.robots {
                    meta name="robots" content=(robots);
                }
                meta property="og:url" content=(&metadata.url);
                meta property="og:type" content="website";
                meta property="og:site_name" content="nixsearch";
                meta property="og:title" content=(&metadata.title);
                meta property="og:description" content=(&metadata.description);
                meta property="og:image" content=(&metadata.image_url);
                link rel="icon" type="image/x-icon" href="/favicon.ico";
                link rel="apple-touch-icon" href="/apple-touch-icon.png";
                script type="module" src=(DATASTAR_JS_URL) {}
                (analytics_script(&state.config.server.analytics_script))
                style { (PreEscaped(CSS)) }
                noscript {
                    style { ".js-ref-radios { display: none; } dialog#entry-modal { display: block; z-index: 201; } .modal-backdrop { display: block; position: fixed; inset: 0; z-index: 200; background: rgb(0 0 0 / 0.6); }" }
                }
            }
            body data-on:nixsearch-reconcile__window=(reconcile_attr) {
                header.header {
                    div.header-inner {
                        a.site-title href="/" aria-label="nixsearch" style=[logo_style] {
                            span.site-title-nix { "nix" }
                            span.site-title-search { "search" }
                        }
                        (search::render_form(
                            &state.config,
                            page_state,
                            &form_action,
                            q,
                        ))
                    }
                }
                main.main {
                    (results_markup)
                    (modal_markup)
                }
                (footer::render_footer(state, served_generation.manifest()))

                script #generation-state type="application/json" {
                    (PreEscaped(&generation_state))
                }
                script #source-metadata type="application/json" {
                    (PreEscaped(&source_metadata))
                }
                @if let Some(initial_history_metadata) = &initial_history_metadata {
                    script #initial-history-metadata type="application/json" {
                        (PreEscaped(initial_history_metadata))
                    }
                }
                script { (PreEscaped(navigation_script())) }
            }
        }
    }
}

fn analytics_script(config: &AnalyticsScriptConfig) -> Markup {
    if !config.enabled {
        return html! {};
    }

    let mut script = String::from("<script src=\"");
    append_escaped(&mut script, &config.src);
    script.push('"');

    for (name, value) in &config.attributes {
        match value {
            ScriptAttributeValue::Bool(true) => {
                script.push(' ');
                script.push_str(name);
            }
            ScriptAttributeValue::Bool(false) => {}
            ScriptAttributeValue::String(value) => {
                script.push(' ');
                script.push_str(name);
                script.push_str("=\"");
                append_escaped(&mut script, value);
                script.push('"');
            }
        }
    }

    script.push_str("></script>");
    PreEscaped(script)
}

fn append_escaped(output: &mut String, value: &str) {
    write!(Escaper::new(output), "{value}").expect("writing to a String should not fail");
}

pub(crate) fn page_head_metadata(
    state: &AppState,
    request: &PageRequest,
    page_state: &PageState,
    page_urls: &PageUrls,
    served_generation: &ServedGenerationSnapshot,
    results_content: ResultsContent<'_>,
    entry: &EntryData,
) -> PageMetadata {
    let search_result_for_metadata = match results_content {
        ResultsContent::SearchResults(result) => Ok(result),
        ResultsContent::Error { message, .. } => Err(message),
        ResultsContent::Home | ResultsContent::DirectEntry(_) => Err(""),
    };

    let index_metadata = page_index_metadata(
        state,
        request,
        page_state,
        served_generation,
        results_content,
        entry,
        page_urls,
    );

    page_metadata(
        &state.config,
        request,
        &page_state.source_filter,
        search_result_for_metadata,
        entry,
        page_urls,
        index_metadata,
    )
}

pub(crate) fn noindex_head_metadata(
    page_urls: &PageUrls,
    title: &str,
    description: &str,
) -> PageMetadata {
    PageMetadata {
        title: title.to_owned(),
        description: description.to_owned(),
        url: page_urls.current_url.clone(),
        image_url: page_urls.image_url.clone(),
        canonical_url: None,
        robots: Some(ROBOTS_NOINDEX_FOLLOW),
    }
}

pub(crate) fn head_metadata_script(
    metadata: &PageMetadata,
    target_public_url: Option<&str>,
) -> String {
    let json = serde_json::to_string(metadata).expect("page metadata should serialize");
    let target_json =
        serde_json::to_string(&target_public_url).expect("target URL should serialize");
    format!(
        "if (window.nixsearchApplyHeadMetadata) window.nixsearchApplyHeadMetadata({json}, {target_json});"
    )
}

pub(crate) fn modal_patch_script(modal_html: &str, target_public_url: &str) -> String {
    let html_json = serde_json::to_string(modal_html).expect("modal HTML should serialize");
    let target_json =
        serde_json::to_string(target_public_url).expect("target URL should serialize");
    format!(
        "if (window.nixsearchApplyModalPatch) window.nixsearchApplyModalPatch({html_json}, {target_json});"
    )
}

pub(crate) fn results_patch_script(results_html: &str, target_public_url: &str) -> String {
    let html_json = serde_json::to_string(results_html).expect("results HTML should serialize");
    let target_json =
        serde_json::to_string(target_public_url).expect("target URL should serialize");
    format!(
        "if (window.nixsearchApplyResultsPatch) window.nixsearchApplyResultsPatch({html_json}, {target_json});"
    )
}

pub(crate) fn generation_change_script(
    generation_id: &str,
    results_html: &str,
    modal_html: &str,
    metadata: &PageMetadata,
    target_public_url: &str,
) -> String {
    let payload = serde_json::json!({
        "generationId": generation_id,
        "generationStateHtml": generation_state_script_html(generation_id),
        "resultsHtml": results_html,
        "modalHtml": modal_html,
        "metadata": metadata,
        "targetUrl": target_public_url,
    });
    let payload_json =
        serde_json::to_string(&payload).expect("generation change payload should serialize");

    format!(
        "if (window.nixsearchApplyGenerationChange) window.nixsearchApplyGenerationChange({payload_json});"
    )
}

fn page_metadata(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    search_result: Result<&SearchResult, &str>,
    entry: &EntryData,
    page_urls: &PageUrls,
    index_metadata: IndexMetadata,
) -> PageMetadata {
    let url = index_metadata
        .canonical_url
        .clone()
        .unwrap_or_else(|| page_urls.current_url.clone());

    PageMetadata {
        title: title_for_entry(config, request, source_filter, entry.document()),
        description: description_for(config, request, source_filter, search_result, entry),
        url,
        image_url: page_urls.image_url.clone(),
        canonical_url: index_metadata.canonical_url,
        robots: index_metadata.robots,
    }
}

fn page_index_metadata(
    state: &AppState,
    request: &PageRequest,
    page_state: &PageState,
    served_generation: &ServedGenerationSnapshot,
    results_content: ResultsContent<'_>,
    entry: &EntryData,
    page_urls: &PageUrls,
) -> IndexMetadata {
    if matches!(results_content, ResultsContent::Error { .. }) {
        return noindex_metadata();
    }

    if request
        .query
        .ref_set
        .as_deref()
        .and_then(non_empty)
        .is_some()
    {
        return noindex_metadata();
    }

    match entry {
        EntryData::Found(entry) => {
            let document = &entry.document;
            let common = document.common();

            if !entry.annotation.unique_within_kind {
                return noindex_metadata();
            }

            if !document.is_seo_eligible_entry() {
                return noindex_metadata();
            }

            if state.search.is_indexable_entry_in_snapshot(
                served_generation,
                &common.source,
                &common.ref_id,
                &common.name,
            ) {
                return canonical_metadata(page_urls.absolute_url(
                    &canonical_entry_path_for_document(&state.config, document, &entry.annotation),
                ));
            }

            return noindex_metadata();
        }
        EntryData::NotFound { .. } | EntryData::Ambiguous(_) | EntryData::Error(_) => {
            return noindex_metadata();
        }
        EntryData::Empty => {}
    }

    if page_state.detail.is_some() {
        return noindex_metadata();
    }

    if normalized_query(&request.query).is_some() || request.query.page.unwrap_or(1) > 1 {
        return noindex_metadata();
    }

    match &page_state.source_filter {
        SourceFilter::All => {
            if request.source.is_none()
                && request.entry.is_none()
                && request
                    .query
                    .ref_id
                    .as_deref()
                    .and_then(non_empty)
                    .is_none()
                && request
                    .query
                    .ref_set
                    .as_deref()
                    .and_then(non_empty)
                    .is_none()
                && request.query.kind.as_deref().and_then(non_empty).is_none()
                && request.query.source.is_none()
            {
                canonical_metadata(page_urls.absolute_url(&canonical_home_path()))
            } else {
                noindex_metadata()
            }
        }
        SourceFilter::Named(source) => {
            if request.entry.is_some()
                || request.query.kind.as_deref().and_then(non_empty).is_some()
                || request.query.source.is_some()
            {
                return noindex_metadata();
            }

            let Some(ref_id) = page_state.source_ref.as_deref() else {
                return noindex_metadata();
            };

            if state
                .search
                .is_indexable_ref_in_snapshot(served_generation, source, ref_id)
            {
                canonical_metadata(page_urls.absolute_url(&canonical_source_path(
                    &state.config,
                    source,
                    ref_id,
                )))
            } else {
                noindex_metadata()
            }
        }
    }
}

fn canonical_metadata(canonical_url: String) -> IndexMetadata {
    IndexMetadata {
        canonical_url: Some(canonical_url),
        robots: None,
    }
}

fn noindex_metadata() -> IndexMetadata {
    IndexMetadata {
        canonical_url: None,
        robots: Some(ROBOTS_NOINDEX_FOLLOW),
    }
}

#[cfg(test)]
fn title_for(config: &AppConfig, request: &PageRequest, source_filter: &SourceFilter) -> String {
    title_for_entry(config, request, source_filter, None)
}

fn title_for_entry(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    entry_document: Option<&nixsearch_core::document::SearchDocument>,
) -> String {
    let mut parts = Vec::new();

    if let Some(document) = entry_document {
        parts.push(document.common().name.to_owned());
    } else if let Some(entry) = request.entry.as_deref().and_then(crate::request::non_empty) {
        parts.push(entry.to_owned());
    } else if let Some(q) = normalized_query(&request.query) {
        parts.push(q.to_owned());
    }

    if let SourceFilter::Named(source_id) = source_filter {
        parts.push(source_display_name(config, source_id).to_owned());
    }

    parts.push("nixsearch".to_owned());
    parts.join(" · ")
}

fn description_for(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    search_result: Result<&SearchResult, &str>,
    entry: &EntryData,
) -> String {
    if let Some(document) = entry.document() {
        return description_for_document(config, document);
    }

    if let (Ok(result), Some(q)) = (search_result, normalized_query(&request.query)) {
        return format!("{} results for {q}", result.total);
    }

    default_description_for(config, source_filter)
}

fn default_description_for(config: &AppConfig, source_filter: &SourceFilter) -> String {
    match source_filter {
        SourceFilter::All => DEFAULT_DESCRIPTION.to_owned(),
        SourceFilter::Named(source) => {
            let source_config = config.sources.get(source);
            let source_name = source_config
                .and_then(|source| source.name.as_deref())
                .unwrap_or(source);
            let kind = source_config
                .map(|source| kind_noun(source.kind))
                .unwrap_or("entries");

            format!("Search {source_name} {kind}")
        }
    }
}

fn kind_noun(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Packages => "packages",
        SourceKind::Options => "options",
        SourceKind::Apps => "apps",
        SourceKind::Services => "services",
        SourceKind::Mixed => "packages and options",
    }
}

fn description_for_document(
    config: &AppConfig,
    document: &nixsearch_core::document::SearchDocument,
) -> String {
    let common = document.common();
    let source = source_display_name(config, &common.source);

    match document {
        nixsearch_core::document::SearchDocument::Option(option) => option
            .description
            .as_ref()
            .and_then(|description| {
                first_non_empty_line(description.plain_text().as_ref()).map(ToOwned::to_owned)
            })
            .map(|description| format!("{} · {description}", common.name))
            .unwrap_or_else(|| format!("{} · {source}", common.name)),
        nixsearch_core::document::SearchDocument::Package(package) => {
            let name = package
                .version
                .as_deref()
                .and_then(crate::request::non_empty)
                .map(|version| format!("{} {version}", common.name))
                .unwrap_or_else(|| common.name.clone());
            let description = package
                .description
                .as_deref()
                .and_then(first_non_empty_line)
                .unwrap_or(source);

            format!("{name} · {description}")
        }
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn source_display_name<'a>(config: &'a AppConfig, source_id: &'a str) -> &'a str {
    config
        .sources
        .get(source_id)
        .and_then(|source| source.name.as_deref())
        .unwrap_or(source_id)
}

fn source_metadata_json(config: &AppConfig) -> String {
    let sources = config
        .sources
        .iter()
        .map(|(id, source)| {
            let refs: Vec<&str> = source.refs.iter().map(|r| r.id.as_str()).collect();

            serde_json::json!({
                "id": id,
                "name": source.name.as_deref().unwrap_or(id),
                "color": source_tag::color_for_source(config, id),
                "refs": refs,
                "defaultRef": source.default_ref.as_deref().unwrap_or(""),
            })
        })
        .collect::<Vec<_>>();

    let ref_sets = config
        .ref_sets
        .iter()
        .map(|(ref_set, ref_set_config)| {
            serde_json::json!({
                "id": ref_set,
                "refs": &ref_set_config.refs,
            })
        })
        .collect::<Vec<_>>();

    json_script_content(&serde_json::json!({
        "sources": sources,
        "refSets": ref_sets,
        "defaultRefSet": config.default_ref_set().unwrap_or(""),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitialHistoryMetadata<'a> {
    return_head_metadata: Option<&'a PageMetadata>,
    return_head_metadata_url: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationState<'a> {
    generation_id: &'a str,
}

pub(crate) fn generation_state_script_html(generation_id: &str) -> String {
    html! {
        script #generation-state type="application/json" {
            (PreEscaped(generation_state_json(generation_id)))
        }
    }
    .into_string()
}

fn generation_state_json(generation_id: &str) -> String {
    json_script_content(&GenerationState { generation_id })
}

fn initial_history_metadata_json(return_metadata: &InitialReturnMetadata) -> String {
    json_script_content(&InitialHistoryMetadata {
        return_head_metadata: Some(&return_metadata.metadata),
        return_head_metadata_url: Some(&return_metadata.url),
    })
}

fn json_script_content<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .expect("JSON script payload should serialize")
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use nixsearch_config::server::ScriptAttributeValue;
    use nixsearch_core::document::{DocText, OptionDoc, PackageDoc, SearchDocument};
    use nixsearch_core::ingest::IngestContext;
    use nixsearch_index::annotation::SearchHitAnnotation;
    use nixsearch_index::search::SearchResult;
    use nixsearch_test_support::{SOURCE_FIXTURES, app_config, utf8_path_buf};
    use tempfile::tempdir;

    use crate::origin::PageUrls;
    use crate::request::{PageQuery, PageRequest, SourceFilter};

    use super::{
        EntryData, IndexMetadata, analytics_script, description_for, generation_state_json,
        json_script_content, page_metadata, title_for, title_for_entry,
    };

    fn config() -> nixsearch_config::app::AppConfig {
        let tempdir = tempdir().unwrap();
        app_config(utf8_path_buf(tempdir.path().join("indexes")))
    }

    fn page_urls() -> PageUrls {
        PageUrls {
            current_url: "https://search.example.com/?q=git".to_owned(),
            image_url: "https://search.example.com/apple-touch-icon.png".to_owned(),
            origin: "https://search.example.com".to_owned(),
        }
    }

    fn ingest_context() -> IngestContext {
        IngestContext {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: "small".to_owned(),
            revision: None,
            repo: None,
        }
    }

    fn found_entry(document: SearchDocument) -> EntryData {
        EntryData::Found(crate::entry::AnnotatedEntryDocument {
            annotation: SearchHitAnnotation {
                ambiguous_entry_url: false,
                unique_within_kind: true,
            },
            document: Box::new(document),
        })
    }

    #[test]
    fn analytics_script_is_omitted_by_default() {
        let config = config();

        assert_eq!(
            analytics_script(&config.server.analytics_script).into_string(),
            ""
        );
    }

    #[test]
    fn analytics_script_renders_configured_attributes() {
        let mut config = config();
        config.server.analytics_script.enabled = true;
        config.server.analytics_script.src = "https://analytics.example.com/script.js".to_owned();
        config.server.analytics_script.attributes.insert(
            "data-site-id".to_owned(),
            ScriptAttributeValue::String("site-123".to_owned()),
        );
        config
            .server
            .analytics_script
            .attributes
            .insert("defer".to_owned(), ScriptAttributeValue::Bool(true));
        config
            .server
            .analytics_script
            .attributes
            .insert("async".to_owned(), ScriptAttributeValue::Bool(false));

        assert_eq!(
            analytics_script(&config.server.analytics_script).into_string(),
            r#"<script src="https://analytics.example.com/script.js" data-site-id="site-123" defer></script>"#
        );
    }

    #[test]
    fn analytics_script_escapes_dynamic_values() {
        let mut config = config();
        config.server.analytics_script.enabled = true;
        config.server.analytics_script.src = "https://example.com/script.js?x=1&y=2".to_owned();
        config.server.analytics_script.attributes.insert(
            "data-site-id".to_owned(),
            ScriptAttributeValue::String("<site>&\"id\"".to_owned()),
        );

        assert_eq!(
            analytics_script(&config.server.analytics_script).into_string(),
            r#"<script src="https://example.com/script.js?x=1&amp;y=2" data-site-id="&lt;site&gt;&amp;&quot;id&quot;"></script>"#
        );
    }

    #[test]
    fn json_script_content_escapes_script_breakout_sequences() {
        let json = json_script_content(&serde_json::json!({
            "value": "</script><b>&"
        }));

        assert!(!json.contains("</script>"));
        assert!(json.contains(r#"\u003c/script\u003e\u003cb\u003e\u0026"#));
    }

    #[test]
    fn generation_state_json_uses_script_safe_camel_case_payload() {
        assert_eq!(
            generation_state_json("sha256:abc"),
            r#"{"generationId":"sha256:abc"}"#
        );

        assert!(!generation_state_json("</script>").contains("</script>"));
    }

    #[test]
    fn title_includes_query_and_named_source() {
        let config = config();
        let request = PageRequest {
            source: Some(SOURCE_FIXTURES.to_owned()),
            entry: None,
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        };

        assert_eq!(
            title_for(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned())
            ),
            "git · Fixtures · nixsearch"
        );
    }

    #[test]
    fn title_omits_all_source_filter() {
        let config = config();
        let request = PageRequest {
            source: None,
            entry: None,
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        };

        assert_eq!(
            title_for(&config, &request, &SourceFilter::All),
            "git · nixsearch"
        );
    }

    #[test]
    fn title_includes_named_source_without_query() {
        let config = config();
        let request = PageRequest {
            source: Some(SOURCE_FIXTURES.to_owned()),
            entry: None,
            query: PageQuery::default(),
        };

        assert_eq!(
            title_for(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned())
            ),
            "Fixtures · nixsearch"
        );
    }

    #[test]
    fn title_uses_entry_document_when_present() {
        let config = config();
        let document = SearchDocument::Package(PackageDoc::new(&ingest_context(), "git"));
        let request = PageRequest {
            source: Some(SOURCE_FIXTURES.to_owned()),
            entry: Some("git".to_owned()),
            query: PageQuery {
                q: Some("version control".to_owned()),
                ..PageQuery::default()
            },
        };

        assert_eq!(
            title_for_entry(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned()),
                Some(&document),
            ),
            "git · Fixtures · nixsearch"
        );
    }

    #[test]
    fn metadata_describes_home_page() {
        let config = config();
        let request = PageRequest::default();
        let search = SearchResult {
            hits: Vec::new(),
            total: 0,
        };
        let metadata = page_metadata(
            &config,
            &request,
            &SourceFilter::All,
            Ok(&search),
            &EntryData::Empty,
            &page_urls(),
            IndexMetadata::default(),
        );

        assert_eq!(metadata.title, "nixsearch");
        assert_eq!(metadata.description, "Search the Nix ecosystem");
        assert_eq!(metadata.url, "https://search.example.com/?q=git");
        assert_eq!(
            metadata.image_url,
            "https://search.example.com/apple-touch-icon.png"
        );
    }

    #[test]
    fn metadata_describes_source_page() {
        let config = config();
        let request = PageRequest {
            source: Some(SOURCE_FIXTURES.to_owned()),
            ..PageRequest::default()
        };
        let search = SearchResult {
            hits: Vec::new(),
            total: 0,
        };
        let metadata = page_metadata(
            &config,
            &request,
            &SourceFilter::Named(SOURCE_FIXTURES.to_owned()),
            Ok(&search),
            &EntryData::Empty,
            &page_urls(),
            IndexMetadata::default(),
        );

        assert_eq!(metadata.description, "Search Fixtures options");
    }

    #[test]
    fn metadata_describes_search_results() {
        let config = config();
        let request = PageRequest {
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
            ..PageRequest::default()
        };
        let search = SearchResult {
            hits: Vec::new(),
            total: 59_526,
        };

        assert_eq!(
            description_for(
                &config,
                &request,
                &SourceFilter::All,
                Ok(&search),
                &EntryData::Empty
            ),
            "59526 results for git"
        );
    }

    #[test]
    fn metadata_describes_package_entry() {
        let config = config();
        let mut package = PackageDoc::new(&ingest_context(), "git");
        package.version = Some("2.54.0".to_owned());
        package.description = Some("Distributed version control system\nextra".to_owned());
        let document = SearchDocument::Package(package);

        assert_eq!(
            description_for(
                &config,
                &PageRequest::default(),
                &SourceFilter::All,
                Err("unused"),
                &found_entry(document)
            ),
            "git 2.54.0 · Distributed version control system"
        );
    }

    #[test]
    fn metadata_describes_option_entry() {
        let config = config();
        let mut option = OptionDoc::new(&ingest_context(), "programs.git.enable");
        option.description = Some(DocText::Markdown(
            "Enable Git support.\nMore details.".to_owned(),
        ));
        let document = SearchDocument::Option(option);

        assert_eq!(
            description_for(
                &config,
                &PageRequest::default(),
                &SourceFilter::All,
                Err("unused"),
                &found_entry(document)
            ),
            "programs.git.enable · Enable Git support."
        );
    }
}
