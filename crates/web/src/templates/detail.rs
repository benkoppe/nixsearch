use maud::{Markup, html};

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::{License, Maintainer, SearchDocument};
use nixsearch_core::source_link::SourceLinkResolver;

use crate::render_docs::{render_doc_text, render_doc_value};

use super::results::source_link_config_for_document;

pub fn render(document: &SearchDocument, config: &AppConfig) -> Markup {
    match document {
        SearchDocument::Option(option) => {
            html! {
                @if let Some(description) = &option.description {
                    (section("Description", render_doc_text(description)))
                }
                @if let Some(option_type) = &option.option_type {
                    (section("Type", html! { p { code { (option_type) } } }))
                }
                @if let Some(default) = &option.default {
                    (section("Default", render_doc_value(default)))
                }
                @if let Some(example) = &option.example {
                    (section("Example", render_doc_value(example)))
                }
                @if let Some(related_packages) = &option.related_packages {
                    (section("Related packages", render_doc_text(related_packages)))
                }
                @if !option.declarations.is_empty() {
                    @let resolver = source_link_config_for_document(config, &option.common)
                        .map(|cfg| SourceLinkResolver::new(cfg, option.common.revision.as_deref()));
                    (section("Declared", html! {
                        ul.plain-list {
                            @for declaration in &option.declarations {
                                li {
                                    @if let Some(url) = resolver.as_ref().and_then(|r| r.resolve_declaration(declaration)) {
                                        a href=(url) rel="noreferrer" { code { (declaration.name) } }
                                    } @else {
                                        code { (declaration.name) }
                                    }
                                }
                            }
                        }
                    }))
                }
            }
        }

        SearchDocument::Package(package) => {
            html! {
                @if let Some(description) = &package.description {
                    (section("Description", html! { p { (description) } }))
                }
                @let summary: Vec<(&str, &str)> = [
                    ("pname", package.pname.as_deref()),
                    ("version", package.version.as_deref()),
                ].into_iter()
                    .filter_map(|(k, v)| v.map(|val| (k, val)))
                    .collect();
                @if !summary.is_empty() {
                    (section("Package info", html! {
                        ul {
                            @for (key, value) in &summary {
                                li { (key) ": " code { (value) } }
                            }
                            @if let Some(broken) = package.broken {
                                li { "broken: " (broken) }
                            }
                        }
                    }))
                }
                @if let Some(main_program) = &package.main_program {
                    (code_tags_section("Main Program", std::slice::from_ref(main_program)))
                }
                @if !package.programs.is_empty() {
                    (code_tags_section("Programs", &package.programs))
                }
                @if let Some(long_description) = &package.long_description {
                    (section("Long description", html! { p { (long_description) } }))
                }
                @if !package.homepages.is_empty() {
                    (section("Homepages", html! {
                        ul.plain-list {
                            @for url in &package.homepages {
                                li {
                                    @if url.starts_with("http://") || url.starts_with("https://") {
                                        a href=(url) rel="noreferrer" { (url) }
                                    } @else {
                                        (url)
                                    }
                                }
                            }
                        }
                    }))
                }
                @if let Some(position) = &package.position {
                    @let resolver = source_link_config_for_document(config, &package.common)
                        .map(|cfg| SourceLinkResolver::new(cfg, package.common.revision.as_deref()));
                    @let url = resolver.as_ref().and_then(|r| r.resolve_package_position(position));
                    (section("Source", html! {
                        p {
                            @if let Some(href) = url {
                                a href=(href) rel="noreferrer" { code { (position) } }
                            } @else {
                                code { (position) }
                            }
                        }
                    }))
                }
                @if !package.platforms.is_empty() {
                    (section("Platforms", html! {
                        ul.tag-list {
                            @for platform in &package.platforms {
                                li { (platform) }
                            }
                        }
                    }))
                }
                @if !package.licenses.is_empty() {
                    (licenses_section(&package.licenses))
                }
                @if !package.maintainers.is_empty() {
                    (maintainers_section(&package.maintainers))
                }
            }
        }
    }
}

fn section(title: &str, body: Markup) -> Markup {
    html! {
        section.entry-section {
            h3 { (title) }
            (body)
        }
    }
}

fn licenses_section(licenses: &[License]) -> Markup {
    section(
        "Licenses",
        html! {
            ul.plain-list {
                @for license in licenses {
                    @let label = license.spdx_id.as_deref()
                        .or(license.name.as_deref())
                        .or(license.full_name.as_deref())
                        .unwrap_or("unknown");
                    li {
                        @if let Some(url) = &license.url {
                            a href=(url) rel="noreferrer" { (label) }
                        } @else {
                            (label)
                        }
                    }
                }
            }
        },
    )
}

fn maintainers_section(maintainers: &[Maintainer]) -> Markup {
    section(
        "Maintainers",
        html! {
            ul.tag-list {
                @for maintainer in maintainers {
                    @let label = maintainer.name.as_deref()
                        .or(maintainer.github.as_deref())
                        .or(maintainer.email.as_deref())
                        .unwrap_or("unknown");
                    li { (label) }
                }
            }
        },
    )
}

fn code_tags_section(title: &str, values: &[String]) -> Markup {
    section(
        title,
        html! {
            ul.tag-list.code-tags {
                @for value in values {
                    li { code { (value) } }
                }
            }
        },
    )
}
