use serde::Serialize;
use unicode_segmentation::UnicodeSegmentation;

use nixsearch_config::app::AppConfig;
use nixsearch_core::document::SearchDocument;
use nixsearch_index::search::{SearchHit, SearchResult};
use nixsearch_service::{SeoFactsResult, ServedGenerationSnapshot};

use crate::AppState;
use crate::entry::EntryData;
use crate::origin::PageUrls;
use crate::request::{
    PageRequest, PageState, PublicRoute, SourceFilter, non_empty, normalized_query,
};
use crate::robots::ROBOTS_NOINDEX_FOLLOW;
use crate::source_labels::{source_display_name, source_kind_noun};
use crate::urls::{
    canonical_entry_path_for_document, canonical_home_path, canonical_search_path,
    canonical_source_path,
};

const DEFAULT_DESCRIPTION: &str = "Search the Nix ecosystem";
const META_DESCRIPTION_MAX_GRAPHEMES: usize = 160;
const META_DESCRIPTION_WORD_BOUNDARY_MIN_GRAPHEMES: usize = 120;
const DESCRIPTION_ELLIPSIS: &str = "…";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageMetadata {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) open_graph: Option<OpenGraphMetadata>,
    pub(crate) canonical_url: Option<String>,
    pub(crate) robots: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenGraphMetadata {
    pub(crate) url: String,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) site_name: &'static str,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) image_url: String,
}

#[derive(Clone, Copy)]
pub(crate) enum MetadataContent<'a> {
    Home,
    SearchResults(&'a SearchResult),
    DirectEntry,
    Error { title: &'a str, message: &'a str },
}

pub(crate) struct PageHeadMetadataInput<'a> {
    pub(crate) state: &'a AppState,
    pub(crate) request: &'a PageRequest,
    pub(crate) page_state: &'a PageState,
    pub(crate) page_urls: &'a PageUrls,
    pub(crate) snapshot: &'a ServedGenerationSnapshot,
    pub(crate) content: MetadataContent<'a>,
    pub(crate) entry: &'a EntryData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexMetadata {
    Canonical { canonical_url: String },
    NoIndex,
}

impl IndexMetadata {
    fn canonical_url(&self) -> Option<&str> {
        match self {
            Self::Canonical { canonical_url } => Some(canonical_url),
            Self::NoIndex => None,
        }
    }

    fn into_head_fields(self) -> (Option<String>, Option<&'static str>) {
        match self {
            Self::Canonical { canonical_url } => (Some(canonical_url), None),
            Self::NoIndex => (None, Some(ROBOTS_NOINDEX_FOLLOW)),
        }
    }
}

pub(crate) fn page_head_metadata(input: PageHeadMetadataInput<'_>) -> PageMetadata {
    let PageHeadMetadataInput {
        state,
        request,
        page_state,
        page_urls,
        snapshot,
        content,
        entry,
    } = input;

    let search_result_for_metadata = match content {
        MetadataContent::SearchResults(result) => Some(result),
        MetadataContent::Home | MetadataContent::DirectEntry | MetadataContent::Error { .. } => {
            None
        }
    };

    let index_metadata = page_index_metadata(
        state, request, page_state, snapshot, content, entry, page_urls,
    );

    let mut metadata = page_metadata(
        &state.config,
        request,
        &page_state.source_filter,
        search_result_for_metadata,
        entry,
        page_urls,
        index_metadata,
    );

    if let MetadataContent::Error { title, message } = content {
        let description = meta_description(message);
        metadata.title = title.to_owned();
        metadata.description = description;
        metadata.open_graph = open_graph_metadata(
            state.config.public_seo_enabled(),
            page_urls,
            metadata.canonical_url.as_deref(),
            title,
            &metadata.description,
        );
    }

    metadata
}

pub(crate) fn noindex_head_metadata(
    public_seo_enabled: bool,
    page_urls: &PageUrls,
    title: &str,
    description: &str,
) -> PageMetadata {
    let description = meta_description(description);
    let open_graph = open_graph_metadata(public_seo_enabled, page_urls, None, title, &description);

    PageMetadata {
        title: title.to_owned(),
        description,
        open_graph,
        canonical_url: None,
        robots: Some(ROBOTS_NOINDEX_FOLLOW),
    }
}

