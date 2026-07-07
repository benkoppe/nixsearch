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

    if matches!(value, "." | "..") {
        return Err(ConfigError::Validation(format!(
            "{name} must not be a dot path segment: {value:?}"
        )));
    }

    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(ConfigError::Validation(format!(
            "{name} must contain only ASCII letters, digits, '.', '_', or '-': {value:?}"
        )));
    }

    Ok(())
}

const RESERVED_SOURCE_IDS: &[&str] = &[
    "-",
    ".",
    "..",
    "robots.txt",
    "sitemap.xml",
    "opensearch.xml",
    "sitemaps",
    "favicon.ico",
    "apple-touch-icon.png",
];

pub(crate) fn validate_source_id(name: &str, value: &str) -> Result<()> {
    validate_id(name, value)?;

    if RESERVED_SOURCE_IDS.contains(&value) {
        return Err(ConfigError::Validation(format!(
            "{name} is reserved for web routing: {value:?}"
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

pub(crate) fn parse_duration_value(s: &str) -> std::result::Result<std::time::Duration, String> {
    let s = s.trim();

    if s.is_empty() {
        return Err("duration must not be empty".to_owned());
    }

    let split_pos = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());

    let (number, unit) = s.split_at(split_pos);

    if number.is_empty() {
        return Err(format!("invalid number in {s:?}"));
    }

    let value: f64 = number
        .parse()
        .map_err(|_| format!("invalid number in {s:?}"))?;

    if !value.is_finite() || value <= 0.0 {
        return Err("duration must be positive".to_owned());
    }

    let seconds = match unit.trim() {
        "s" | "sec" | "secs" => value,
        "m" | "min" | "mins" => value * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => value * 3600.0,
        "d" | "day" | "days" => value * 86400.0,
        other => return Err(format!("unknown time unit {other:?}; use s, m, h, or d")),
    };

    if !seconds.is_finite() {
        return Err("duration is out of range".to_owned());
    }

    std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|_| "duration is out of range".to_owned())
}
