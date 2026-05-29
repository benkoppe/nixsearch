use maud::{Markup, html};

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::SearchDocument;
use nixsearch_index::search::{EntryLookup, EntryLookupResult, SearchIndex};

use crate::AppState;
use crate::request::{LinkOrigin, PageQuery, PageRequest, parse_document_kind, resolve_entry_ref};
use crate::urls::{
    close_url_for, entry_url_for, ref_id_for_link, ref_set_for_link, search_url_for,
};

use super::{detail, source_tag};

pub fn render(state: &AppState, request: &PageRequest) -> Markup {
    let Some(source) = request.source.as_deref() else {
        return render_empty();
    };

    let Some(entry) = request.entry.as_deref() else {
        return render_empty();
    };

    let ref_id = match resolve_entry_ref(&state.config, source, request.query.ref_id.as_deref()) {
        Ok(ref_id) => ref_id,
        Err(error) => return render_error(request, &format!("{error:#}")),
    };

    let kind = match parse_document_kind(request.query.kind.as_deref()) {
        Ok(kind) => kind,
        Err(error) => return render_error(request, &error),
    };

    let index_path = state
        .index_path
        .read()
        .expect("index path lock poisoned")
        .clone();

    let index = match SearchIndex::open(&index_path) {
        Ok(index) => index,
        Err(error) => return render_error(request, &format!("{error:#}")),
    };

    let lookup = EntryLookup {
        source: source.to_owned(),
        ref_id,
        name: entry.to_owned(),
        kind,
    };

    match index.find_entry(lookup) {
        Ok(EntryLookupResult::Found(document)) => render_entry(request, &document, &state.config),
        Ok(EntryLookupResult::NotFound) => render_error(request, "Entry not found."),
        Ok(EntryLookupResult::Ambiguous(documents)) => {
            render_ambiguous(request, &documents, &state.config)
        }
        Err(error) => render_error(request, &format!("{error:#}")),
    }
}

fn render_empty() -> Markup {
    html! {
        div #entry-modal-container {}
    }
}

fn render_entry(request: &PageRequest, document: &SearchDocument, config: &AppConfig) -> Markup {
    let common = document.common();
    let close_href = close_url_for(request);
    let source_color = source_tag::color_for_source(config, &common.source);
    let source_href = search_url_for(
        Some(&common.source),
        &PageQuery {
            q: request.query.q.clone(),
            ..PageQuery::default()
        },
    );

    html! {
        div #entry-modal-container {
            a.modal-backdrop href=(close_href) aria-label="Close modal" {}
            dialog #entry-modal data-close-url=(close_href) {
                article.entry {
                    header {
                        div {
                            div.entry-title-row {
                                h2 { (common.name) }
                                button.entry-copy
                                    type="button"
                                    data-copy-entry=(common.name)
                                    aria-label="Copy entry name"
                                    title="Copy"
                                    style=(format!("--source-color: {source_color};")) {
                                    svg.copy-icon xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" aria-hidden="true" {
                                        path d="M8 7a3 3 0 0 1 3-3h6a3 3 0 0 1 3 3v6a3 3 0 0 1-3 3h-1v1a3 3 0 0 1-3 3H7a3 3 0 0 1-3-3v-6a3 3 0 0 1 3-3h1V7Zm2 1h3a3 3 0 0 1 3 3v3h1a1 1 0 0 0 1-1V7a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v1Zm-3 2a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h6a1 1 0 0 0 1-1v-6a1 1 0 0 0-1-1H7Z" {}
                                    }
                                    svg.check-icon xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" aria-hidden="true" {
                                        path d="M9.55 17.6 4.7 12.75l1.4-1.4 3.45 3.45 8.35-8.35 1.4 1.4-9.75 9.75Z" {}
                                    }
                                }
                            }
                            div.meta {
                                (source_tag::render_link(config, &common.source, &source_href))
                                (common.kind.as_str()) " · " (common.ref_id)
                                @if let Some(revision) = &common.revision {
                                    " · " code { (&revision[..revision.len().min(8)]) }
                                }
                            }
                        }
                        a.entry-close href=(close_href) data-role="entry-close" autofocus { "✕ Close" }
                    }
                    (detail::render(document, config))
                }
            }
        }
    }
}

fn render_error(request: &PageRequest, message: &str) -> Markup {
    let close_href = close_url_for(request);

    html! {
        div #entry-modal-container {
            a.modal-backdrop href=(close_href) aria-label="Close modal" {}
            dialog #entry-modal data-close-url=(close_href) {
                article.entry {
                    header {
                        h2 { "Error" }
                        a.entry-close href=(close_href) data-role="entry-close" autofocus { "✕ Close" }
                    }
                    div.results-error { (message) }
                }
            }
        }
    }
}

fn render_ambiguous(
    request: &PageRequest,
    documents: &[SearchDocument],
    config: &AppConfig,
) -> Markup {
    let close_href = close_url_for(request);

    html! {
        div #entry-modal-container {
            a.modal-backdrop href=(close_href) aria-label="Close modal" {}
            dialog #entry-modal data-close-url=(close_href) {
                article.entry {
                    header {
                        h2 { "Multiple entries found" }
                        a.entry-close href=(close_href) data-role="entry-close" autofocus { "✕ Close" }
                    }
                    p { "This name exists in multiple forms. Choose one:" }
                    ul {
                        @for document in documents {
                            @let common = document.common();
                            @let from_scope = if request.source.is_none() || request.query.source == Some(LinkOrigin::All) {
                                Some(LinkOrigin::All)
                            } else {
                                None
                            };
                            @let href = entry_url_for(
                                &common.source,
                                &common.name,
                                Some(common.kind.as_str()),
                                &PageQuery {
                                    q: request.query.q.clone(),
                                    ref_id: ref_id_for_link(config, &common.source, &common.ref_id),
                                    ref_set: request
                                        .query
                                        .ref_set
                                        .as_deref()
                                        .and_then(|ref_set| ref_set_for_link(config, ref_set)),
                                    kind: None,
                                    source: from_scope,
                                    page: request.query.page,
                                },
                            );
                            li {
                                a href=(href) {
                                    (common.kind.as_str()) " · " (common.source) "/" (common.ref_id)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
