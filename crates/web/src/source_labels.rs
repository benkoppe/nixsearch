use nixsearch_config::app::AppConfig;
use nixsearch_config::source::SourceKind;

pub(crate) fn source_display_name<'a>(config: &'a AppConfig, source_id: &'a str) -> &'a str {
    config
        .sources
        .get(source_id)
        .and_then(|source| source.name.as_deref())
        .unwrap_or(source_id)
}

pub(crate) fn source_kind_noun(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Packages => "packages",
        SourceKind::Options => "options",
        SourceKind::Apps => "apps",
        SourceKind::Services => "services",
        SourceKind::Mixed => "packages and options",
    }
}

#[cfg(test)]
mod tests {
    use nixsearch_config::source::SourceKind;

    use super::source_kind_noun;

    #[test]
    fn source_kind_noun_maps_source_kinds() {
        assert_eq!(source_kind_noun(SourceKind::Packages), "packages");
        assert_eq!(source_kind_noun(SourceKind::Options), "options");
        assert_eq!(source_kind_noun(SourceKind::Mixed), "packages and options");
    }
}
