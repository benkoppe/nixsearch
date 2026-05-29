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
#[serde(tag = "kind", content = "text", rename_all = "kebab-case")]
pub enum DocText {
    Markdown(String),
    DocBook(String),
    Plain(String),
}

impl DocText {
    pub fn plain_text(&self) -> &str {
        match self {
            Self::Markdown(value) | Self::DocBook(value) | Self::Plain(value) => value,
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
            Self::NixExpression(value)
            | Self::Markdown(value)
            | Self::DocBook(value)
            | Self::Plain(value) => value.clone(),
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

    use super::{CommonDoc, DocumentKind, PackageDoc};

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
}
