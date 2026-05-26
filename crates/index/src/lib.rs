use std::fs;
use std::io::Write as _;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::QueryParser;
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT, TantivyDocument, Value as _};
use tantivy::{Index, IndexReader, IndexWriter, doc};
use time::OffsetDateTime;

use nixsearch_core::{ArtifactKind, DocumentKind, OptionDoc, PackageDoc, SearchDocument};

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

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub total: usize,
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
    pub offset: usize,
    pub scopes: Vec<SearchScope>,
}

#[derive(Debug, Clone)]
pub struct EntryLookup {
    pub source: String,
    pub ref_id: String,
    pub name: String,
    pub kind: Option<DocumentKind>,
}

#[derive(Debug, Clone)]
pub enum EntryLookupResult {
    Found(Box<SearchDocument>),
    NotFound,
    Ambiguous(Vec<SearchDocument>),
}

impl SearchIndex {
    pub fn create_or_replace(path: impl AsRef<Utf8Path>) -> Result<Self> {
        let path = path.as_ref();

        if path.exists() {
            fs::remove_dir_all(path)
                .with_context(|| format!("failed to remove existing index {path}"))?;
        }

        fs::create_dir_all(path)
            .with_context(|| format!("failed to create index directory {path}"))?;

        let schema = build_schema();
        let fields = IndexFields::from_schema(&schema)?;
        let index = Index::create_in_dir(path, schema)
            .with_context(|| format!("failed to create Tantivy index at {path}"))?;
        let reader = index.reader().context("failed to create index reader")?;

        Ok(Self {
            index,
            reader,
            fields,
        })
    }

    pub fn open(path: impl AsRef<Utf8Path>) -> Result<Self> {
        let path = path.as_ref();
        let index = Index::open_in_dir(path)
            .with_context(|| format!("failed to open Tantivy index at {path}"))?;
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

    pub fn search(&self, options: SearchOptions) -> Result<SearchResult> {
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

        let (top_docs, total) = searcher
            .search(
                &query,
                &(
                    TopDocs::with_limit(options.limit)
                        .and_offset(options.offset)
                        .order_by_score(),
                    Count,
                ),
            )
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

        Ok(SearchResult { hits, total })
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<SearchDocument>> {
        let searcher = self.reader.searcher();

        let query = tantivy::query::TermQuery::new(
            tantivy::Term::from_field_text(self.fields.id, id),
            tantivy::schema::IndexRecordOption::Basic,
        );

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(1).order_by_score())
            .context("entry lookup by id failed")?;

        let Some((_, address)) = top_docs.into_iter().next() else {
            return Ok(None);
        };

        self.document_at(address).map(Some)
    }

    pub fn find_entry(&self, lookup: EntryLookup) -> Result<EntryLookupResult> {
        let searcher = self.reader.searcher();

        let mut clauses: Vec<(tantivy::query::Occur, Box<dyn tantivy::query::Query>)> = vec![
            (
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    tantivy::Term::from_field_text(self.fields.source, &lookup.source),
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ),
            (
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    tantivy::Term::from_field_text(self.fields.ref_id, &lookup.ref_id),
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ),
            (
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    tantivy::Term::from_field_text(self.fields.name_exact, &lookup.name),
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ),
        ];

        if let Some(kind) = lookup.kind {
            clauses.push((
                tantivy::query::Occur::Must,
                Box::new(tantivy::query::TermQuery::new(
                    tantivy::Term::from_field_text(self.fields.kind, kind.as_str()),
                    tantivy::schema::IndexRecordOption::Basic,
                )),
            ));
        }

        let query = tantivy::query::BooleanQuery::new(clauses);

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(10).order_by_score())
            .context("entry lookup failed")?;

        let mut documents = Vec::with_capacity(top_docs.len());

        for (_, address) in top_docs {
            documents.push(self.document_at(address)?);
        }

        match documents.len() {
            0 => Ok(EntryLookupResult::NotFound),
            1 => Ok(EntryLookupResult::Found(Box::new(documents.remove(0)))),
            _ => Ok(EntryLookupResult::Ambiguous(documents)),
        }
    }

