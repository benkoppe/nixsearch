use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, TantivyDocument, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, doc};
use time::OffsetDateTime;

use nix_search_core::{ArtifactKind, OptionDoc, PackageDoc, SearchDocument};

const WRITER_MEMORY_BYTES: usize = 50_000_000;

pub const INDEX_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
struct IndexFields {
    id: Field,
    source: Field,
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
            source: schema.get_field("source").context("missing field source")?,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope {
    pub source: String,
    pub ref_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub query: String,
    pub limit: usize,
    pub scopes: Vec<SearchScope>,
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

    pub fn search(&self, options: SearchOptions) -> Result<Vec<SearchHit>> {
        let searcher = self.reader.searcher();

        let mut parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.name_exact,
                self.fields.name_groups,
                self.fields.name_text,
                self.fields.name_root,
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
        parser.set_field_boost(self.fields.name_groups, 15.0);
        parser.set_field_boost(self.fields.name_text, 10.0);
        parser.set_field_boost(self.fields.name_root, 8.0);
        parser.set_field_boost(self.fields.name_leaf, 6.0);
        parser.set_field_boost(self.fields.attribute_exact, 25.0);
        parser.set_field_boost(self.fields.attribute_text, 12.0);
        parser.set_field_boost(self.fields.main_program, 20.0);
        parser.set_field_boost(self.fields.option_set, 3.0);
        parser.set_field_boost(self.fields.parents, 2.0);
        parser.set_field_boost(self.fields.package_set, 2.0);
        parser.set_field_boost(self.fields.option_type, 1.5);
        parser.set_field_boost(self.fields.platforms, 1.0);
        parser.set_field_boost(self.fields.description, 1.0);

        let parsed_query = parser
            .parse_query(&options.query)
            .with_context(|| format!("failed to parse query {:?}", options.query))?;

        let mut clauses: Vec<(tantivy::query::Occur, Box<dyn tantivy::query::Query>)> =
            vec![(tantivy::query::Occur::Must, parsed_query)];

        if !options.scopes.is_empty() {
            let mut scope_clauses = Vec::with_capacity(options.scopes.len());

            for scope in options.scopes {
                let source_query: Box<dyn tantivy::query::Query> =
                    Box::new(tantivy::query::TermQuery::new(
                        tantivy::Term::from_field_text(self.fields.source, &scope.source),
                        tantivy::schema::IndexRecordOption::Basic,
                    ));

                let ref_query: Box<dyn tantivy::query::Query> =
                    Box::new(tantivy::query::TermQuery::new(
                        tantivy::Term::from_field_text(self.fields.ref_id, &scope.ref_id),
                        tantivy::schema::IndexRecordOption::Basic,
                    ));

                let pair_query: Box<dyn tantivy::query::Query> =
                    Box::new(tantivy::query::BooleanQuery::new(vec![
                        (tantivy::query::Occur::Must, source_query),
                        (tantivy::query::Occur::Must, ref_query),
                    ]));

                scope_clauses.push((tantivy::query::Occur::Should, pair_query));
            }

            clauses.push((
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::BooleanQuery::new(scope_clauses)),
            ));
        }

        let query = tantivy::query::BooleanQuery::new(clauses);

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(options.limit).order_by_score())
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
            self.fields.source => common.source.clone(),
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
            self.fields.source => common.source.clone(),
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
    builder.add_text_field("source", STRING | STORED);
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

#[derive(Debug, Clone)]
pub struct IndexStore {
    root: PathBuf,
}

impl IndexStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn generations_dir(&self) -> PathBuf {
        self.root.join("generations")
    }

    pub fn current_file(&self) -> PathBuf {
        self.root.join("CURRENT")
    }

    pub fn create_generation_path(&self) -> Result<PathBuf> {
        fs::create_dir_all(self.generations_dir()).with_context(|| {
            format!(
                "failed to create generations dir {}",
                self.generations_dir().display()
            )
        })?;

        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();
        let path = self
            .generations_dir()
            .join(format!("generation-{timestamp}"));

        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create generation {}", path.display()))?;

        Ok(path)
    }

