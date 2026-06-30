use axum::http::{HeaderMap, Uri};
use url::Url;

use nixsearch_config::app::AppConfig;

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

pub(crate) fn configured_public_origin(config: &AppConfig) -> Option<String> {
    let public_url = config.server.public_url.as_deref()?;
    let base = Url::parse(public_url).ok()?;

    Some(origin_from_url(base))
}

fn origin_for_request(config: &AppConfig, _headers: &HeaderMap) -> String {
    configured_public_origin(config).unwrap_or_else(|| "http://localhost".to_owned())
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
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Uri};
    use nixsearch_config::app::AppConfig;
    use url::Url;

    use super::{page_urls, page_urls_for_public_uri, page_urls_from_base, public_uri_for_request};

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
        let headers = HeaderMap::new();

        let error =
            public_uri_for_request(&config, &headers, "https://search.example.com/fixtures")
                .unwrap_err();

        assert!(error.contains("does not match expected origin"));
    }
}
