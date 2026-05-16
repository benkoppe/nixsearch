use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, TantivyDocument, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, doc};

use nix_search_core::{OptionDoc, PackageDoc, SearchDocument};

const WRITER_MEMORY_BYTES: usize = 50_000_000;

#[derive(Debug, Clone)]
struct IndexFields {
    id: Field,
    project: Field,
    dataset: Field,
    ref_id: Field,
    kind: Field,
    name_exact: Field,
    name_text: Field,
    name_root: Field,
    name_groups: Field,
    name_leaf: Field,
    description: Field,
    option_set: Field,
    parents: Field,
    option_type: Field,
    stored_json: Field,
    attribute_exact: Field,
    attribute_text: Field,
    package_set: Field,
    platforms: Field,
    main_program: Field,
}

impl IndexFields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            id: schema.get_field("id").context("missing field id")?,
            project: schema
                .get_field("project")
                .context("missing field project")?,
            dataset: schema
                .get_field("dataset")
                .context("missing field dataset")?,
            ref_id: schema.get_field("ref").context("missing field ref")?,
            kind: schema.get_field("kind").context("missing field kind")?,
            name_exact: schema
                .get_field("name_exact")
                .context("missing field name_exact")?,
            name_text: schema
                .get_field("name_text")
                .context("missing field name_text")?,
            name_root: schema
                .get_field("name_root")
                .context("missing field name_root")?,
            name_groups: schema
                .get_field("name_groups")
                .context("missing field name_groups")?,
            name_leaf: schema
                .get_field("name_leaf")
                .context("missing field name_leaf")?,
            description: schema
                .get_field("description")
                .context("missing field description")?,
            option_set: schema
                .get_field("option_set")
                .context("missing field option_set")?,
            parents: schema
                .get_field("parents")
                .context("missing field parents")?,
            option_type: schema
                .get_field("option_type")
                .context("missing field option_type")?,
            stored_json: schema
                .get_field("stored_json")
                .context("missing field stored_json")?,
            attribute_exact: schema
                .get_field("attribute_exact")
                .context("missing field attribute_exact")?,
            attribute_text: schema
                .get_field("attribute_text")
                .context("missing field attribute_text")?,
            package_set: schema
                .get_field("package_set")
                .context("missing field package_set")?,
            platforms: schema
                .get_field("platforms")
                .context("missing field platforms")?,
            main_program: schema
                .get_field("main_program")
                .context("missing field main_program")?,
        })
    }
}

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: IndexFields,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub score: f32,
    pub document: SearchDocument,
}

impl SearchIndex {
    pub fn create_or_replace(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("failed to remove existing index {}", path.display()))?;
        }

        fs::create_dir_all(path)
            .with_context(|| format!("failed to create index directory {}", path.display()))?;

        let schema = build_schema();
        let fields = IndexFields::from_schema(&schema)?;
        let index = Index::create_in_dir(path, schema)
            .with_context(|| format!("failed to create Tantivy index at {}", path.display()))?;
        let reader = index.reader().context("failed to create index reader")?;

        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let index = Index::open_in_dir(path)
            .with_context(|| format!("failed to open Tantivy index at {}", path.display()))?;
        let schema = index.schema();
        let fields = IndexFields::from_schema(&schema)?;
        let reader = index.reader().context("failed to create index reader")?;

        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    pub fn writer(&self) -> Result<SearchIndexWriter> {
        let writer = self
            .index
            .writer(WRITER_MEMORY_BYTES)
            .context("failed to create index writer")?;

        Ok(SearchIndexWriter {
            writer,
            fields: self.fields.clone(),
        })
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let searcher = self.reader.searcher();

        let mut parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.name_exact,
                self.fields.name_text,
                self.fields.name_root,
                self.fields.name_groups,
                self.fields.name_leaf,
                self.fields.description,
                self.fields.option_set,
                self.fields.parents,
                self.fields.option_type,
                self.fields.attribute_exact,
                self.fields.attribute_text,
                self.fields.package_set,
                self.fields.platforms,
                self.fields.main_program,
            ],
        );

        parser.set_field_boost(self.fields.name_exact, 20.0);
        parser.set_field_boost(self.fields.name_text, 10.0);
        parser.set_field_boost(self.fields.name_groups, 15.0);
        parser.set_field_boost(self.fields.name_root, 8.0);
        parser.set_field_boost(self.fields.name_leaf, 6.0);
        parser.set_field_boost(self.fields.option_set, 3.0);
        parser.set_field_boost(self.fields.parents, 2.0);
        parser.set_field_boost(self.fields.option_type, 1.5);
        parser.set_field_boost(self.fields.description, 1.0);
        parser.set_field_boost(self.fields.attribute_exact, 25.0);
        parser.set_field_boost(self.fields.attribute_text, 12.0);
        parser.set_field_boost(self.fields.main_program, 20.0);
        parser.set_field_boost(self.fields.package_set, 2.0);
        parser.set_field_boost(self.fields.platforms, 1.0);

        let query = parser
            .parse_query(query)
            .with_context(|| format!("failed to parse query {query:?}"))?;

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .context("search failed")?;

        let mut hits = Vec::with_capacity(top_docs.len());

        for (score, address) in top_docs {
            let retrieved: TantivyDocument = searcher
                .doc(address)
                .context("failed to retrieve search result document")?;

            let stored_json = retrieved
                .get_first(self.fields.stored_json)
                .and_then(|value| value.as_str())
                .context("search result did not contain stored_json")?;

            let document = serde_json::from_str(stored_json)
                .context("failed to deserialize stored search document")?;

            hits.push(SearchHit { score, document });
        }

        Ok(hits)
    }
}