    pub fn publish(&self, generation_path: &Path) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create index root {}", self.root.display()))?;

        let generation_path = generation_path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", generation_path.display()))?;

        let current_tmp = self.root.join("CURRENT.tmp");

        fs::write(&current_tmp, generation_path.to_string_lossy().as_bytes())
            .with_context(|| format!("failed to write {}", current_tmp.display()))?;

        fs::rename(&current_tmp, self.current_file()).with_context(|| {
            format!(
                "failed to publish current index {}",
                self.current_file().display()
            )
        })?;

        Ok(())
    }

    pub fn current_path(&self) -> Result<PathBuf> {
        let raw = fs::read_to_string(self.current_file()).with_context(|| {
            format!(
                "failed to read current index file {}; run `nix-search update` first",
                self.current_file().display()
            )
        })?;

        let path = PathBuf::from(raw.trim());

        if path.as_os_str().is_empty() {
            anyhow::bail!("current index file is empty");
        }

        Ok(path)
    }

    pub fn manifest_path(&self, generation_path: &Path) -> PathBuf {
        generation_path.join("index-manifest.json")
    }

    pub fn write_manifest(
        &self,
        generation_path: &Path,
        manifest: &IndexGenerationManifest,
    ) -> Result<()> {
        let path = self.manifest_path(generation_path);
        let bytes = serde_json::to_vec_pretty(manifest)
            .context("failed to serialize index generation manifest")?;

        fs::write(&path, bytes)
            .with_context(|| format!("failed to write index metadata {}", path.display()))?;

        Ok(())
    }

    pub fn read_manifest(&self, generation_path: &Path) -> Result<IndexGenerationManifest> {
        let path = self.manifest_path(generation_path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read index metadata {}", path.display()))?;

        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse index metadata {}", path.display()))
    }

    pub fn current_manifest(&self) -> Result<IndexGenerationManifest> {
        let current = self.current_path()?;
        self.read_manifest(&current)
    }

    pub fn try_current_manifest(&self) -> Result<Option<IndexGenerationManifest>> {
        if !self.current_file().exists() {
            return Ok(None);
        }

        self.current_manifest().map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexGenerationManifest {
    pub schema_version: u32,

    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,

    pub document_count: usize,
    pub targets: Vec<IndexTargetManifest>,
}

impl IndexGenerationManifest {
    pub fn new(document_count: usize, targets: Vec<IndexTargetManifest>) -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            generated_at: OffsetDateTime::now_utc(),
            document_count,
            targets,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexTargetManifest {
    pub source: String,
    pub ref_id: String,
    pub artifact_kind: ArtifactKind,
    pub document_count: usize,
    pub artifact_hash: Option<String>,
    pub revision: Option<String>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use nix_search_core::ArtifactKind;
    use nix_search_test_support::{REF_SMALL, SOURCE_FIXTURES};

    #[test]
    fn index_store_publishes_current_generation() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(tempdir.path());

        let generation = store.create_generation_path().unwrap();
        assert!(generation.exists());

        store.publish(&generation).unwrap();

        let current = store.current_path().unwrap();

        assert_eq!(current, generation.canonicalize().unwrap());
    }

    #[test]
    fn index_store_writes_and_reads_generation_manifest() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(tempdir.path());

        let generation = store.create_generation_path().unwrap();

        let manifest = super::IndexGenerationManifest::new(
            10,
            vec![super::IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 10,
                artifact_hash: Some("abc123".into()),
                revision: None,
            }],
        );

        store.write_manifest(&generation, &manifest).unwrap();
        store.publish(&generation).unwrap();

        let loaded = store.current_manifest().unwrap();

        assert_eq!(loaded.document_count, 10);
        assert_eq!(loaded.targets.len(), 1);
        assert_eq!(loaded.targets[0].source, SOURCE_FIXTURES);
        assert_eq!(loaded.targets[0].ref_id, REF_SMALL);
    }

    #[test]
    fn index_store_returns_none_when_current_manifest_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(tempdir.path());

        let manifest = store.try_current_manifest().unwrap();

        assert!(manifest.is_none());
    }
}
