use crate::DEFAULT_LIMIT;
use crate::MORE_RESULTS_URL;
use crate::request::LinkOrigin;

const DEFAULT_LIMIT_PLACEHOLDER: &str = "__DEFAULT_LIMIT__";
const MORE_RESULTS_URL_PLACEHOLDER: &str = "__MORE_RESULTS_URL__";
const SOURCE_ALL_VALUE_PLACEHOLDER: &str = "__SOURCE_ALL_VALUE__";

pub fn dialog_reconcile_script() -> &'static str {
    include_str!("scripts/dialog-reconcile.js")
}

pub fn navigation_script() -> String {
    include_str!("scripts/navigation.js")
        .replace(MORE_RESULTS_URL_PLACEHOLDER, MORE_RESULTS_URL)
        .replace(DEFAULT_LIMIT_PLACEHOLDER, &DEFAULT_LIMIT.to_string())
        .replace(SOURCE_ALL_VALUE_PLACEHOLDER, LinkOrigin::All.as_str())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LIMIT_PLACEHOLDER, MORE_RESULTS_URL_PLACEHOLDER, SOURCE_ALL_VALUE_PLACEHOLDER,
        dialog_reconcile_script, navigation_script,
    };

    #[test]
    fn navigation_script_replaces_template_placeholders() {
        let script = navigation_script();

        assert!(!script.contains(DEFAULT_LIMIT_PLACEHOLDER));
        assert!(!script.contains(MORE_RESULTS_URL_PLACEHOLDER));
        assert!(!script.contains(SOURCE_ALL_VALUE_PLACEHOLDER));
        assert!(script.contains("const PAGE_SIZE = 50;"));
        assert!(script.contains(r#"const MORE_URL = "/-/more";"#));
    }

    #[test]
    fn navigation_script_prevents_duplicate_query_reconciles() {
        let script = navigation_script();

        assert!(script.contains("if (target === current)"));
        assert!(script.contains("function clearPendingQueryNavigation()"));
        assert!(script.contains("clearPendingQueryNavigation();"));
    }

    #[test]
    fn navigation_script_resets_scroll_for_new_results() {
        let script = navigation_script();

        assert!(script.contains("if (loadsResults)"));
        assert!(script.contains("window.scrollTo(0, 0);"));
        assert!(script.find("setLoading(loadsResults);").unwrap() < script.find("reconcile(current);").unwrap());
    }

    #[test]
    fn navigation_script_focuses_search_with_slash() {
        let script = navigation_script();

        assert!(script.contains(r#"if (evt.key !== "/") return;"#));
        assert!(script.contains("if (dialog && dialog.open) return;"));
        assert!(script.contains(r#"document.querySelector('[data-nixsearch-input="q"]')"#));
        assert!(script.contains("input.select();"));
    }

    #[test]
    fn dialog_reconcile_script_loads_asset() {
        assert!(dialog_reconcile_script().contains("entry-modal"));
        assert!(dialog_reconcile_script().contains("showModal"));
    }
}
