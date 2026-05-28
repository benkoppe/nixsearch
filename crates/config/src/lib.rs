pub mod app;
pub mod data;
pub mod error;
pub mod producer;
pub mod server;
pub mod source;
mod validation;

pub use app::AppConfig;
pub use error::{ConfigError, Result};

#[cfg(test)]
mod tests;
