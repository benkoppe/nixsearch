use crate::DEFAULT_LIMIT;
use crate::RESULTS_SLICE_URL;
use crate::request::LinkOrigin;

const DEFAULT_LIMIT_PLACEHOLDER: &str = "__DEFAULT_LIMIT__";
const RESULTS_SLICE_URL_PLACEHOLDER: &str = "__RESULTS_SLICE_URL__";
const SOURCE_ALL_VALUE_PLACEHOLDER: &str = "__SOURCE_ALL_VALUE__";

pub fn datastar_script() -> &'static str {
    include_str!(env!(
        "DATASTAR_JS_PATH",
        "DATASTAR_JS_PATH must be set by Nix; run `nix develop .#` before cargo commands"
    ))
}

pub fn dialog_reconcile_script() -> &'static str {
    include_str!("scripts/dialog-reconcile.js")
}

pub fn navigation_script() -> String {
    include_str!("scripts/navigation.js")
        .replace(RESULTS_SLICE_URL_PLACEHOLDER, RESULTS_SLICE_URL)
        .replace(DEFAULT_LIMIT_PLACEHOLDER, &DEFAULT_LIMIT.to_string())
        .replace(SOURCE_ALL_VALUE_PLACEHOLDER, LinkOrigin::All.as_str())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LIMIT_PLACEHOLDER, RESULTS_SLICE_URL_PLACEHOLDER, SOURCE_ALL_VALUE_PLACEHOLDER,
        dialog_reconcile_script, navigation_script,
    };

    #[test]
    fn navigation_script_replaces_template_placeholders() {
        let script = navigation_script();

        assert!(!script.contains(DEFAULT_LIMIT_PLACEHOLDER));
        assert!(!script.contains(RESULTS_SLICE_URL_PLACEHOLDER));
        assert!(!script.contains(SOURCE_ALL_VALUE_PLACEHOLDER));
        assert!(script.contains("const PAGE_SIZE = 50;"));
        assert!(script.contains(r#"const RESULTS_SLICE_URL = "/-/results/slice";"#));
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
        assert!(
            script.find("setLoading(loadsResults);").unwrap()
                < script.find("reconcile(current);").unwrap()
        );
    }

    #[test]
    fn navigation_script_preserves_invalid_ref_without_selecting_it() {
        let script = navigation_script();

        assert!(
            script.contains(r#"refId: requestedRef && requestedRef !== refs[0] ? "" : refs[0]"#)
        );
        assert!(script.contains("invalidRefId:"));
        assert!(script.contains("requestedRef && requestedRef !== refs[0] ? requestedRef : \"\""));
        assert!(
            script.contains("const preservedRef = context.refId || context.invalidRefId || \"\";")
        );
    }

    #[test]
    fn navigation_script_resyncs_inputs_after_bfcache_restore() {
        let script = navigation_script();

        assert!(script.contains(r#"window.addEventListener("pageshow""#));
        assert!(script.contains("if (!evt.persisted) return;"));
        assert!(script.contains("syncInputsFromUrl();"));
    }

    #[test]
    fn navigation_script_routes_modal_cancel_through_navigation() {
        let script = navigation_script();

        assert!(script.contains(r#"addEventListener("cancel""#));
        assert!(script.contains("closeEntryModal(dialog)"));
        assert!(script.contains("evt.preventDefault();"));
    }

    #[test]
    fn dialog_reconcile_script_loads_asset() {
        assert!(dialog_reconcile_script().contains("entry-modal"));
        assert!(dialog_reconcile_script().contains("showModal"));
    }
}