pub struct SearchIndexWriter {
    writer: IndexWriter,
    fields: IndexFields,
}

impl SearchIndexWriter {
    pub fn add_document(&mut self, document: &SearchDocument) -> Result<()> {
        match document {
            SearchDocument::Option(option) => self.add_option(option),
            SearchDocument::Package(package) => self.add_package(package),
        }
    }

    pub fn commit(mut self) -> Result<()> {
        self.writer
            .commit()
            .context("failed to commit index writer")?;
        Ok(())
    }

    fn add_option(&mut self, option: &OptionDoc) -> Result<()> {
        let common = &option.common;
        let stored_json = serde_json::to_string(&SearchDocument::Option(option.clone()))
            .context("failed to serialize search document")?;

        let mut document = doc!(
            self.fields.id => common.id.clone(),
            self.fields.project => common.project.clone(),
            self.fields.dataset => common.dataset.clone(),
            self.fields.ref_id => common.ref_id.clone(),
            self.fields.kind => common.kind.as_str(),
            self.fields.name_exact => common.name.clone(),
            self.fields.name_text => common.name.clone(),
            self.fields.stored_json => stored_json,
        );

        add_name_parts(&mut document, &self.fields, common);

        if let Some(description) = &option.description {
            document.add_text(self.fields.description, description);
        }

        if let Some(option_set) = &option.option_set {
            document.add_text(self.fields.option_set, option_set);
        }

        if let Some(option_type) = &option.option_type {
            document.add_text(self.fields.option_type, option_type);
        }

        for parent in &option.parents {
            document.add_text(self.fields.parents, parent);
        }

        self.writer
            .add_document(document)
            .context("failed to add document to index")?;

        Ok(())
    }

    fn add_package(&mut self, package: &PackageDoc) -> Result<()> {
        let common = &package.common;
        let stored_json = serde_json::to_string(&SearchDocument::Package(package.clone()))
            .context("failed to serialize search document")?;

        let mut document = doc!(
            self.fields.id => common.id.clone(),
            self.fields.project => common.project.clone(),
            self.fields.dataset => common.dataset.clone(),
            self.fields.ref_id => common.ref_id.clone(),
            self.fields.kind => common.kind.as_str(),
            self.fields.name_exact => common.name.clone(),
            self.fields.name_text => common.name.clone(),
            self.fields.attribute_exact => package.attribute.clone(),
            self.fields.attribute_text => package.attribute.clone(),
            self.fields.stored_json => stored_json,
        );

        add_name_parts(&mut document, &self.fields, common);

        if let Some(description) = &package.description {
            document.add_text(self.fields.description, description);
        }

        if let Some(package_set) = &package.package_set {
            document.add_text(self.fields.package_set, package_set);
        }

        if let Some(main_program) = &package.main_program {
            document.add_text(self.fields.main_program, main_program);
        }

        for platform in &package.platforms {
            document.add_text(self.fields.platforms, platform);
        }

        self.writer
            .add_document(document)
            .context("failed to add package document to index")?;

        Ok(())
    }
}

fn add_name_parts(
    document: &mut TantivyDocument,
    fields: &IndexFields,
    common: &nix_search_core::CommonDoc,
) {
    if let Some(root) = &common.name_parts.root {
        document.add_text(fields.name_root, root);
    }

    if let Some(leaf) = &common.name_parts.leaf {
        document.add_text(fields.name_leaf, leaf);
    }

    for group in &common.name_parts.groups {
        document.add_text(fields.name_groups, group);
    }
}

