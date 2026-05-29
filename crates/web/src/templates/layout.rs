use std::fmt::Write;

use maud::{DOCTYPE, Escaper, Markup, PreEscaped, html};

use nixsearch_config::app::AppConfig;
use nixsearch_config::server::{AnalyticsScriptConfig, ScriptAttributeValue};
use nixsearch_config::source::SourceKind;
use nixsearch_index::search::SearchResult;

use crate::AppState;
use crate::RECONCILE_EVENTS_URL;
use crate::request::{PageRequest, SourceFilter, normalized_query};
use crate::scripts::navigation_script;
use crate::urls::source_path;

use super::footer;
use super::home;
use super::modal;
use super::modal::EntryData;
use super::results;
use super::search;
use super::source_tag;

static CSS: &str = include_str!("../../style.css");
const DEFAULT_DESCRIPTION: &str = "Search the Nix ecosystem";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageUrls {
    pub current_url: String,
    pub image_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PageMetadata {
    title: String,
    description: String,
    url: String,
    image_url: String,
}

pub fn render_full_page(
    state: &AppState,
    request: &PageRequest,
    page_state: &crate::request::PageState,
    page_urls: &PageUrls,
    search_result: Result<&SearchResult, &str>,
    entry: &EntryData,
) -> Markup {
    let q = request.query.q.as_deref().unwrap_or("");
    let source_filter = &page_state.source_filter;

    let results_markup = match search_result {
        Ok(result) if normalized_query(&request.query).is_some() => {
            results::render(page_state, &result.hits, result.total, &state.config)
        }
        Ok(_) => home::render(state, request, page_state),
        Err(error) => results::render_error(error),
    };

    let modal_markup = modal::render(&state.config, page_state, entry);
    let source_metadata = source_metadata_json(&state.config);
    let metadata = page_metadata(
        &state.config,
        request,
        source_filter,
        search_result,
        entry,
        page_urls,
    );

    let form_action = match source_filter {
        SourceFilter::All => "/".to_owned(),
        SourceFilter::Named(source) => source_path(source),
    };
    let logo_style = match source_filter {
        SourceFilter::All => None,
        SourceFilter::Named(source) => Some(format!(
            "--logo-accent: {};",
            source_tag::color_for_source(&state.config, source)
        )),
    };

    let reconcile_attr = format!(
        "@get('{RECONCILE_EVENTS_URL}?url=' + encodeURIComponent(location.pathname + location.search) + '&previous_url=' + encodeURIComponent(window.nixsearchPreviousUrl || ''))"
    );

    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (&metadata.title) }
                meta name="description" content=(&metadata.description);
                meta property="og:url" content=(&metadata.url);
                meta property="og:type" content="website";
                meta property="og:site_name" content="nixsearch";
                meta property="og:title" content=(&metadata.title);
                meta property="og:description" content=(&metadata.description);
                meta property="og:image" content=(&metadata.image_url);
                link rel="icon" type="image/x-icon" href="/favicon.ico";
                link rel="apple-touch-icon" href="/apple-touch-icon.png";
                script type="module"
                    src="https://cdn.jsdelivr.net/gh/starfederation/datastar@main/bundles/datastar.js" {}
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
                (footer::render_footer(state))

                script #source-metadata type="application/json" {
                    (PreEscaped(&source_metadata))
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

fn page_metadata(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    search_result: Result<&SearchResult, &str>,
    entry: &EntryData,
    page_urls: &PageUrls,
) -> PageMetadata {
    PageMetadata {
        title: title_for_entry(config, request, source_filter, entry.document()),
        description: description_for(config, request, source_filter, search_result, entry),
        url: page_urls.current_url.clone(),
        image_url: page_urls.image_url.clone(),
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

    serde_json::json!({
        "sources": sources,
        "refSets": ref_sets,
        "defaultRefSet": config.default_ref_set().unwrap_or(""),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use nixsearch_config::server::ScriptAttributeValue;
    use nixsearch_core::document::{DocText, OptionDoc, PackageDoc, SearchDocument};
    use nixsearch_core::ingest::IngestContext;
    use nixsearch_index::search::SearchResult;
    use nixsearch_test_support::{SOURCE_FIXTURES, app_config, utf8_path_buf};
    use tempfile::tempdir;

    use crate::request::{PageQuery, PageRequest, SourceFilter};

    use super::{
        EntryData, PageUrls, analytics_script, description_for, page_metadata, title_for,
        title_for_entry,
    };

    fn config() -> nixsearch_config::app::AppConfig {
        let tempdir = tempdir().unwrap();
        app_config(utf8_path_buf(tempdir.path().join("indexes")))
    }

    fn page_urls() -> PageUrls {
        PageUrls {
            current_url: "https://search.example.com/?q=git".to_owned(),
            image_url: "https://search.example.com/apple-touch-icon.png".to_owned(),
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
                &EntryData::Found(Box::new(document))
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
                &EntryData::Found(Box::new(document))
            ),
            "programs.git.enable · Enable Git support."
        );
    }
}