fn page_metadata(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    search_result: Option<&SearchResult>,
    entry: &EntryData,
    page_urls: &PageUrls,
    index_metadata: IndexMetadata,
) -> PageMetadata {
    let title = title_for_entry(config, request, source_filter, entry.document());
    let description = meta_description(&description_for(
        config,
        request,
        source_filter,
        search_result,
        entry,
    ));
    let open_graph = open_graph_metadata(
        config.public_seo_enabled(),
        page_urls,
        index_metadata.canonical_url(),
        &title,
        &description,
    );
    let (canonical_url, robots) = index_metadata.into_head_fields();

    PageMetadata {
        title,
        description,
        open_graph,
        canonical_url,
        robots,
    }
}

fn open_graph_metadata(
    public_seo_enabled: bool,
    page_urls: &PageUrls,
    canonical_url: Option<&str>,
    title: &str,
    description: &str,
) -> Option<OpenGraphMetadata> {
    let canonical_url = canonical_url?;

    public_seo_enabled.then(|| OpenGraphMetadata {
        url: canonical_url.to_owned(),
        kind: "website",
        site_name: "nixsearch",
        title: title.to_owned(),
        description: description.to_owned(),
        image_url: page_urls.image_url.clone(),
    })
}

fn page_index_metadata(
    state: &AppState,
    request: &PageRequest,
    page_state: &PageState,
    served_generation: &ServedGenerationSnapshot,
    content: MetadataContent<'_>,
    entry: &EntryData,
    page_urls: &PageUrls,
) -> IndexMetadata {
    if !state.config.public_seo_enabled() {
        return noindex_metadata();
    }

    if matches!(content, MetadataContent::Error { .. }) {
        return noindex_metadata();
    }

    if request
        .query
        .ref_set
        .as_deref()
        .and_then(non_empty)
        .is_some()
    {
        return noindex_metadata();
    }

    match entry {
        EntryData::Found(entry) => {
            let document = &entry.document;

            if !entry.annotation.unique_within_kind {
                return noindex_metadata();
            }

            if !document.is_seo_eligible_entry() {
                return noindex_metadata();
            }

            if !matches!(
                state
                    .search
                    .document_allowed_for_public_seo(served_generation, document),
                Ok(true)
            ) {
                return noindex_metadata();
            }

            return canonical_metadata(
                page_urls.absolute_url(&canonical_entry_path_for_document(&state.config, document)),
            );
        }
        EntryData::NotFound { .. } | EntryData::Ambiguous(_) | EntryData::Error(_) => {
            return noindex_metadata();
        }
        EntryData::Empty => {}
    }

    if page_state.detail.is_some() {
        return noindex_metadata();
    }

    if let Some(q) = normalized_query(&request.query) {
        return search_index_metadata(
            state,
            request,
            page_state,
            served_generation,
            content,
            page_urls,
            q,
        );
    }

    if request.query.page.unwrap_or(1) > 1 {
        return noindex_metadata();
    }

    match &page_state.source_filter {
        SourceFilter::All => {
            if matches!(request.route, PublicRoute::Home)
                && request
                    .query
                    .ref_id
                    .as_deref()
                    .and_then(non_empty)
                    .is_none()
                && request.query.source.is_none()
            {
                canonical_metadata(page_urls.absolute_url(&canonical_home_path()))
            } else {
                noindex_metadata()
            }
        }
        SourceFilter::Named(source) => {
            if request.route.is_entry() || request.query.source.is_some() {
                return noindex_metadata();
            }

            let Some(ref_id) = page_state.source_ref.as_deref() else {
                return noindex_metadata();
            };

            source_index_metadata(
                state
                    .search
                    .source_has_indexable_entries(served_generation, source, ref_id),
                page_urls.absolute_url(&canonical_source_path(&state.config, source, ref_id)),
            )
        }
    }
}

fn search_index_metadata(
    state: &AppState,
    request: &PageRequest,
    page_state: &PageState,
    served_generation: &ServedGenerationSnapshot,
    content: MetadataContent<'_>,
    page_urls: &PageUrls,
    q: &str,
) -> IndexMetadata {
    let MetadataContent::SearchResults(result) = content else {
        return noindex_metadata();
    };

    if result.total == 0 || request.query.page.unwrap_or(1) > 1 || request.query.source.is_some() {
        return noindex_metadata();
    }

    let PublicRoute::Source { source } = &request.route else {
        return noindex_metadata();
    };

    if !matches!(&page_state.source_filter, SourceFilter::Named(named_source) if named_source == source)
    {
        return noindex_metadata();
    }

    let Some(ref_id) = page_state.source_ref.as_deref() else {
        return noindex_metadata();
    };

    if !source_uses_default_ref(&state.config, source, ref_id) {
        return noindex_metadata();
    }

    if !search_result_has_public_seo_hit(state, served_generation, &result.hits) {
        return noindex_metadata();
    }

    canonical_metadata(page_urls.absolute_url(&canonical_search_path(
        &state.config,
        source,
        ref_id,
        q,
    )))
}

