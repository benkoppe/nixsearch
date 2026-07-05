use nixsearch_config::app::AppConfig;

use crate::source_labels::source_display_name;
use crate::urls::source_path;

pub(crate) const OPENSEARCH_CONTENT_TYPE: &str =
    "application/opensearchdescription+xml; charset=utf-8";

pub(crate) fn root_opensearch_xml(origin: &str) -> String {
    render_description(
        "nixsearch",
        "Search the Nix ecosystem",
        &format!("{origin}/"),
        &format!("{origin}/?q={{searchTerms}}"),
        &format!("{origin}/favicon.ico"),
    )
}

pub(crate) fn source_opensearch_xml(
    config: &AppConfig,
    origin: &str,
    source: &str,
) -> Option<String> {
    config.sources.contains_key(source).then(|| {
        let source_name = source_display_name(config, source);
        let path = source_path(source);
        render_description(
            &format!("nixsearch {source_name}"),
            &format!("Search {source_name} with nixsearch"),
            &format!("{origin}{path}"),
            &format!("{origin}{path}?q={{searchTerms}}"),
            &format!("{origin}/favicon.ico"),
        )
    })
}

fn render_description(
    short_name: &str,
    description: &str,
    search_form: &str,
    template: &str,
    image: &str,
) -> String {
    let mut xml = String::new();
    xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push_str(r#"<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">"#);
    push_text_element(&mut xml, "ShortName", short_name);
    push_text_element(&mut xml, "Description", description);
    push_text_element(&mut xml, "InputEncoding", "UTF-8");
    push_image(&mut xml, image);
    push_url(&mut xml, template);
    push_text_element(&mut xml, "SearchForm", search_form);
    xml.push_str("</OpenSearchDescription>");
    xml
}

fn push_text_element(xml: &mut String, element: &str, value: &str) {
    xml.push('<');
    xml.push_str(element);
    xml.push('>');
    html_escape::encode_text_to_string(value, xml);
    xml.push_str("</");
    xml.push_str(element);
    xml.push('>');
}

fn push_image(xml: &mut String, image: &str) {
    xml.push_str(r#"<Image height="32" width="32" type="image/x-icon">"#);
    html_escape::encode_text_to_string(image, xml);
    xml.push_str("</Image>");
}

fn push_url(xml: &mut String, template: &str) {
    xml.push_str(r#"<Url type="text/html" method="get" template=""#);
    html_escape::encode_double_quoted_attribute_to_string(template, xml);
    xml.push_str(r#""/>"#);
}

#[cfg(test)]
mod tests {
    use nixsearch_test_support::{app_config_with_public_url, utf8_path_buf};

    use super::*;

    #[test]
    fn root_opensearch_uses_absolute_search_template() {
        let xml = root_opensearch_xml("https://search.example.com");

        assert!(xml.contains("<ShortName>nixsearch</ShortName>"));
        assert!(xml.contains(r#"template="https://search.example.com/?q={searchTerms}""#));
        assert!(xml.contains("<SearchForm>https://search.example.com/</SearchForm>"));
    }

    #[test]
    fn source_opensearch_uses_absolute_source_template() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = app_config_with_public_url(utf8_path_buf(tempdir.path().join("indexes")));

        let xml = source_opensearch_xml(&config, "https://search.example.com", "fixtures").unwrap();

        assert!(xml.contains("<ShortName>nixsearch Fixtures</ShortName>"));
        assert!(xml.contains(r#"template="https://search.example.com/fixtures?q={searchTerms}""#));
        assert!(xml.contains("<SearchForm>https://search.example.com/fixtures</SearchForm>"));
    }
}
