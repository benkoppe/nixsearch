use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use comrak::{Options, markdown_to_html};
use html_escape::encode_safe;
use maud::{Markup, PreEscaped, html};
use nixsearch_core::document::{docbook_to_plain_text, nix_doc_role_at_start};
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

        static ONEDARK_THEME: OnceLock<Option<themes::Theme>> = OnceLock::new();

        let theme = ONEDARK_THEME
            .get_or_init(|| themes::get("onedark").ok())
            .clone()?;
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
        DocText::DocBook(value) => html! { p { (docbook_to_plain_text(value)) } },
        DocText::Plain(value) => html! { p { (value) } },
        DocText::Unknown { text, .. } => html! { p { (text) } },
        DocText::Json(value) => render_code(CodeLanguage::Nix, &format_nix(&json_to_nix(value))),
    }
}

pub fn render_doc_value(value: &DocValue) -> Markup {
    match value {
        DocValue::NixExpression(value) => render_code(CodeLanguage::Nix, &format_nix(value)),
        DocValue::Json(value) => render_code(CodeLanguage::Nix, &format_nix(&json_to_nix(value))),
        DocValue::Markdown(value) => render_markdown(value),
        DocValue::DocBook(value) => html! { p { (docbook_to_plain_text(value)) } },
        DocValue::Plain(value) => render_code(CodeLanguage::PlainText, value),
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
    let mut code_blocks = Vec::new();
    let markdown = extract_fenced_code_blocks(&markdown, &mut code_blocks);
    let mut options = Options::default();
    options.extension.table = true;
    options.render.unsafe_ = false;
    let mut html = markdown_to_html(&markdown, &options);

    for code_block in code_blocks {
        let placeholder = code_block.placeholder;
        html = replace_code_block_placeholder(html, &placeholder, &code_block.html);
    }

    let html = sanitize_rendered_markdown(&html);

    html! { div.doc-content { (PreEscaped(html)) } }
}

fn sanitize_rendered_markdown(html: &str) -> String {
    ammonia::Builder::default()
        .add_tags([
            "code", "div", "pre", "span", "table", "thead", "tbody", "tr", "th", "td",
        ])
        .add_generic_attributes(["class"])
        .clean(html)
        .to_string()
}

fn replace_code_block_placeholder(mut html: String, placeholder: &str, code_block: &str) -> String {
    if replace_once(&mut html, &format!("<p>{placeholder}</p>"), code_block) {
        return html;
    }

    for closing_tag in ["</li>", "</blockquote>", "</td>"] {
        if replace_once(
            &mut html,
            &format!("\n{placeholder}{closing_tag}"),
            &format!("\n{code_block}{closing_tag}"),
        ) || replace_once(
            &mut html,
            &format!("\n{placeholder}\n{closing_tag}"),
            &format!("\n{code_block}\n{closing_tag}"),
        ) {
            return html;
        }
    }

    html
}

fn replace_once(value: &mut String, needle: &str, replacement: &str) -> bool {
    let Some(start) = value.find(needle) else {
        return false;
    };

    value.replace_range(start..start + needle.len(), replacement);
    true
}

struct ExtractedCodeBlock {
    placeholder: String,
    html: String,
}

fn extract_fenced_code_blocks(value: &str, code_blocks: &mut Vec<ExtractedCodeBlock>) -> String {
    let mut output = String::with_capacity(value.len());
    let lines = value.split_inclusive('\n').collect::<Vec<_>>();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let Some(opening) = opening_fence(line) else {
            output.push_str(line);
            index += 1;
            continue;
        };

        let mut code = String::new();
        let mut closing_index = None;
        let mut code_index = index + 1;

        while code_index < lines.len() {
            if closing_fence(lines[code_index], opening.marker()) {
                closing_index = Some(code_index);
                break;
            }

            let code_line = strip_quote_prefix(lines[code_index], opening.quote_prefix);
            push_dedented_code_line(&mut code, code_line, opening.indent);
            code_index += 1;
        }

        if let Some(closing_index) = closing_index {
            let language = language_from_info(opening.info, &code);
            let formatted = format_code(language, &code);
            let placeholder = code_block_placeholder(code_blocks.len(), &code, value);

            code_blocks.push(ExtractedCodeBlock {
                placeholder: placeholder.clone(),
                html: render_code(language, &formatted).into_string(),
            });
            output.push_str(opening.quote_prefix);
            output.push_str(&" ".repeat(opening.indent));
            output.push_str(&placeholder);
            output.push('\n');
            index = closing_index + 1;
        } else {
            output.push_str(line);
            output.push_str(&code);
            index = lines.len();
        }
    }

    output
}

