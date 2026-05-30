use std::borrow::Cow;

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, Options, parse_document};
use html_escape::decode_html_entities;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use crate::ingest::IngestContext;
use crate::name::{NameParts, make_document_id};
use crate::source_link::{Declaration, Repo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DocumentKind {
    Option,
    Package,
    App,
    Service,
}

impl DocumentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Option => "option",
            Self::Package => "package",
            Self::App => "app",
            Self::Service => "service",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommonDoc {
    pub id: String,
    pub source: String,
    pub ref_id: String,
    pub kind: DocumentKind,
    pub name: String,
    pub name_parts: NameParts,
    pub revision: Option<String>,
    pub repo: Option<Repo>,
    #[serde(with = "time::serde::rfc3339")]
    pub imported_at: OffsetDateTime,
}

impl CommonDoc {
    pub fn new(context: &IngestContext, kind: DocumentKind, name: impl Into<String>) -> Self {
        let name = name.into();
        let id = make_document_id(&context.source, &context.ref_id, kind.as_str(), &name);

        Self {
            id,
            source: context.source.clone(),
            ref_id: context.ref_id.clone(),
            kind,
            name_parts: NameParts::from_dotted(&name),
            name,
            revision: context.revision.clone(),
            repo: context.repo.clone(),
            imported_at: OffsetDateTime::now_utc(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionDoc {
    #[serde(flatten)]
    pub common: CommonDoc,

    pub loc: Vec<String>,
    pub parents: Vec<String>,
    pub option_set: Option<String>,
    pub declarations: Vec<Declaration>,
    pub description: Option<DocText>,
    pub option_type: Option<String>,
    pub default: Option<DocValue>,
    pub example: Option<DocValue>,
    pub related_packages: Option<DocText>,
    pub read_only: Option<bool>,
    pub internal: Option<bool>,
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum DocText {
    Markdown(String),
    DocBook(String),
    Plain(String),
    Unknown { text: String, raw: Value },
    Json(Value),
}

impl DocText {
    pub fn plain_text(&self) -> Cow<'_, str> {
        match self {
            Self::Markdown(value) => Cow::Owned(markdown_to_plain_text(value)),
            Self::DocBook(value) => Cow::Owned(docbook_to_plain_text(value)),
            Self::Plain(value) => Cow::Borrowed(value),
            Self::Unknown { text, .. } => Cow::Borrowed(text),
            Self::Json(value) => Cow::Owned(value.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum DocValue {
    NixExpression(String),
    Markdown(String),
    DocBook(String),
    Json(Value),
    Plain(String),
}

impl DocValue {
    pub fn plain_text(&self) -> String {
        match self {
            Self::NixExpression(value) | Self::Plain(value) => value.clone(),
            Self::Markdown(value) => markdown_to_plain_text(value),
            Self::DocBook(value) => docbook_to_plain_text(value),
            Self::Json(value) => value.to_string(),
        }
    }

    pub fn nix_expression(&self) -> Option<&str> {
        match self {
            Self::NixExpression(value) => Some(value),
            _ => None,
        }
    }
}

pub fn markdown_to_plain_text(value: &str) -> String {
    let value = markdown_nix_doc_roles_to_plain_text(value);
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.table = true;
    let root = parse_document(&arena, &value, &options);
    let mut output = String::with_capacity(value.len());

    append_markdown_plain_text(root, &mut output);

    collapse_line_whitespace(&output)
}

fn append_markdown_plain_text<'a>(node: &'a AstNode<'a>, output: &mut String) {
    let mut skip_children = false;
    let mut trailing_separator = None;

    {
        let data = node.data.borrow();
        match &data.value {
            NodeValue::Text(value) | NodeValue::Raw(value) => {
                output.push_str(&strip_html_to_text_preserve_lines(value));
            }
            NodeValue::Code(value) => output.push_str(&value.literal),
            NodeValue::CodeBlock(value) => {
                push_line_separator(output);
                output.push_str(value.literal.trim_end());
                trailing_separator = Some('\n');
                skip_children = true;
            }
            NodeValue::HtmlBlock(value) => {
                push_line_separator(output);
                output.push_str(&strip_html_to_text_preserve_lines(&value.literal));
                trailing_separator = Some('\n');
                skip_children = true;
            }
            NodeValue::HtmlInline(value) => {
                output.push_str(&strip_html_to_text_preserve_lines(value))
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => output.push('\n'),
            NodeValue::ThematicBreak => trailing_separator = Some('\n'),
            NodeValue::TableCell => trailing_separator = Some(' '),
            NodeValue::Paragraph
            | NodeValue::Heading(_)
            | NodeValue::Item(_)
            | NodeValue::DescriptionTerm
            | NodeValue::DescriptionDetails
            | NodeValue::TableRow(_) => trailing_separator = Some('\n'),
            _ => {}
        }
    }

    if !skip_children {
        for child in node.children() {
            append_markdown_plain_text(child, output);
        }
    }

    match trailing_separator {
        Some('\n') => push_line_separator(output),
        Some(' ') => push_space_separator(output),
        _ => {}
    }
}

fn push_line_separator(output: &mut String) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

fn push_space_separator(output: &mut String) {
    if !output.is_empty() && !output.chars().next_back().is_some_and(char::is_whitespace) {
        output.push(' ');
    }
}

pub fn docbook_to_plain_text(value: &str) -> String {
    strip_html_to_text(value)
}

pub fn looks_like_docbook_text(value: &str) -> bool {
    let value = value.trim_start();
    let Some(value) = value.strip_prefix('<') else {
        return false;
    };

    let Some(tag_end) = value.find('>') else {
        return false;
    };

    let tag = &value[..tag_end];
    let Some(name) = markup_tag_name(tag) else {
        return false;
    };

    if !is_known_markup_tag_name(name) {
        return false;
    }

    if tag.trim_start().starts_with('/') {
        return false;
    }

    if tag_has_attributes_or_self_closes(tag) || is_spacing_markup_tag(tag) {
        return true;
    }

    value[tag_end + 1..].contains(&format!("</{name}>"))
}

fn strip_html_to_text(value: &str) -> String {
    collapse_whitespace(&strip_html_to_text_preserve_lines(value))
}

fn strip_html_to_text_preserve_lines(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(tag_start) = rest.find('<') {
        output.push_str(&decode_html_entities(&rest[..tag_start]));
        rest = &rest[tag_start..];

        let Some(tag_end) = rest.find('>') else {
            output.push_str(&decode_html_entities(rest));
            return output;
        };

        let tag = &rest[1..tag_end];
        if is_known_markup_tag_in_context(tag, &rest[tag_end + 1..]) {
            if is_spacing_markup_tag(tag) {
                output.push(' ');
            }
            rest = &rest[tag_end + 1..];
        } else {
            output.push('<');
            rest = &rest[1..];
        }
    }

    output.push_str(&decode_html_entities(rest));
    output
}

fn is_known_markup_tag(tag: &str) -> bool {
    let tag = tag.trim();
    if tag.starts_with('!') || tag.starts_with('?') {
        return true;
    }

    let Some(name) = markup_tag_name(tag) else {
        return false;
    };

    is_known_markup_tag_name(name)
}

fn is_known_markup_tag_in_context(tag: &str, after_tag: &str) -> bool {
    if !is_known_markup_tag(tag) {
        return false;
    }

    let Some(name) = markup_tag_name(tag) else {
        return true;
    };

    if tag.trim_start().starts_with('/')
        || tag_has_attributes_or_self_closes(tag)
        || is_spacing_markup_tag(tag)
        || is_unambiguous_inline_markup_tag(name)
    {
        return true;
    }

    after_tag.contains(&format!("</{name}>"))
}

fn tag_has_attributes_or_self_closes(tag: &str) -> bool {
    let tag = tag.trim().trim_end();
    tag.ends_with('/') || tag.contains(char::is_whitespace)
}

fn is_unambiguous_inline_markup_tag(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "b"
            | "code"
            | "em"
            | "emphasis"
            | "filename"
            | "link"
            | "literal"
            | "span"
            | "strong"
            | "varname"
            | "xref"
    )
}

fn is_known_markup_tag_name(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "b"
            | "blockquote"
            | "br"
            | "code"
            | "command"
            | "div"
            | "em"
            | "emphasis"
            | "filename"
            | "itemizedlist"
            | "li"
            | "link"
            | "listitem"
            | "literal"
            | "ol"
            | "option"
            | "orderedlist"
            | "p"
            | "para"
            | "pre"
            | "programlisting"
            | "replaceable"
            | "simpara"
            | "span"
            | "strong"
            | "table"
            | "tbody"
            | "td"
            | "term"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "varname"
            | "variablelist"
            | "varlistentry"
            | "xref"
    )
}

fn is_spacing_markup_tag(tag: &str) -> bool {
    let Some(name) = markup_tag_name(tag) else {
        return false;
    };

    matches!(
        name,
        "blockquote"
            | "br"
            | "div"
            | "itemizedlist"
            | "li"
            | "listitem"
            | "ol"
            | "orderedlist"
            | "p"
            | "para"
            | "pre"
            | "programlisting"
            | "simpara"
            | "table"
            | "tbody"
            | "td"
            | "term"
            | "th"
            | "thead"
            | "tr"
            | "ul"
            | "variablelist"
            | "varlistentry"
    )
}

fn markup_tag_name(tag: &str) -> Option<&str> {
    let tag = tag
        .trim()
        .trim_start_matches('/')
        .trim_start()
        .trim_end_matches('/')
        .trim_end();
    let name_end = tag
        .find(|ch: char| ch.is_ascii_whitespace() || ch == '/')
        .unwrap_or(tag.len());
    let name = &tag[..name_end];

    if !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        Some(name)
    } else {
        None
    }
}

pub fn markdown_nix_doc_roles_to_plain_text(value: &str) -> String {
    preprocess_markdown_nix_doc_roles(value, |output, text| output.push_str(text))
}

pub fn markdown_nix_doc_roles_to_inline_code(value: &str) -> String {
    preprocess_markdown_nix_doc_roles(value, |output, text| {
        output.push('`');
        output.push_str(text);
        output.push('`');
    })
}

fn preprocess_markdown_nix_doc_roles(
    value: &str,
    mut push_role: impl FnMut(&mut String, &str),
) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_fence = None;

    for line in value.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);

        if let Some(fence) = in_fence {
            output.push_str(line);
            if markdown_closing_fence(line_without_newline, fence) {
                in_fence = None;
            }
            continue;
        }

        if let Some(fence) = markdown_opening_fence(line_without_newline) {
            in_fence = Some(fence.marker());
            output.push_str(line);
            continue;
        }

        output.push_str(&preprocess_markdown_nix_doc_roles_line(
            line_without_newline,
            &mut push_role,
        ));
        if line.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

fn preprocess_markdown_nix_doc_roles_line(
    value: &str,
    push_role: &mut impl FnMut(&mut String, &str),
) -> String {
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
            push_role(&mut output, text);
            rest = remaining;
            continue;
        }

        let ch = rest.chars().next().expect("rest is not empty");
        output.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    output
}

#[derive(Debug, Clone, Copy)]
pub struct MarkdownFence<'a> {
    marker: char,
    length: usize,
    indent: usize,
    info: &'a str,
    quote_depth: usize,
}

impl<'a> MarkdownFence<'a> {
    pub fn indent(self) -> usize {
        self.indent
    }

    pub fn info(self) -> &'a str {
        self.info
    }

    pub fn quote_depth(self) -> usize {
        self.quote_depth
    }

    pub fn marker(self) -> MarkdownFenceMarker {
        MarkdownFenceMarker {
            marker: self.marker,
            length: self.length,
            quote_depth: self.quote_depth,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MarkdownFenceMarker {
    marker: char,
    length: usize,
    quote_depth: usize,
}

pub fn markdown_opening_fence(line: &str) -> Option<MarkdownFence<'_>> {
    let line = strip_line_ending(line);
    let (quote_depth, line) = strip_block_quote_markers(line, usize::MAX).unwrap_or((0, line));
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

    Some(MarkdownFence {
        marker,
        length,
        indent,
        info,
        quote_depth,
    })
}

pub fn markdown_closing_fence(line: &str, opening: MarkdownFenceMarker) -> bool {
    let line = strip_line_ending(line);
    let Some((_, line)) = strip_block_quote_markers(line, opening.quote_depth) else {
        return false;
    };
    let (indent, rest) = leading_spaces(line);
    if indent > 3 || !rest.starts_with(opening.marker) {
        return false;
    }

    let length = rest.chars().take_while(|&ch| ch == opening.marker).count();

    length >= opening.length && rest[length..].trim().is_empty()
}

pub fn strip_markdown_quote_prefix(line: &str, quote_depth: usize) -> &str {
    strip_block_quote_markers(line, quote_depth)
        .map(|(_, line)| line)
        .unwrap_or(line)
}

fn strip_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn leading_spaces(value: &str) -> (usize, &str) {
    let count = value.bytes().take_while(|&byte| byte == b' ').count();
    (count, &value[count..])
}

fn strip_block_quote_markers(mut line: &str, max_depth: usize) -> Option<(usize, &str)> {
    let mut depth = 0;

    while depth < max_depth {
        let (indent, rest) = leading_spaces(line);
        if indent > 3 || !rest.starts_with('>') {
            break;
        }

        line = &rest[1..];
        if let Some(rest) = line.strip_prefix(' ') {
            line = rest;
        }
        depth += 1;
    }

    if depth == max_depth || max_depth == usize::MAX {
        Some((depth, line))
    } else {
        None
    }
}

pub fn nix_doc_role_at_start(value: &str) -> Option<(&str, &str)> {
    let role_end = value.strip_prefix('{')?.find("}`")? + 1;
    let role = &value[1..role_end];
    if !matches!(
        role,
        "option" | "file" | "var" | "command" | "env" | "manpage"
    ) {
        return None;
    }

    let after_role = &value[role_end + 2..];
    let value_end = after_role.find('`')?;
    Some((&after_role[..value_end], &after_role[value_end + 1..]))
}

fn collapse_whitespace(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut pending_space = false;

    for part in value.split_whitespace() {
        if pending_space {
            output.push(' ');
        }
        output.push_str(part);
        pending_space = true;
    }

    output
}

fn collapse_line_whitespace(value: &str) -> String {
    value
        .lines()
        .map(collapse_whitespace)
        .collect::<Vec<_>>()
        .join("\n")
}

impl OptionDoc {
    pub fn new(context: &IngestContext, name: impl Into<String>) -> Self {
        Self {
            common: CommonDoc::new(context, DocumentKind::Option, name),
            loc: Vec::new(),
            parents: Vec::new(),
            option_set: None,
            declarations: Vec::new(),
            description: None,
            option_type: None,
            default: None,
            example: None,
            related_packages: None,
            read_only: None,
            internal: None,
            visible: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct License {
    pub name: Option<String>,
    pub full_name: Option<String>,
    pub spdx_id: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintainer {
    pub name: Option<String>,
    pub github: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageDoc {
    #[serde(flatten)]
    pub common: CommonDoc,

    pub attribute: String,
    pub package_set: Option<String>,
    pub pname: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub long_description: Option<String>,
    pub homepages: Vec<String>,
    pub platforms: Vec<String>,
    pub licenses: Vec<License>,
    pub maintainers: Vec<Maintainer>,
    pub main_program: Option<String>,
    pub programs: Vec<String>,
    pub position: Option<String>,
    pub broken: Option<bool>,
}

impl PackageDoc {
    pub fn new(context: &IngestContext, attribute: impl Into<String>) -> Self {
        let attribute = attribute.into();

        Self {
            common: CommonDoc::new(context, DocumentKind::Package, attribute.clone()),
            package_set: package_set_from_attribute(&attribute),
            attribute,
            pname: None,
            version: None,
            description: None,
            long_description: None,
            homepages: Vec::new(),
            platforms: Vec::new(),
            licenses: Vec::new(),
            maintainers: Vec::new(),
            main_program: None,
            programs: Vec::new(),
            position: None,
            broken: None,
        }
    }
}

fn package_set_from_attribute(attribute: &str) -> Option<String> {
    attribute
        .split_once('.')
        .map(|(package_set, _)| package_set.to_owned())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "document_type", rename_all = "kebab-case")]
pub enum SearchDocument {
    Option(OptionDoc),
    Package(PackageDoc),
}

impl SearchDocument {
    pub fn common(&self) -> &CommonDoc {
        match self {
            Self::Option(doc) => &doc.common,
            Self::Package(doc) => &doc.common,
        }
    }

    pub fn id(&self) -> &str {
        &self.common().id
    }

    pub fn name(&self) -> &str {
        &self.common().name
    }

    pub fn kind(&self) -> &DocumentKind {
        &self.common().kind
    }
}

#[cfg(test)]
mod tests {
    use crate::ingest::IngestContext;

    use super::{CommonDoc, DocText, DocumentKind, PackageDoc};

    #[test]
    fn common_doc_uses_context_identity() {
        let context = IngestContext {
            source: "nixos".into(),
            ref_id: "unstable".into(),
            revision: Some("abc123".into()),
            repo: None,
        };

        let doc = CommonDoc::new(&context, DocumentKind::Option, "programs.git.enable");

        assert_eq!(doc.source, "nixos");
        assert_eq!(doc.ref_id, "unstable");
        assert_eq!(doc.revision.as_deref(), Some("abc123"));
        assert_eq!(doc.kind, DocumentKind::Option);
        assert_eq!(doc.name, "programs.git.enable");
        assert_eq!(doc.id, "nixos/unstable/option/programs.git.enable");

        assert_eq!(doc.name_parts.root.as_deref(), Some("programs"));
        assert_eq!(doc.name_parts.groups, ["programs", "programs.git"]);
        assert_eq!(doc.name_parts.leaf.as_deref(), Some("enable"));
    }

    #[test]
    fn package_doc_uses_attribute_as_document_name() {
        let context = IngestContext {
            source: "nixpkgs".into(),
            ref_id: "unstable".into(),
            revision: None,
            repo: None,
        };

        let doc = PackageDoc::new(&context, "python3Packages.requests");

        assert_eq!(doc.common.name, "python3Packages.requests");
        assert_eq!(doc.attribute, "python3Packages.requests");
        assert_eq!(doc.package_set.as_deref(), Some("python3Packages"));
    }

    #[test]
    fn docbook_plain_text_strips_tags_and_decodes_entities() {
        let value = DocText::DocBook(
            "<para>Hello <literal>world</literal> &amp; friends</para>".to_owned(),
        );

        assert_eq!(value.plain_text(), "Hello world & friends");
    }

    #[test]
    fn docbook_plain_text_strips_emphasis_and_variable_lists() {
        let value = DocText::DocBook(
            "<variablelist><varlistentry><term><emphasis>foo</emphasis></term><listitem><para>bar</para></listitem></varlistentry></variablelist>"
                .to_owned(),
        );

        let text = value.plain_text();

        assert!(text.contains("foo"));
        assert!(text.contains("bar"));
        assert!(!text.contains("<emphasis>"));
        assert!(!text.contains("<variablelist>"));
        assert!(!text.contains("<varlistentry>"));
    }

    #[test]
    fn markdown_plain_text_strips_html_and_markup() {
        let value = DocText::Markdown(
            "Use **Git** and {option}`programs.git.enable` <em>safely</em>.".to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Use Git and programs.git.enable safely."
        );
    }

    #[test]
    fn markdown_plain_text_preserves_angle_placeholders() {
        let value = DocText::Markdown(
            "Configure services.<name>.enable with paths like <path>.".to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Configure services.<name>.enable with paths like <path>."
        );
    }

    #[test]
    fn markdown_plain_text_preserves_known_angle_placeholders() {
        let value = DocText::Markdown("Use <option> and <command> placeholders.".to_owned());

        assert_eq!(
            value.plain_text(),
            "Use <option> and <command> placeholders."
        );
    }

    #[test]
    fn markdown_plain_text_does_not_strip_roles_inside_code() {
        let value = DocText::Markdown(
            "Use {option}`services.nginx.enable`, but keep ``{option}`literal` `` and:\n\n```\n{option}`literal`\n```"
                .to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Use services.nginx.enable, but keep {option}`literal` and:\n{option}`literal`"
        );
    }

    #[test]
    fn markdown_plain_text_does_not_strip_roles_inside_blockquote_fences() {
        let value = DocText::Markdown(
            "> Example:\n>\n> ```\n> {option}`services.nginx.enable`\n> ```".to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Example:\n{option}`services.nginx.enable`"
        );
    }

    #[test]
    fn markdown_plain_text_handles_blockquote_fence_marker_spacing() {
        let value = DocText::Markdown(
            "> Example:\n> ```\n>{option}`services.nginx.enable`\n>```".to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Example:\n{option}`services.nginx.enable`"
        );
    }

    #[test]
    fn markdown_plain_text_does_not_strip_roles_inside_tilde_fences() {
        let value = DocText::Markdown("~~~~\n{option}`services.nginx.enable`\n~~~~".to_owned());

        assert_eq!(value.plain_text(), "{option}`services.nginx.enable`");
    }

    #[test]
    fn markdown_plain_text_preserves_identifiers_and_code() {
        let value = DocText::Markdown(
            "Use `ip_forward` with services.foo_bar.enable and **bold_name**.".to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Use ip_forward with services.foo_bar.enable and bold_name."
        );
    }

    #[test]
    fn markdown_plain_text_uses_link_labels_not_destinations() {
        let value =
            DocText::Markdown("Read [the manual](https://example.invalid/path).".to_owned());

        assert_eq!(value.plain_text(), "Read the manual.");
    }

    #[test]
    fn markdown_plain_text_extracts_blocks_and_tables() {
        let value = DocText::Markdown(
            "# Heading\n\n- first_item\n- second_item\n\n| A | B |\n| - | - |\n| c_d | e |"
                .to_owned(),
        );

        assert_eq!(
            value.plain_text(),
            "Heading\nfirst_item\nsecond_item\nA B\nc_d e"
        );
    }

    #[test]
    fn unknown_doc_text_indexes_from_text_and_preserves_raw() {
        let value = DocText::Unknown {
            text: "Readable text.".to_owned(),
            raw: serde_json::json!({ "_type": "custom", "text": "Readable text." }),
        };

        assert_eq!(value.plain_text(), "Readable text.");
    }

    #[test]
    fn plain_doc_text_is_borrowed_unchanged() {
        let value = DocText::Plain("Already plain.".to_owned());

        assert_eq!(value.plain_text(), "Already plain.");
    }
}
