use maud::{Markup, html};

use nixsearch_config::app::AppConfig;

use crate::request::{PageState, SourceFilter};
use crate::urls::{all_tab_url_from_state, source_tab_url_from_state};

use super::source_tag::color_for_source;

const ALL_TAB_COLOR: &str = "#d4d4d8";

pub fn render_form(config: &AppConfig, state: &PageState, form_action: &str, q: &str) -> Markup {
    let has_multiple_sources = config.sources.len() > 1;
    let source_color = match &state.source_filter {
        SourceFilter::Named(source_id) if config.sources.contains_key(source_id) => {
            Some(color_for_source(config, source_id))
        }
        SourceFilter::All => None,
        SourceFilter::Named(_) => None,
    };
    html! {
        form.search-form action=(form_action) method="get"
            style=[source_color.as_ref().map(|color| format!("--search-focus-color: {color};"))] {
            @if has_multiple_sources {
                (render_source_tabs(config, state))
            }

            div.search-bar-row {
                input type="search" name="q" value=(q)
                    placeholder="Search packages and options…"
                    autocomplete="off" autocorrect="off" autocapitalize="none" spellcheck="false" autofocus
                    data-nixsearch-input="q";

                div.ref-radios.js-ref-radios
                    data-nixsearch-ref-container=""
                    style=[source_color.as_ref().map(|color| format!("--source-color: {color};"))] {
                    (render_ref_radios(config, state))
                }

                noscript {
                    (render_ref_links(config, state, source_color.as_deref()))
                }
            }

            button.search-submit type="submit" { "Search" }
        }
    }
}

fn render_source_tabs(config: &AppConfig, state: &PageState) -> Markup {
    html! {
        div.source-tabs-container {
            nav.source-tabs {
                a.source-tab href=(all_tab_url_from_state(config, state))
                    data-nixsearch-source=""
                    data-active[state.source_filter == SourceFilter::All]
                    style=(format!("--tab-color: {ALL_TAB_COLOR};")) {
                    "All"
                }
                @for (id, source) in &config.sources {
                    @let name = source.name.as_deref().unwrap_or(id);
                    @let is_selected = matches!(&state.source_filter, SourceFilter::Named(s) if s == id);
                    @let color = color_for_source(config, id);
                    a.source-tab href=(source_tab_url_from_state(config, state, id))
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

fn render_ref_links(config: &AppConfig, state: &PageState, source_color: Option<&str>) -> Markup {
    if matches!(state.source_filter, SourceFilter::All) {
        return html! {
            div.ref-radios.noscript-ref-radios {
                @for ref_set in config.ref_sets.keys() {
                    @let is_selected = state.active_ref_set() == Some(ref_set.as_str());
                    @let next = {
                        let mut next = state.clone();
                        next.set_explicit_ref_set(ref_set.clone());
                        next
                    };
                    a.ref-radio-label.ref-radio-link href=(all_tab_url_from_state(config, &next))
                        data-active[is_selected] {
                        span.ref-radio-dot {}
                        span { (ref_set) }
                    }
                }
            }
        };
    }

    let SourceFilter::Named(source_id) = &state.source_filter else {
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
                @let is_selected = state.source_ref.as_deref() == Some(ref_id);
                @let next = {
                    let mut next = state.clone();
                    next.source_ref.replace(ref_id.to_owned());
                    next.clear_ref_set_context();
                    next
                };
                a.ref-radio-label.ref-radio-link href=(source_tab_url_from_state(config, &next, source_id))
                    data-active[is_selected] {
                    span.ref-radio-dot {}
                    span { (ref_id) }
                }
            }
        }
    }
}

fn render_ref_radios(config: &AppConfig, state: &PageState) -> Markup {
    let (refs, current, input_name): (Vec<&str>, Option<&str>, &str) = match &state.source_filter {
        SourceFilter::All => (
            config.ref_sets.keys().map(String::as_str).collect(),
            state.active_ref_set(),
            "ref_set",
        ),
        SourceFilter::Named(source_id) => match config.sources.get(source_id.as_str()) {
            Some(source) => (
                source.refs.iter().map(|r| r.id.as_str()).collect(),
                state.source_ref.as_deref(),
                "ref",
            ),
            None => (Vec::new(), None, "ref"),
        },
    };

    html! {
        @for ref_id in &refs {
            @let is_selected = current == Some(*ref_id);
            label.ref-radio-label {
                input type="radio" name=(input_name) value=(ref_id)
                    checked[is_selected]
                    data-nixsearch-input="ref";
                span { (ref_id) }
            }
        }
    }
}
