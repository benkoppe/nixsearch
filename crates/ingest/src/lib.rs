use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use nixsearch_core::document::{
    DocText, DocValue, License, Maintainer, OptionDoc, PackageDoc, SearchDocument,
    looks_like_docbook_text,
};
use nixsearch_core::ingest::IngestContext;
use nixsearch_core::source_link::Declaration;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawDeclaration {
    Path(String),
    Link {
        name: String,
        #[serde(default)]
        url: Option<String>,
    },
}

impl From<RawDeclaration> for Declaration {
    fn from(value: RawDeclaration) -> Self {
        match value {
            RawDeclaration::Path(path) => Self {
                name: path,
                url: None,
            },
            RawDeclaration::Link { name, url } => Self { name, url },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOption {
    #[serde(default)]
    declarations: Vec<RawDeclaration>,

    #[serde(default)]
    description: Option<Value>,

    #[serde(default, rename = "type")]
    option_type: Option<String>,

    #[serde(default)]
    default: Option<Value>,

    #[serde(default)]
    example: Option<Value>,

    #[serde(default)]
    related_packages: Option<Value>,

    #[serde(default)]
    loc: Vec<String>,

    #[serde(default)]
    read_only: Option<bool>,

    #[serde(default)]
    internal: Option<bool>,

    #[serde(default)]
    visible: Option<bool>,
}

pub fn parse_options_json<R: Read>(
    reader: R,
    context: &IngestContext,
) -> Result<Vec<SearchDocument>> {
    parse_options_json_with_strip_prefixes(reader, context, &[])
}

pub fn parse_options_json_with_strip_prefixes<R: Read>(
    reader: R,
    context: &IngestContext,
    strip_prefixes: &[String],
) -> Result<Vec<SearchDocument>> {
    let raw_options: serde_json::Map<String, Value> =
        serde_json::from_reader(reader).context("failed to parse options JSON")?;

    let mut documents = Vec::with_capacity(raw_options.len());

    for (name, value) in raw_options {
        let raw: RawOption = serde_json::from_value(value)
            .with_context(|| format!("failed to parse option {name}"))?;

        documents.push(SearchDocument::Option(convert_option(
            name,
            raw,
            context,
            strip_prefixes,
        )));
    }

    documents.sort_by(|left, right| left.id().cmp(right.id()));

    Ok(documents)
}

fn convert_option(
    name: String,
    raw: RawOption,
    context: &IngestContext,
    strip_prefixes: &[String],
) -> OptionDoc {
    let name = strip_name_prefix(name, strip_prefixes);
    let mut doc = OptionDoc::new(context, name);

    doc.loc = if raw.loc.is_empty() {
        doc.common.name.split('.').map(ToOwned::to_owned).collect()
    } else {
        strip_loc_prefix(raw.loc, strip_prefixes)
    };

    doc.parents = parents_from_loc(&doc.loc);
    doc.option_set = doc.loc.first().cloned();
    doc.declarations = raw.declarations.into_iter().map(Into::into).collect();
    doc.description = raw.description.map(doc_text_from_value);
    doc.option_type = raw.option_type;
    doc.default = raw.default.map(doc_value_from_value);
    doc.example = raw.example.map(doc_value_from_value);
    doc.related_packages = raw.related_packages.map(doc_text_from_value);
    doc.read_only = raw.read_only;
    doc.internal = raw.internal;
    doc.visible = raw.visible;

    doc
}

fn doc_text_from_value(value: Value) -> DocText {
    match value {
        Value::String(value) if looks_like_docbook_text(&value) => DocText::DocBook(value),
        Value::String(value) => DocText::Markdown(value),
        Value::Object(object) => {
            let original = Value::Object(object.clone());
            let doc_type = object.get("_type").and_then(Value::as_str);
            let text = object.get("text").and_then(Value::as_str);

            match (doc_type, text) {
                (Some("mdDoc" | "literalMD"), Some(text)) => DocText::Markdown(text.to_owned()),
                (Some("literalDocBook"), Some(text)) => DocText::DocBook(text.to_owned()),
                (Some("literalExpression" | "literalExample"), Some(text)) => {
                    DocText::Plain(text.to_owned())
                }
                (Some(_), Some(text)) => DocText::Unknown {
                    text: text.to_owned(),
                    raw: original,
                },
                _ => DocText::Json(original),
            }
        }
        other => DocText::Json(other),
    }
}

fn doc_value_from_value(value: Value) -> DocValue {
    match value {
        Value::Object(mut object) => {
            let original = Value::Object(object.clone());
            let doc_type = object
                .remove("_type")
                .and_then(|value| value.as_str().map(ToOwned::to_owned));
            let text = object
                .remove("text")
                .and_then(|value| value.as_str().map(ToOwned::to_owned));

            match (doc_type.as_deref(), text) {
                (Some("literalExpression" | "literalExample"), Some(text)) => {
                    DocValue::NixExpression(text)
                }
                (Some("mdDoc" | "literalMD"), Some(text)) => DocValue::Markdown(text),
                (Some("literalDocBook"), Some(text)) => DocValue::DocBook(text),
                _ => DocValue::Json(original),
            }
        }
        other => DocValue::Json(other),
    }
}

fn parents_from_loc(loc: &[String]) -> Vec<String> {
    if loc.len() <= 1 {
        return Vec::new();
    }

    (1..loc.len()).map(|end| loc[..end].join(".")).collect()
}

fn strip_name_prefix(name: String, strip_prefixes: &[String]) -> String {
    for prefix in strip_prefixes {
        if let Some(stripped) = name.strip_prefix(prefix) {
            return stripped.to_owned();
        }
    }

    name
}

fn strip_loc_prefix(loc: Vec<String>, strip_prefixes: &[String]) -> Vec<String> {
    for prefix in strip_prefixes {
        let prefix_loc = loc_prefix_from_name_prefix(prefix);

        if !prefix_loc.is_empty() && loc.starts_with(&prefix_loc) {
            return loc[prefix_loc.len()..].to_vec();
        }
    }

    loc
}

fn loc_prefix_from_name_prefix(prefix: &str) -> Vec<String> {
    prefix
        .trim_end_matches('.')
        .split('.')
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawPackagesJson {
    packages: BTreeMap<String, RawPackage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackage {
    #[serde(default)]
    pname: Option<String>,

    #[serde(default)]
    version: Option<String>,

    #[serde(default)]
    programs: Vec<String>,

    #[serde(default)]
    main_program: Option<String>,

    #[serde(default)]
    meta: RawPackageMeta,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackageMeta {
    #[serde(default)]
    description: Option<String>,

    #[serde(default)]
    long_description: Option<String>,

    #[serde(default)]
    homepage: Option<Value>,

    #[serde(default)]
    home_page: Option<Value>,

    #[serde(default)]
    platforms: Option<Value>,

    #[serde(default)]
    license: Option<Value>,

    #[serde(default)]
    maintainers: Option<Value>,

    #[serde(default)]
    main_program: Option<String>,

    #[serde(default)]
    position: Option<String>,

    #[serde(default)]
    broken: Option<bool>,
}

pub fn parse_packages_json<R: Read>(
    reader: R,
    context: &IngestContext,
) -> Result<Vec<SearchDocument>> {
    parse_packages_json_with_strip_prefixes(reader, context, &[])
}

pub fn parse_packages_json_with_strip_prefixes<R: Read>(
    reader: R,
    context: &IngestContext,
    strip_prefixes: &[String],
) -> Result<Vec<SearchDocument>> {
    let raw: RawPackagesJson =
        serde_json::from_reader(reader).context("failed to parse packages JSON")?;

    let mut documents = Vec::with_capacity(raw.packages.len());

    for (attribute, package) in raw.packages {
        documents.push(SearchDocument::Package(convert_package(
            attribute,
            package,
            context,
            strip_prefixes,
        )));
    }

    documents.sort_by(|left, right| left.id().cmp(right.id()));

    Ok(documents)
}

fn convert_package(
    attribute: String,
    raw: RawPackage,
    context: &IngestContext,
    strip_prefixes: &[String],
) -> PackageDoc {
    let attribute = strip_name_prefix(attribute, strip_prefixes);
    let mut doc = PackageDoc::new(context, attribute);

    doc.pname = raw.pname;
    doc.version = raw.version;
    doc.description = raw.meta.description;
    doc.long_description = raw.meta.long_description;
    doc.homepages = raw
        .meta
        .homepage
        .or(raw.meta.home_page)
        .map(strings_from_json_value)
        .unwrap_or_default();
    doc.platforms = raw
        .meta
        .platforms
        .map(strings_from_json_value)
        .unwrap_or_default();
    doc.licenses = raw
        .meta
        .license
        .map(licenses_from_json_value)
        .unwrap_or_default();
    doc.maintainers = raw
        .meta
        .maintainers
        .map(maintainers_from_json_value)
        .unwrap_or_default();
    doc.main_program = raw.meta.main_program.or(raw.main_program);
    doc.programs = raw.programs;
    doc.position = raw.meta.position;
    doc.broken = raw.meta.broken;

    doc
}

fn strings_from_json_value(value: Value) -> Vec<String> {
    match value {
        Value::String(value) => vec![value],
        Value::Array(values) => values
            .into_iter()
            .filter_map(|value| match value {
                Value::String(value) => Some(value),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn licenses_from_json_value(value: Value) -> Vec<License> {
    match value {
        Value::String(name) => vec![License {
            name: Some(name),
            full_name: None,
            spdx_id: None,
            url: None,
        }],
        Value::Object(object) => vec![license_from_object(object)],
        Value::Array(values) => values
            .into_iter()
            .flat_map(licenses_from_json_value)
            .collect(),
        _ => Vec::new(),
    }
}

fn license_from_object(object: serde_json::Map<String, Value>) -> License {
    License {
        name: string_field(&object, "shortName"),
        full_name: string_field(&object, "fullName"),
        spdx_id: string_field(&object, "spdxId"),
        url: string_field(&object, "url").or_else(|| string_field(&object, "appendixUrl")),
    }
}

fn maintainers_from_json_value(value: Value) -> Vec<Maintainer> {
    match value {
        Value::String(name) => vec![Maintainer {
            name: Some(name),
            github: None,
            email: None,
        }],
        Value::Object(object) => vec![maintainer_from_object(object)],
        Value::Array(values) => values
            .into_iter()
            .flat_map(maintainers_from_json_value)
            .collect(),
        _ => Vec::new(),
    }
}

fn maintainer_from_object(object: serde_json::Map<String, Value>) -> Maintainer {
    Maintainer {
        name: string_field(&object, "name"),
        github: string_field(&object, "github"),
        email: string_field(&object, "email"),
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| match value {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use nixsearch_core::document::{DocText, DocValue, SearchDocument};
    use nixsearch_test_support::{OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, ingest_context};

    use super::{
        parse_options_json, parse_options_json_with_strip_prefixes,
        parse_packages_json_with_strip_prefixes,
    };

    #[test]
    fn derives_parents_from_loc() {
        let json = r#"
           {
             "boot.loader.systemd-boot.enable": {
               "description": "Whether to enable systemd-boot.",
               "loc": ["boot", "loader", "systemd-boot", "enable"]
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert_eq!(
            option.parents,
            ["boot", "boot.loader", "boot.loader.systemd-boot"]
        );
        assert_eq!(option.option_set.as_deref(), Some("boot"));
    }

    #[test]
    fn falls_back_to_name_segments_when_loc_is_missing() {
        let json = r#"
           {
             "services.nginx.enable": {
               "description": "Whether to enable Nginx."
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert_eq!(option.loc, ["services", "nginx", "enable"]);
        assert_eq!(option.parents, ["services", "services.nginx"]);
        assert_eq!(option.option_set.as_deref(), Some("services"));
    }

    #[test]
    fn strips_configured_option_name_and_loc_prefix() {
        let json = r#"
           {
             "hjem.users.<username>.files": {
               "loc": ["hjem", "users", "<username>", "files"]
             }
           }
           "#;

        let docs = parse_options_json_with_strip_prefixes(
            json.as_bytes(),
            &ingest_context(),
            &["hjem.".to_owned()],
        )
        .unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert_eq!(option.common.name, "users.<username>.files");
        assert_eq!(option.loc, ["users", "<username>", "files"]);
        assert_eq!(option.parents, ["users", "users.<username>"]);
        assert_eq!(option.option_set.as_deref(), Some("users"));
    }

    #[test]
    fn strips_configured_package_attribute_prefix() {
        let json = r#"
           {
             "packages": {
               "packages.foo": {
                 "pname": "foo",
                 "version": "1.0",
                 "meta": {}
               }
             }
           }
           "#;

        let docs = parse_packages_json_with_strip_prefixes(
            json.as_bytes(),
            &ingest_context(),
            &["packages.".to_owned()],
        )
        .unwrap();

        let SearchDocument::Package(package) = &docs[0] else {
            panic!("expected package document");
        };

        assert_eq!(package.common.name, "foo");
        assert_eq!(package.attribute, "foo");
    }

    #[test]
    fn parses_canonical_package_program_fields() {
        let json = r#"
           {
             "packages": {
               "foo": {
                 "programs": ["foo", "fooctl"],
                 "mainProgram": "foo"
               }
             }
           }
           "#;

        let docs = parse_packages_json_with_strip_prefixes(json.as_bytes(), &ingest_context(), &[])
            .unwrap();
        let SearchDocument::Package(package) = &docs[0] else {
            panic!("expected package document");
        };

        assert_eq!(package.programs, ["foo", "fooctl"]);
        assert_eq!(package.main_program.as_deref(), Some("foo"));
    }

    #[test]
    fn ignores_legacy_package_program_aliases() {
        let json = r#"
           {
             "packages": {
               "foo": {
                 "package_programs": ["foo"],
                 "package_mainProgram": "foo"
               }
             }
           }
           "#;

        let docs = parse_packages_json_with_strip_prefixes(json.as_bytes(), &ingest_context(), &[])
            .unwrap();
        let SearchDocument::Package(package) = &docs[0] else {
            panic!("expected package document");
        };

        assert!(package.programs.is_empty());
        assert_eq!(package.main_program, None);
    }

    #[test]
    fn parses_link_declarations() {
        let json = r#"
           {
             "programs.git.enable": {
               "declarations": [
                 {
                   "name": "git.nix",
                   "url": "https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/programs/git.nix"
                 }
               ],
               "description": "Whether to enable Git."
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert_eq!(option.declarations.len(), 1);
        assert_eq!(option.declarations[0].name, "git.nix");
        assert_eq!(
            option.declarations[0].url.as_deref(),
            Some("https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/programs/git.nix")
        );
    }

    #[test]
    fn parses_option_doc_values_semantically() {
        let json = r#"
           {
              "programs.firefox.profiles.<name>.settings": {
                "description": "Attribute set of Firefox preferences.",
                "default": { "_type": "literalExpression", "text": "{ }" },
                "relatedPackages": { "_type": "mdDoc", "text": "Use `firefox`." },
                "example": {
                  "_type": "literalExpression",
                  "text": "{\n  \"browser.startup.homepage\" = \"https://nixos.org\";\n}\n"
               }
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert!(matches!(option.description, Some(DocText::Markdown(_))));
        assert_eq!(
            option.default,
            Some(DocValue::NixExpression("{ }".to_owned()))
        );
        assert!(
            matches!(option.example, Some(DocValue::NixExpression(ref text)) if text.contains("browser.startup.homepage"))
        );
        assert_eq!(
            option.related_packages,
            Some(DocText::Markdown("Use `firefox`.".to_owned()))
        );
    }

    #[test]
    fn preserves_unknown_typed_option_values_as_json() {
        let json = r#"
           {
             "programs.example.value": {
               "default": {
                 "_type": "customValue",
                 "text": "Preserved text.",
                 "extra": true
               }
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let SearchDocument::Option(option) = &docs[0] else {
            panic!("expected option document");
        };

        assert_eq!(
            option.default,
            Some(DocValue::Json(serde_json::json!({
                "_type": "customValue",
                "text": "Preserved text.",
                "extra": true
            })))
        );
    }

    #[test]
    fn parses_option_doc_text_objects_without_losing_text() {
        let json = r#"
           {
              "programs.example.unknown": {
                "description": { "_type": "unknownDoc", "text": "Preserved text.", "extra": true }
              },
              "programs.example.docbook": {
                "description": { "_type": "literalDocBook", "text": "<para>Hello</para>" }
              },
              "programs.example.docbook-string": {
                "description": "<para>Hello <literal>world</literal></para>"
              },
              "programs.example.docbook-emphasis-string": {
                "description": "<emphasis>Hello</emphasis>"
              },
               "programs.example.docbook-variablelist-string": {
                 "description": "<variablelist><varlistentry><term>foo</term><listitem><para>bar</para></listitem></varlistentry></variablelist>"
              },
               "programs.example.docbook-replaceable-string": {
                 "description": "<replaceable>name</replaceable>"
                },
                "programs.example.placeholder-string": {
                  "description": "<option> is a placeholder"
                },
                "programs.example.object": {
                  "description": { "unexpected": true }
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        let descriptions = docs
            .iter()
            .map(|doc| match doc {
                SearchDocument::Option(option) => {
                    (option.common.name.as_str(), &option.description)
                }
                SearchDocument::Package(_) => unreachable!("expected option documents"),
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(
            descriptions["programs.example.unknown"],
            &Some(DocText::Unknown {
                text: "Preserved text.".to_owned(),
                raw: serde_json::json!({
                    "_type": "unknownDoc",
                    "text": "Preserved text.",
                    "extra": true
                })
            })
        );
        assert_eq!(
            descriptions["programs.example.docbook"],
            &Some(DocText::DocBook("<para>Hello</para>".to_owned()))
        );
        assert_eq!(
            descriptions["programs.example.docbook-string"],
            &Some(DocText::DocBook(
                "<para>Hello <literal>world</literal></para>".to_owned()
            ))
        );
        assert_eq!(
            descriptions["programs.example.docbook-emphasis-string"],
            &Some(DocText::DocBook("<emphasis>Hello</emphasis>".to_owned()))
        );
        assert_eq!(
            descriptions["programs.example.docbook-variablelist-string"],
            &Some(DocText::DocBook(
                "<variablelist><varlistentry><term>foo</term><listitem><para>bar</para></listitem></varlistentry></variablelist>".to_owned()
            ))
        );
        assert_eq!(
            descriptions["programs.example.docbook-replaceable-string"],
            &Some(DocText::DocBook(
                "<replaceable>name</replaceable>".to_owned()
            ))
        );
        assert_eq!(
            descriptions["programs.example.placeholder-string"],
            &Some(DocText::Markdown("<option> is a placeholder".to_owned()))
        );
        assert_eq!(
            descriptions["programs.example.object"],
            &Some(DocText::Json(serde_json::json!({ "unexpected": true })))
        );
    }

    #[test]
    fn sorts_documents_by_stable_id() {
        let json = r#"
           {
             "services.nginx.enable": {
               "description": "Whether to enable Nginx."
             },
             "programs.git.enable": {
               "description": "Whether to enable Git."
             }
           }
           "#;

        let docs = parse_options_json(json.as_bytes(), &ingest_context()).unwrap();

        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].name(), OPTION_GIT_ENABLE);
        assert_eq!(docs[1].name(), OPTION_NGINX_ENABLE);
    }
}
