use axum::http::{HeaderMap, Uri};
use url::Url;

use nixsearch_config::app::AppConfig;

#[cfg(test)]
use crate::request::non_empty;
#[cfg(test)]
use crate::request::public_uri;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageUrls {
    pub current_url: String,
    pub image_url: String,
    pub origin: String,
}

impl PageUrls {
    pub fn absolute_url(&self, path_and_query: &str) -> String {
        if path_and_query.starts_with('/') {
            format!("{}{}", self.origin, path_and_query)
        } else {
            format!("{}/{}", self.origin, path_and_query)
        }
    }
}

pub fn page_urls(config: &AppConfig, headers: &HeaderMap, uri: &Uri) -> PageUrls {
    page_urls_for_uri(config, headers, uri)
}

#[cfg(test)]
pub(crate) fn public_uri_for_request(
    config: &AppConfig,
    headers: &HeaderMap,
    raw_url: &str,
) -> std::result::Result<Uri, String> {
    if raw_url.starts_with("//") {
        return Err("public URL must not be protocol-relative".to_owned());
    }

    let uri = public_uri(raw_url).map_err(|error| error.to_string())?;

    if let Some(url_origin) = absolute_url_origin(raw_url) {
        let trusted_origin = origin_for_request(config, headers);
        if url_origin != trusted_origin {
            return Err(format!(
                "public URL origin {url_origin:?} does not match expected origin {trusted_origin:?}"
            ));
        }
    }

    Ok(uri)
}

pub(crate) fn public_path_and_query(uri: &Uri) -> String {
    match uri.query() {
        Some(query) => format!("{}?{query}", uri.path()),
        None => uri.path().to_owned(),
    }
}

pub(crate) fn page_urls_for_public_uri(
    config: &AppConfig,
    headers: &HeaderMap,
    uri: &Uri,
) -> PageUrls {
    page_urls_for_uri(config, headers, uri)
}

fn page_urls_for_uri(config: &AppConfig, headers: &HeaderMap, uri: &Uri) -> PageUrls {
    let path = uri.path();
    let query = uri.query();

    page_urls_from_origin(origin_for_request(config, headers), path, query)
}

#[cfg(test)]
fn page_urls_from_base(base: Url, path: &str, query: Option<&str>) -> PageUrls {
    let origin = origin_from_url(base);
    page_urls_from_origin(origin, path, query)
}

fn origin_for_request(config: &AppConfig, headers: &HeaderMap) -> String {
    if let Some(public_url) = config.server.public_url.as_deref()
        && let Ok(base) = Url::parse(public_url)
    {
        return origin_from_url(base);
    }

    let _ = headers;
    "http://localhost".to_owned()
}

#[cfg(test)]
fn page_urls_from_headers(headers: &HeaderMap, path: &str, query: Option<&str>) -> PageUrls {
    page_urls_from_origin(origin_from_headers(headers), path, query)
}

#[cfg(test)]
fn origin_from_headers(headers: &HeaderMap) -> String {
    let forwarded = forwarded_proto_host(headers);
    let proto = forwarded
        .as_ref()
        .and_then(|(proto, _)| proto.as_deref())
        .or_else(|| first_header_value(headers, "x-forwarded-proto"))
        .unwrap_or("http");
    let host = forwarded
        .as_ref()
        .and_then(|(_, host)| host.as_deref())
        .or_else(|| first_header_value(headers, "x-forwarded-host"))
        .or_else(|| first_header_value(headers, axum::http::header::HOST.as_str()))
        .unwrap_or("localhost");

    let origin = format!("{proto}://{host}");
    Url::parse(&origin).map(origin_from_url).unwrap_or(origin)
}

fn page_urls_from_origin(origin: String, path: &str, query: Option<&str>) -> PageUrls {
    let path_and_query = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };

    PageUrls {
        current_url: format!("{origin}{path_and_query}"),
        image_url: format!("{origin}/apple-touch-icon.png"),
        origin,
    }
}

fn origin_from_url(mut url: Url) -> String {
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string().trim_end_matches('/').to_owned()
}

#[cfg(test)]
fn absolute_url_origin(raw_url: &str) -> Option<String> {
    let raw_url = raw_url
        .split_once('#')
        .map_or(raw_url, |(before_fragment, _)| before_fragment);
    let url = Url::parse(raw_url).ok()?;

    url.host_str().is_some().then(|| origin_from_url(url))
}

#[cfg(test)]
fn forwarded_proto_host(headers: &HeaderMap) -> Option<(Option<String>, Option<String>)> {
    let header = first_header_value(headers, "forwarded")?;
    let first = header.split(',').next().unwrap_or(header);
    let mut proto = None;
    let mut host = None;

    for part in first.split(';') {
        let Some((key, value)) = part.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "proto" => proto = Some(value.to_owned()),
            "host" => host = Some(value.to_owned()),
            _ => {}
        }
    }

    (proto.is_some() || host.is_some()).then_some((proto, host))
}

