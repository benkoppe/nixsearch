use maud::{Markup, PreEscaped, html};

use nixsearch_config::app::AppConfig;
use nixsearch_service::ServedGenerationSnapshot;

use crate::AppState;
use crate::request::{PageRequest, PageState, SourceFilter};
use crate::source_labels::{source_display_name, source_kind_noun};

use super::source_tag::color_for_source;

/// Inlined so the magnifying glass can be recolored per mode via `currentColor`.
static GLASS_SVG: &str = include_str!("../../magnify-glass.svg");

/// The muted Nix-logo blue used when searching every source ("All" mode).
const ALL_GLASS_COLOR: &str = "#5873B9";

pub fn render(
    state: &AppState,
    _request: &PageRequest,
    page_state: &PageState,
    served_generation: &ServedGenerationSnapshot,
) -> Markup {
    let config = &state.config;
    let source_filter = &page_state.source_filter;

    let (title_prefix, title_accent) = title_for(config, source_filter);
    // "All" keeps the muted glass blue and lets the title fall back to the
    // logo's default blues; named sources tint both with the source color.
    let hero_style = match source_filter {
        SourceFilter::All => format!("--glass-color: {ALL_GLASS_COLOR};"),
        SourceFilter::Named(source) => {
            let color = color_for_source(config, source);
            format!("--glass-color: {color}; --home-accent: {color};")
        }
    };
    let count = count_for(state, page_state, served_generation);

    html! {
        div #results.home-hero style=(hero_style) {
            div.home-inner {
                div.home-glass {
                    (PreEscaped(GLASS_SVG))
                }
                h1.home-title {
                    (title_prefix)
                    span.home-title-accent { (title_accent) }
                }
                @if let Some((total, noun)) = &count {
                    p.home-count {
                        strong { (format_count(*total)) } " " (noun)
                    }
                }
                p.home-hint {
                    "Press " kbd { "/" } " to search"
                }
                p.home-hint {
                    kbd { "Ctrl" } "+" kbd { "[" } " / " kbd { "]" } " to filter"
                }
            }
        }
    }
}

/// Splits the hero title into a plain prefix and the accented remainder that is
/// styled like the "search" word in the logo (e.g. `("Search the ", "Nix ecosystem")`).
fn title_for(config: &AppConfig, source_filter: &SourceFilter) -> (&'static str, String) {
    match source_filter {
        SourceFilter::All => ("Search the ", "Nix ecosystem".to_owned()),
        SourceFilter::Named(source) => ("Search ", source_display_name(config, source).to_owned()),
    }
}

/// Returns the entry count and its noun (e.g. `(123456, "packages and options")`)
/// for the active set, or `None` when counts are unavailable (e.g. the index has
/// no matching targets yet).
fn count_for(
    state: &AppState,
    page_state: &PageState,
    served_generation: &ServedGenerationSnapshot,
) -> Option<(usize, &'static str)> {
    let config = &state.config;

    let (source, ref_id, ref_set) = match &page_state.source_filter {
        SourceFilter::All => (None, None, page_state.active_ref_set()),
        SourceFilter::Named(source) => (
            Some(source.as_str()),
            page_state.source_ref.as_deref(),
            None,
        ),
    };

    let count = state
        .search
        .served_search_document_count_for_snapshot(served_generation, source, ref_id, ref_set)
        .ok()?;

    if count == 0 {
        return None;
    }

    Some((count, kind_noun_for(config, &page_state.source_filter)))
}

fn kind_noun_for(config: &AppConfig, source_filter: &SourceFilter) -> &'static str {
    match source_filter {
        SourceFilter::All => "packages and options",
        SourceFilter::Named(source) => config
            .sources
            .get(source)
            .map(|source| source_kind_noun(source.kind))
            .unwrap_or("entries"),
    }
}

/// Formats a count with `,` thousands separators (e.g. 123456 -> "123,456").
fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*byte as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_count_inserts_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(42), "42");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(123_456), "123,456");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }
}