    fn document_at(&self, address: tantivy::DocAddress) -> Result<SearchDocument> {
        let searcher = self.reader.searcher();

        let retrieved: TantivyDocument = searcher
            .doc(address)
            .context("failed to retrieve entry document")?;

        let stored_json = retrieved
            .get_first(self.fields.stored_json)
            .and_then(|value| value.as_str())
            .context("entry document did not contain stored_json")?;

        serde_json::from_str(stored_json).context("failed to deserialize entry document")
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
    common: &nixsearch_core::CommonDoc,
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
    root: Utf8PathBuf,
}

impl IndexStore {
    pub fn new(root: impl AsRef<Utf8Path>) -> Self {
        let root = root.as_ref().to_owned();
        Self { root }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub fn generations_dir(&self) -> Utf8PathBuf {
        self.root.join("generations")
    }

    pub fn current_file(&self) -> Utf8PathBuf {
        self.root.join("CURRENT")
    }

    pub fn create_generation_path(&self) -> Result<Utf8PathBuf> {
        fs::create_dir_all(self.generations_dir()).with_context(|| {
            format!(
                "failed to create generations dir {}",
                self.generations_dir().as_str()
            )
        })?;

        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

        for attempt in 0..100 {
            let path = self.generations_dir().join(format!(
                "generation-{}-{timestamp}-{attempt}",
                std::process::id()
            ));

            match fs::create_dir(&path) {
                Ok(()) => return Ok(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create generation {}", path.as_str()));
                }
            }
        }

        anyhow::bail!(
            "failed to create unique generation directory in {}",
            self.generations_dir().as_str()
        )
    }

    // Atomically publishes generation_path as CURRENT. Update callers should still use the
    // update lock to avoid redundant concurrent generation work.
    pub fn publish(&self, generation_path: &Utf8Path) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create index root {}", self.root.as_str()))?;

        let generation_path = self.validate_generation_path(generation_path)?;

        // On Unix, rename atomically replaces CURRENT with this fully written file.
        let current_tmp = self.create_temp_file(
            &self.root,
            "CURRENT.tmp",
            generation_path.as_str().as_bytes(),
        )?;

        if let Err(error) = fs::rename(&current_tmp, self.current_file()) {
            let _ = fs::remove_file(&current_tmp);

            return Err(error).with_context(|| {
                format!(
                    "failed to publish current index {}",
                    self.current_file().as_str()
                )
            });
        }

        Self::sync_dir(&self.root)?;

        Ok(())
    }

    fn validate_generation_path(&self, generation_path: &Utf8Path) -> Result<Utf8PathBuf> {
        let generation_path = generation_path
            .canonicalize_utf8()
            .with_context(|| format!("failed to canonicalize {generation_path}"))?;

        let generations_dir = self
            .generations_dir()
            .canonicalize_utf8()
            .with_context(|| {
                format!(
                    "failed to canonicalize generations dir {}",
                    self.generations_dir()
                )
            })?;

        if !generation_path.starts_with(&generations_dir) {
            anyhow::bail!(
                "generation {} is outside generations dir {}",
                generation_path.as_str(),
                generations_dir.as_str()
            );
        }

        if generation_path.parent() != Some(generations_dir.as_path()) {
            anyhow::bail!(
                "generation {} is not a direct child of generations dir {}",
                generation_path.as_str(),
                generations_dir.as_str()
            );
        }

        if !generation_path.is_dir() {
            anyhow::bail!("generation {} is not a directory", generation_path.as_str());
        }

        Ok(generation_path)
    }

    fn create_temp_file(&self, dir: &Utf8Path, prefix: &str, bytes: &[u8]) -> Result<Utf8PathBuf> {
        let timestamp = OffsetDateTime::now_utc().unix_timestamp_nanos();

        for attempt in 0..100 {
            let temp_path = dir.join(format!(
                "{prefix}.{}.{}.{}",
                std::process::id(),
                timestamp,
                attempt
            ));

            let mut file = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", temp_path.as_str()));
                }
            };

            if let Err(error) = file.write_all(bytes) {
                let _ = fs::remove_file(&temp_path);

                return Err(error)
                    .with_context(|| format!("failed to write {}", temp_path.as_str()));
            }

            if let Err(error) = file.sync_all() {
                let _ = fs::remove_file(&temp_path);

                return Err(error)
                    .with_context(|| format!("failed to sync {}", temp_path.as_str()));
            }

            return Ok(temp_path);
        }

