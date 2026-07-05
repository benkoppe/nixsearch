use std::fmt::Write;

use maud::{DOCTYPE, Escaper, Markup, PreEscaped, html};
use serde::Serialize;

use nixsearch_config::app::AppConfig;
use nixsearch_config::server::{AnalyticsScriptConfig, ScriptAttributeValue};
use nixsearch_index::search::SearchResult;
use nixsearch_service::ServedGenerationSnapshot;

use crate::AppState;
use crate::RECONCILE_EVENTS_URL;
use crate::entry::EntryData;
use crate::metadata::{MetadataContent, PageHeadMetadataInput, PageMetadata};
use crate::origin::PageUrls;
use crate::request::{PageRequest, PageState, SourceFilter};
use crate::scripts::{datastar_script_url, navigation_script_url, style_css_url};
use crate::urls::source_path;

use super::footer;
use super::home;
use super::modal;
use super::results;
use super::search;
use super::source_tag;

#[derive(Clone, Copy)]
pub enum ResultsContent<'a> {
    Home,
    SearchResults(&'a SearchResult),
    DirectEntry(&'a EntryData),
    Error { title: &'a str, message: &'a str },
}

impl<'a> ResultsContent<'a> {
    pub(crate) fn metadata_content(self) -> MetadataContent<'a> {
        match self {
            Self::Home => MetadataContent::Home,
            Self::SearchResults(result) => MetadataContent::SearchResults(result),
            Self::DirectEntry(_) => MetadataContent::DirectEntry,
            Self::Error { title, message } => MetadataContent::Error { title, message },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InitialReturnMetadata {
    pub metadata: PageMetadata,
    pub url: String,
}

pub(crate) struct FullPageRender<'a> {
    pub state: &'a AppState,
    pub request: &'a PageRequest,
    pub page_state: &'a PageState,
    pub page_urls: &'a PageUrls,
    pub served_generation: &'a ServedGenerationSnapshot,
    pub results_content: ResultsContent<'a>,
    pub entry: &'a EntryData,
    pub initial_return_metadata: Option<&'a InitialReturnMetadata>,
}

pub fn render_full_page(page: FullPageRender<'_>) -> Markup {
    let FullPageRender {
        state,
        request,
        page_state,
        page_urls,
        served_generation,
        results_content,
        entry,
        initial_return_metadata,
    } = page;
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

    let metadata = crate::metadata::page_head_metadata(PageHeadMetadataInput {
        state,
        request,
        page_state,
        page_urls,
        snapshot: served_generation,
        content: results_content.metadata_content(),
        entry,
    });

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
                @if let Some(open_graph) = &metadata.open_graph {
                    meta property="og:url" content=(&open_graph.url);
                    meta property="og:type" content=(&open_graph.kind);
                    meta property="og:site_name" content=(&open_graph.site_name);
                    meta property="og:title" content=(&open_graph.title);
                    meta property="og:description" content=(&open_graph.description);
                    meta property="og:image" content=(&open_graph.image_url);
                }
                link rel="icon" type="image/x-icon" href="/favicon.ico";
                link rel="apple-touch-icon" href="/apple-touch-icon.png";
                link rel="stylesheet" href=(style_css_url());
                script type="module" src=(datastar_script_url()) {}
                (analytics_script(&state.config.server.analytics_script))
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
                script src=(navigation_script_url()) {}
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

fn source_metadata_json(config: &AppConfig) -> String {
    let sources = config
        .sources
        .iter()
        .filter(|(_, source)| source.has_searchable_refs())
        .map(|(id, source)| {
            let refs: Vec<&str> = source.searchable_refs().map(|r| r.id.as_str()).collect();

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
    use nixsearch_test_support::{app_config, utf8_path_buf};
    use tempfile::tempdir;

    use super::{analytics_script, generation_state_json, json_script_content};

    fn config() -> nixsearch_config::app::AppConfig {
        let tempdir = tempdir().unwrap();
        app_config(utf8_path_buf(tempdir.path().join("indexes")))
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
}
