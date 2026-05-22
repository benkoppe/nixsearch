use maud::{Markup, html};

use nix_search_config::AppConfig;

const DEFAULT_SOURCE_COLOR: &str = "#94a3b8";

const GENERATED_SATURATION: u64 = 78;
const GENERATED_LIGHTNESS: u64 = 67;

pub fn render(config: &AppConfig, source_id: &str) -> Markup {
    let color = color_for_source(config, source_id);

    html! {
        span.source-tag
            data-source=(source_id)
            style=(format!("--source-color: {color};")) {
            (source_id)
        }
    }
}

pub fn color_for_source(config: &AppConfig, source_id: &str) -> String {
    if let Some(color) = config
        .sources
        .get(source_id)
        .and_then(|source| source.color.as_deref())
    {
        return color.to_owned();
    }

    if source_id.is_empty() {
        return DEFAULT_SOURCE_COLOR.to_owned();
    }

    generated_color(source_id)
}

fn generated_color(source_id: &str) -> String {
    let hash = stable_hash(source_id);
    let hue = hash % 360;

    format!("hsl({hue} {GENERATED_SATURATION}% {GENERATED_LIGHTNESS}%)")
}

fn stable_hash(value: &str) -> u64 {
    // FNV-1a, fixed so generated colors do not change across Rust versions/runs.
    let mut hash = 0xcbf29ce484222325;

    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }

    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_configured_source_color() {
        let mut config = nix_search_test_support::app_config("./data/indexes");

        config.sources.get_mut("fixtures").unwrap().color = Some("#abcdef".to_owned());

        assert_eq!(color_for_source(&config, "fixtures"), "#abcdef");
    }

    #[test]
    fn generated_source_color_is_deterministic() {
        let config = AppConfig::default();

        assert_eq!(
            color_for_source(&config, "custom-source"),
            color_for_source(&config, "custom-source")
        );
    }

    #[test]
    fn generated_source_color_is_readable_hsl() {
        let config = AppConfig::default();

        assert_eq!(
            color_for_source(&config, "custom-source"),
            "hsl(20 78% 67%)"
        );
    }
}
