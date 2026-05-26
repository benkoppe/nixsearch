use maud::{Markup, html};

use nixsearch_config::AppConfig;
use nixsearch_core::SearchDocument;
use nixsearch_index::{EntryLookup, EntryLookupResult, SearchIndex};

use crate::AppState;
use crate::request::{LinkOrigin, PageQuery, PageRequest, parse_document_kind, resolve_entry_ref};
use crate::urls::{close_url_for, entry_url_for, ref_id_for_link};

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

    html! {
        div #entry-modal-container {
            a.modal-backdrop href=(close_href) aria-label="Close modal" {}
            dialog #entry-modal data-close-url=(close_href) {
                article.entry {
                    header {
                        div {
                            h2 { (common.name) }
                            div.meta {
                                (source_tag::render(config, &common.source))
                                " " (common.kind.as_str()) " · " (common.ref_id)
                                @if let Some(revision) = &common.revision {
                                    " · " code { (&revision[..revision.len().min(8)]) }
                                }
                            }
                        }
                        a.entry-close href=(close_href) data-role="entry-close" { "✕ Close" }
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
                        a.entry-close href=(close_href) data-role="entry-close" { "✕ Close" }
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
                        a.entry-close href=(close_href) data-role="entry-close" { "✕ Close" }
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
