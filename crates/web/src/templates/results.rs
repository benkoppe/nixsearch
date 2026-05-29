use maud::{Markup, html};

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::{CommonDoc, SearchDocument};
use nixsearch_core::source_link::SourceLinkConfig;

use nixsearch_index::search::SearchHit;

use crate::DEFAULT_LIMIT;
use crate::request::{PageRequest, PageState, SourceFilter, page_state};
use crate::urls::{entry_url_for_hit, search_url_for_state};

use super::source_tag;

pub fn render(state: &PageState, hits: &[SearchHit], total: usize, config: &AppConfig) -> Markup {
    let Some(q) = state.q.as_deref() else {
        return render_empty();
    };

    if hits.is_empty() && total == 0 {
        return html! {
            div #results.results-status {
                "No results for " strong { (q) } "."
            }
        };
    }

    if hits.is_empty() {
        return html! {
            div #results.results-status {
                "No results on this page for " strong { (q) } "."
            }
        };
    }

    let show_source = state.source_filter == SourceFilter::All;
    let page = state.page.unwrap_or(1).max(1);
    let offset = (page - 1) * DEFAULT_LIMIT;

    html! {
        div #results aria-live="polite"
            data-total=(total)
            data-start-offset=(offset) {
            div.results-status {
                strong { (total) }
                " result" @if total != 1 { "s" }
                " for " strong { (q) }
            }
            table.results-table {
                thead {
                    tr {
                        th.col-name { "Name" }
                        th.col-desc { "Description" }
                    }
                }
                tbody #results-body {
                    @for (index, hit) in hits.iter().enumerate() {
                        @let result_offset = offset + index;
                        @let result_page = page_for_offset(result_offset);
                        (render_hit_row(state, hit, config, show_source, result_page, result_offset))
                    }
                }
            }
            @if page > 1 || offset + hits.len() < total {
                noscript {
                    nav.pagination {
                        @if page > 1 {
                            @let mut prev_state = state.clone();
                            @let _ = prev_state.page.replace(page - 1);
                            a.pagination-link href=(search_url_for_state(config, &prev_state)) { "← Previous" }
                        }
                        @if offset + hits.len() < total {
                            @let mut next_state = state.clone();
                            @let _ = next_state.page.replace(page + 1);
                            a.pagination-link href=(search_url_for_state(config, &next_state)) { "Next →" }
                        }
                    }
                }
            }
        }
    }
}

/// Renders just the table row HTML for a batch of hits (no wrapper element).
pub fn render_rows_only(
    request: &PageRequest,
    hits: &[SearchHit],
    config: &AppConfig,
    offset: usize,
) -> String {
    let state = page_state(config, request);
    let show_source = state.source_filter == SourceFilter::All;

    let markup = html! {
        @for (index, hit) in hits.iter().enumerate() {
            @let result_offset = offset + index;
            @let result_page = page_for_offset(result_offset);
            (render_hit_row(&state, hit, config, show_source, result_page, result_offset))
        }
    };

    markup.into_string()
}

pub fn render_empty() -> Markup {
    html! {
        div #results.results-empty {
            "Search packages and options across Nix sources."
        }
    }
}

pub fn render_error(error: &str) -> Markup {
    html! {
        div #results.results-error {
            strong { "Search failed:" } " " (error)
        }
    }
}

fn render_hit_row(
    state: &PageState,
    hit: &SearchHit,
    config: &AppConfig,
    show_source: bool,
    result_page: usize,
    result_offset: usize,
) -> Markup {
    let common = hit.document.common();
    let summary = summary_for_document(&hit.document);

    let entry_href = entry_url_for_hit(config, state, hit, None, Some(result_page));

    let desc = summary.as_deref().unwrap_or("");
    let source_color = source_tag::color_for_source(config, &common.source);
    let row_parity = if result_offset.is_multiple_of(2) {
        "even"
    } else {
        "odd"
    };

    html! {
        tr data-href=(entry_href) data-result-page=(result_page) data-row-parity=(row_parity) {
            td.col-name title=(common.name) {
                a.entry-name href=(entry_href)
                    style=(format!("--source-color: {source_color};")) {
                    @if show_source {
                        (source_tag::render(config, &common.source))
                    }
                    (common.name)
                }
            }
            td.col-desc title=(desc) {
                a.entry-desc href=(entry_href) tabindex="-1" { (desc) }
            }
        }
    }
}

fn page_for_offset(offset: usize) -> usize {
    (offset / DEFAULT_LIMIT) + 1
}

fn summary_for_document(document: &SearchDocument) -> Option<String> {
    match document {
        SearchDocument::Option(option) => option.description.as_ref().and_then(|description| {
            first_non_empty_line(description.plain_text().as_ref()).map(ToOwned::to_owned)
        }),
        SearchDocument::Package(package) => package
            .description
            .as_deref()
            .and_then(first_non_empty_line)
            .map(ToOwned::to_owned),
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

pub fn source_link_config_for_document<'a>(
    config: &'a AppConfig,
    common: &CommonDoc,
) -> Option<&'a SourceLinkConfig> {
    let source = config.sources.get(&common.source)?;
    let ref_config = source.refs.iter().find(|r| r.id == common.ref_id)?;
    ref_config.source_links.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::PageQuery;
    use nixsearch_core::document::{OptionDoc, SearchDocument};
    use nixsearch_core::ingest::IngestContext;
    use nixsearch_core::source_link::{Declaration, SourceLinkResolver};

    #[test]
    fn resolves_source_link_when_available() {
        let config = nixsearch_test_support::app_config("./data/indexes");

        let mut option = OptionDoc::new(
            &IngestContext {
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
        let common = document.common();
        let source_links = source_link_config_for_document(&config, common).unwrap();
        let resolver = SourceLinkResolver::new(source_links, common.revision.as_deref());

        let result = match &document {
            SearchDocument::Option(opt) => opt
                .declarations
                .iter()
                .find_map(|d| resolver.resolve_declaration(d)),
            _ => None,
        };

        assert_eq!(
            result.as_deref(),
            Some("https://github.com/example/repo/blob/abc123/module.nix#L4")
        );
    }

    #[test]
    fn render_empty_page_distinguishes_out_of_range_page() {
        let config = nixsearch_test_support::app_config("./data/indexes");
        let request = PageRequest {
            query: PageQuery {
                q: Some("git".to_owned()),
                ..Default::default()
            },
            ..Default::default()
        };

        let state = page_state(&config, &request);
        let html = render(&state, &[], 1_010, &config).into_string();

        assert!(html.contains("No results on this page"));
        assert!(!html.contains("No results for"));
    }
}
