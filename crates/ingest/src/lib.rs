use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use nixsearch_core::{
    Declaration, IngestContext, License, Maintainer, OptionDoc, PackageDoc, SearchDocument,
};

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
    description: Option<String>,

    #[serde(default, rename = "type")]
    option_type: Option<String>,

    #[serde(default)]
    default: Option<Value>,

    #[serde(default)]
    example: Option<Value>,

    #[serde(default)]
    related_packages: Option<String>,

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
    let raw_options: serde_json::Map<String, Value> =
        serde_json::from_reader(reader).context("failed to parse options JSON")?;

    let mut documents = Vec::with_capacity(raw_options.len());

    for (name, value) in raw_options {
        let raw: RawOption = serde_json::from_value(value)
            .with_context(|| format!("failed to parse option {name}"))?;

        documents.push(SearchDocument::Option(convert_option(name, raw, context)));
    }

    documents.sort_by(|left, right| left.id().cmp(right.id()));

    Ok(documents)
}

fn convert_option(name: String, raw: RawOption, context: &IngestContext) -> OptionDoc {
    let mut doc = OptionDoc::new(context, name);

    doc.loc = if raw.loc.is_empty() {
        doc.common.name.split('.').map(ToOwned::to_owned).collect()
    } else {
        raw.loc
    };

    doc.parents = parents_from_loc(&doc.loc);
    doc.option_set = doc.loc.first().cloned();
    doc.declarations = raw.declarations.into_iter().map(Into::into).collect();
    doc.description = raw.description;
    doc.option_type = raw.option_type;
    doc.default = raw.default;
    doc.example = raw.example;
    doc.related_packages = raw.related_packages;
    doc.read_only = raw.read_only;
    doc.internal = raw.internal;
    doc.visible = raw.visible;

    doc
}

fn parents_from_loc(loc: &[String]) -> Vec<String> {
    if loc.len() <= 1 {
        return Vec::new();
    }

    (1..loc.len()).map(|end| loc[..end].join(".")).collect()
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
    let raw: RawPackagesJson =
        serde_json::from_reader(reader).context("failed to parse packages JSON")?;

    let mut documents = Vec::with_capacity(raw.packages.len());

    for (attribute, package) in raw.packages {
        documents.push(SearchDocument::Package(convert_package(
            attribute, package, context,
        )));
    }

    documents.sort_by(|left, right| left.id().cmp(right.id()));

    Ok(documents)
}

fn convert_package(attribute: String, raw: RawPackage, context: &IngestContext) -> PackageDoc {
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
    doc.main_program = raw.meta.main_program;
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
    use nixsearch_core::SearchDocument;
    use nixsearch_test_support::{OPTION_GIT_ENABLE, OPTION_NGINX_ENABLE, ingest_context};

    use super::parse_options_json;

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
