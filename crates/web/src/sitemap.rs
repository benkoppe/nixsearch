use std::ops::Range;
use std::sync::{Arc, Mutex};

const MAX_SITEMAP_URLS: usize = 50_000;
const MAX_SITEMAP_BYTES: usize = 50 * 1024 * 1024;
const MAX_SITEMAP_INDEX_ENTRIES: usize = 50_000;
const MAX_SITEMAP_INDEX_BYTES: usize = 50 * 1024 * 1024;
const SITEMAP_XML_NAMESPACE: &str = "http://www.sitemaps.org/schemas/sitemap/0.9";
const SITEMAP_SHARD_QUERY_PARAM: &str = "shard";
const SITEMAP_SHARD_WIDTH: usize = 5;
const SITEMAP_MAX_SHARD_NUMBER: usize = 99_999;
const SITEMAP_XML_DECLARATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;
const SITEMAP_URLSET_CLOSE: &str = "</urlset>";
const SITEMAP_INDEX_CLOSE: &str = "</sitemapindex>";

#[derive(Debug, Clone, Copy)]
pub(crate) struct SitemapLimits {
    pub max_urlset_urls: usize,
    pub max_urlset_bytes: usize,
    pub max_index_entries: usize,
    pub max_index_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SitemapShard {
    number: usize,
    query_value: String,
    path_range: Range<usize>,
    byte_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SitemapDocument {
    Urlset(String),
    Index(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SitemapQuery {
    EntryPoint,
    Shard(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SitemapQueryError {
    UnknownQuery,
    DuplicateShard,
    EmptyShard,
    MalformedShard,
    ShardOutOfRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SitemapRenderError {
    MalformedQuery(SitemapQueryError),
    ShardNotAvailable,
    ShardOutOfRange,
    IndexTooLarge,
    NoRepresentableUrls,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SitemapCacheKey {
    generation_id: String,
    origin: String,
}

impl SitemapCacheKey {
    pub(crate) fn new(generation_id: String, origin: String) -> Self {
        Self {
            generation_id,
            origin,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SitemapCache {
    inner: Arc<Mutex<Option<CachedSitemapPlan>>>,
}

#[derive(Debug, Clone)]
struct CachedSitemapPlan {
    key: SitemapCacheKey,
    plan: Arc<SitemapPlan>,
}

impl SitemapCache {
    pub(crate) fn get_or_try_insert_with<E>(
        &self,
        key: SitemapCacheKey,
        build: impl FnOnce() -> Result<SitemapPlan, E>,
    ) -> Result<Arc<SitemapPlan>, E> {
        let mut cached = self
            .inner
            .lock()
            .expect("sitemap cache mutex should not be poisoned");

        if let Some(cached) = cached.as_ref()
            && cached.key == key
        {
            return Ok(Arc::clone(&cached.plan));
        }

        let plan = Arc::new(build()?);
        *cached = Some(CachedSitemapPlan {
            key,
            plan: Arc::clone(&plan),
        });

        Ok(plan)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SitemapPlan {
    origin: String,
    paths: Vec<String>,
    shards: Vec<SitemapShard>,
    limits: SitemapLimits,
}

impl SitemapPlan {
    pub(crate) fn new(
        origin: String,
        mut paths: Vec<String>,
        limits: SitemapLimits,
    ) -> Result<Self, SitemapRenderError> {
        paths.sort();
        paths.dedup();
        let paths = representable_sitemap_paths(&origin, paths, limits)?;
        let shards = shard_sitemap_paths(&origin, &paths, limits)?;

        Ok(Self {
            origin,
            paths,
            shards,
            limits,
        })
    }

    pub(crate) fn render(
        &self,
        raw_query: Option<&str>,
    ) -> Result<SitemapDocument, SitemapRenderError> {
        let query = parse_sitemap_query(raw_query).map_err(SitemapRenderError::MalformedQuery)?;

        let sitemap_index = if self.shards.len() > 1 {
            Some(render_sitemap_index(
                &self.origin,
                &self.shards,
                self.limits,
            )?)
        } else {
            None
        };

        match query {
            SitemapQuery::EntryPoint if self.shards.len() == 1 => Ok(SitemapDocument::Urlset(
                self.render_urlset_for_shard(&self.shards[0]),
            )),
            SitemapQuery::EntryPoint => Ok(SitemapDocument::Index(
                sitemap_index.expect("multi-shard sitemap index is rendered"),
            )),
            SitemapQuery::Shard(_) if self.shards.len() <= 1 => {
                Err(SitemapRenderError::ShardNotAvailable)
            }
            SitemapQuery::Shard(number) => self
                .shards
                .iter()
                .find(|shard| shard.number == number)
                .map(|shard| SitemapDocument::Urlset(self.render_urlset_for_shard(shard)))
                .ok_or(SitemapRenderError::ShardOutOfRange),
        }
    }

    fn render_urlset_for_shard(&self, shard: &SitemapShard) -> String {
        render_urlset_from_paths(
            &self.origin,
            &self.paths[shard.path_range.clone()],
            shard.byte_len,
        )
    }
}

pub(crate) fn protocol_sitemap_limits() -> SitemapLimits {
    SitemapLimits {
        max_urlset_urls: MAX_SITEMAP_URLS,
        max_urlset_bytes: MAX_SITEMAP_BYTES,
        max_index_entries: MAX_SITEMAP_INDEX_ENTRIES,
        max_index_bytes: MAX_SITEMAP_INDEX_BYTES,
    }
}

#[cfg(test)]
pub(crate) fn render_sitemap_entrypoint(
    origin: &str,
    paths: Vec<String>,
    raw_query: Option<&str>,
    limits: SitemapLimits,
) -> Result<SitemapDocument, SitemapRenderError> {
    let plan = SitemapPlan::new(origin.to_owned(), paths, limits)?;
    plan.render(raw_query)
}

pub(crate) fn validate_sitemap_query(raw_query: Option<&str>) -> Result<(), SitemapQueryError> {
    parse_sitemap_query(raw_query).map(|_| ())
}

fn parse_sitemap_query(raw_query: Option<&str>) -> Result<SitemapQuery, SitemapQueryError> {
    let Some(raw_query) = raw_query else {
        return Ok(SitemapQuery::EntryPoint);
    };

    let shard_prefix = format!("{SITEMAP_SHARD_QUERY_PARAM}=");
    let shard_count = raw_query
        .split('&')
        .filter(|part| part.starts_with(&shard_prefix))
        .count();

    if shard_count > 1 {
        return Err(SitemapQueryError::DuplicateShard);
    }

    if raw_query.contains('&') || !raw_query.starts_with(&shard_prefix) {
        return Err(SitemapQueryError::UnknownQuery);
    }

    let value = &raw_query[shard_prefix.len()..];
    let number = parse_sitemap_shard_query_value(value)?;

    Ok(SitemapQuery::Shard(number))
}

fn parse_sitemap_shard_query_value(value: &str) -> Result<usize, SitemapQueryError> {
    if value.is_empty() {
        return Err(SitemapQueryError::EmptyShard);
    }

    if value.len() != SITEMAP_SHARD_WIDTH || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SitemapQueryError::MalformedShard);
    }

    let number = value
        .parse::<usize>()
        .map_err(|_| SitemapQueryError::MalformedShard)?;

    if number == 0 || number > SITEMAP_MAX_SHARD_NUMBER {
        return Err(SitemapQueryError::ShardOutOfRange);
    }

    Ok(number)
}

fn sitemap_shard_query_value(number: usize) -> Option<String> {
    (1..=SITEMAP_MAX_SHARD_NUMBER)
        .contains(&number)
        .then(|| format!("{number:0SITEMAP_SHARD_WIDTH$}"))
}

fn sitemap_shard_location_path_and_query(number: usize) -> Option<String> {
    sitemap_shard_query_value(number)
        .map(|value| format!("/sitemap.xml?{SITEMAP_SHARD_QUERY_PARAM}={value}"))
}

#[cfg(test)]
fn render_url_entry(origin: &str, path: &str) -> String {
    render_loc_entry("url", origin, path)
}

fn render_index_entry(origin: &str, shard_number: usize) -> Option<String> {
    let path = sitemap_shard_location_path_and_query(shard_number)?;

    Some(render_loc_entry("sitemap", origin, &path))
}

fn render_loc_entry(element: &str, origin: &str, path: &str) -> String {
    let mut rendered = String::with_capacity(render_loc_entry_len(element, origin, path));
    push_loc_entry(&mut rendered, element, origin, path);
    rendered
}

fn render_loc_entry_len(element: &str, origin: &str, path: &str) -> usize {
    (2 * element.len()) + 16 + encoded_text_len(origin) + encoded_text_len(path)
}

fn encoded_text_len(value: &str) -> usize {
    value.bytes().fold(0, |len, byte| {
        len + match byte {
            b'&' => "&amp;".len(),
            b'<' => "&lt;".len(),
            b'>' => "&gt;".len(),
            _ => 1,
        }
    })
}

fn push_loc_entry(output: &mut String, element: &str, origin: &str, path: &str) {
    output.push('<');
    output.push_str(element);
    output.push_str("><loc>");
    let url = format!("{origin}{path}");
    html_escape::encode_text_to_string(&url, output);
    output.push_str("</loc></");
    output.push_str(element);
    output.push('>');
}

#[cfg(test)]
fn render_urlset_from_entries(entries: &[String]) -> String {
    let entries_len = entries.iter().map(String::len).sum();
    let mut rendered = String::with_capacity(urlset_document_len(entries_len));
    rendered.push_str(&urlset_prefix());
    for entry in entries {
        rendered.push_str(entry);
    }
    rendered.push_str(SITEMAP_URLSET_CLOSE);
    rendered
}

fn render_urlset_from_paths(origin: &str, paths: &[String], byte_len: usize) -> String {
    let mut rendered = String::with_capacity(byte_len);
    rendered.push_str(&urlset_prefix());
    for path in paths {
        push_loc_entry(&mut rendered, "url", origin, path);
    }
    rendered.push_str(SITEMAP_URLSET_CLOSE);
    rendered
}

fn render_sitemap_index_from_entries(entries: &[String]) -> String {
    let entries_len = entries.iter().map(String::len).sum();
    let mut rendered = String::with_capacity(sitemap_index_document_len(entries_len));
    rendered.push_str(&sitemap_index_prefix());
    for entry in entries {
        rendered.push_str(entry);
    }
    rendered.push_str(SITEMAP_INDEX_CLOSE);
    rendered
}

fn shard_sitemap_paths(
    origin: &str,
    paths: &[String],
    limits: SitemapLimits,
) -> Result<Vec<SitemapShard>, SitemapRenderError> {
    if limits.max_urlset_urls == 0 || urlset_document_len(0) > limits.max_urlset_bytes {
        return Err(SitemapRenderError::NoRepresentableUrls);
    }

    let mut shards = Vec::new();
    let mut shard_start = 0;
    let mut entries_count = 0;
    let mut entries_len = 0;

    for (path_index, path) in paths.iter().enumerate() {
        let entry_len = render_loc_entry_len("url", origin, path);

        let next_count = entries_count + 1;
        let next_len = urlset_document_len(entries_len + entry_len);
        if entries_count > 0
            && (next_count > limits.max_urlset_urls || next_len > limits.max_urlset_bytes)
        {
            push_sitemap_shard(&mut shards, shard_start..path_index, entries_len)?;
            shard_start = path_index;
            entries_count = 0;
            entries_len = 0;
        }

        entries_count += 1;
        entries_len += entry_len;
    }

    if entries_count > 0 {
        push_sitemap_shard(&mut shards, shard_start..paths.len(), entries_len)?;
    }

    if shards.is_empty() {
        return Err(SitemapRenderError::NoRepresentableUrls);
    }

    Ok(shards)
}

fn representable_sitemap_paths(
    origin: &str,
    mut paths: Vec<String>,
    limits: SitemapLimits,
) -> Result<Vec<String>, SitemapRenderError> {
    if limits.max_urlset_urls == 0 || urlset_document_len(0) > limits.max_urlset_bytes {
        return Err(SitemapRenderError::NoRepresentableUrls);
    }

    let mut skipped_entries = 0;
    paths.retain(|path| {
        let entry_len = render_loc_entry_len("url", origin, path);
        let fits = urlset_document_len(entry_len) <= limits.max_urlset_bytes;
        if !fits {
            skipped_entries += 1;
            tracing::warn!(
                path = %path,
                entry_bytes = entry_len,
                max_urlset_bytes = limits.max_urlset_bytes,
                "skipping sitemap URL entry that cannot fit in one sitemap document"
            );
        }
        fits
    });

    if skipped_entries > 0 {
        tracing::warn!(
            skipped_entries,
            "skipped sitemap URL entries that exceeded sitemap document size limits"
        );
    }

    if paths.is_empty() {
        return Err(SitemapRenderError::NoRepresentableUrls);
    }

    Ok(paths)
}

fn push_sitemap_shard(
    shards: &mut Vec<SitemapShard>,
    path_range: Range<usize>,
    entries_len: usize,
) -> Result<(), SitemapRenderError> {
    let number = shards.len() + 1;
    let Some(query_value) = sitemap_shard_query_value(number) else {
        return Err(SitemapRenderError::IndexTooLarge);
    };

    shards.push(SitemapShard {
        number,
        query_value,
        path_range,
        byte_len: urlset_document_len(entries_len),
    });

    Ok(())
}

fn render_sitemap_index(
    origin: &str,
    shards: &[SitemapShard],
    limits: SitemapLimits,
) -> Result<String, SitemapRenderError> {
    if shards.len() > limits.max_index_entries || shards.len() > SITEMAP_MAX_SHARD_NUMBER {
        return Err(SitemapRenderError::IndexTooLarge);
    }

    if sitemap_index_document_len(0) > limits.max_index_bytes {
        return Err(SitemapRenderError::IndexTooLarge);
    }

    let entries = shards
        .iter()
        .map(|shard| {
            render_index_entry(origin, shard.number).ok_or(SitemapRenderError::IndexTooLarge)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rendered = render_sitemap_index_from_entries(&entries);

    if rendered.len() > limits.max_index_bytes {
        return Err(SitemapRenderError::IndexTooLarge);
    }

    Ok(rendered)
}

fn urlset_prefix() -> String {
    format!(r#"{SITEMAP_XML_DECLARATION}<urlset xmlns="{SITEMAP_XML_NAMESPACE}">"#)
}

fn sitemap_index_prefix() -> String {
    format!(r#"{SITEMAP_XML_DECLARATION}<sitemapindex xmlns="{SITEMAP_XML_NAMESPACE}">"#)
}

fn urlset_document_len(entries_len: usize) -> usize {
    urlset_prefix().len() + entries_len + SITEMAP_URLSET_CLOSE.len()
}

fn sitemap_index_document_len(entries_len: usize) -> usize {
    sitemap_index_prefix().len() + entries_len + SITEMAP_INDEX_CLOSE.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://search.example.com";

    fn limits(
        max_urlset_urls: usize,
        max_urlset_bytes: usize,
        max_index_entries: usize,
        max_index_bytes: usize,
    ) -> SitemapLimits {
        SitemapLimits {
            max_urlset_urls,
            max_urlset_bytes,
            max_index_entries,
            max_index_bytes,
        }
    }

    fn generous_limits() -> SitemapLimits {
        limits(50_000, 1_000_000, 50_000, 1_000_000)
    }

    fn paths(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn document_body(document: SitemapDocument) -> String {
        match document {
            SitemapDocument::Urlset(body) | SitemapDocument::Index(body) => body,
        }
    }

    #[test]
    fn parse_sitemap_query_accepts_entrypoint_and_canonical_shard() {
        assert_eq!(parse_sitemap_query(None), Ok(SitemapQuery::EntryPoint));
        assert_eq!(
            parse_sitemap_query(Some("shard=00001")),
            Ok(SitemapQuery::Shard(1))
        );
        assert_eq!(
            parse_sitemap_query(Some("shard=99999")),
            Ok(SitemapQuery::Shard(99_999))
        );
    }

    #[test]
    fn parse_sitemap_query_rejects_noncanonical_queries() {
        for raw_query in [
            "",
            "foo=bar",
            "shard=",
            "shard=1",
            "shard=001",
            "shard=00000",
            "shard=100000",
            "shard=abcde",
            "shard=%30%30%30%30%31",
            "shard=00001&x=1",
            "x=1&shard=00001",
            "shard=00001&shard=00002",
            "shard=00001/extra",
        ] {
            assert!(
                parse_sitemap_query(Some(raw_query)).is_err(),
                "accepted query {raw_query:?}"
            );
        }
    }

    #[test]
    fn sitemap_shard_query_values_are_canonical() {
        assert_eq!(sitemap_shard_query_value(1).as_deref(), Some("00001"));
        assert_eq!(sitemap_shard_query_value(99_999).as_deref(), Some("99999"));
        assert_eq!(sitemap_shard_query_value(0), None);
        assert_eq!(sitemap_shard_query_value(100_000), None);
        assert_eq!(
            sitemap_shard_location_path_and_query(1).as_deref(),
            Some("/sitemap.xml?shard=00001")
        );
    }

    #[test]
    fn shard_sitemap_paths_splits_by_url_count() {
        let path_values = paths(&["/", "/a", "/b"]);
        let shards = shard_sitemap_paths(
            ORIGIN,
            &path_values,
            limits(1, 1_000_000, 50_000, 1_000_000),
        )
        .unwrap();

        assert_eq!(shards.len(), 3);
        assert_eq!(shards[0].query_value, "00001");
        assert_eq!(shards[1].query_value, "00002");
        assert_eq!(shards[2].query_value, "00003");
    }

    #[test]
    fn shard_sitemap_paths_splits_by_full_document_byte_size() {
        let first = render_url_entry(ORIGIN, "/a");
        let second = render_url_entry(ORIGIN, "/b");
        let one_entry_len = render_urlset_from_entries(std::slice::from_ref(&first)).len();
        let two_entry_len = render_urlset_from_entries(&[first, second]).len();

        let path_values = paths(&["/a", "/b"]);
        let shards = shard_sitemap_paths(
            ORIGIN,
            &path_values,
            limits(50_000, two_entry_len - 1, 50_000, 1_000_000),
        )
        .unwrap();

        assert!(one_entry_len < two_entry_len);
        assert_eq!(shards.len(), 2);
        assert!(shards.iter().all(|shard| shard.byte_len < two_entry_len));
    }

    #[test]
    fn exact_byte_limit_fits() {
        let entry = render_url_entry(ORIGIN, "/a");
        let document_len = render_urlset_from_entries(std::slice::from_ref(&entry)).len();
        let document = render_sitemap_entrypoint(
            ORIGIN,
            paths(&["/a"]),
            None,
            limits(1, document_len, 50_000, 1_000_000),
        )
        .unwrap();

        assert_eq!(document_body(document).len(), document_len);
    }

    #[test]
    fn byte_accounting_matches_rendered_urlset_length() {
        let path_values = paths(&["/a", "/b"]);
        let shards = shard_sitemap_paths(ORIGIN, &path_values, generous_limits()).unwrap();
        let rendered = render_urlset_from_paths(
            ORIGIN,
            &path_values[shards[0].path_range.clone()],
            shards[0].byte_len,
        );

        assert_eq!(shards[0].byte_len, rendered.len());
    }

    #[test]
    fn oversized_single_entry_is_skipped() {
        let short_entry = render_url_entry(ORIGIN, "/ok");
        let short_document_len =
            render_urlset_from_entries(std::slice::from_ref(&short_entry)).len();
        let rendered = document_body(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/ok", "/this-path-is-too-long-to-fit"]),
                None,
                limits(50_000, short_document_len, 50_000, 1_000_000),
            )
            .unwrap(),
        );

        assert!(rendered.contains("/ok"));
        assert!(!rendered.contains("this-path-is-too-long"));
    }

    #[test]
    fn all_skipped_input_returns_no_representable_urls() {
        assert!(matches!(
            SitemapPlan::new(
                ORIGIN.to_owned(),
                paths(&["/too-long"]),
                limits(50_000, urlset_document_len(0), 50_000, 1_000_000),
            ),
            Err(SitemapRenderError::NoRepresentableUrls)
        ));
    }

    #[test]
    fn impossible_urlset_limits_return_no_representable_urls() {
        assert!(matches!(
            SitemapPlan::new(
                ORIGIN.to_owned(),
                paths(&["/"]),
                limits(0, 1_000_000, 50_000, 1_000_000)
            ),
            Err(SitemapRenderError::NoRepresentableUrls)
        ));
        assert!(matches!(
            SitemapPlan::new(
                ORIGIN.to_owned(),
                paths(&["/"]),
                limits(50_000, urlset_document_len(0) - 1, 50_000, 1_000_000),
            ),
            Err(SitemapRenderError::NoRepresentableUrls)
        ));
    }

    #[test]
    fn entrypoint_renders_urlset_for_single_shard() {
        let document =
            render_sitemap_entrypoint(ORIGIN, paths(&["/"]), None, generous_limits()).unwrap();
        let SitemapDocument::Urlset(body) = document else {
            panic!("expected urlset");
        };

        assert!(body.contains("<urlset"));
        assert!(body.contains("<loc>https://search.example.com/</loc>"));
        assert!(!body.contains("<sitemapindex"));
    }

    #[test]
    fn entrypoint_renders_sitemap_index_for_multiple_shards() {
        let document = render_sitemap_entrypoint(
            ORIGIN,
            paths(&["/", "/a"]),
            None,
            limits(1, 1_000_000, 50_000, 1_000_000),
        )
        .unwrap();
        let SitemapDocument::Index(body) = document else {
            panic!("expected sitemap index");
        };

        assert!(body.contains("<sitemapindex"));
        assert!(body.contains("<loc>https://search.example.com/sitemap.xml?shard=00001</loc>"));
        assert!(body.contains("<loc>https://search.example.com/sitemap.xml?shard=00002</loc>"));
    }

    #[test]
    fn shard_query_renders_selected_urlset_only_for_multiple_shards() {
        let document = render_sitemap_entrypoint(
            ORIGIN,
            paths(&["/", "/a"]),
            Some("shard=00002"),
            limits(1, 1_000_000, 50_000, 1_000_000),
        )
        .unwrap();
        let SitemapDocument::Urlset(body) = document else {
            panic!("expected urlset");
        };

        assert!(body.contains("<loc>https://search.example.com/a</loc>"));
        assert!(!body.contains("<loc>https://search.example.com/</loc>"));
    }

    #[test]
    fn shard_query_is_unavailable_for_single_shard() {
        assert_eq!(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/"]),
                Some("shard=00001"),
                generous_limits(),
            ),
            Err(SitemapRenderError::ShardNotAvailable)
        );
    }

    #[test]
    fn out_of_range_shard_returns_error() {
        assert_eq!(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/", "/a"]),
                Some("shard=00003"),
                limits(1, 1_000_000, 50_000, 1_000_000),
            ),
            Err(SitemapRenderError::ShardOutOfRange)
        );
    }

    #[test]
    fn malformed_query_preserves_query_error() {
        assert_eq!(
            render_sitemap_entrypoint(ORIGIN, paths(&["/"]), Some("foo=bar"), generous_limits()),
            Err(SitemapRenderError::MalformedQuery(
                SitemapQueryError::UnknownQuery
            ))
        );
    }

    #[test]
    fn sitemap_index_count_overflow_returns_error() {
        assert_eq!(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/", "/a"]),
                None,
                limits(1, 1_000_000, 1, 1_000_000),
            ),
            Err(SitemapRenderError::IndexTooLarge)
        );
    }

    #[test]
    fn shard_query_also_enforces_sitemap_index_limits() {
        assert_eq!(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/", "/a"]),
                Some("shard=00001"),
                limits(1, 1_000_000, 1, 1_000_000),
            ),
            Err(SitemapRenderError::IndexTooLarge)
        );
    }

    #[test]
    fn sitemap_index_byte_overflow_returns_error() {
        assert_eq!(
            render_sitemap_entrypoint(
                ORIGIN,
                paths(&["/", "/a"]),
                None,
                limits(1, 1_000_000, 50_000, sitemap_index_document_len(0)),
            ),
            Err(SitemapRenderError::IndexTooLarge)
        );
    }

    #[test]
    fn xml_escapes_url_and_index_locations() {
        let url_entry = render_url_entry("https://example.com&x=<tag>", "/fixtures/git?ref=small");
        let index_entry = render_index_entry("https://example.com&x=<tag>", 1).unwrap();

        assert!(url_entry.contains("https://example.com&amp;x=&lt;tag&gt;/fixtures/git?ref=small"));
        assert!(
            index_entry.contains("https://example.com&amp;x=&lt;tag&gt;/sitemap.xml?shard=00001")
        );
        assert!(!url_entry.contains("https://example.com&x=<tag>"));
        assert!(!index_entry.contains("https://example.com&x=<tag>"));
    }

    #[test]
    fn entrypoint_sorts_and_deduplicates_paths() {
        let document =
            render_sitemap_entrypoint(ORIGIN, paths(&["/b", "/a", "/a"]), None, generous_limits())
                .unwrap();
        let body = document_body(document);
        let a_index = body.find("/a</loc>").unwrap();
        let b_index = body.find("/b</loc>").unwrap();

        assert!(a_index < b_index);
        assert_eq!(body.matches("/a</loc>").count(), 1);
    }
}
