use maud::{Markup, PreEscaped, html};

use nixsearch_config::app::AppConfig;
use nixsearch_config::source::SourceKind;

use crate::AppState;
use crate::request::{PageRequest, SourceFilter, non_empty};

use super::source_tag::color_for_source;

/// Inlined so the magnifying glass can be recolored per mode via `currentColor`.
static GLASS_SVG: &str = include_str!("../../magnify-glass.svg");

/// The muted Nix-logo blue used when searching every source ("All" mode).
const ALL_GLASS_COLOR: &str = "#5873B9";

pub fn render(state: &AppState, request: &PageRequest) -> Markup {
    let config = &state.config;
    let source_filter = SourceFilter::from_request(request);

    let (title_prefix, title_accent) = title_for(config, &source_filter);
    // "All" keeps the muted glass blue and lets the title fall back to the
    // logo's default blues; named sources tint both with the source color.
    let hero_style = match &source_filter {
        SourceFilter::All => format!("--glass-color: {ALL_GLASS_COLOR};"),
        SourceFilter::Named(source) => {
            let color = color_for_source(config, source);
            format!("--glass-color: {color}; --home-accent: {color};")
        }
    };
    let count = count_for(state, request, &source_filter);

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

fn source_display_name<'a>(config: &'a AppConfig, source_id: &'a str) -> &'a str {
    config
        .sources
        .get(source_id)
        .and_then(|source| source.name.as_deref())
        .unwrap_or(source_id)
}

/// Returns the entry count and its noun (e.g. `(123456, "packages and options")`)
/// for the active set, or `None` when counts are unavailable (e.g. the index has
/// no matching targets yet).
fn count_for(
    state: &AppState,
    request: &PageRequest,
    source_filter: &SourceFilter,
) -> Option<(usize, &'static str)> {
    let config = &state.config;

    let scopes = match source_filter {
        SourceFilter::All => config.resolve_search_scopes(
            None,
            None,
            request.query.ref_set.as_deref().and_then(non_empty),
        ),
        SourceFilter::Named(source) => config.resolve_search_scopes(
            Some(source),
            request.query.ref_id.as_deref().and_then(non_empty),
            None,
        ),
    }
    .ok()?;

    let manifest = state.manifest.read().expect("manifest lock poisoned");

    let count: usize = scopes
        .iter()
        .map(|scope| {
            manifest
                .targets
                .iter()
                .filter(|target| target.source == scope.source && target.ref_id == scope.ref_id)
                .map(|target| target.document_count)
                .sum::<usize>()
        })
        .sum();

    if count == 0 {
        return None;
    }

    Some((count, kind_noun_for(config, source_filter)))
}

fn kind_noun_for(config: &AppConfig, source_filter: &SourceFilter) -> &'static str {
    match source_filter {
        SourceFilter::All => "packages and options",
        SourceFilter::Named(source) => config
            .sources
            .get(source)
            .map(|source| kind_noun(source.kind))
            .unwrap_or("entries"),
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

    #[test]
    fn kind_noun_maps_source_kinds() {
        assert_eq!(kind_noun(SourceKind::Packages), "packages");
        assert_eq!(kind_noun(SourceKind::Options), "options");
        assert_eq!(kind_noun(SourceKind::Mixed), "packages and options");
    }
}