fn push_dedented_code_line(output: &mut String, line: &str, indent: usize) {
    let spaces = line
        .bytes()
        .take_while(|&byte| byte == b' ')
        .count()
        .min(indent);

    output.push_str(&line[spaces..]);
}

fn code_block_placeholder(index: usize, code: &str, document: &str) -> String {
    let mut hasher = DefaultHasher::new();
    index.hash(&mut hasher);
    code.hash(&mut hasher);
    document.len().hash(&mut hasher);

    format!(
        "\u{e000}NIXSEARCH_CODE_BLOCK_{index}_{:016x}\u{e001}",
        hasher.finish()
    )
}

#[derive(Debug, Clone, Copy)]
struct Fence<'a> {
    marker: char,
    length: usize,
    indent: usize,
    info: &'a str,
    quote_prefix: &'a str,
}

impl<'a> Fence<'a> {
    fn marker(self) -> FenceMarker<'a> {
        FenceMarker {
            marker: self.marker,
            length: self.length,
            quote_prefix: self.quote_prefix,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FenceMarker<'a> {
    marker: char,
    length: usize,
    quote_prefix: &'a str,
}

fn opening_fence(line: &str) -> Option<Fence<'_>> {
    let line = strip_line_ending(line);
    let (quote_prefix, line) = split_quote_prefix(line);
    let (indent, rest) = leading_spaces(line);
    if indent > 3 {
        return None;
    }

    let marker = rest.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }

    let length = rest.chars().take_while(|&ch| ch == marker).count();
    if length < 3 {
        return None;
    }

    let info = rest[length..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }

    Some(Fence {
        marker,
        length,
        indent,
        info,
        quote_prefix,
    })
}

fn closing_fence(line: &str, opening: FenceMarker<'_>) -> bool {
    let line = strip_line_ending(line);
    let line = strip_quote_prefix(line, opening.quote_prefix);
    let (indent, rest) = leading_spaces(line);
    if indent > 3 || !rest.starts_with(opening.marker) {
        return false;
    }

    let length = rest.chars().take_while(|&ch| ch == opening.marker).count();

    length >= opening.length && rest[length..].trim().is_empty()
}

fn strip_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn leading_spaces(value: &str) -> (usize, &str) {
    let count = value.bytes().take_while(|&byte| byte == b' ').count();
    (count, &value[count..])
}

fn split_quote_prefix(line: &str) -> (&str, &str) {
    let (indent, mut rest) = leading_spaces(line);
    if indent > 3 || !rest.starts_with('>') {
        return ("", line);
    }

    let mut prefix_len = indent;
    while rest.starts_with('>') {
        prefix_len += 1;
        rest = &line[prefix_len..];

        if rest.starts_with(' ') {
            prefix_len += 1;
            rest = &line[prefix_len..];
        }
    }

    (&line[..prefix_len], &line[prefix_len..])
}

