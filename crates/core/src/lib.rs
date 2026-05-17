use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameParts {
    pub segments: Vec<String>,
    pub root: Option<String>,
    pub groups: Vec<String>,
    pub leaf: Option<String>,
}

impl NameParts {
    pub fn from_dotted(name: &str) -> Self {
        let segments: Vec<String> = name
            .split('.')
            .filter(|part| !part.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        let root = if segments.len() > 1 {
            segments.first().cloned()
        } else {
            None
        };

        let groups = if segments.len() <= 1 {
            Vec::new()
        } else {
            (1..segments.len())
                .map(|end| segments[..end].join("."))
                .collect()
        };

        let leaf = segments.last().cloned();

        Self {
            segments,
            root,
            groups,
            leaf,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    OptionsJson,
    PackagesJson,
    FlakeInfoJson,
}

impl ArtifactKind {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::OptionsJson => "options.json",
            Self::PackagesJson => "packages.json",
            Self::FlakeInfoJson => "flake-info.json",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptionsJson => "options-json",
            Self::PackagesJson => "packages-json",
            Self::FlakeInfoJson => "flake-info-json",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub host: RepoHost,
    pub owner: String,
    pub repo: String,
    pub revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RepoHost {
    Github,
    Gitlab,
    Sourcehut,
    Codeberg,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SourceLinkConfig {
    Github {
        owner: String,
        repo: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        strip_prefixes: Vec<String>,
    },

    Prefix {
        url_prefix: String,
        #[serde(default)]
        strip_prefixes: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct SourceLinkResolver<'a> {
    config: &'a SourceLinkConfig,
    revision: Option<&'a str>,
}

impl<'a> SourceLinkResolver<'a> {
    pub fn new(config: &'a SourceLinkConfig, revision: Option<&'a str>) -> Self {
        Self { config, revision }
    }

    pub fn resolve_declaration(&self, declaration: &Declaration) -> Option<String> {
        if let Some(url) = &declaration.url {
            return Some(url.clone());
        }

        self.resolve_location(&declaration.name)
    }

    pub fn resolve_package_position(&self, position: &str) -> Option<String> {
        self.resolve_location(position)
    }

    pub fn resolve_location(&self, location: &str) -> Option<String> {
        if is_absolute_url(location) {
            return Some(location.to_owned());
        }

        let normalized = normalize_source_location(location, self.config.strip_prefixes())?;
        self.url_for_path(&normalized.path, normalized.line.as_deref())
    }

    fn url_for_path(&self, path: &str, line: Option<&str>) -> Option<String> {
        let path = path.trim_start_matches('/');

        if path.is_empty() {
            return None;
        }

        let mut url = match self.config {
            SourceLinkConfig::Github {
                owner,
                repo,
                revision,
                ..
            } => {
                let revision = self.revision.or(revision.as_deref()).unwrap_or("master");

                format!(
                    "https://github.com/{owner}/{repo}/blob/{revision}/{path}",
                    owner = owner,
                    repo = repo,
                    revision = revision,
                    path = path
                )
            }

            SourceLinkConfig::Prefix { url_prefix, .. } => {
                format!("{}/{}", url_prefix.trim_end_matches('/'), path)
            }
        };

        if let Some(line) = line {
            url.push_str("#L");
            url.push_str(line);
        }

        Some(url)
    }
}

impl SourceLinkConfig {
    fn strip_prefixes(&self) -> &[String] {
        match self {
            Self::Github { strip_prefixes, .. } | Self::Prefix { strip_prefixes, .. } => {
                strip_prefixes
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedSourceLocation {
    path: String,
    line: Option<String>,
}

fn normalize_source_location(
    location: &str,
    strip_prefixes: &[String],
) -> Option<NormalizedSourceLocation> {
    let mut raw = location.trim();

    if raw.is_empty() {
        return None;
    }

    if raw.starts_with('<') && raw.ends_with('>') {
        raw = &raw[1..raw.len() - 1];
    }

    let (raw_path, line) = split_line_suffix(raw);
    let mut path = raw_path.to_owned();

    path = apply_strip_prefixes(&path, strip_prefixes);
    path = strip_nix_store_source_prefix(&path).unwrap_or(path);

    if let Some(rest) = path.strip_prefix("nixpkgs/") {
        path = rest.to_owned();
    }

    if path.starts_with('/') {
        return None;
    }

    let path = path.trim_start_matches("./").to_owned();

    if path.is_empty() {
        return None;
    }

    Some(NormalizedSourceLocation { path, line })
}

fn split_line_suffix(value: &str) -> (&str, Option<String>) {
    let Some((path, line)) = value.rsplit_once(':') else {
        return (value, None);
    };

    if line.chars().all(|ch| ch.is_ascii_digit()) {
        (path, Some(line.to_owned()))
    } else {
        (value, None)
    }
}

fn apply_strip_prefixes(path: &str, strip_prefixes: &[String]) -> String {
    for prefix in strip_prefixes {
        let prefix = prefix.trim();

        if prefix.is_empty() {
            continue;
        }

        if let Some(rest) = path.strip_prefix(prefix) {
            return rest.trim_start_matches('/').to_owned();
        }
    }

    path.to_owned()
}

fn strip_nix_store_source_prefix(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/nix/store/")?;
    let (_, relative) = rest.split_once('/')?;

    if relative.is_empty() {
        None
    } else {
        Some(relative.to_owned())
    }
}

fn is_absolute_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestContext {
    pub project: String,
    pub dataset: String,
    pub ref_id: String,
    pub revision: Option<String>,
    pub repo: Option<Repo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommonDoc {
    pub id: String,
    pub project: String,
    pub dataset: String,
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
        let id = make_document_id(
            &context.project,
            &context.dataset,
            &context.ref_id,
            kind.as_str(),
            &name,
        );

        Self {
            id,
            project: context.project.clone(),
            dataset: context.dataset.clone(),
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
    pub description: Option<String>,
    pub option_type: Option<String>,
    pub default: Option<Value>,
    pub example: Option<Value>,
    pub related_packages: Option<String>,
    pub read_only: Option<bool>,
    pub internal: Option<bool>,
    pub visible: Option<bool>,
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

pub fn make_document_id(
    project: &str,
    dataset: &str,
    ref_id: &str,
    kind: &str,
    name: &str,
) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        escape_id_part(project),
        escape_id_part(dataset),
        escape_id_part(ref_id),
        escape_id_part(kind),
        escape_id_part(name),
    )
}

fn escape_id_part(part: &str) -> String {
    part.replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::{
        CommonDoc, Declaration, DocumentKind, IngestContext, NameParts, SourceLinkConfig,
        SourceLinkResolver, make_document_id,
    };

    #[test]
    fn document_ids_are_stable_and_include_identity_dimensions() {
        let id = make_document_id(
            "nixpkgs",
            "nixos-options",
            "unstable",
            "option",
            "programs.git.enable",
        );

        assert_eq!(
            id,
            "nixpkgs/nixos-options/unstable/option/programs.git.enable"
        );
    }

    #[test]
    fn document_id_parts_escape_slashes() {
        let id = make_document_id(
            "my/project",
            "options",
            "release/25.05",
            "option",
            "programs.git.enable",
        );

        assert_eq!(
            id,
            "my%2Fproject/options/release%2F25.05/option/programs.git.enable"
        );
    }

    #[test]
    fn common_doc_uses_context_identity() {
        let context = IngestContext {
            project: "nixpkgs".into(),
            dataset: "nixos-options".into(),
            ref_id: "unstable".into(),
            revision: Some("abc123".into()),
            repo: None,
        };

        let doc = CommonDoc::new(&context, DocumentKind::Option, "programs.git.enable");

        assert_eq!(doc.project, "nixpkgs");
        assert_eq!(doc.dataset, "nixos-options");
        assert_eq!(doc.ref_id, "unstable");
        assert_eq!(doc.revision.as_deref(), Some("abc123"));
        assert_eq!(doc.kind, DocumentKind::Option);
        assert_eq!(doc.name, "programs.git.enable");
        assert_eq!(
            doc.id,
            "nixpkgs/nixos-options/unstable/option/programs.git.enable"
        );

        assert_eq!(doc.name_parts.root.as_deref(), Some("programs"));
        assert_eq!(doc.name_parts.groups, ["programs", "programs.git"]);
        assert_eq!(doc.name_parts.leaf.as_deref(), Some("enable"));
    }

    #[test]
    fn package_doc_uses_attribute_as_document_name() {
        let context = IngestContext {
            project: "nixpkgs".into(),
            dataset: "packages".into(),
            ref_id: "unstable".into(),
            revision: None,
            repo: None,
        };

        let doc = super::PackageDoc::new(&context, "python3Packages.requests");

        assert_eq!(doc.common.name, "python3Packages.requests");
        assert_eq!(doc.attribute, "python3Packages.requests");
        assert_eq!(doc.package_set.as_deref(), Some("python3Packages"));
    }

    #[test]
    fn name_parts_for_nested_dotted_name() {
        let parts = NameParts::from_dotted("services.tailscale.enable");

        assert_eq!(parts.segments, ["services", "tailscale", "enable"]);
        assert_eq!(parts.root.as_deref(), Some("services"));
        assert_eq!(parts.groups, ["services", "services.tailscale"]);
        assert_eq!(parts.leaf.as_deref(), Some("enable"));
    }

    #[test]
    fn name_parts_for_package_set_name() {
        let parts = NameParts::from_dotted("rubyPackages.rails");

        assert_eq!(parts.segments, ["rubyPackages", "rails"]);
        assert_eq!(parts.root.as_deref(), Some("rubyPackages"));
        assert_eq!(parts.groups, ["rubyPackages"]);
        assert_eq!(parts.leaf.as_deref(), Some("rails"));
    }

    #[test]
    fn name_parts_for_single_name() {
        let parts = NameParts::from_dotted("git");

        assert_eq!(parts.segments, ["git"]);
        assert_eq!(parts.root, None);
        assert!(parts.groups.is_empty());
        assert_eq!(parts.leaf.as_deref(), Some("git"));
    }

    #[test]
    fn name_parts_ignores_empty_segments() {
        let parts = NameParts::from_dotted(".services..tailscale.enable.");

        assert_eq!(parts.segments, ["services", "tailscale", "enable"]);
        assert_eq!(parts.root.as_deref(), Some("services"));
        assert_eq!(parts.groups, ["services", "services.tailscale"]);
        assert_eq!(parts.leaf.as_deref(), Some("enable"));
    }

    #[test]
    fn github_source_link_resolves_relative_declaration_with_line() {
        let config = SourceLinkConfig::Github {
            owner: "NixOS".into(),
            repo: "nixpkgs".into(),
            revision: Some("abc123".into()),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        let declaration = Declaration {
            name: "nixos/modules/programs/git.nix:42".into(),
            url: None,
        };

        assert_eq!(
            resolver.resolve_declaration(&declaration).as_deref(),
            Some("https://github.com/NixOS/nixpkgs/blob/abc123/nixos/modules/programs/git.nix#L42")
        );
    }

    #[test]
    fn github_source_link_uses_document_revision_as_fallback() {
        let config = SourceLinkConfig::Github {
            owner: "NixOS".into(),
            repo: "nixpkgs".into(),
            revision: None,
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, Some("def456"));

        assert_eq!(
            resolver
                .resolve_location("pkgs/applications/version-management/git/default.nix")
                .as_deref(),
            Some(
                "https://github.com/NixOS/nixpkgs/blob/def456/pkgs/applications/version-management/git/default.nix"
            )
        );
    }

    #[test]
    fn prefix_source_link_resolves_relative_location() {
        let config = SourceLinkConfig::Prefix {
            url_prefix: "https://example.com/blob/main/".into(),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver.resolve_location("modules/foo.nix:7").as_deref(),
            Some("https://example.com/blob/main/modules/foo.nix#L7")
        );
    }

    #[test]
    fn source_link_preserves_existing_declaration_url() {
        let config = SourceLinkConfig::Prefix {
            url_prefix: "https://example.com/blob/main/".into(),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        let declaration = Declaration {
            name: "foo.nix".into(),
            url: Some("https://already.example/source.nix".into()),
        };

        assert_eq!(
            resolver.resolve_declaration(&declaration).as_deref(),
            Some("https://already.example/source.nix")
        );
    }

    #[test]
    fn source_link_preserves_absolute_location_url() {
        let config = SourceLinkConfig::Prefix {
            url_prefix: "https://example.com/blob/main/".into(),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver
                .resolve_location("https://already.example/source.nix#L1")
                .as_deref(),
            Some("https://already.example/source.nix#L1")
        );
    }

    #[test]
    fn source_link_strips_nix_store_source_prefix() {
        let config = SourceLinkConfig::Github {
            owner: "example".into(),
            repo: "repo".into(),
            revision: Some("main".into()),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver
                .resolve_location("/nix/store/abc123-source/modules/foo.nix:9")
                .as_deref(),
            Some("https://github.com/example/repo/blob/main/modules/foo.nix#L9")
        );
    }

    #[test]
    fn source_link_applies_configured_strip_prefixes() {
        let config = SourceLinkConfig::Github {
            owner: "example".into(),
            repo: "repo".into(),
            revision: Some("main".into()),
            strip_prefixes: vec!["/build/source/".into()],
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver
                .resolve_location("/build/source/modules/foo.nix")
                .as_deref(),
            Some("https://github.com/example/repo/blob/main/modules/foo.nix")
        );
    }

    #[test]
    fn source_link_strips_nixpkgs_angle_prefix() {
        let config = SourceLinkConfig::Github {
            owner: "NixOS".into(),
            repo: "nixpkgs".into(),
            revision: Some("main".into()),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver
                .resolve_location("<nixpkgs/nixos/modules/foo.nix>")
                .as_deref(),
            Some("https://github.com/NixOS/nixpkgs/blob/main/nixos/modules/foo.nix")
        );
    }

    #[test]
    fn source_link_returns_none_for_unstripped_absolute_paths() {
        let config = SourceLinkConfig::Github {
            owner: "example".into(),
            repo: "repo".into(),
            revision: Some("main".into()),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, None);

        assert_eq!(
            resolver.resolve_location("/some/random/local/path.nix"),
            None
        );
    }

    #[test]
    fn github_source_link_prefers_document_revision_over_config_revision() {
        let config = SourceLinkConfig::Github {
            owner: "NixOS".into(),
            repo: "nixpkgs".into(),
            revision: Some("branch-name".into()),
            strip_prefixes: Vec::new(),
        };

        let resolver = SourceLinkResolver::new(&config, Some("exact-commit"));

        assert_eq!(
            resolver
                .resolve_location("nixos/modules/foo.nix")
                .as_deref(),
            Some("https://github.com/NixOS/nixpkgs/blob/exact-commit/nixos/modules/foo.nix")
        );
    }
}