#[cfg(test)]
fn first_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .and_then(non_empty)
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri};
    use nixsearch_config::app::AppConfig;
    use url::Url;

    use super::{
        page_urls, page_urls_for_public_uri, page_urls_from_base, page_urls_from_headers,
        public_uri_for_request,
    };

    #[test]
    fn page_urls_use_public_url_origin() {
        let urls = page_urls_from_base(
            Url::parse("https://search.example.com/").unwrap(),
            "/nixpkgs/git",
            Some("q=git"),
        );

        assert_eq!(
            urls.current_url,
            "https://search.example.com/nixpkgs/git?q=git"
        );
        assert_eq!(
            urls.image_url,
            "https://search.example.com/apple-touch-icon.png"
        );
        assert_eq!(urls.origin, "https://search.example.com");
    }

    #[test]
    fn page_urls_fall_back_to_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "forwarded",
            HeaderValue::from_static("for=127.0.0.1;proto=https;host=nixsearch.example.com"),
        );
        let urls = page_urls_from_headers(&headers, "/", None);

        assert_eq!(urls.current_url, "https://nixsearch.example.com/");
        assert_eq!(
            urls.image_url,
            "https://nixsearch.example.com/apple-touch-icon.png"
        );
    }

    #[test]
    fn page_urls_fall_back_to_host_and_proto_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:3000"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let urls = page_urls_from_headers(&headers, "/nixpkgs", Some("q=git"));

        assert_eq!(urls.current_url, "https://localhost:3000/nixpkgs?q=git");
        assert_eq!(
            urls.image_url,
            "https://localhost:3000/apple-touch-icon.png"
        );
    }

    #[test]
    fn page_urls_use_x_forwarded_host_when_public_url_is_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("internal.local"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("search.example.com"),
        );

        let urls = page_urls_from_headers(&headers, "/fixtures", None);

        assert_eq!(urls.current_url, "https://search.example.com/fixtures");
    }

    #[test]
    fn page_urls_ignore_host_when_public_url_is_configured() {
        let mut config = AppConfig::default();
        config.server.public_url = Some("https://search.example.com/".to_owned());

        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("attacker.example"));
        headers.insert(
            "forwarded",
            HeaderValue::from_static("proto=https;host=proxy.example"),
        );

        let urls = page_urls(&config, &headers, &Uri::from_static("/fixtures?q=git"));

        assert_eq!(
            urls.current_url,
            "https://search.example.com/fixtures?q=git"
        );
        assert_eq!(urls.origin, "https://search.example.com");
    }

    #[test]
    fn page_urls_ignore_forwarded_headers_when_public_url_is_absent() {
        let config = AppConfig::default();
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("internal.local"));
        headers.insert(
            "forwarded",
            HeaderValue::from_static("proto=https;host=search.example.com"),
        );

        let urls = page_urls(&config, &headers, &Uri::from_static("/fixtures"));

        assert_eq!(urls.current_url, "http://localhost/fixtures");
    }

    #[test]
    fn page_urls_for_public_uri_uses_requested_uri_not_sse_endpoint() {
        let mut config = AppConfig::default();
        config.server.public_url = Some("https://search.example.com/".to_owned());

        let headers = HeaderMap::new();
        let urls = page_urls_for_public_uri(
            &config,
            &headers,
            &Uri::from_static("/fixtures/programs.git.enable?q=git"),
        );

        assert_eq!(
            urls.current_url,
            "https://search.example.com/fixtures/programs.git.enable?q=git"
        );
    }

    #[test]
    fn page_urls_for_public_uri_accepts_absolute_requested_uri() {
        let mut config = AppConfig::default();
        config.server.public_url = Some("https://search.example.com/".to_owned());

        let headers = HeaderMap::new();
        let urls = page_urls_for_public_uri(
            &config,
            &headers,
            &Uri::from_static("https://internal.example/fixtures?q=git"),
        );

        assert_eq!(
            urls.current_url,
            "https://search.example.com/fixtures?q=git"
        );
    }

    #[test]
    fn public_uri_for_request_accepts_matching_public_url_origin() {
        let mut config = AppConfig::default();
        config.server.public_url = Some("https://search.example.com/".to_owned());

        let headers = HeaderMap::new();
        let uri = public_uri_for_request(
            &config,
            &headers,
            "https://search.example.com/fixtures?q=git",
        )
        .unwrap();

        assert_eq!(uri.path(), "/fixtures");
        assert_eq!(uri.query(), Some("q=git"));
    }

    #[test]
    fn public_uri_for_request_rejects_foreign_public_url_origin() {
        let mut config = AppConfig::default();
        config.server.public_url = Some("https://search.example.com/".to_owned());

        let headers = HeaderMap::new();
        let error =
            public_uri_for_request(&config, &headers, "https://evil.example/fixtures").unwrap_err();

        assert!(error.contains("does not match expected origin"));
    }

    #[test]
    fn public_uri_for_request_rejects_absolute_url_without_public_url() {
        let config = AppConfig::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("search.example.com"),
        );

        let error =
            public_uri_for_request(&config, &headers, "https://search.example.com/fixtures")
                .unwrap_err();

        assert!(error.contains("does not match expected origin"));
    }
}