fn search_result_has_public_seo_hit(
    state: &AppState,
    served_generation: &ServedGenerationSnapshot,
    hits: &[SearchHit],
) -> bool {
    hits.iter().any(|hit| {
        hit.document.is_seo_eligible_entry()
            && matches!(
                state
                    .search
                    .document_allowed_for_public_seo(served_generation, &hit.document),
                Ok(true)
            )
    })
}

fn source_uses_default_ref(config: &AppConfig, source: &str, ref_id: &str) -> bool {
    config
        .sources
        .get(source)
        .and_then(|source| source.default_ref.as_deref())
        == Some(ref_id)
}

fn source_index_metadata(
    has_indexable_entries: SeoFactsResult<bool>,
    canonical_url: String,
) -> IndexMetadata {
    match has_indexable_entries {
        Ok(true) => canonical_metadata(canonical_url),
        Ok(false) | Err(_) => noindex_metadata(),
    }
}

fn canonical_metadata(canonical_url: String) -> IndexMetadata {
    IndexMetadata::Canonical { canonical_url }
}

fn noindex_metadata() -> IndexMetadata {
    IndexMetadata::NoIndex
}

#[cfg(test)]
fn title_for(config: &AppConfig, request: &PageRequest, source_filter: &SourceFilter) -> String {
    title_for_entry(config, request, source_filter, None)
}

fn title_for_entry(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    entry_document: Option<&SearchDocument>,
) -> String {
    let mut parts = Vec::new();

    if let Some(document) = entry_document {
        parts.push(document.common().name.to_owned());
    } else if let PublicRoute::Entry { entry, .. } = &request.route
        && let Some(entry) = non_empty(entry)
    {
        parts.push(entry.to_owned());
    } else if let Some(q) = normalized_query(&request.query) {
        parts.push(q.to_owned());
    }

    if let SourceFilter::Named(source_id) = source_filter {
        parts.push(source_display_name(config, source_id).to_owned());
    }

    parts.push("nixsearch".to_owned());
    parts.join(" · ")
}

fn description_for(
    config: &AppConfig,
    request: &PageRequest,
    source_filter: &SourceFilter,
    search_result: Option<&SearchResult>,
    entry: &EntryData,
) -> String {
    if let Some(document) = entry.document() {
        return description_for_document(config, document);
    }

    if let (Some(result), Some(q)) = (search_result, normalized_query(&request.query)) {
        return format!("{} results for {q}", result.total);
    }

    default_description_for(config, source_filter)
}

fn default_description_for(config: &AppConfig, source_filter: &SourceFilter) -> String {
    match source_filter {
        SourceFilter::All => DEFAULT_DESCRIPTION.to_owned(),
        SourceFilter::Named(source) => {
            let source_config = config.sources.get(source);
            let source_name = source_config
                .and_then(|source| source.name.as_deref())
                .unwrap_or(source);
            let kind = source_config
                .map(|source| source_kind_noun(source.kind))
                .unwrap_or("entries");

            format!("Search {source_name} {kind}")
        }
    }
}