fn strip_quote_prefix<'a>(line: &'a str, quote_prefix: &str) -> &'a str {
    if quote_prefix.is_empty() {
        line
    } else {
        line.strip_prefix(quote_prefix).unwrap_or(line)
    }
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
    let mut in_fence = None;

    for line in value.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);

        if let Some(fence) = in_fence {
            output.push_str(line);
            if closing_fence(line_without_newline, fence) {
                in_fence = None;
            }
            continue;
        }

        if let Some(fence) = opening_fence(line_without_newline) {
            in_fence = Some(fence.marker());
            output.push_str(line);
            continue;
        }

        output.push_str(&preprocess_nix_doc_roles_line(line_without_newline));
        if line.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

fn preprocess_nix_doc_roles_line(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while !rest.is_empty() {
        if rest.starts_with('`') {
            let tick_count = rest.bytes().take_while(|&byte| byte == b'`').count();
            let fence = &rest[..tick_count];
            let after_ticks = &rest[tick_count..];
            let Some(tick_end) = after_ticks.find(fence) else {
                output.push_str(rest);
                return output;
            };
            let code_end = tick_count + tick_end + tick_count;
            output.push_str(&rest[..code_end]);
            rest = &rest[code_end..];
            continue;
        }

        if let Some((text, remaining)) = nix_doc_role_at_start(rest) {
            output.push('`');
            output.push_str(text);
            output.push('`');
            rest = remaining;
            continue;
        }

        let ch = rest.chars().next().expect("rest is not empty");
        output.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

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
        Value::String(value) => nix_string(value),
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
    if is_bare_nix_attr_name(value) {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(nix_string(value))
    }
}

fn is_bare_nix_attr_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    !is_nix_keyword(value)
        && (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '\''))
}

fn is_nix_keyword(value: &str) -> bool {
    matches!(
        value,
        "assert"
            | "else"
            | "false"
            | "if"
            | "in"
            | "inherit"
            | "let"
            | "null"
            | "or"
            | "rec"
            | "then"
            | "true"
            | "with"
    )
}

