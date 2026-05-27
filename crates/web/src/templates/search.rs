use maud::{Markup, html};

use nixsearch_config::AppConfig;

use crate::request::{PageQuery, SourceFilter, non_empty};
use crate::urls::{ref_id_for_link, search_url_for};

use super::source_tag::color_for_source;

const ALL_TAB_COLOR: &str = "#d4d4d8";

pub fn render_form(
    config: &AppConfig,
    source_filter: &SourceFilter,
    form_action: &str,
    q: &str,
    current_ref: &str,
) -> Markup {
    let has_multiple_sources = config.sources.len() > 1;
    let source_color = match source_filter {
        SourceFilter::Named(source_id) => Some(color_for_source(config, source_id)),
        SourceFilter::All => None,
    };

    html! {
        form.search-form action=(form_action) method="get" {
            @if has_multiple_sources {
                (render_source_tabs(config, source_filter, q))
            }

            div.search-bar-row {
                input type="search" name="q" value=(q)
                    placeholder="Search packages and options…"
                    autocomplete="off" autofocus
                    data-nixsearch-input="q";

                div.ref-radios.js-ref-radios
                    data-nixsearch-ref-container=""
                    style=[source_color.as_ref().map(|color| format!("--source-color: {color};"))] {
                    (render_ref_radios(config, source_filter, current_ref))
                }

                noscript {
                    (render_ref_links(config, source_filter, current_ref, q, source_color.as_deref()))
                }
            }

            button.search-submit type="submit" { "Search" }
        }
    }
}

fn render_source_tabs(config: &AppConfig, selected: &SourceFilter, q: &str) -> Markup {
    let query = PageQuery {
        q: non_empty(q).map(ToOwned::to_owned),
        ..PageQuery::default()
    };

    html! {
        div.source-tabs-container {
            nav.source-tabs {
                a.source-tab href=(search_url_for(None, &query))
                    data-nixsearch-source=""
                    data-active[*selected == SourceFilter::All]
                    style=(format!("--tab-color: {ALL_TAB_COLOR};")) {
                    "All"
                }
                @for (id, source) in &config.sources {
                    @let name = source.name.as_deref().unwrap_or(id);
                    @let is_selected = matches!(selected, SourceFilter::Named(s) if s == id);
                    @let color = color_for_source(config, id);
                    a.source-tab href=(search_url_for(Some(id), &query))
                        data-nixsearch-source=(id)
                        data-active[is_selected]
                        style=(format!("--tab-color: {color};")) {
                        (name)
                    }
                }
            }
        }
    }
}

fn render_ref_links(
    config: &AppConfig,
    selected_source: &SourceFilter,
    current_ref: &str,
    q: &str,
    source_color: Option<&str>,
) -> Markup {
    let SourceFilter::Named(source_id) = selected_source else {
        return html! {};
    };

    let Some(source) = config.sources.get(source_id.as_str()) else {
        return html! {};
    };

    html! {
        div.ref-radios.noscript-ref-radios
            style=[source_color.map(|color| format!("--source-color: {color};"))] {
            @for ref_config in &source.refs {
                @let ref_id = ref_config.id.as_str();
                @let is_selected = if current_ref.is_empty() {
                    source.default_ref.as_deref() == Some(ref_id)
                } else {
                    current_ref == ref_id
                };
                @let query = PageQuery {
                    q: non_empty(q).map(ToOwned::to_owned),
                    ref_id: ref_id_for_link(config, source_id, ref_id),
                    ..PageQuery::default()
                };
                a.ref-radio-label.ref-radio-link href=(search_url_for(Some(source_id), &query))
                    data-active[is_selected] {
                    span.ref-radio-dot {}
                    span { (ref_id) }
                }
            }
        }
    }
}

fn render_ref_radios(
    config: &AppConfig,
    selected_source: &SourceFilter,
    current_ref: &str,
) -> Markup {
    let (refs, default_ref): (Vec<&str>, Option<&str>) = match selected_source {
        SourceFilter::All => (Vec::new(), None),
        SourceFilter::Named(source_id) => match config.sources.get(source_id.as_str()) {
            Some(source) => (
                source.refs.iter().map(|r| r.id.as_str()).collect(),
                source.default_ref.as_deref(),
            ),
            None => (Vec::new(), None),
        },
    };

    html! {
        @for ref_id in &refs {
            @let is_selected = if current_ref.is_empty() {
                default_ref == Some(*ref_id)
            } else {
                *ref_id == current_ref
            };
            label.ref-radio-label {
                input type="radio" name="ref" value=(ref_id)
                    checked[is_selected]
                    data-nixsearch-input="ref";
                span { (ref_id) }
            }
        }
    }
}
