pub mod annotation;
mod atomic_file;
pub mod generation;
pub mod generation_validator;
mod integrity;
pub mod manifest;
mod query;
mod ranking;
pub mod schema;
pub mod search;
pub mod seo;
pub mod seo_sidecar;
pub mod store;
mod tokenize;
pub mod writer;

pub use query::query_uses_structured_syntax;

const WRITER_MEMORY_BYTES: usize = 50_000_000;
