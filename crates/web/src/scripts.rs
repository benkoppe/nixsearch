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

#[cfg(test)]
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
    fn navigation_script_blurs_search_on_escape() {
        let script = navigation_script();

        assert!(script.contains(r#"evt.key === "Escape""#));
        assert!(script.contains("document.activeElement === input"));
        assert!(script.contains("input.blur();"));
    }

    #[test]
    fn navigation_script_moves_results_with_j_and_k() {
        let script = navigation_script();

        assert!(script.contains(r#"evt.key === "j" || evt.key === "k""#));
        assert!(script.contains("!isEditableTarget(evt.target)"));
        assert!(script.contains(r#"moveResultSelection(evt.key === "j" ? 1 : -1)"#));
    }

    #[test]
    fn navigation_script_cycles_sources_with_bare_brackets_outside_inputs() {
        let script = navigation_script();

        assert!(script.contains("function isSourceCycleShortcut(evt)"));
        assert!(script.contains(r#"if (evt.key !== "[" && evt.key !== "]") return false;"#));
        assert!(script.contains("return evt.ctrlKey || !isEditableTarget(evt.target);"));
        assert!(script.contains("if (isSourceCycleShortcut(evt))"));
    }

    #[test]
    fn navigation_script_stores_head_metadata_in_history_state() {
        let script = navigation_script();

        assert!(!script.contains("headMetadataCache"));
        assert!(!script.contains("syncTitle"));
        assert!(script.contains("nixsearchHeadMetadata"));
        assert!(script.contains("nixsearchHeadMetadataUrl"));
        assert!(script.contains("nixsearchHeadMetadataPendingUrl"));
        assert!(script.contains("nixsearchReturnHeadMetadata"));
        assert!(script.contains("nixsearchReturnHeadMetadataUrl"));
        assert!(script.contains("initial-history-metadata"));
        assert!(script.contains("returnHeadMetadata"));
        assert!(script.contains("returnHeadMetadataUrl"));
        assert!(script.contains("function publicUrlKey"));
        assert!(script.contains("exactHeadMetadataFromState"));
        assert!(script.contains("pendingHistoryState"));
        assert!(script.contains("history.replaceState("));
        assert!(script.contains("history.pushState(nextState"));
        assert!(script.contains("publicUrlKey(targetUrl) !== publicUrlKey()"));
    }

    #[test]
    fn navigation_script_routes_close_links_through_modal_close() {
        let script = navigation_script();

        assert!(script.contains(r#"link.matches(".modal-backdrop, [data-role='entry-close']")"#));
        assert!(script.contains("closeEntryModal(dialog)"));
        assert!(script.contains("const returnMetadataUrl = state.nixsearchReturnHeadMetadataUrl;"));
        assert!(script.contains("returnMetadataUrl === closeTargetKey"));
        assert!(script.contains("reconcileMode: \"unless-restored\""));
        assert!(script.contains("reconcileSameUrl: true"));
        assert!(script.contains("optimisticallyRemoveEntryModal()"));
        assert!(script.contains("container.innerHTML = \"\";"));
        assert!(script.contains("classList.remove(\"modal-open\")"));
    }

    #[test]
    fn navigation_script_guards_modal_patches_by_target_url() {
        let script = navigation_script();

        assert!(script.contains("window.nixsearchApplyModalPatch = applyModalPatch;"));
        assert!(script.contains("publicUrlKey(targetUrl) !== publicUrlKey()"));
        assert!(script.contains("modalContainerFromHtml(html)"));
        assert!(script.contains("existing.replaceWith(parsed)"));
    }

    #[test]
    fn navigation_script_routes_page_replacements_through_navigation() {
        let script = navigation_script();

        assert!(script.contains("navigate(target, { push: false });"));
        assert!(!script.contains("history.replaceState(historyStateWithMetadata"));
    }

    #[test]
    fn navigation_script_reads_generation_state() {
        let script = navigation_script();

        assert!(script.contains("generation-state"));
        assert!(script.contains("function readGenerationId()"));
        assert!(script.contains("window.nixsearchGenerationId = currentGenerationId;"));
    }

    #[test]
    fn navigation_script_sends_generation_with_slice_requests() {
        let script = navigation_script();

        assert!(script.contains(r#"params.set("generation_id", requestGenerationId);"#));
        assert!(script.contains("fetchResultSlice("));
    }

    #[test]
    fn navigation_script_uses_generation_in_virtual_cache_keys() {
        let script = navigation_script();

        assert!(script.contains(
            "function virtualSliceCacheKey(requestGenerationId, requestUrl, offset, limit)"
        ));
        assert!(
            script.contains("JSON.stringify([requestGenerationId, requestUrl, offset, limit])")
        );
    }

    #[test]
    fn navigation_script_has_target_guarded_results_patch() {
        let script = navigation_script();

        assert!(script.contains("function applyResultsPatch(html, targetUrl)"));
        assert!(script.contains("publicUrlKey(targetUrl) !== publicUrlKey()"));
        assert!(script.contains("window.nixsearchApplyResultsPatch = applyResultsPatch;"));
    }

    #[test]
    fn navigation_script_applies_generation_change_atomically_after_target_check() {
        let script = navigation_script();

        let apply = script
            .find("function applyGenerationChange(payload)")
            .unwrap();
        let target_type_guard = script[apply..]
            .find(r#"typeof payload.targetUrl !== "string""#)
            .unwrap();
        let target_guard = script[apply..]
            .find("publicUrlKey(payload.targetUrl) !== publicUrlKey()")
            .unwrap();
        let begin = script[apply..].find("beginGenerationChange();").unwrap();
        let finally = script[apply..].find("finally").unwrap();
        let finish = script[apply..].find("finishGenerationChange();").unwrap();

        assert!(target_type_guard < target_guard);
        assert!(target_guard < begin);
        assert!(begin < finally);
        assert!(finally < finish);
        assert!(script.contains("window.nixsearchApplyGenerationChange = applyGenerationChange;"));
    }

    #[test]
    fn navigation_script_directly_updates_generation_id_from_payload() {
        let script = navigation_script();

        assert!(script.contains(r#"typeof payload.generationId === "string""#));
        assert!(script.contains("generationId = payload.generationId;"));
    }

    #[test]
    fn navigation_script_handles_stale_generation_before_applying_slice() {
        let script = navigation_script();

        let stale = script.find(r#"data.error === "stale_generation""#).unwrap();
        let remember = script
            .find("rememberVirtualSlice(cacheKey, data);")
            .unwrap();
        let apply = script
            .find("applyVirtualSlice(data, mode, normalizedOffset)")
            .unwrap();

        assert!(stale < remember);
        assert!(stale < apply);
        assert!(script.contains("function handleStaleGenerationSlice()"));
        assert!(script.contains("beginGenerationChange();"));
    }

    #[test]
    fn navigation_script_gates_virtual_loading_during_generation_change() {
        let script = navigation_script();

        assert!(script.contains("let generationChanging = false;"));
        assert!(script.contains("function beginGenerationChange()"));
        assert!(script.contains("function finishGenerationChange()"));
        assert!(script.contains("let generationChangeWatchdog = null;"));
        assert!(script.contains("clearGenerationChangeWatchdog();"));
        assert!(script.contains("virtualSliceCache.clear();"));
        assert!(script.contains("virtualRequestEpoch += 1;"));
        assert!(script.contains(
            "if (generationChanging || !virtualResults || virtualLoadScheduled) return;"
        ));
    }

    #[test]
    fn dialog_reconcile_script_loads_asset() {
        assert!(dialog_reconcile_script().contains("entry-modal"));
        assert!(dialog_reconcile_script().contains("showModal"));
    }
}
