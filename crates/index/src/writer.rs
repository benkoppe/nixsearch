use anyhow::{Context, Result};
use tantivy::schema::TantivyDocument;
use tantivy::{IndexWriter, doc};

use nixsearch_core::document::{CommonDoc, OptionDoc, PackageDoc, SearchDocument};

use crate::schema::IndexFields;

pub struct SearchIndexWriter {
    pub(crate) writer: IndexWriter,
    pub(crate) fields: IndexFields,
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
            document.add_text(self.fields.description, description.plain_text().as_ref());
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

fn add_name_parts(document: &mut TantivyDocument, fields: &IndexFields, common: &CommonDoc) {
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
