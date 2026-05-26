use maud::{Markup, html};

use nixsearch_config::AppConfig;

use crate::request::SourceFilter;

use super::source_tag::color_for_source;

const ALL_TAB_COLOR: &str = "#d4d4d8";

pub fn render_form(
    config: &AppConfig,
    source_filter: &SourceFilter,
    form_action: &str,
    q: &str,
) -> Markup {
    let has_multiple_sources = config.sources.len() > 1;

    html! {
        form.search-form action=(form_action) method="get" {
            @if has_multiple_sources {
                (render_source_tabs(config, source_filter))
            }

            div.search-bar-row {
                input type="search" name="q" value=(q)
                    placeholder="Search packages and options…"
                    autocomplete="off" autofocus
                    data-nixsearch-input="q";

                div.ref-radios data-nixsearch-ref-container="" {
                    (render_ref_radios(config, source_filter, ""))
                }
            }

            button.search-submit type="submit" { "Search" }
        }
    }
}

fn render_source_tabs(config: &AppConfig, selected: &SourceFilter) -> Markup {
    html! {
        div.source-tabs-container {
            nav.source-tabs {
                button.source-tab type="button"
                    data-nixsearch-source=""
                    data-active[*selected == SourceFilter::All]
                    style=(format!("--tab-color: {ALL_TAB_COLOR};")) {
                    "All"
                }
                @for (id, source) in &config.sources {
                    @let name = source.name.as_deref().unwrap_or(id);
                    @let is_selected = matches!(selected, SourceFilter::Named(s) if s == id);
                    @let color = color_for_source(config, id);
                    button.source-tab type="button"
                        data-nixsearch-source=(id)
                        data-active[is_selected]
                        style=(format!("--tab-color: {color};")) {
                        (name)
                    }
                }
            }
            select.source-tabs-overflow-select
                hidden
                data-nixsearch-overflow-select="" {}
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
