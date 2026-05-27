use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write as _;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, Query};
use tantivy::schema::{
    Field, IndexRecordOption, STORED, STRING, Schema, TEXT, TantivyDocument, Value as _,
};
use tantivy::tokenizer::TokenStream as _;
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, Score, Searcher, Term, doc};
use time::OffsetDateTime;

use nixsearch_core::{ArtifactKind, DocumentKind, OptionDoc, PackageDoc, SearchDocument};

const WRITER_MEMORY_BYTES: usize = 50_000_000;
const MAX_RERANK_CANDIDATES: usize = 1_000;

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
        let candidate_limit = rerank_candidate_limit(options.limit, options.offset);
        let diversify_sources = options.scopes.len() > 1;
        let analysis = QueryAnalysis::new(&self.index, self.fields.name_text, &options.query)?;

        let (candidates, total) = if diversify_sources {
            let mut candidates = Vec::new();
            let mut total = 0;

            for scope in &options.scopes {
                let (mut scope_candidates, scope_total) = self.search_candidates(
                    &searcher,
                    &options.query,
                    std::slice::from_ref(scope),
                    candidate_limit,
                )?;
                total += scope_total;
                candidates.append(&mut scope_candidates);
            }

            (candidates, total)
        } else {
            self.search_candidates(&searcher, &options.query, &options.scopes, candidate_limit)?
        };
        let candidate_window_len = candidates.len();
        let candidate_ids = candidates
            .iter()
            .map(|candidate| candidate.document.id().to_owned())
            .collect::<HashSet<_>>();

        let mut hits = rerank_candidates(
            candidates,
            &analysis,
            diversify_sources,
            options.offset,
            options.limit,
        );

        if hits.len() < options.limit && total > options.offset.saturating_add(hits.len()) {
            let remaining = options.limit - hits.len();
            let mut fallback_hits = if diversify_sources {
                self.search_native_filtered_hits(
                    &searcher,
                    &options.query,
                    &options.scopes,
                    &candidate_ids,
                    options.offset.saturating_sub(candidate_window_len),
                    remaining,
                    total,
                )?
            } else {
                self.search_native_hits(
                    &searcher,
                    &options.query,
                    &options.scopes,
                    options.offset.max(candidate_window_len),
                    remaining,
                )?
            };

            hits.append(&mut fallback_hits);
        }

        Ok(SearchResult { hits, total })
    }

    fn search_candidates(
        &self,
        searcher: &Searcher,
        query_text: &str,
        scopes: &[SearchScope],
        limit: usize,
    ) -> Result<(Vec<SearchCandidate>, usize)> {
        let query = self.build_query(query_text, scopes)?;

        let (total, top_docs) = searcher
            .search(
                &*query,
                &(Count, TopDocs::with_limit(limit).order_by_score()),
            )
            .context("search failed")?;

        let mut candidates = Vec::with_capacity(top_docs.len());

        for (score, address) in top_docs {
            candidates.push(SearchCandidate {
                score,
                document: self.document_at_with_searcher(searcher, address)?,
            });
        }

        Ok((candidates, total))
    }

    fn search_native_hits(
        &self,
        searcher: &Searcher,
        query_text: &str,
        scopes: &[SearchScope],
        offset: usize,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query = self.build_query(query_text, scopes)?;
        let top_docs = searcher
            .search(
                &*query,
                &TopDocs::with_limit(limit)
                    .and_offset(offset)
                    .order_by_score(),
            )
            .context("native fallback search failed")?;

        let mut hits = Vec::with_capacity(top_docs.len());

        for (score, address) in top_docs {
            hits.push(SearchHit {
                score,
                document: self.document_at_with_searcher(searcher, address)?,
            });
        }

        Ok(hits)
    }

    fn search_native_filtered_hits(
        &self,
        searcher: &Searcher,
        query_text: &str,
        scopes: &[SearchScope],
        excluded_ids: &HashSet<String>,
        skip: usize,
        limit: usize,
        total: usize,
    ) -> Result<Vec<SearchHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query = self.build_query(query_text, scopes)?;
        let mut hits = Vec::with_capacity(limit);
        let mut skipped = 0;
        let mut native_offset = 0;

        while hits.len() < limit && native_offset < total {
            let batch_limit = skip
                .saturating_sub(skipped)
                .saturating_add(limit - hits.len())
                .saturating_add(excluded_ids.len())
                .clamp(200, 5_000)
                .min(total - native_offset);

            if batch_limit == 0 {
                break;
            }

            let top_docs = searcher
                .search(
                    &*query,
                    &TopDocs::with_limit(batch_limit)
                        .and_offset(native_offset)
                        .order_by_score(),
                )
                .context("filtered native fallback search failed")?;

            if top_docs.is_empty() {
                break;
            }

            native_offset += top_docs.len();

            for (score, address) in top_docs {
                let document = self.document_at_with_searcher(searcher, address)?;

                if excluded_ids.contains(document.id()) {
                    continue;
                }

                if skipped < skip {
                    skipped += 1;
                    continue;
                }

                hits.push(SearchHit { score, document });

                if hits.len() == limit {
                    break;
                }
            }
        }

        Ok(hits)
    }

    fn build_query(&self, query_text: &str, scopes: &[SearchScope]) -> Result<Box<dyn Query>> {
        let text_query = self.build_text_query(query_text)?;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];

        if !scopes.is_empty() {
            let mut scope_clauses = Vec::with_capacity(scopes.len());

            for scope in scopes {
                let source_query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
                    Term::from_field_text(self.fields.source, &scope.source),
                    tantivy::schema::IndexRecordOption::Basic,
                ));

                let ref_query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
                    Term::from_field_text(self.fields.ref_id, &scope.ref_id),
                    tantivy::schema::IndexRecordOption::Basic,
                ));

                let pair_query: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                    (Occur::Must, source_query),
                    (Occur::Must, ref_query),
                ]));

                scope_clauses.push((Occur::Should, pair_query));
            }

            clauses.push((Occur::Must, Box::new(BooleanQuery::new(scope_clauses))));
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn build_text_query(&self, query_text: &str) -> Result<Box<dyn Query>> {
        let mut text_clauses = self.build_exact_text_clauses(query_text)?;

        if let Some(fuzzy_query) = build_fuzzy_query(&self.index, query_text, &self.fields)? {
            text_clauses.push((Occur::Should, fuzzy_query));
        }

        Ok(Box::new(BooleanQuery::new(text_clauses)))
    }

    fn build_exact_text_clauses(&self, query_text: &str) -> Result<Vec<(Occur, Box<dyn Query>)>> {
        let mut clauses = Vec::new();
        let raw = query_text.trim();

        if !raw.is_empty() {
            for value in dedup_preserving_order(vec![raw.to_owned(), raw.to_lowercase()]) {
                self.add_string_field_clauses(&mut clauses, &value);
            }
        }

        let mut terms = tokenized_query_terms(&self.index, self.fields.name_text, query_text)?;
        let compact = compact_identifier(query_text);

        if !compact.is_empty() {
            terms.push(compact);
        }

        for term in dedup_preserving_order(terms) {
            self.add_token_field_clauses(&mut clauses, &term);
        }

        Ok(clauses)
    }

    fn add_string_field_clauses(&self, clauses: &mut Vec<(Occur, Box<dyn Query>)>, value: &str) {
        add_boosted_term_clause(
            clauses,
            self.fields.name_exact,
            value,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_groups,
            value,
            15.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_root,
            value,
            8.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_leaf,
            value,
            6.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.attribute_exact,
            value,
            25.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.main_program,
            value,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_set,
            value,
            3.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.parents,
            value,
            2.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.package_set,
            value,
            2.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.platforms,
            value,
            1.0,
            IndexRecordOption::Basic,
        );
    }

    fn add_token_field_clauses(&self, clauses: &mut Vec<(Occur, Box<dyn Query>)>, term: &str) {
        add_boosted_term_clause(
            clauses,
            self.fields.name_text,
            term,
            10.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.attribute_text,
            term,
            12.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.description,
            term,
            1.0,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_type,
            term,
            1.5,
            IndexRecordOption::WithFreqs,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_root,
            term,
            8.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.name_leaf,
            term,
            6.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.main_program,
            term,
            20.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.option_set,
            term,
            3.0,
            IndexRecordOption::Basic,
        );
        add_boosted_term_clause(
            clauses,
            self.fields.package_set,
            term,
            2.0,
            IndexRecordOption::Basic,
        );
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

    fn document_at(&self, address: DocAddress) -> Result<SearchDocument> {
        let searcher = self.reader.searcher();
        self.document_at_with_searcher(&searcher, address)
    }

    fn document_at_with_searcher(
        &self,
        searcher: &Searcher,
        address: DocAddress,
    ) -> Result<SearchDocument> {
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

fn add_boosted_term_clause(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    field: Field,
    value: &str,
    boost: Score,
    index_record_option: IndexRecordOption,
) {
    if value.is_empty() {
        return;
    }

    let query: Box<dyn Query> = Box::new(tantivy::query::TermQuery::new(
        Term::from_field_text(field, value),
        index_record_option,
    ));

    clauses.push((Occur::Should, Box::new(BoostQuery::new(query, boost))));
}

#[derive(Debug)]
struct SearchCandidate {
    score: Score,
    document: SearchDocument,
}

#[derive(Debug)]
struct RankedCandidate {
    base_score: Score,
    final_score: Score,
    source: String,
    cluster_key: Option<String>,
    cluster_explicit: bool,
    document: SearchDocument,
}

#[derive(Debug)]
struct QueryAnalysis {
    normalized: String,
    compact: String,
    terms: Vec<String>,
    structured_terms: Vec<String>,
}

impl QueryAnalysis {
    fn new(index: &Index, field: Field, query: &str) -> Result<Self> {
        let normalized = query.trim().to_lowercase();
        let compact = compact_identifier(query);
        let identifier_terms = identifier_terms(query);
        let structured_terms = structured_query_terms(query);

        let mut terms = tokenized_query_terms(index, field, query)?;
        terms.extend(identifier_terms);

        if !compact.is_empty() {
            terms.push(compact.clone());
        }

        Ok(Self {
            normalized,
            compact,
            terms: dedup_preserving_order(terms),
            structured_terms,
        })
    }

    fn contains_term(&self, term: &str) -> bool {
        self.terms.iter().any(|candidate| candidate == term)
    }

    fn is_structured(&self) -> bool {
        !self.structured_terms.is_empty()
    }
}

fn rerank_candidate_limit(limit: usize, offset: usize) -> usize {
    let target = offset.saturating_add(limit).max(1);
    target.saturating_mul(4).max(200).min(MAX_RERANK_CANDIDATES)
}

fn rerank_candidates(
    candidates: Vec<SearchCandidate>,
    analysis: &QueryAnalysis,
    diversify_sources: bool,
    offset: usize,
    limit: usize,
) -> Vec<SearchHit> {
    if limit == 0 {
        return Vec::new();
    }

    let mut remaining = candidates
        .into_iter()
        .map(|candidate| {
            let source = candidate.document.common().source.clone();
            let cluster_key = duplicate_cluster_key(&candidate.document);
            let cluster_explicit = duplicate_cluster_explicit(&candidate.document, analysis);
            let base_score = domain_relevance_score(candidate.score, &candidate.document, analysis);

            RankedCandidate {
                base_score,
                final_score: base_score,
                source,
                cluster_key,
                cluster_explicit,
                document: candidate.document,
            }
        })
        .collect::<Vec<_>>();

    let needed = offset.saturating_add(limit);
    let mut selected = Vec::with_capacity(needed.min(remaining.len()));
    let mut source_counts: HashMap<String, usize> = HashMap::new();
    let mut cluster_counts: HashMap<String, usize> = HashMap::new();

    while selected.len() < needed && !remaining.is_empty() {
        let mut best_index = 0;
        let mut best_score = f32::NEG_INFINITY;

        for (index, candidate) in remaining.iter().enumerate() {
            let adjusted_score = adjusted_candidate_score(
                candidate,
                diversify_sources,
                &source_counts,
                &cluster_counts,
            );

            if adjusted_score > best_score
                || (adjusted_score == best_score
                    && candidate_sort_key(candidate) < candidate_sort_key(&remaining[best_index]))
            {
                best_index = index;
                best_score = adjusted_score;
            }
        }

        let mut candidate = remaining.swap_remove(best_index);
        candidate.final_score = best_score;

        *source_counts.entry(candidate.source.clone()).or_default() += 1;

        if let Some(cluster_key) = &candidate.cluster_key {
            *cluster_counts.entry(cluster_key.clone()).or_default() += 1;
        }

        selected.push(candidate);
    }

    selected
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|candidate| SearchHit {
            score: candidate.final_score,
            document: candidate.document,
        })
        .collect()
}

fn adjusted_candidate_score(
    candidate: &RankedCandidate,
    diversify_sources: bool,
    source_counts: &HashMap<String, usize>,
    cluster_counts: &HashMap<String, usize>,
) -> Score {
    let source_count = source_counts.get(&candidate.source).copied().unwrap_or(0);
    let source_factor = if diversify_sources {
        1.0 / (1.0 + source_count as Score * 0.18)
    } else {
        1.0
    };

    let cluster_count = candidate
        .cluster_key
        .as_ref()
        .and_then(|key| cluster_counts.get(key))
        .copied()
        .unwrap_or(0);

    candidate.base_score
        * source_factor
        * duplicate_cluster_factor(cluster_count, candidate.cluster_explicit)
}

fn duplicate_cluster_factor(count: usize, explicit: bool) -> Score {
    if count == 0 {
        return 1.0;
    }

    if explicit {
        return 1.0 / (1.0 + count as Score * 0.15);
    }

    match count {
        1 => 0.45,
        2 => 0.25,
        _ => 0.15,
    }
}

fn candidate_sort_key(candidate: &RankedCandidate) -> (&str, &str) {
    (candidate.source.as_str(), candidate.document.name())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StructuredMatchTier {
    Unstructured,
    PathPrefix,
    AllTermsInPath,
    PartialPath,
    NoPathMatch,
}

impl StructuredMatchTier {
    fn score_bonus(self) -> Score {
        match self {
            Self::Unstructured => 0.0,
            Self::PathPrefix => 500.0,
            Self::AllTermsInPath => 260.0,
            Self::PartialPath => -120.0,
            Self::NoPathMatch => -180.0,
        }
    }
}

fn domain_relevance_score(
    original_score: Score,
    document: &SearchDocument,
    analysis: &QueryAnalysis,
) -> Score {
    let mut score = original_score.max(0.0).ln_1p() * 4.0;
    let common = document.common();
    let name = common.name.to_lowercase();
    let name_compact = compact_identifier(&common.name);
    let name_terms = identifier_terms(&common.name);
    let path_terms = document_path_terms(document);
    let structured_tier = structured_match_tier(analysis, &path_terms);

    score += structured_tier.score_bonus();

    if structured_tier == StructuredMatchTier::PathPrefix {
        score -= structured_path_depth_penalty(document, analysis);
    }

    if name == analysis.normalized {
        score += 90.0;
    }

    if !analysis.compact.is_empty() && name_compact == analysis.compact {
        score += 80.0;
    }

    if !analysis.normalized.is_empty() && name.starts_with(&analysis.normalized) {
        score += 35.0;
    }

    if all_query_terms_match(&analysis.structured_terms, &name_terms) {
        score += 18.0;
    }

    match document {
        SearchDocument::Option(option) => {
            let segments = if option.loc.is_empty() {
                common
                    .name_parts
                    .segments
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            } else {
                option.loc.iter().map(String::as_str).collect::<Vec<_>>()
            };

            score += option_name_score(analysis, &segments);
        }

        SearchDocument::Package(package) => {
            score += package_name_score(analysis, package);
        }
    }

    score.max(0.0)
}

fn structured_match_tier(analysis: &QueryAnalysis, path_terms: &[String]) -> StructuredMatchTier {
    if !analysis.is_structured() {
        return StructuredMatchTier::Unstructured;
    }

    if starts_with_query_terms(path_terms, &analysis.structured_terms) {
        return StructuredMatchTier::PathPrefix;
    }

    if all_query_terms_match_path(&analysis.structured_terms, path_terms) {
        return StructuredMatchTier::AllTermsInPath;
    }

    if analysis.structured_terms.iter().any(|term| {
        path_terms
            .iter()
            .any(|path_term| path_term_matches_query_term(path_term, term))
    }) {
        return StructuredMatchTier::PartialPath;
    }

    StructuredMatchTier::NoPathMatch
}

fn starts_with_query_terms(values: &[String], prefix: &[String]) -> bool {
    values.len() >= prefix.len()
        && values
            .iter()
            .zip(prefix)
            .all(|(value, prefix)| path_term_matches_query_term(value, prefix))
}

fn all_query_terms_match_path(query_terms: &[String], path_terms: &[String]) -> bool {
    !query_terms.is_empty()
        && query_terms.iter().all(|term| {
            path_terms
                .iter()
                .any(|path_term| path_term_matches_query_term(path_term, term))
        })
}

fn path_term_matches_query_term(path_term: &str, query_term: &str) -> bool {
    path_term == query_term
        || (query_term.chars().count() >= 3 && path_term.starts_with(query_term))
}

fn structured_path_depth_penalty(document: &SearchDocument, analysis: &QueryAnalysis) -> Score {
    let depth = document_path_depth(document);
    let query_depth = analysis.structured_terms.len();
    depth.saturating_sub(query_depth) as Score * 3.0
}

fn document_path_terms(document: &SearchDocument) -> Vec<String> {
    document_path_segments(document)
        .into_iter()
        .flat_map(|segment| identifier_terms(&segment))
        .collect()
}

fn document_path_depth(document: &SearchDocument) -> usize {
    document_path_segments(document).len()
}

fn document_path_segments(document: &SearchDocument) -> Vec<String> {
    match document {
        SearchDocument::Option(option) if !option.loc.is_empty() => option.loc.clone(),
        SearchDocument::Option(option) => option.common.name_parts.segments.clone(),
        SearchDocument::Package(package) => package
            .attribute
            .split('.')
            .filter(|segment| !segment.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    }
}

fn option_name_score(analysis: &QueryAnalysis, segments: &[&str]) -> Score {
    let normalized_segments = segments
        .iter()
        .map(|segment| segment.to_lowercase())
        .collect::<Vec<_>>();
    let expanded_terms = segments
        .iter()
        .flat_map(|segment| identifier_terms(segment))
        .collect::<Vec<_>>();

    let mut score = 0.0;

    for term in &analysis.terms {
        if normalized_segments.iter().any(|segment| segment == term) {
            score += 52.0;
        } else if expanded_terms.iter().any(|segment| segment == term) {
            score += 34.0;
        }
    }

    if all_query_terms_match(&analysis.structured_terms, &expanded_terms) {
        score += 20.0;
    }

    score
}

fn package_name_score(analysis: &QueryAnalysis, package: &PackageDoc) -> Score {
    let mut score = 0.0;
    let attribute = package.attribute.to_lowercase();
    let attribute_compact = compact_identifier(&package.attribute);
    let leaf = package
        .attribute
        .rsplit('.')
        .next()
        .unwrap_or(&package.attribute);
    let leaf_normalized = leaf.to_lowercase();
    let leaf_terms = identifier_terms(leaf);
    let nested = package.package_set.is_some();
    let package_set_explicit = package
        .package_set
        .as_deref()
        .is_some_and(|package_set| query_mentions_package_set(analysis, package_set));

    if attribute == analysis.normalized {
        score += 110.0;
    }

    if !analysis.compact.is_empty() && attribute_compact == analysis.compact {
        score += 90.0;
    }

    if leaf_normalized == analysis.normalized {
        score += if nested && !package_set_explicit {
            34.0
        } else {
            70.0
        };
    }

    for term in &analysis.terms {
        if leaf_terms.iter().any(|leaf_term| leaf_term == term) {
            score += if nested && !package_set_explicit {
                18.0
            } else {
                32.0
            };
        }
    }

    if let Some(main_program) = &package.main_program {
        if main_program.to_lowercase() == analysis.normalized {
            score += 85.0;
        }
    }

    if package_set_explicit {
        score += 35.0;
    } else if nested {
        score -= 6.0;
    }

    score
}

fn all_query_terms_match(query_terms: &[String], document_terms: &[String]) -> bool {
    let meaningful_terms = query_terms
        .iter()
        .filter(|term| term.chars().count() > 1)
        .collect::<Vec<_>>();

    !meaningful_terms.is_empty()
        && meaningful_terms.iter().all(|term| {
            document_terms
                .iter()
                .any(|document_term| document_term == *term)
        })
}

fn duplicate_cluster_key(document: &SearchDocument) -> Option<String> {
    let SearchDocument::Package(package) = document else {
        return None;
    };

    let package_set = package.package_set.as_deref()?;
    let leaf = package.attribute.rsplit('.').next()?;

    Some(format!(
        "{}:{}:{}",
        package.common.source.as_str(),
        package_set_family(package_set),
        compact_identifier(leaf)
    ))
}

fn duplicate_cluster_explicit(document: &SearchDocument, analysis: &QueryAnalysis) -> bool {
    let SearchDocument::Package(package) = document else {
        return false;
    };

    let Some(package_set) = package.package_set.as_deref() else {
        return false;
    };

    query_mentions_package_set(analysis, package_set)
}

fn query_mentions_package_set(analysis: &QueryAnalysis, package_set: &str) -> bool {
    let package_set_compact = compact_identifier(package_set);
    let family = package_set_family(package_set);
    let family_compact = compact_identifier(&family);

    if !analysis.compact.is_empty()
        && (analysis.compact.contains(&package_set_compact)
            || analysis.compact.contains(&family_compact))
    {
        return true;
    }

    let family_terms = identifier_terms(&family)
        .into_iter()
        .filter(|term| term.chars().any(|ch| ch.is_alphabetic()))
        .collect::<Vec<_>>();

    !family_terms.is_empty() && family_terms.iter().all(|term| analysis.contains_term(term))
}

fn package_set_family(package_set: &str) -> String {
    let mut family = package_set;

    while let Some((prefix, suffix)) = family.rsplit_once('_') {
        if suffix.chars().all(|ch| ch.is_ascii_digit()) {
            family = prefix;
        } else {
            break;
        }
    }

    family.to_owned()
}

fn tokenized_query_terms(index: &Index, field: Field, query: &str) -> Result<Vec<String>> {
    let mut analyzer = index
        .tokenizer_for_field(field)
        .context("failed to get tokenizer for query field")?;
    let mut token_stream = analyzer.token_stream(query);
    let mut terms = Vec::new();

    token_stream.process(&mut |token| {
        terms.push(token.text.clone());
    });

    Ok(terms)
}

fn structured_query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .filter_map(|part| {
            let segments = part
                .split('.')
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>();

            if segments.len() >= 2
                && segments
                    .iter()
                    .any(|segment| segment.chars().any(|ch| ch.is_alphabetic()))
            {
                Some(identifier_terms(part))
            } else {
                None
            }
        })
        .max_by_key(Vec::len)
        .unwrap_or_default()
}

fn identifier_terms(value: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut previous_was_lowercase = false;

    for ch in value.chars() {
        if !ch.is_alphanumeric() {
            push_identifier_term(&mut terms, &mut current);
            previous_was_lowercase = false;
            continue;
        }

        if ch.is_uppercase() && previous_was_lowercase {
            push_identifier_term(&mut terms, &mut current);
        }

        for lowercase in ch.to_lowercase() {
            current.push(lowercase);
        }
        previous_was_lowercase = ch.is_lowercase();
    }

    push_identifier_term(&mut terms, &mut current);
    dedup_preserving_order(terms)
}

fn push_identifier_term(terms: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        terms.push(std::mem::take(current));
    }
}

fn compact_identifier(value: &str) -> String {
    let mut compact = String::new();

    for ch in value.chars().filter(|ch| ch.is_alphanumeric()) {
        compact.extend(ch.to_lowercase());
    }

    compact
}

fn dedup_preserving_order(terms: Vec<String>) -> Vec<String> {
    let mut deduped = Vec::new();

    for term in terms {
        if !deduped.contains(&term) {
            deduped.push(term);
        }
    }

    deduped
}

fn build_fuzzy_query(
    index: &Index,
    query: &str,
    fields: &IndexFields,
) -> Result<Option<Box<dyn Query>>> {
    let terms = fuzzy_query_terms(index, fields.name_text, query)?;

    let mut term_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

    for term in terms {
        let Some(distance) = fuzzy_distance(&term) else {
            continue;
        };

        let mut field_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        add_fuzzy_field_clause(&mut field_clauses, fields.name_text, &term, distance, 10.0);
        add_fuzzy_field_clause(
            &mut field_clauses,
            fields.attribute_text,
            &term,
            distance,
            12.0,
        );

        term_clauses.push((Occur::Should, Box::new(BooleanQuery::new(field_clauses))));
    }

    if term_clauses.is_empty() {
        return Ok(None);
    }

    Ok(Some(Box::new(BooleanQuery::new(term_clauses))))
}

fn fuzzy_query_terms(index: &Index, field: Field, query: &str) -> Result<Vec<String>> {
    let mut terms = tokenized_query_terms(index, field, query)?;
    let compact = compact_identifier(query);

    if !compact.is_empty() {
        terms.push(compact);
    }

    Ok(dedup_preserving_order(terms))
}

fn add_fuzzy_field_clause(
    clauses: &mut Vec<(Occur, Box<dyn Query>)>,
    field: Field,
    term: &str,
    distance: u8,
    field_boost: f32,
) {
    let fuzzy_query: Box<dyn Query> = Box::new(FuzzyTermQuery::new_prefix(
        Term::from_field_text(field, term),
        distance,
        true,
    ));

    clauses.push((
        Occur::Should,
        Box::new(BoostQuery::new(
            fuzzy_query,
            fuzzy_boost(distance, field_boost),
        )),
    ));
}

fn fuzzy_distance(term: &str) -> Option<u8> {
    match term.chars().count() {
        0..=3 => None,
        4..=7 => Some(1),
        _ => Some(2),
    }
}

fn fuzzy_boost(distance: u8, field_boost: f32) -> f32 {
    let distance_boost = match distance {
        0 => 1.0,
        1 => 0.25,
        _ => 0.10,
    };

    field_boost * distance_boost
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
    fn structured_query_terms_uses_only_path_like_token() {
        assert_eq!(
            super::structured_query_terms("services.nginx web server"),
            vec!["services", "nginx"]
        );
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
