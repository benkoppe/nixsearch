pub mod annotation;
pub mod manifest;
mod query;
mod ranking;
pub mod schema;
pub mod search;
pub mod seo;
pub mod store;
mod tokenize;
pub mod writer;

const WRITER_MEMORY_BYTES: usize = 50_000_000;