fn description_for_document(config: &AppConfig, document: &SearchDocument) -> String {
    let common = document.common();
    let source = source_display_name(config, &common.source);

    match document {
        SearchDocument::Option(option) => option
            .description
            .as_ref()
            .and_then(|description| {
                first_non_empty_line(description.plain_text().as_ref()).map(ToOwned::to_owned)
            })
            .map(|description| format!("{} · {description}", common.name))
            .unwrap_or_else(|| format!("{} · {source}", common.name)),
        SearchDocument::Package(package) => {
            let name = package
                .version
                .as_deref()
                .and_then(crate::request::non_empty)
                .map(|version| format!("{} {version}", common.name))
                .unwrap_or_else(|| common.name.clone());
            let description = package
                .description
                .as_deref()
                .and_then(first_non_empty_line)
                .unwrap_or(source);

            format!("{name} · {description}")
        }
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn meta_description(value: &str) -> String {
    truncate_description(&collapse_description_whitespace(value))
}

fn collapse_description_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_description(value: &str) -> String {
    if value.graphemes(true).count() <= META_DESCRIPTION_MAX_GRAPHEMES {
        return value.to_owned();
    }

    let ellipsis_graphemes = DESCRIPTION_ELLIPSIS.graphemes(true).count();
    let max_content_graphemes = META_DESCRIPTION_MAX_GRAPHEMES.saturating_sub(ellipsis_graphemes);
    let mut cutoff_byte = value.len();
    let mut word_boundary_cutoff = None;

    for (grapheme_index, (byte_index, grapheme)) in value.grapheme_indices(true).enumerate() {
        if grapheme_index == max_content_graphemes {
            cutoff_byte = byte_index;
            break;
        }

        if grapheme == " " && grapheme_index >= META_DESCRIPTION_WORD_BOUNDARY_MIN_GRAPHEMES {
            word_boundary_cutoff = Some(byte_index);
        }
    }

    let cutoff_byte = word_boundary_cutoff.unwrap_or(cutoff_byte);
    let mut description = value[..cutoff_byte].trim_end().to_owned();
    description.push_str(DESCRIPTION_ELLIPSIS);
    description
}

#[cfg(test)]
mod tests {
    use nixsearch_core::document::{DocText, OptionDoc, PackageDoc, SearchDocument};
    use nixsearch_core::ingest::IngestContext;
    use nixsearch_index::annotation::SearchHitAnnotation;
    use nixsearch_index::search::SearchResult;
    use nixsearch_test_support::{
        SOURCE_FIXTURES, app_config, app_config_with_public_url, utf8_path_buf,
    };
    use tempfile::tempdir;
    use unicode_segmentation::UnicodeSegmentation;

    use crate::entry::{AnnotatedEntryDocument, EntryData};
    use crate::origin::PageUrls;
    use crate::request::{PageQuery, PageRequest, PublicRoute, SourceFilter};

    use super::{
        IndexMetadata, META_DESCRIPTION_MAX_GRAPHEMES, description_for, meta_description,
        noindex_head_metadata, page_metadata, title_for, title_for_entry,
    };

    fn config() -> nixsearch_config::app::AppConfig {
        let tempdir = tempdir().unwrap();
        app_config(utf8_path_buf(tempdir.path().join("indexes")))
    }

    fn public_config() -> nixsearch_config::app::AppConfig {
        let tempdir = tempdir().unwrap();
        app_config_with_public_url(utf8_path_buf(tempdir.path().join("indexes")))
    }

    fn page_urls() -> PageUrls {
        PageUrls {
            current_url: "https://search.example.com/?q=git".to_owned(),
            image_url: "https://search.example.com/apple-touch-icon.png".to_owned(),
            origin: "https://search.example.com".to_owned(),
        }
    }

    fn ingest_context() -> IngestContext {
        IngestContext {
            source: SOURCE_FIXTURES.to_owned(),
            ref_id: "small".to_owned(),
            revision: None,
            repo: None,
        }
    }

    fn home_request(query: PageQuery) -> PageRequest {
        PageRequest {
            route: PublicRoute::Home,
            query,
        }
    }

    fn source_request(source: &str, query: PageQuery) -> PageRequest {
        PageRequest {
            route: PublicRoute::Source {
                source: source.to_owned(),
            },
            query,
        }
    }

    fn entry_request(source: &str, entry: &str, query: PageQuery) -> PageRequest {
        PageRequest {
            route: PublicRoute::Entry {
                source: source.to_owned(),
                entry: entry.to_owned(),
            },
            query,
        }
    }

    fn found_entry(document: SearchDocument) -> EntryData {
        EntryData::Found(AnnotatedEntryDocument {
            annotation: SearchHitAnnotation {
                unique_within_kind: true,
            },
            document: Box::new(document),
        })
    }

    #[test]
    fn title_includes_query_and_named_source() {
        let config = config();
        let request = source_request(
            SOURCE_FIXTURES,
            PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        );

        assert_eq!(
            title_for(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned())
            ),
            "git · Fixtures · nixsearch"
        );
    }

    #[test]
    fn title_omits_all_source_filter() {
        let config = config();
        let request = home_request(PageQuery {
            q: Some("git".to_owned()),
            ..PageQuery::default()
        });

        assert_eq!(
            title_for(&config, &request, &SourceFilter::All),
            "git · nixsearch"
        );
    }

    #[test]
    fn title_includes_named_source_without_query() {
        let config = config();
        let request = source_request(SOURCE_FIXTURES, PageQuery::default());

        assert_eq!(
            title_for(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned())
            ),
            "Fixtures · nixsearch"
        );
    }

    #[test]
    fn title_uses_entry_document_when_present() {
        let config = config();
        let document = SearchDocument::Package(PackageDoc::new(&ingest_context(), "git"));
        let request = entry_request(
            SOURCE_FIXTURES,
            "git",
            PageQuery {
                q: Some("version control".to_owned()),
                ..PageQuery::default()
            },
        );

        assert_eq!(
            title_for_entry(
                &config,
                &request,
                &SourceFilter::Named(SOURCE_FIXTURES.to_owned()),
                Some(&document),
            ),
            "git · Fixtures · nixsearch"
        );
    }

    #[test]
    fn metadata_describes_home_page() {
        let config = public_config();
        let request = PageRequest::default();
        let search = SearchResult {
            hits: Vec::new(),
            total: 0,
        };
        let metadata = page_metadata(
            &config,
            &request,
            &SourceFilter::All,
            Some(&search),
            &EntryData::Empty,
            &page_urls(),
            IndexMetadata::NoIndex,
        );

        assert_eq!(metadata.title, "nixsearch");
        assert_eq!(metadata.description, "Search the Nix ecosystem");
        assert!(metadata.open_graph.is_none());
    }

    #[test]
    fn metadata_omits_open_graph_without_public_url() {
        let config = config();
        let request = PageRequest::default();
        let search = SearchResult {
            hits: Vec::new(),
            total: 0,
        };

        let metadata = page_metadata(
            &config,
            &request,
            &SourceFilter::All,
            Some(&search),
            &EntryData::Empty,
            &page_urls(),
            IndexMetadata::NoIndex,
        );

        assert_eq!(metadata.open_graph, None);
    }

    #[test]
    fn metadata_describes_source_page() {
        let config = config();
        let request = source_request(SOURCE_FIXTURES, PageQuery::default());
        let search = SearchResult {
            hits: Vec::new(),
            total: 0,
        };
        let metadata = page_metadata(
            &config,
            &request,
            &SourceFilter::Named(SOURCE_FIXTURES.to_owned()),
            Some(&search),
            &EntryData::Empty,
            &page_urls(),
            IndexMetadata::NoIndex,
        );

        assert_eq!(metadata.description, "Search Fixtures options");
    }

    #[test]
    fn metadata_describes_search_results() {
        let config = config();
        let request = home_request(PageQuery {
            q: Some("git".to_owned()),
            ..PageQuery::default()
        });
        let search = SearchResult {
            hits: Vec::new(),
            total: 59_526,
        };

        assert_eq!(
            description_for(
                &config,
                &request,
                &SourceFilter::All,
                Some(&search),
                &EntryData::Empty
            ),
            "59526 results for git"
        );
    }

    #[test]
    fn metadata_describes_package_entry() {
        let config = config();
        let mut package = PackageDoc::new(&ingest_context(), "git");
        package.version = Some("2.54.0".to_owned());
        package.description = Some("Distributed version control system\nextra".to_owned());
        let document = SearchDocument::Package(package);

        assert_eq!(
            description_for(
                &config,
                &PageRequest::default(),
                &SourceFilter::All,
                None,
                &found_entry(document)
            ),
            "git 2.54.0 · Distributed version control system"
        );
    }

    #[test]
    fn metadata_describes_option_entry() {
        let config = config();
        let mut option = OptionDoc::new(&ingest_context(), "programs.git.enable");
        option.description = Some(DocText::Markdown(
            "Enable Git support.\nMore details.".to_owned(),
        ));
        let document = SearchDocument::Option(option);

        assert_eq!(
            description_for(
                &config,
                &PageRequest::default(),
                &SourceFilter::All,
                None,
                &found_entry(document)
            ),
            "programs.git.enable · Enable Git support."
        );
    }

    #[test]
    fn meta_description_collapses_whitespace() {
        assert_eq!(
            meta_description("  First line.\n\nSecond\tline.  "),
            "First line. Second line."
        );
    }

    #[test]
    fn meta_description_truncates_at_word_boundary() {
        let description = meta_description(&format!(
            "{} final words that should be removed",
            "word ".repeat(40)
        ));

        assert!(description.ends_with('…'));
        assert!(description.graphemes(true).count() <= META_DESCRIPTION_MAX_GRAPHEMES);
        assert!(!description.contains("removed"));
    }

    #[test]
    fn meta_description_truncates_grapheme_safely_without_word_boundary() {
        let description = meta_description(&"👩🏽‍💻".repeat(200));

        assert!(description.ends_with('…'));
        assert_eq!(
            description.graphemes(true).count(),
            META_DESCRIPTION_MAX_GRAPHEMES
        );
        assert!(std::str::from_utf8(description.as_bytes()).is_ok());
    }

    #[test]
    fn noindex_metadata_caps_error_description() {
        let metadata =
            noindex_head_metadata(true, &page_urls(), "Request failed", &"x".repeat(400));

        assert!(metadata.description.ends_with('…'));
        assert!(metadata.description.graphemes(true).count() <= META_DESCRIPTION_MAX_GRAPHEMES);
    }
}
