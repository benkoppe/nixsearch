use nixsearch_config::AppConfig;

use crate::request::{LinkOrigin, PageQuery, PageRequest, non_empty};

pub fn source_path(source: &str) -> String {
    format!("/{}", encode_path(source))
}

pub fn search_url_for(source: Option<&str>, query: &PageQuery) -> String {
    let path = source.map(source_path).unwrap_or_else(|| "/".to_owned());
    let page_str = query.page.filter(|&p| p > 1).map(|p| p.to_string());

    let qs = query_string([
        ("q", query.q.as_deref()),
        ("ref", query.ref_id.as_deref()),
        ("source", query.source.map(|s| s.as_str())),
        ("page", page_str.as_deref()),
    ]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

pub fn paginated_search_url(source: Option<&str>, query: &PageQuery, page: usize) -> String {
    let mut paged_query = query.clone();
    paged_query.page = if page > 1 { Some(page) } else { None };
    search_url_for(source, &paged_query)
}

pub fn entry_url_for(source: &str, entry: &str, kind: Option<&str>, query: &PageQuery) -> String {
    let path = format!("{}/{}", source_path(source), encode_path(entry));
    let page_str = query.page.filter(|&p| p > 1).map(|p| p.to_string());

    let qs = query_string([
        ("q", query.q.as_deref()),
        ("ref", query.ref_id.as_deref()),
        ("kind", kind.or(query.kind.as_deref())),
        ("source", query.source.map(|s| s.as_str())),
        ("page", page_str.as_deref()),
    ]);

    if qs.is_empty() {
        path
    } else {
        format!("{path}?{qs}")
    }
}

pub fn close_url_for(request: &PageRequest) -> String {
    if request.query.source == Some(LinkOrigin::All) {
        return search_url_for(
            None,
            &PageQuery {
                q: request.query.q.clone(),
                page: request.query.page,
                ..PageQuery::default()
            },
        );
    }

    search_url_for(
        request.source.as_deref(),
        &PageQuery {
            q: request.query.q.clone(),
            ref_id: request.query.ref_id.clone(),
            page: request.query.page,
            ..PageQuery::default()
        },
    )
}

pub fn ref_id_for_link(config: &AppConfig, source: &str, ref_id: &str) -> Option<String> {
    let default_ref = config
        .sources
        .get(source)
        .and_then(|source| source.default_ref.as_deref());

    if default_ref == Some(ref_id) {
        None
    } else {
        Some(ref_id.to_owned())
    }
}

fn encode_path(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn encode_query(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

fn query_string<const N: usize>(pairs: [(&str, Option<&str>); N]) -> String {
    pairs
        .into_iter()
        .filter_map(|(key, value)| {
            let value = value.and_then(non_empty)?;
            Some(format!("{}={}", encode_query(key), encode_query(value)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{LinkOrigin, PageQuery, PageRequest};

    #[test]
    fn search_url_for_root_with_query() {
        let url = search_url_for(
            None,
            &PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(url, "/?q=git");
    }

    #[test]
    fn search_url_for_source_with_query_and_ref() {
        let url = search_url_for(
            Some("fixtures"),
            &PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(url, "/fixtures?q=git&ref=small");
    }

    #[test]
    fn entry_url_for_includes_kind() {
        let url = entry_url_for(
            "fixtures",
            "programs.git.enable",
            Some("option"),
            &PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                ..PageQuery::default()
            },
        );
        assert_eq!(
            url,
            "/fixtures/programs.git.enable?q=git&ref=small&kind=option"
        );
    }

    #[test]
    fn close_url_for_strips_entry_segment() {
        let request = PageRequest {
            source: Some("fixtures".to_owned()),
            entry: Some("programs.git.enable".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: Some("small".to_owned()),
                kind: Some("option".to_owned()),
                source: None,
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/fixtures?q=git&ref=small");
    }

    #[test]
    fn close_url_for_returns_root_when_no_source() {
        let request = PageRequest {
            source: None,
            entry: None,
            query: PageQuery {
                q: Some("git".to_owned()),
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/?q=git");
    }

    #[test]
    fn close_url_for_all_scope_returns_to_root() {
        let request = PageRequest {
            source: Some("nixpkgs".to_owned()),
            entry: Some("rubyPackages.git".to_owned()),
            query: PageQuery {
                q: Some("git".to_owned()),
                ref_id: None,
                kind: None,
                source: Some(LinkOrigin::All),
                ..PageQuery::default()
            },
        };
        assert_eq!(close_url_for(&request), "/?q=git");
    }
}