fn nix_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');

    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => output.push_str(r#"\""#),
            '\\' => output.push_str(r"\\"),
            '\n' => output.push_str(r"\n"),
            '\r' => output.push_str(r"\r"),
            '\t' => output.push_str(r"\t"),
            '$' if chars.peek() == Some(&'{') => output.push_str(r"\$"),
            ch if ch.is_control() => {
                output.push_str(r"\\");
                output.extend(ch.escape_default().skip(1));
            }
            ch => output.push(ch),
        }
    }

    output.push('"');
    output
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
    fn json_strings_are_escaped_for_nix() {
        assert_eq!(json_to_nix(&json!("${pkgs.hello}")), r#""\${pkgs.hello}""#);
        assert_eq!(json_to_nix(&json!("quote: \"")), r#""quote: \"""#);
        assert_eq!(json_to_nix(&json!("back\\slash")), r#""back\\slash""#);
        assert_eq!(json_to_nix(&json!("one\ntwo")), r#""one\ntwo""#);
        assert_eq!(json_to_nix(&json!("one\rtwo")), r#""one\rtwo""#);
        assert_eq!(json_to_nix(&json!("one\ttwo")), r#""one\ttwo""#);
        assert_eq!(json_to_nix(&json!("one\u{8}two")), r#""one\\u{8}two""#);
    }

    #[test]
    fn json_attr_keys_are_escaped_for_nix() {
        assert_eq!(
            json_to_nix(&json!({ "valid-key'": "x" })),
            r#"{ valid-key' = "x"; }"#
        );
        assert_eq!(
            json_to_nix(&json!({ "${pkgs.hello}": "x" })),
            r#"{ "\${pkgs.hello}" = "x"; }"#
        );
        assert_eq!(json_to_nix(&json!({ "-bad": "x" })), r#"{ "-bad" = "x"; }"#);
        assert_eq!(json_to_nix(&json!({ "or": "x" })), r#"{ "or" = "x"; }"#);
        assert_eq!(
            json_to_nix(&json!({ "with space": "x" })),
            r#"{ "with space" = "x"; }"#
        );
        assert_eq!(
            json_to_nix(&json!({ "quote\"key": "x" })),
            r#"{ "quote\"key" = "x"; }"#
        );
    }

    #[test]
    fn json_objects_are_printed_as_nix() {
        assert_eq!(
            json_to_nix(&json!({ "not valid": "x" })),
            r#"{ "not valid" = "x"; }"#
        );
    }

    #[test]
    fn nix_doc_roles_become_inline_code() {
        assert_eq!(
            preprocess_nix_doc_roles("Use {option}`services.nginx.enable` here."),
            "Use `services.nginx.enable` here."
        );
    }

    #[test]
    fn nix_doc_roles_inside_code_are_unchanged() {
        assert_eq!(
            preprocess_nix_doc_roles("``{option}`services.nginx.enable` ``"),
            "``{option}`services.nginx.enable` ``"
        );
        assert_eq!(
            preprocess_nix_doc_roles("```\n{option}`services.nginx.enable`\n```"),
            "```\n{option}`services.nginx.enable`\n```"
        );
    }

    #[test]
    fn malformed_nix_doc_roles_are_unchanged() {
        assert_eq!(
            preprocess_nix_doc_roles("Use {unknown}`value` and {option}`unterminated."),
            "Use {unknown}`value` and {option}`unterminated."
        );
    }

    #[test]
    fn docbook_renders_as_readable_plain_text() {
        let rendered = render_doc_text(&DocText::DocBook(
            "<para>Hello <literal>world</literal> &amp; friends</para>".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("Hello world &amp; friends"));
        assert!(!rendered.contains("<para>"));
        assert!(!rendered.contains("<literal>"));
    }

    #[test]
    fn docbook_renders_extended_tags_as_readable_plain_text() {
        let rendered = render_doc_text(&DocText::DocBook(
            "<variablelist><varlistentry><term><emphasis>foo</emphasis></term><listitem><para>bar</para></listitem></varlistentry></variablelist>"
                .to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("foo"));
        assert!(rendered.contains("bar"));
        assert!(!rendered.contains("emphasis"));
        assert!(!rendered.contains("variablelist"));
        assert!(!rendered.contains("varlistentry"));
    }

    #[test]
    fn docbook_value_does_not_render_executable_html() {
        let rendered = render_doc_value(&DocValue::DocBook(
            "<para>Hello<script>alert('no')</script></para>".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("Hello"));
        assert!(rendered.contains("alert"));
        assert!(!rendered.contains("<script>"));
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
    fn markdown_tilde_fences_are_formatted_and_highlighted() {
        let rendered = render_doc_text(&DocText::Markdown(
            "Example:\n\n~~~json\n{\"foo\":\"bar\"}\n~~~".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("code-block"));
        assert!(rendered.contains("language-json"));
        assert!(rendered.contains("foo"));
        assert!(rendered.contains("bar"));
        assert!(!rendered.contains("~~~"));
    }

    #[test]
    fn markdown_fences_require_matching_marker_and_length() {
        let rendered = render_doc_text(&DocText::Markdown(
            "````nix\n```\n{ foo = \"bar\"; }\n````".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("code-block"));
        assert!(rendered.contains("language-nix"));
        assert!(rendered.contains("foo"));
    }

    #[test]
    fn markdown_code_block_placeholders_are_not_replaced_in_links() {
        let placeholder = "\u{e000}NIXSEARCH_CODE_BLOCK_0_user\u{e001}";
        let rendered = render_doc_text(&DocText::Markdown(format!(
            "[link]({placeholder})\n\n```nix\n{{ }}\n```"
        )))
        .into_string();

        assert_eq!(rendered.matches("<pre").count(), 1);
        assert!(!rendered.contains("href=\"<pre"));
        assert!(!rendered.contains("href=\"&lt;pre"));
    }

    #[test]
    fn markdown_tables_are_rendered() {
        let rendered = render_doc_text(&DocText::Markdown(
            "| Name | Value |\n| --- | --- |\n| foo | bar |".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("<table>"));
        assert!(rendered.contains("<th>Name</th>"));
        assert!(rendered.contains("<td>bar</td>"));
    }

    #[test]
    fn markdown_fences_inside_lists_stay_nested() {
        let rendered = render_doc_text(&DocText::Markdown(
            "- item\n\n  ```nix\n  { foo = \"bar\"; }\n  ```".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("<pre"));
        assert!(rendered.find("<li>").unwrap() < rendered.find("item").unwrap());
        assert!(rendered.find("item").unwrap() < rendered.find("<pre").unwrap());
        assert!(rendered.find("<pre").unwrap() < rendered.find("</li>").unwrap());
    }

    #[test]
    fn markdown_fences_inside_tight_lists_are_replaced() {
        let rendered = render_doc_text(&DocText::Markdown(
            "- item\n  ```nix\n  { foo = \"bar\"; }\n  ```".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("<pre"));
        assert!(!rendered.contains("NIXSEARCH_CODE_BLOCK_0"));
        assert!(rendered.find("item").unwrap() < rendered.find("<pre").unwrap());
        assert!(rendered.find("<pre").unwrap() < rendered.find("</li>").unwrap());
    }

    #[test]
    fn markdown_fences_inside_lists_are_dedented() {
        let mut code_blocks = Vec::new();
        let markdown = extract_fenced_code_blocks(
            "- item\n\n  ```text\n  hello\n    world\n  ```",
            &mut code_blocks,
        );

        assert!(markdown.starts_with("- item\n\n  \u{e000}NIXSEARCH_CODE_BLOCK_0_"));
        assert!(markdown.ends_with("\u{e001}\n"));
        assert_eq!(code_blocks.len(), 1);
        assert!(code_blocks[0].html.contains("hello"));
        assert!(code_blocks[0].html.contains("  world"));
        assert!(!code_blocks[0].html.contains("  hello"));
    }

    #[test]
    fn markdown_fences_inside_blockquotes_are_formatted_and_highlighted() {
        let rendered = render_doc_text(&DocText::Markdown(
            "> Example:\n>\n> ```nix\n> { foo = \"bar\"; }\n> ```".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("<blockquote>"));
        assert!(rendered.contains("<pre"));
        assert!(rendered.contains("language-nix"));
        assert!(rendered.find("<blockquote>").unwrap() < rendered.find("<pre").unwrap());
        assert!(rendered.find("<pre").unwrap() < rendered.find("</blockquote>").unwrap());
        assert!(!rendered.contains("NIXSEARCH_CODE_BLOCK_0"));
    }

    #[test]
    fn nix_doc_roles_inside_tilde_fences_are_unchanged() {
        let value = "~~~~\n{option}`services.nginx.enable`\n~~~~";

        assert_eq!(preprocess_nix_doc_roles(value), value);
    }

    #[test]
    fn nix_doc_roles_inside_blockquote_fences_are_unchanged() {
        let value = "> ```\n> {option}`services.nginx.enable`\n> ```";

        assert_eq!(preprocess_nix_doc_roles(value), value);
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

    #[test]
    fn highlighted_markdown_code_is_sanitized_after_insertion() {
        let rendered = render_doc_text(&DocText::Markdown(
            "```text\n</code><script>alert('no')</script>\n```".to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("code-block"));
        assert!(!rendered.contains("<script"));
    }

    #[test]
    fn markdown_output_drops_untrusted_inline_styles() {
        let rendered = render_doc_text(&DocText::Markdown(
            r#"Safe <span style="position:fixed">styled</span> text"#.to_owned(),
        ))
        .into_string();

        assert!(rendered.contains("Safe"));
        assert!(rendered.contains("styled"));
        assert!(!rendered.contains("position:fixed"));
        assert!(!rendered.contains("style="));
    }
}
