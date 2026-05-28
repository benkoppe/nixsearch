use std::path::Path;

use crate::error::{ConfigError, Result};

pub(crate) fn validate_non_empty(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ConfigError::Validation(format!("{name} must not be empty")));
    }

    Ok(())
}

pub(crate) fn validate_id(name: &str, value: &str) -> Result<()> {
    validate_non_empty(name, value)?;

    if value.contains('/') {
        return Err(ConfigError::Validation(format!(
            "{name} must not contain '/': {value:?}"
        )));
    }

    Ok(())
}

pub(crate) fn validate_hex_color(name: &str, value: &str) -> Result<()> {
    let Some(hex) = value.strip_prefix('#') else {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
        )));
    };

    if hex.len() != 3 && hex.len() != 6 {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
        )));
    }
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(ConfigError::Validation(format!(
            "{name} must be a hex color like #abc or #aabbcc"
        )));
    }

    Ok(())
}

pub(crate) fn validate_producer_non_empty(
    source_id: &str,
    ref_id: &str,
    field: &str,
    value: &str,
) -> Result<()> {
    if value.trim().is_empty() {
        return producer_error(source_id, ref_id, &format!("{field} must not be empty"));
    }

    Ok(())
}

pub(crate) fn validate_nix_path_name(source_id: &str, ref_id: &str, value: &str) -> Result<()> {
    validate_producer_non_empty(source_id, ref_id, "nix_path_name", value)?;

    if value.contains('/')
        || value.contains('=')
        || value.contains('<')
        || value.contains('>')
        || value.chars().any(char::is_whitespace)
    {
        return producer_error(
            source_id,
            ref_id,
            "nix_path_name must not contain '/', '=', '<', '>', or whitespace",
        );
    }

    Ok(())
}

pub(crate) fn validate_relative_output_path(
    source_id: &str,
    ref_id: &str,
    field: &str,
    path: &Path,
) -> Result<()> {
    if path.as_os_str().is_empty() {
        return producer_error(source_id, ref_id, &format!("{field} must not be empty"));
    }

    if path.is_absolute() {
        return producer_error(source_id, ref_id, &format!("{field} must be relative"));
    }

    Ok(())
}

pub(crate) fn producer_error<T>(source_id: &str, ref_id: &str, message: &str) -> Result<T> {
    Err(ConfigError::Validation(format!(
        "sources.{source_id}.refs.{ref_id}: {message}"
    )))
}
