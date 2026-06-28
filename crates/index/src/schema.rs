use anyhow::{Context, Result};
use tantivy::schema::{Field, STORED, STRING, Schema, TEXT};

pub const INDEX_SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Clone)]
pub(crate) struct IndexFields {
    pub(crate) id: Field,
    pub(crate) source: Field,
    pub(crate) ref_id: Field,
    pub(crate) kind: Field,
    pub(crate) entry_unique_within_kind: Field,
    pub(crate) name_exact: Field,
    pub(crate) name_text: Field,
    pub(crate) name_root: Field,
    pub(crate) name_groups: Field,
    pub(crate) name_leaf: Field,
    pub(crate) description: Field,
    pub(crate) option_set: Field,
    pub(crate) parents: Field,
    pub(crate) option_type: Field,
    pub(crate) stored_json: Field,
    pub(crate) attribute_exact: Field,
    pub(crate) attribute_text: Field,
    pub(crate) package_set: Field,
    pub(crate) platforms: Field,
    pub(crate) main_program: Field,
    pub(crate) programs: Field,
}

impl IndexFields {
    pub(crate) fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            id: schema.get_field("id").context("missing field id")?,
            source: schema.get_field("source").context("missing field source")?,
            ref_id: schema.get_field("ref").context("missing field ref")?,
            kind: schema.get_field("kind").context("missing field kind")?,
            entry_unique_within_kind: schema
                .get_field("entry_unique_within_kind")
                .context("missing field entry_unique_within_kind")?,
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
            programs: schema
                .get_field("programs")
                .context("missing field programs")?,
        })
    }
}

pub(crate) fn build_schema() -> Schema {
    let mut builder = Schema::builder();

    builder.add_text_field("id", STRING | STORED);
    builder.add_text_field("source", STRING | STORED);
    builder.add_text_field("ref", STRING | STORED);
    builder.add_text_field("kind", STRING | STORED);
    builder.add_bool_field("entry_unique_within_kind", STORED);

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
    builder.add_text_field("programs", STRING | STORED);

    builder.add_text_field("stored_json", STORED);

    builder.build()
}
