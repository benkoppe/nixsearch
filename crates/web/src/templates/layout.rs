use maud::{DOCTYPE, Markup, PreEscaped, html};

use nixsearch_config::AppConfig;
use nixsearch_index::SearchResult;

use crate::AppState;
use crate::RECONCILE_EVENTS_URL;
use crate::request::{PageRequest, SourceFilter, normalized_query};
use crate::scripts::navigation_script;
use crate::urls::source_path;

use super::footer;
use super::modal;
use super::results;
use super::search;
use super::source_tag;

static CSS: &str = include_str!("../../style.css");

pub fn render_full_page(
    state: &AppState,
    request: &PageRequest,
    search_result: Result<&SearchResult, &str>,
) -> Markup {
    let q = request.query.q.as_deref().unwrap_or("");
    let source_filter = SourceFilter::from_request(request);

    let results_markup = match search_result {
        Ok(result) if normalized_query(&request.query).is_some() => {
            results::render(request, &result.hits, result.total, &state.config)
        }
        Ok(_) => results::render_empty(),
        Err(error) => results::render_error(error),
    };

    let modal_markup = modal::render(state, request);
    let source_metadata = source_metadata_json(&state.config);

    let form_action = match &source_filter {
        SourceFilter::All => "/".to_owned(),
        SourceFilter::Named(source) => source_path(source),
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
                title { "nixsearch" }
                script type="module"
                    src="https://cdn.jsdelivr.net/gh/starfederation/datastar@main/bundles/datastar.js" {}
                style { (PreEscaped(CSS)) }
                noscript {
                    style { ".js-ref-radios { display: none; } dialog#entry-modal { display: block; z-index: 201; } .modal-backdrop { display: block; position: fixed; inset: 0; z-index: 200; background: rgb(0 0 0 / 0.6); }" }
                }
            }
            body data-on:nixsearch-reconcile__window=(reconcile_attr) {
                header.header {
                    div.header-inner {
                        a.site-title href="/" { "nixsearch" }
                        (search::render_form(
                            &state.config,
                            &source_filter,
                            &form_action,
                            q,
                            request.query.ref_id.as_deref().unwrap_or(""),
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

fn source_metadata_json(config: &AppConfig) -> String {
    let entries: Vec<String> = config
           .sources
           .iter()
           .map(|(id, source)| {
               let name = source.name.as_deref().unwrap_or(id);
               let color = source_tag::color_for_source(config, id);
               let refs: Vec<&str> = source.refs.iter().map(|r| r.id.as_str()).collect();
               let refs_json = refs
                   .iter()
                   .map(|r| format!("\"{}\"", r.replace('"', "\\\"")))
                   .collect::<Vec<_>>()
                   .join(",");
               let default_ref = source.default_ref.as_deref().unwrap_or("");

               format!(
                   r#"{{"id":"{id}","name":"{name}","color":"{color}","refs":[{refs_json}],"defaultRef":"{default_ref}"}}"#,
                   id = id.replace('"', "\\\""),
                   name = name.replace('"', "\\\""),
                   color = color.replace('"', "\\\""),
                   default_ref = default_ref.replace('"', "\\\""),
               )
           })
           .collect();

    format!("[{}]", entries.join(","))
}