        anyhow::bail!(
            "failed to create unique temporary {prefix} file in {}",
            dir.as_str()
        )
    }

    fn sync_dir(path: &Utf8Path) -> Result<()> {
        fs::File::open(path)
            .with_context(|| format!("failed to open directory {}", path.as_str()))?
            .sync_all()
            .with_context(|| format!("failed to sync directory {}", path.as_str()))
    }

    pub fn current_path(&self) -> Result<Utf8PathBuf> {
        self.try_current_path()?.with_context(|| {
            format!(
                "failed to read current index file {}; run `nixsearch update` first",
                self.current_file().as_str()
            )
        })
    }

    pub fn try_current_path(&self) -> Result<Option<Utf8PathBuf>> {
        let raw = match fs::read_to_string(self.current_file()) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to read current index file {}",
                        self.current_file().as_str()
                    )
                });
            }
        };

        let path = Utf8PathBuf::from(raw);

        if path.as_str().is_empty() {
            anyhow::bail!("current index file is empty");
        }

        self.validate_generation_path(&path).map(Some)
    }

    pub fn manifest_path(&self, generation_path: &Utf8Path) -> Utf8PathBuf {
        generation_path.join("index-manifest.json")
    }

    pub fn write_manifest(
        &self,
        generation_path: &Utf8Path,
        manifest: &IndexGenerationManifest,
    ) -> Result<()> {
        let generation_path = self.validate_generation_path(generation_path)?;
        let path = self.manifest_path(&generation_path);
        let bytes = serde_json::to_vec_pretty(manifest)
            .context("failed to serialize index generation manifest")?;

        let temp_path =
            self.create_temp_file(&generation_path, "index-manifest.json.tmp", &bytes)?;

        if let Err(error) = fs::rename(&temp_path, &path) {
            let _ = fs::remove_file(&temp_path);

            return Err(error)
                .with_context(|| format!("failed to write index metadata {}", path.as_str()));
        }

        Self::sync_dir(&generation_path)?;

        Ok(())
    }

    pub fn read_manifest(&self, generation_path: &Utf8Path) -> Result<IndexGenerationManifest> {
        let path = self.manifest_path(generation_path);
        let bytes = fs::read(&path)
            .with_context(|| format!("failed to read index metadata {}", path.as_str()))?;

        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse index metadata {}", path.as_str()))
    }

    pub fn current_manifest(&self) -> Result<IndexGenerationManifest> {
        let current = self.current_path()?;
        self.read_manifest(&current)
    }

    pub fn try_current_manifest(&self) -> Result<Option<IndexGenerationManifest>> {
        let Some(current) = self.try_current_path()? else {
            return Ok(None);
        };

        self.read_manifest(&current).map(Some)
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
        Self::with_generated_at(document_count, targets, OffsetDateTime::now_utc())
    }

    pub fn with_generated_at(
        document_count: usize,
        targets: Vec<IndexTargetManifest>,
        generated_at: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            generated_at,
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
    use std::fs;

    use camino::Utf8PathBuf;
    use tempfile::{TempDir, tempdir};

    use nixsearch_core::ArtifactKind;

    const SOURCE_FIXTURES: &str = "fixtures";
    const REF_SMALL: &str = "small";

    fn utf8_path(path: std::path::PathBuf) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path).expect("test path must be valid UTF-8")
    }

    fn store_for(tempdir: &TempDir) -> super::IndexStore {
        super::IndexStore::new(utf8_path(tempdir.path().to_path_buf()))
    }

    #[test]
    fn index_store_publishes_current_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let generation = store.create_generation_path().unwrap();
        assert!(generation.exists());

        store.publish(&generation).unwrap();

        let current = store.current_path().unwrap();

        assert_eq!(current, generation.canonicalize_utf8().unwrap());
    }

    #[test]
    fn index_store_creates_distinct_generation_paths() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let first = store.create_generation_path().unwrap();
        let second = store.create_generation_path().unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn index_store_publish_updates_current_generation() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let first = store.create_generation_path().unwrap();
        let second = store.create_generation_path().unwrap();

        store.publish(&first).unwrap();
        store.publish(&second).unwrap();

        assert_eq!(
            store.current_path().unwrap(),
            second.canonicalize_utf8().unwrap()
        );
    }

    #[test]
    fn index_store_publish_removes_temporary_current_file_on_success() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();

        store.publish(&generation).unwrap();

        let temporary_current_files = fs::read_dir(tempdir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("CURRENT.tmp.")
            })
            .collect::<Vec<_>>();

        assert!(temporary_current_files.is_empty());
    }

    #[test]
    fn index_store_publish_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();

        let error = store.publish(&external_generation).unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_publish_rejects_generations_dir_itself() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();

        let error = store.publish(&store.generations_dir()).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_publish_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();

        let error = store.publish(&nested).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_publish_rejects_file_under_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let file = store.generations_dir().join("generation-file");
        fs::write(&file, b"not a directory").unwrap();

        let error = store.publish(&file).unwrap_err();

        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn index_store_current_path_preserves_whitespace() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::create_dir_all(store.generations_dir()).unwrap();
        let path = store
            .generations_dir()
            .join("generation with trailing space ");
        fs::create_dir(&path).unwrap();
        fs::write(store.current_file(), path.as_str().as_bytes()).unwrap();

        assert_eq!(
            store.current_path().unwrap(),
            path.canonicalize_utf8().unwrap()
        );
    }

    #[test]
    fn index_store_current_path_rejects_non_utf8_current_file() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::write(store.current_file(), b"\xff").unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("stream did not contain valid UTF-8"));
    }

    #[test]
    fn index_store_current_path_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();
        fs::write(
            store.current_file(),
            external_generation.as_str().as_bytes(),
        )
        .unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_current_path_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(store.current_file(), nested.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_current_path_rejects_missing_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let missing = store.generations_dir().join("missing-generation");
        fs::write(store.current_file(), missing.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("failed to canonicalize"));
    }

    #[test]
    fn index_store_current_path_rejects_file_under_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        store.create_generation_path().unwrap();
        let file = store.generations_dir().join("generation-file");
        fs::write(&file, b"not a directory").unwrap();
        fs::write(store.current_file(), file.as_str().as_bytes()).unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn index_store_try_current_path_returns_none_when_current_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        assert_eq!(store.try_current_path().unwrap(), None);
    }

    #[test]
    fn index_store_current_path_errors_with_update_hint_when_current_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("run `nixsearch update` first"));
    }

    #[test]
    fn index_store_current_path_errors_when_current_is_empty() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        fs::write(store.current_file(), b"").unwrap();

        let error = store.current_path().unwrap_err();

        assert!(format!("{error:#}").contains("current index file is empty"));
    }

    #[test]
    fn index_store_writes_and_reads_generation_manifest() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let generation = store.create_generation_path().unwrap();

        let manifest = super::IndexGenerationManifest::with_generated_at(
            10,
            vec![super::IndexTargetManifest {
                source: SOURCE_FIXTURES.to_owned(),
                ref_id: REF_SMALL.to_owned(),
                artifact_kind: ArtifactKind::OptionsJson,
                document_count: 10,
                artifact_hash: Some("abc123".into()),
                revision: None,
            }],
            time::OffsetDateTime::UNIX_EPOCH,
        );

        store.write_manifest(&generation, &manifest).unwrap();
        store.publish(&generation).unwrap();

        let loaded = store.current_manifest().unwrap();

        assert_eq!(loaded.document_count, 10);
        assert_eq!(loaded.targets.len(), 1);
        assert_eq!(loaded.targets[0].source, SOURCE_FIXTURES);
        assert_eq!(loaded.targets[0].ref_id, REF_SMALL);

        let temporary_manifest_files = fs::read_dir(&generation)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("index-manifest.json.tmp.")
            })
            .collect::<Vec<_>>();

        assert!(temporary_manifest_files.is_empty());
    }

    #[test]
    fn index_store_write_manifest_rejects_paths_outside_generations_dir() {
        let tempdir = tempdir().unwrap();
        let store = super::IndexStore::new(utf8_path(tempdir.path().join("indexes")));
        store.create_generation_path().unwrap();
        let external_generation =
            Utf8PathBuf::from_path_buf(tempdir.path().join("external-generation")).unwrap();
        fs::create_dir(&external_generation).unwrap();
        let manifest = super::IndexGenerationManifest::new(0, Vec::new());

        let error = store
            .write_manifest(&external_generation, &manifest)
            .unwrap_err();

        assert!(format!("{error:#}").contains("is outside generations dir"));
    }

    #[test]
    fn index_store_write_manifest_rejects_nested_generation_path() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);
        let generation = store.create_generation_path().unwrap();
        let nested = generation.join("nested");
        fs::create_dir(&nested).unwrap();
        let manifest = super::IndexGenerationManifest::new(0, Vec::new());

        let error = store.write_manifest(&nested, &manifest).unwrap_err();

        assert!(format!("{error:#}").contains("is not a direct child"));
    }

    #[test]
    fn index_store_returns_none_when_current_manifest_is_missing() {
        let tempdir = tempdir().unwrap();
        let store = store_for(&tempdir);

        let manifest = store.try_current_manifest().unwrap();

        assert!(manifest.is_none());
    }
}
