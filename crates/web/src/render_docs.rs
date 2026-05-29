use std::borrow::Cow;
use std::fmt::Write;

use comrak::{Options, markdown_to_html};
use html_escape::encode_safe;
use maud::{Markup, PreEscaped, html};
use serde_json::Value;

use nixsearch_core::document::{DocText, DocValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CodeLanguage {
    Nix,
    Toml,
    Json,
    Bash,
    Fish,
    Ini,
    Yaml,
    Xml,
    Sql,
    Nushell,
    PlainText,
}

pub trait CodeHighlighter {
    fn highlight(&self, language: CodeLanguage, code: &str) -> Option<String>;
}

#[derive(Debug, Default)]
pub struct LumisHighlighter;

impl CodeHighlighter for LumisHighlighter {
    fn highlight(&self, language: CodeLanguage, code: &str) -> Option<String> {
        use lumis::formatters::Formatter;
        use lumis::{HtmlInlineBuilder, languages::Language, themes};

        let pre_class = format!("code-block {}", language_class(language));
        let language = match language {
            CodeLanguage::Nix => Language::Nix,
            CodeLanguage::Toml => Language::Toml,
            CodeLanguage::Json => Language::JSON,
            CodeLanguage::Bash => Language::Bash,
            CodeLanguage::Fish => Language::Fish,
            CodeLanguage::Ini => Language::INI,
            CodeLanguage::Yaml => Language::YAML,
            CodeLanguage::Xml => Language::XML,
            CodeLanguage::Sql => Language::SQL,
            CodeLanguage::Nushell => Language::Nushell,
            CodeLanguage::PlainText => Language::PlainText,
        };

        let theme = themes::get("onedark").ok()?;
        let formatter = HtmlInlineBuilder::new()
            .language(language)
            .theme(Some(theme))
            .pre_class(Some(pre_class))
            .build()
            .ok()?;
        let mut output = Vec::new();
        formatter.format(code, &mut output).ok()?;
        String::from_utf8(output).ok()
    }
}

pub fn render_doc_text(value: &DocText) -> Markup {
    match value {
        DocText::Markdown(value) => render_markdown(value),
        DocText::DocBook(value) | DocText::Plain(value) => html! { p { (value) } },
    }
}

pub fn render_doc_value(value: &DocValue) -> Markup {
    match value {
        DocValue::NixExpression(value) => render_code(CodeLanguage::Nix, &format_nix(value)),
        DocValue::Json(value) => render_code(CodeLanguage::Nix, &format_nix(&json_to_nix(value))),
        DocValue::Markdown(value) => render_markdown(value),
        DocValue::DocBook(value) | DocValue::Plain(value) => {
            render_code(CodeLanguage::PlainText, value)
        }
    }
}

pub fn render_code(language: CodeLanguage, code: &str) -> Markup {
    let highlighter = LumisHighlighter;
    if let Some(body) = highlighter.highlight(language, code) {
        return html! { (PreEscaped(body)) };
    }

    let body = encode_safe(code).into_owned();
    let language_class = language_class(language);

    html! {
        pre.code-block class=(language_class) { code { (PreEscaped(body)) } }
    }
}

fn language_class(language: CodeLanguage) -> &'static str {
    match language {
        CodeLanguage::Nix => "language-nix",
        CodeLanguage::Toml => "language-toml",
        CodeLanguage::Json => "language-json",
        CodeLanguage::Bash => "language-bash",
        CodeLanguage::Fish => "language-fish",
        CodeLanguage::Ini => "language-ini",
        CodeLanguage::Yaml => "language-yaml",
        CodeLanguage::Xml => "language-xml",
        CodeLanguage::Sql => "language-sql",
        CodeLanguage::Nushell => "language-nushell",
        CodeLanguage::PlainText => "language-plain-text",
    }
}