fn build_schema() -> Schema {
    let mut builder = Schema::builder();

    builder.add_text_field("id", STRING | STORED);
    builder.add_text_field("project", STRING | STORED);
    builder.add_text_field("dataset", STRING | STORED);
    builder.add_text_field("ref", STRING | STORED);
    builder.add_text_field("kind", STRING | STORED);

    builder.add_text_field("name_exact", STRING | STORED);
    builder.add_text_field("name_text", TEXT | STORED);
    builder.add_text_field("name_root", STRING | STORED);
    builder.add_text_field("name_groups", STRING | STORED);
    builder.add_text_field("name_leaf", STRING | STORED);
    builder.add_text_field("description", TEXT);
    builder.add_text_field("option_set", STRING | STORED);
    builder.add_text_field("parents", STRING | STORED);
    builder.add_text_field("option_type", TEXT | STORED);

    builder.add_text_field("attribute_exact", STRING | STORED);
    builder.add_text_field("attribute_text", TEXT | STORED);
    builder.add_text_field("package_set", STRING | STORED);
    builder.add_text_field("platforms", STRING | STORED);
    builder.add_text_field("main_program", STRING | STORED);

    builder.add_text_field("stored_json", STORED);

    builder.build()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use nix_search_core::{IngestContext, OptionDoc, SearchDocument};

    use super::SearchIndex;

    fn test_context() -> IngestContext {
        IngestContext {
            project: "nixpkgs".into(),
            dataset: "nixos-options".into(),
            ref_id: "unstable".into(),
            revision: None,
            repo: None,
        }
    }

    fn option_doc(name: &str, description: &str, loc: &[&str]) -> SearchDocument {
        let mut doc = OptionDoc::new(&test_context(), name);

        doc.description = Some(description.to_owned());
        doc.loc = loc.iter().map(|part| (*part).to_owned()).collect();
        doc.option_set = doc.loc.first().cloned();
        doc.parents = (1..doc.loc.len())
            .map(|end| doc.loc[..end].join("."))
            .collect();

        SearchDocument::Option(doc)
    }

    #[test]
    fn indexes_and_searches_options() {
        let tempdir = tempdir().unwrap();

        let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
        let mut writer = index.writer().unwrap();

        writer
            .add_document(&option_doc(
                "programs.git.enable",
                "Whether to enable Git.",
                &["programs", "git", "enable"],
            ))
            .unwrap();

        writer
            .add_document(&option_doc(
                "services.nginx.enable",
                "Whether to enable Nginx.",
                &["services", "nginx", "enable"],
            ))
            .unwrap();

        writer.commit().unwrap();

        let index = SearchIndex::open(tempdir.path()).unwrap();

        let hits = index.search("git", 10).unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].document.name(), "programs.git.enable");
    }

    #[test]
    fn exact_option_name_query_finds_option() {
        let tempdir = tempdir().unwrap();

        let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
        let mut writer = index.writer().unwrap();

        writer
            .add_document(&option_doc(
                "programs.git.enable",
                "Whether to enable Git.",
                &["programs", "git", "enable"],
            ))
            .unwrap();

        writer.commit().unwrap();

        let index = SearchIndex::open(tempdir.path()).unwrap();

        let hits = index.search("programs.git.enable", 10).unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].document.name(), "programs.git.enable");
    }

    #[test]
    fn description_query_finds_matching_option() {
        let tempdir = tempdir().unwrap();

        let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
        let mut writer = index.writer().unwrap();

        writer
            .add_document(&option_doc(
                "boot.loader.systemd-boot.enable",
                "Whether to enable the systemd-boot EFI boot manager.",
                &["boot", "loader", "systemd-boot", "enable"],
            ))
            .unwrap();

        writer.commit().unwrap();

        let index = SearchIndex::open(tempdir.path()).unwrap();

        let hits = index.search("EFI", 10).unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].document.name(), "boot.loader.systemd-boot.enable");
    }

    #[test]
    fn group_query_finds_nested_option() {
        let tempdir = tempdir().unwrap();

        let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
        let mut writer = index.writer().unwrap();

        writer
            .add_document(&option_doc(
                "services.tailscale.enable",
                "Whether to enable Tailscale.",
                &["services", "tailscale", "enable"],
            ))
            .unwrap();

        writer
            .add_document(&option_doc(
                "services.nginx.enable",
                "Whether to enable Nginx.",
                &["services", "nginx", "enable"],
            ))
            .unwrap();

        writer.commit().unwrap();

        let index = SearchIndex::open(tempdir.path()).unwrap();

        let hits = index.search("services.tailscale", 10).unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].document.name(), "services.tailscale.enable");
    }

    #[test]
    fn leaf_query_finds_option() {
        let tempdir = tempdir().unwrap();

        let index = SearchIndex::create_or_replace(tempdir.path()).unwrap();
        let mut writer = index.writer().unwrap();

        writer
            .add_document(&option_doc(
                "services.tailscale.enable",
                "Whether to enable Tailscale.",
                &["services", "tailscale", "enable"],
            ))
            .unwrap();

        writer.commit().unwrap();

        let index = SearchIndex::open(tempdir.path()).unwrap();

        let hits = index.search("enable", 10).unwrap();

        assert!(!hits.is_empty());
        assert_eq!(hits[0].document.name(), "services.tailscale.enable");
    }
}