fn render_markdown(value: &str) -> Markup {
    let markdown = preprocess_nix_doc_roles(value);
    let markdown = render_fenced_code_blocks(&markdown);
    let mut options = Options::default();
    options.render.unsafe_ = true;
    let html = markdown_to_html(&markdown, &options);
    let html = ammonia::Builder::default()
        .add_tags([
            "code", "pre", "span", "table", "thead", "tbody", "tr", "th", "td",
        ])
        .add_generic_attributes(["class", "style"])
        .clean(&html)
        .to_string();

    html! { div.doc-content { (PreEscaped(html)) } }
}

fn render_fenced_code_blocks(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut lines = value.lines();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        let info = trimmed.trim_start_matches("```").trim();
        let mut code = String::new();
        for code_line in lines.by_ref() {
            if code_line.trim_start().starts_with("```") {
                break;
            }
            code.push_str(code_line);
            code.push('\n');
        }

        let language = language_from_info(info, &code);
        let formatted = format_code(language, &code);
        output.push_str(&render_code(language, &formatted).into_string());
        output.push('\n');
    }

    output
}

fn language_from_info(info: &str, code: &str) -> CodeLanguage {
    let info = info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match info.as_str() {
        "nix" => CodeLanguage::Nix,
        "toml" => CodeLanguage::Toml,
        "json" => CodeLanguage::Json,
        "bash" | "sh" | "shell" | "console" | "shellsession" => CodeLanguage::Bash,
        "fish" => CodeLanguage::Fish,
        "ini" => CodeLanguage::Ini,
        "yaml" | "yml" => CodeLanguage::Yaml,
        "xml" => CodeLanguage::Xml,
        "sql" => CodeLanguage::Sql,
        "nushell" | "nu" => CodeLanguage::Nushell,
        "" if nixfmt_rs::format(code).is_ok() => CodeLanguage::Nix,
        "" if serde_json::from_str::<Value>(code).is_ok() => CodeLanguage::Json,
        _ => CodeLanguage::PlainText,
    }
}

fn format_code(language: CodeLanguage, code: &str) -> Cow<'_, str> {
    match language {
        CodeLanguage::Nix => Cow::Owned(format_nix(code)),
        CodeLanguage::Json => serde_json::from_str::<Value>(code)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed(code)),
        _ => Cow::Borrowed(code.trim_end()),
    }
}

fn preprocess_nix_doc_roles(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(role_end) = rest.find("}`") else {
            output.push_str(rest);
            return output;
        };
        let role = &rest[1..role_end];
        let after_role = &rest[role_end + 2..];
        let Some(value_end) = after_role.find('`') else {
            output.push_str(rest);
            return output;
        };
        let text = &after_role[..value_end];
        if matches!(
            role,
            "option" | "file" | "var" | "command" | "env" | "manpage"
        ) {
            output.push('`');
            output.push_str(text);
            output.push('`');
        } else {
            output.push_str(&rest[..role_end + 2 + value_end + 1]);
        }
        rest = &after_role[value_end + 1..];
    }

    output.push_str(rest);
    output
}

pub fn format_nix(value: &str) -> String {
    nixfmt_rs::format(value)
        .map(|value| value.trim_end_matches('\n').to_owned())
        .unwrap_or_else(|_| value.trim_end().to_owned())
}

fn json_to_nix(value: &Value) -> String {
    json_to_nix_indent(value, 0)
}

fn json_to_nix_indent(value: &Value, indent: usize) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => nix_string(value, indent),
        Value::Array(values) => nix_array(values, indent),
        Value::Object(values) => nix_attrset(values, indent),
    }
}

fn nix_array(values: &[Value], indent: usize) -> String {
    if values.is_empty() {
        return "[ ]".to_owned();
    }
    if values.len() == 1 {
        let value = json_to_nix_indent(&values[0], indent);
        if !value.contains('\n') {
            return format!("[ {value} ]");
        }
    }

    let next_indent = indent + 2;
    let mut output = String::from("[\n");
    for value in values {
        let _ = writeln!(
            output,
            "{space}{value}",
            space = " ".repeat(next_indent),
            value = json_to_nix_indent(value, next_indent)
        );
    }
    output.push_str(&" ".repeat(indent));
    output.push(']');
    output
}

fn nix_attrset(values: &serde_json::Map<String, Value>, indent: usize) -> String {
    if values.is_empty() {
        return "{ }".to_owned();
    }
    if values.len() == 1 {
        let (key, value) = values.iter().next().expect("single item exists");
        let value = json_to_nix_indent(value, indent);
        if !value.contains('\n') {
            return format!("{{ {} = {value}; }}", nix_attr_key(key));
        }
    }

    let next_indent = indent + 2;
    let mut output = String::from("{\n");
    for (key, value) in values {
        let _ = writeln!(
            output,
            "{space}{key} = {value};",
            space = " ".repeat(next_indent),
            key = nix_attr_key(key),
            value = json_to_nix_indent(value, next_indent)
        );
    }
    output.push_str(&" ".repeat(indent));
    output.push('}');
    output
}

fn nix_attr_key(value: &str) -> Cow<'_, str> {
    let valid = !value.is_empty()
        && !value.contains(['/', ' '])
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '\''));
    if valid {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(format!("{:?}", value))
    }
}

fn nix_string(value: &str, indent: usize) -> String {
    let lines = value.lines().collect::<Vec<_>>();
    if lines.len() > 1 {
        let next_indent = " ".repeat(indent + 2);
        let current_indent = " ".repeat(indent);
        let lines = lines.join(&format!("\n{next_indent}"));
        return format!("''\n{next_indent}{lines}\n{current_indent}''");
    }
    format!("{:?}", value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn json_values_are_printed_as_nix() {
        assert_eq!(json_to_nix(&json!({})), "{ }");
        assert_eq!(
            json_to_nix(&json!({ "hello": "world" })),
            "{ hello = \"world\"; }"
        );
        assert_eq!(json_to_nix(&json!([1])), "[ 1 ]");
    }

    #[test]
    fn nix_doc_roles_become_inline_code() {
        assert_eq!(
            preprocess_nix_doc_roles("Use {option}`services.nginx.enable` here."),
            "Use `services.nginx.enable` here."
        );
    }

    #[test]
    fn literal_expression_renders_as_highlighted_nix_not_json_wrapper() {
        let value = DocValue::NixExpression(
            r#"{
  "browser.startup.homepage" = "https://nixos.org";
  "browser.search.isUS" = false;
}
"#
            .to_owned(),
        );

        let rendered = render_doc_value(&value).into_string();

        assert!(rendered.contains("browser.startup.homepage"));
        assert!(rendered.contains("https://nixos.org"));
        assert!(!rendered.contains("literalExpression"));
        assert!(!rendered.contains("_type"));
        assert!(!rendered.contains(r#"\n"#));
    }

    #[test]
    fn highlighted_code_does_not_render_nested_pre_blocks() {
        let rendered = render_code(CodeLanguage::Nix, "{ }").into_string();

        assert_eq!(rendered.matches("<pre").count(), 1);
        assert!(rendered.contains("code-block"));
        assert!(rendered.contains("language-nix"));
    }

    #[test]
    fn markdown_fences_are_formatted_and_highlighted() {
        let rendered = render_doc_text(&DocText::Markdown(
            "Example:\n\n```nix\n{foo=\"bar\";}\n```".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("code-block"));
        assert!(rendered.contains("language-nix"));
        assert!(rendered.contains("foo"));
        assert!(rendered.contains("bar"));
        assert!(!rendered.contains("```"));
    }

    #[test]
    fn markdown_output_is_sanitized() {
        let rendered = render_doc_text(&DocText::Markdown(
            "Safe <script>alert('no')</script> text".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("Safe"));
        assert!(rendered.contains("text"));
        assert!(!rendered.contains("<script"));
    }
}
