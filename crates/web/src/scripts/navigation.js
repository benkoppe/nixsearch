(() => {
  const RECONCILE_EVENT = "nixsearch-reconcile";
  const metadata = JSON.parse(
    document.getElementById("source-metadata").textContent,
  );
  const PAGE_SIZE = __DEFAULT_LIMIT__;
  const VIRTUAL_REPLACE_LIMIT = PAGE_SIZE * 3;
  const VIRTUAL_JUMP_GAP = PAGE_SIZE * 4;
  const VIRTUAL_JUMP_DELTA = PAGE_SIZE * 3;
  let currentUrl = currentPublicUrl();
  let lastFocusedResultHref = "";

  if ("scrollRestoration" in history) {
    history.scrollRestoration = "manual";
  }

  function currentPublicUrl() {
    return window.location.pathname + window.location.search;
  }

  function titleForUrl(url) {
    const parsed = new URL(url, window.location.href);
    const params = new URLSearchParams(parsed.search);
    const parts = parsed.pathname.split("/").filter(Boolean);
    const sourceId =
      params.get("source") === "__SOURCE_ALL_VALUE__"
        ? ""
        : parts[0]
          ? decodeURIComponent(parts[0])
          : "";
    const q = (params.get("q") || "").trim();
    const titleParts = [];

    if (q) titleParts.push(q);

    const source = sourceMetadata(sourceId);
    if (source) {
      titleParts.push(source.name || source.id);
    } else if (sourceId) {
      titleParts.push(sourceId);
    }

    titleParts.push("nixsearch");
    return titleParts.join(" · ");
  }

  function syncTitle(url = currentPublicUrl()) {
    document.title = titleForUrl(url);
  }

  function replaceVisiblePageInUrl(page) {
    const nextPage = Math.max(1, page || 1);
    const url = new URL(window.location.href);
    const previous = currentPublicUrl();

    if (nextPage > 1) {
      url.searchParams.set("page", String(nextPage));
    } else {
      url.searchParams.delete("page");
    }

    const target = url.pathname + url.search;
    if (target === previous) return;

    history.replaceState(null, "", target);
    currentUrl = currentPublicUrl();
  }

  function currentPageFromUrl() {
    const page = parseInt(
      new URLSearchParams(window.location.search).get("page") || "1",
      10,
    );
    return Number.isFinite(page) ? Math.max(1, page) : 1;
  }

  function scrollToResultPage(page) {
    if (page <= 1) return false;

    const row = document.querySelector(
      `#results-body tr[data-result-page="${CSS.escape(String(page))}"]`,
    );
    if (!row) return false;

    const header = document.querySelector(".header");
    const top =
      window.scrollY +
      row.getBoundingClientRect().top -
      (header ? header.offsetHeight : 0);
    window.scrollTo(0, Math.max(0, top));
    return true;
  }

  let pageSyncScheduled = false;
  let virtualResults = null;
  let virtualLoadScheduled = false;
  let virtualRequestSeq = 0;
  let virtualActiveRequest = null;
  let virtualLastTargetOffset = null;
  const virtualSliceCache = new Map();

  function scheduleVisiblePageSync() {
    if (pageSyncScheduled) return;
    pageSyncScheduled = true;
    requestAnimationFrame(() => {
      pageSyncScheduled = false;
      const page = visibleResultPage();
      if (page) replaceVisiblePageInUrl(page);
    });
  }

  function visibleResultPage() {
    const results = document.getElementById("results");
    if (!results || results.classList.contains("results-loading")) return null;

    if (virtualResults) {
      return pageForOffset(virtualOffsetAtViewport());
    }

    const rows = document.querySelectorAll(
      "#results-body tr[data-result-page]",
    );
    if (!rows.length) return null;

    const header = document.querySelector(".header");
    const top = (header ? header.offsetHeight : 0) + 1;
    let lastAbove = null;

    for (const row of rows) {
      const page = parseInt(row.dataset.resultPage || "", 10);
      if (!Number.isFinite(page)) continue;

      const rect = row.getBoundingClientRect();
      if (rect.bottom <= top) {
        lastAbove = page;
        continue;
      }

      if (rect.top < window.innerHeight) {
        return page;
      }
    }

    return lastAbove;
  }

  function reconcile(previousUrl) {
    window.nixsearchPreviousUrl = previousUrl || "";
    window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
    currentUrl = currentPublicUrl();
  }

  function setLoading(active) {
    const results = document.getElementById("results");
    if (results) {
      if (active) {
        results.classList.add("results-loading");
      } else {
        results.classList.remove("results-loading");
      }
    }
  }

  // Clear loading state when results are patched by Datastar.
  (() => {
    const main = document.querySelector("main.main");
    if (!main) return;
    const observer = new MutationObserver(() => {
      const results = document.getElementById("results");
      if (results && !results.classList.contains("results-loading")) {
        setLoading(false);
        initializeVirtualResults();
        scheduleVisiblePageSync();
      }
    });
    observer.observe(main, { childList: true, subtree: true });
  })();

  function resultsContextForUrl(url) {
    const parsed = new URL(url, window.location.href);
    const params = new URLSearchParams(parsed.search);
    const q = (params.get("q") || "").trim();
    const state = normalizedStateFromUrl(url);
    const source = state.sourceId;
    const ref = state.refId;
    const refSet = state.activeRefSet;

    return JSON.stringify({ q, source, ref, refSet });
  }

  function shouldLoadResults(previousUrl, nextUrl) {
    return resultsContextForUrl(previousUrl) !== resultsContextForUrl(nextUrl);
  }

  function getActiveSourceTab() {
    return document.querySelector(".source-tab[data-active]");
  }

  function currentSourceFromTabs() {
    const tab = getActiveSourceTab();
    return tab ? tab.dataset.nixsearchSource || "" : "";
  }

  function sourceIdsFromTabs() {
    return Array.from(
      document.querySelectorAll(".source-tab[data-nixsearch-source]"),
      (tab) => tab.dataset.nixsearchSource || "",
    );
  }

  function setActiveSourceTab(sourceId) {
    document.querySelectorAll(".source-tab").forEach((tab) => {
      if (tab.dataset.nixsearchSource === sourceId) {
        tab.setAttribute("data-active", "");
      } else {
        tab.removeAttribute("data-active");
      }
    });
  }

  function getRefContainer() {
    return document.querySelector("[data-nixsearch-ref-container]");
  }

  function currentRefFromRadios() {
    const checked = document.querySelector(
      '[data-nixsearch-input="ref"]:checked',
    );
    return checked ? checked.value : "";
  }

  function sourceMetadata(sourceId) {
    return metadata.sources.find((s) => s.id === sourceId);
  }

  function refSetId(refSet) {
    return typeof refSet === "string" ? refSet : refSet.id;
  }

  function refSetMetadata(refSetIdValue) {
    return (metadata.refSets || []).find((r) => refSetId(r) === refSetIdValue);
  }

  function refSetIds() {
    return (metadata.refSets || []).map(refSetId);
  }

  function refsForRefSetSource(refSetIdValue, sourceId) {
    const refSet = refSetMetadata(refSetIdValue);
    if (!refSet || typeof refSet === "string") return [];
    const refs = (refSet.refs && refSet.refs[sourceId]) || [];
    return Array.isArray(refs) ? refs : refs ? [refs] : [];
  }

  function firstRefForRefSetSource(refSetIdValue, sourceId) {
    return refsForRefSetSource(refSetIdValue, sourceId)[0] || "";
  }

  function refSetContainsSourceRef(refSetIdValue, sourceId, refId) {
    return refsForRefSetSource(refSetIdValue, sourceId).includes(refId);
  }

  function syncLogoAccent(sourceId) {
    const logo = document.querySelector(".site-title");
    if (!logo) return;

    const source = sourceMetadata(sourceId);
    if (source) {
      logo.style.setProperty("--logo-accent", source.color);
    } else {
      logo.style.removeProperty("--logo-accent");
    }
  }

  function syncSearchFocusColor(sourceId) {
    const form = document.querySelector(".search-form");
    if (!form) return;

    const source = sourceMetadata(sourceId);
    if (source) {
      form.style.setProperty("--search-focus-color", source.color);
    } else {
      form.style.removeProperty("--search-focus-color");
    }
  }

  function defaultRefSet() {
    return metadata.defaultRefSet || "";
  }

  function normalizeAllRefSet(refSetIdValue) {
    if (refSetIdValue && refSetMetadata(refSetIdValue)) return refSetIdValue;
    return defaultRefSet();
  }

  function normalizeSourceRefSetContext(sourceId, refId, refSetIdValue) {
    if (
      refSetIdValue &&
      refSetMetadata(refSetIdValue) &&
      refsForRefSetSource(refSetIdValue, sourceId).length > 0
    ) {
      return { activeRefSet: refSetIdValue, activeRefSetExplicit: true };
    }

    return {
      activeRefSet: "",
      activeRefSetExplicit: false,
    };
  }

  function normalizedStateFromUrl(url = window.location.href) {
    const parsed = new URL(url, window.location.href);
    const params = new URLSearchParams(parsed.search);
    const parts = parsed.pathname.split("/").filter(Boolean);
    const sourceAll = params.get("source") === "__SOURCE_ALL_VALUE__";
    const sourceId = sourceAll ? "" : parts[0] ? decodeURIComponent(parts[0]) : "";
    const source = sourceMetadata(sourceId);
    const requestedRefSet = (params.get("ref_set") || "").trim();

    if (!sourceId) {
      return {
        sourceId: "",
        refId: "",
        activeRefSet: requestedRefSet || defaultRefSet(),
        activeRefSetExplicit: true,
      };
    }

    const requestedRef = (params.get("ref") || "").trim();
    if (requestedRefSet) {
      const refs = refsForRefSetSource(requestedRefSet, sourceId);

      if (refs.length === 1) {
        return {
          sourceId,
          refId: requestedRef && requestedRef !== refs[0] ? "" : refs[0],
          invalidRefId: requestedRef && requestedRef !== refs[0] ? requestedRef : "",
          activeRefSet: requestedRefSet,
          activeRefSetExplicit: true,
        };
      }

      return {
        sourceId,
        refId: refs.length > 1 ? requestedRef : "",
        invalidRefId: "",
        activeRefSet: requestedRefSet,
        activeRefSetExplicit: true,
      };
    }

    return {
      sourceId,
      refId: requestedRef || (source ? source.defaultRef : ""),
      invalidRefId: "",
      activeRefSet: "",
      activeRefSetExplicit: false,
    };
  }

  function populateRefRadios(
    sourceId,
    activeRefSet = "",
    selectedRefId = undefined,
  ) {
    const container = getRefContainer();
    if (!container) return;

    const source = sourceMetadata(sourceId);
    syncLogoAccent(sourceId);
    syncSearchFocusColor(sourceId);

    if (source) {
      container.style.setProperty("--source-color", source.color);
    } else {
      container.style.removeProperty("--source-color");
    }

    if (!sourceId) {
      const refSets = refSetIds();
      if (refSets.length === 0) {
        container.innerHTML = "";
        syncHeaderHeight();
        return;
      }

      const selectedRefSet =
        selectedRefId === undefined
          ? activeRefSet || defaultRefSet()
          : selectedRefId;
      container.innerHTML = refSets
        .map((r) => {
          const checked = r === selectedRefSet ? " checked" : "";
          return `<label class="ref-radio-label"><input type="radio" name="ref_set" value="${r}"${checked} data-nixsearch-input="ref"><span>${r}</span></label>`;
        })
        .join("");
      syncHeaderHeight();
      return;
    }

    if (!source || source.refs.length === 0) {
      container.innerHTML = "";
      syncHeaderHeight();
      return;
    }

    const selectedRef =
      selectedRefId === undefined
        ? firstRefForRefSetSource(activeRefSet, sourceId) || source.defaultRef
        : selectedRefId;
    container.innerHTML = source.refs
      .map((r) => {
        const checked = r === selectedRef ? " checked" : "";
        return `<label class="ref-radio-label"><input type="radio" name="ref" value="${r}"${checked} data-nixsearch-input="ref"><span>${r}</span></label>`;
      })
      .join("");
    syncHeaderHeight();
  }

  function syncHeaderHeight() {
    const header = document.querySelector(".header");
    if (header) {
      document.documentElement.style.setProperty(
        "--header-height",
        header.offsetHeight + "px",
      );
    }
  }

  const VISUAL_VIEWPORT_KEYBOARD_DELTA = 180;
  const VISUAL_VIEWPORT_HEIGHT_EPSILON = 12;
  let footerViewportStateScheduled = false;
  let footerEditableFocused = false;
  let stableVisualViewportHeight = null;

  function scheduleFooterViewportStateSync() {
    if (footerViewportStateScheduled) return;
    footerViewportStateScheduled = true;
    requestAnimationFrame(syncFooterViewportState);
  }

  function shouldGuardFooterViewport() {
    if (!window.visualViewport) return false;
    return (
      window.matchMedia("(pointer: coarse)").matches ||
      window.matchMedia("(any-pointer: coarse)").matches
    );
  }

  function isStandaloneDisplay() {
    return (
      window.matchMedia("(display-mode: standalone)").matches ||
      window.navigator.standalone === true
    );
  }

  function syncFooterViewportState() {
    footerViewportStateScheduled = false;

    if (!shouldGuardFooterViewport()) {
      resetFooterViewportState();
      return;
    }

    const viewport = window.visualViewport;
    if (!viewport) {
      resetFooterViewportState();
      return;
    }

    if (stableVisualViewportHeight === null) {
      stableVisualViewportHeight = viewport.height;
    }

    const keyboardOpen =
      footerEditableFocused &&
      stableVisualViewportHeight - viewport.height > VISUAL_VIEWPORT_KEYBOARD_DELTA;

    if (
      !footerEditableFocused ||
      (!keyboardOpen &&
        viewport.height > stableVisualViewportHeight + VISUAL_VIEWPORT_HEIGHT_EPSILON)
    ) {
      stableVisualViewportHeight = viewport.height;
    }

    const standalone = isStandaloneDisplay();
    document.documentElement.classList.toggle(
      "footer-safe-bottom-enabled",
      standalone,
    );
    document.documentElement.classList.toggle(
      "footer-browser-compact",
      !standalone,
    );
    document.documentElement.classList.toggle(
      "footer-keyboard-open",
      keyboardOpen,
    );
  }

  function resetFooterViewportState() {
    footerEditableFocused = false;
    stableVisualViewportHeight = null;
    document.documentElement.classList.remove("footer-keyboard-open");
    document.documentElement.classList.remove("footer-safe-bottom-enabled");
    document.documentElement.classList.remove("footer-browser-compact");
  }

  function syncModalState() {
    const dialog = document.getElementById("entry-modal");

    document.documentElement.classList.toggle(
      "modal-open",
      !!dialog && dialog.open,
    );

    if (dialog && !dialog.dataset.modalStateBound) {
      dialog.dataset.modalStateBound = "true";
      dialog.addEventListener("close", syncModalState);
      dialog.addEventListener("cancel", (evt) => {
        if (!closeEntryModal(dialog)) return;
        evt.preventDefault();
      });
    }
  }

  window.nixsearchSyncModalState = syncModalState;

  function sourcePath(sourceId) {
    return sourceId ? "/" + encodeURIComponent(sourceId) : "/";
  }

  function refSetForLink(refSetIdValue) {
    return refSetIdValue && refSetIdValue !== defaultRefSet() ? refSetIdValue : "";
  }

  function buildSearchUrlFromInputs(context = null) {
    if (context === null) {
      context = normalizedStateFromUrl();
    }
    const contextActiveRefSet = context.activeRefSet || "";
    const contextActiveRefSetExplicit = !!context.activeRefSetExplicit;
    const activeSourceTab = getActiveSourceTab();
    const sourceId = activeSourceTab
      ? activeSourceTab.dataset.nixsearchSource || ""
      : context.sourceId || "";
    const path = sourcePath(sourceId);
    const params = new URLSearchParams();

    const q = document.querySelector('[data-nixsearch-input="q"]');
    if (q && q.value.trim()) params.set("q", q.value.trim());

    if (sourceId) {
      const refValue = currentRefFromRadios();
      const source = sourceMetadata(sourceId);
      const refSetRefs = contextActiveRefSetExplicit
        ? refsForRefSetSource(contextActiveRefSet, sourceId)
        : [];
      const shouldUseRefSet = refSetRefs.length > 0;
      const shouldSetRef = !shouldUseRefSet || refSetRefs.length > 1;
      const sourceMatchesContext = context.sourceId === sourceId;

      if (
        shouldSetRef &&
        refValue &&
        (!source || shouldUseRefSet || refValue !== source.defaultRef)
      ) {
        params.set("ref", refValue);
      } else if (shouldSetRef && sourceMatchesContext) {
        const preservedRef = context.refId || context.invalidRefId || "";
        if (preservedRef) params.set("ref", preservedRef);
      }
      if (shouldUseRefSet && refSetForLink(contextActiveRefSet)) {
        params.set("ref_set", contextActiveRefSet);
      } else if (
        !refValue &&
        sourceMatchesContext &&
        contextActiveRefSetExplicit &&
        contextActiveRefSet
      ) {
        params.set("ref_set", contextActiveRefSet);
      }
    } else {
      const refSetValue = currentRefFromRadios();
      const activeRefSet = refSetValue
        ? normalizeAllRefSet(refSetValue)
        : contextActiveRefSetExplicit
          ? contextActiveRefSet
          : normalizeAllRefSet(contextActiveRefSet);
      if (refSetForLink(activeRefSet)) {
        params.set("ref_set", activeRefSet);
      }
    }

    const qs = params.toString();
    return qs ? path + "?" + qs : path;
  }

  function selectSource(
    sourceId,
    { push = true, preserveSourceKeyboardHistory = false } = {},
  ) {
    const previousState = normalizedStateFromUrl();
    resetQueryHistoryGrouping();
    if (!preserveSourceKeyboardHistory) resetSourceKeyboardHistoryGrouping();
    setActiveSourceTab(sourceId);
    populateRefRadios(sourceId, previousState.activeRefSet);

    const dropdown = document.querySelector("[data-nixsearch-overflow-menu]");
    if (dropdown) dropdown.hidden = true;

    return navigate(buildSearchUrlFromInputs(previousState), { push });
  }

  function selectSourceFromKeyboard(sourceId) {
    scheduleSourceKeyboardHistoryBoundary();
    const changed = selectSource(sourceId, {
      push: nextSourceKeyboardNavigationPushes,
      preserveSourceKeyboardHistory: true,
    });
    if (changed) nextSourceKeyboardNavigationPushes = false;
  }

  function cycleSourceFilter(direction) {
    const sourceIds = sourceIdsFromTabs();
    if (sourceIds.length < 2) return false;

    const currentIndex = sourceIds.indexOf(currentSourceFromTabs());
    const startIndex = currentIndex >= 0 ? currentIndex : 0;
    const nextIndex =
      (startIndex + direction + sourceIds.length) % sourceIds.length;

    selectSourceFromKeyboard(sourceIds[nextIndex]);
    return true;
  }

  function resultRows() {
    return Array.from(document.querySelectorAll("#results-body tr[data-href]"));
  }

  function rememberResultLink(link) {
    if (!link) return;
    lastFocusedResultHref = link.href || link.getAttribute("href") || "";
  }

  function restoreResultFocus() {
    if (!lastFocusedResultHref) return false;

    const dialog = document.getElementById("entry-modal");
    if (dialog && dialog.open) return false;

    const link = Array.from(
      document.querySelectorAll("#results-body a.entry-name"),
    ).find((candidate) => candidate.href === lastFocusedResultHref);
    if (!link) return false;

    link.focus({ preventScroll: true });
    return true;
  }

  window.nixsearchRestoreResultFocus = restoreResultFocus;

  function firstVisibleResultRowIndex(rows) {
    const visible = firstVisibleResultRow();
    const index = visible ? rows.indexOf(visible) : -1;
    return index >= 0 ? index : 0;
  }

  function focusedResultRowIndex(rows) {
    const active = document.activeElement;
    if (!(active instanceof Element)) return -1;

    const row = active.closest("#results-body tr[data-href]");
    return row ? rows.indexOf(row) : -1;
  }

  function moveResultSelection(direction) {
    const rows = resultRows();
    if (rows.length === 0) return false;

    const focusedIndex = focusedResultRowIndex(rows);
    const currentIndex =
      focusedIndex >= 0 ? focusedIndex : firstVisibleResultRowIndex(rows);
    const nextIndex = Math.max(
      0,
      Math.min(
        rows.length - 1,
        focusedIndex >= 0 ? currentIndex + direction : currentIndex,
      ),
    );

    const link = rows[nextIndex].querySelector("a.entry-name");
    if (!link) return false;

    rememberResultLink(link);
    link.focus();
    link.scrollIntoView({ block: "nearest" });
    return true;
  }

  function navigate(url, { push = true, syncInputs = false } = {}) {
    const next = new URL(url, window.location.href);
    const target = next.pathname + next.search;
    const current = currentPublicUrl();

    if (target === current) {
      if (syncInputs) {
        syncInputsFromUrl();
      }
      syncTitle(target);
      return false;
    }

    const loadsResults = shouldLoadResults(current, target);

    if (push) {
      history.pushState(null, "", target);
    } else {
      history.replaceState(null, "", target);
    }

    if (syncInputs) {
      syncInputsFromUrl();
    }

    setLoading(loadsResults);
    if (loadsResults) {
      window.scrollTo(0, 0);
    }
    syncTitle(target);
    reconcile(current);
    return true;
  }

  function closeEntryModal(dialog) {
    const url = dialog.getAttribute("data-close-url");
    if (!url) return false;

    resetQueryHistoryGrouping();
    resetSourceKeyboardHistoryGrouping();
    navigate(url);
    return true;
  }

  function syncInputsFromUrl() {
    const params = new URLSearchParams(window.location.search);
    const state = normalizedStateFromUrl();
    const effectiveSource = state.sourceId;

    setActiveSourceTab(effectiveSource);
    populateRefRadios(
      effectiveSource,
      state.activeRefSetExplicit ? state.activeRefSet : "",
      effectiveSource ? state.refId : state.activeRefSet,
    );

    const refParam = effectiveSource ? state.refId : state.activeRefSet;
    if (refParam) {
      const radio = document.querySelector(
        `[data-nixsearch-input="ref"][value="${CSS.escape(refParam)}"]`,
      );
      if (radio) radio.checked = true;
    }

    const q = document.querySelector('[data-nixsearch-input="q"]');
    if (q) q.value = params.get("q") || "";
  }

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }

    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.top = "-9999px";
    document.body.appendChild(textarea);
    textarea.select();

    try {
      const copied = document.execCommand("copy");
      return copied
        ? Promise.resolve()
        : Promise.reject(new Error("copy failed"));
    } finally {
      textarea.remove();
    }
  }

  document.addEventListener("click", (evt) => {
    const button = evt.target.closest("[data-copy-entry]");
    if (!button) return;

    evt.preventDefault();
    copyText(button.dataset.copyEntry || "").then(() => {
      button.dataset.copied = "true";
      button.setAttribute("aria-label", "Copied entry name");
      button.setAttribute("title", "Copied");
      clearTimeout(button._copyReset);
      button._copyReset = setTimeout(() => {
        button.removeAttribute("data-copied");
        button.setAttribute("aria-label", "Copy entry name");
        button.setAttribute("title", "Copy");
      }, 1500);
    });
  });

  document.addEventListener("click", (evt) => {
    const tab = evt.target.closest(".source-tab, .source-tabs-dropdown button");
    if (!tab) return;
    if (!tab.hasAttribute("data-nixsearch-source")) return;

    evt.preventDefault();
    let sourceId = tab.dataset.nixsearchSource || "";
    if (sourceId && sourceId === currentSourceFromTabs()) {
      sourceId = "";
    }
    selectSource(sourceId);
  });

  document.addEventListener("change", (evt) => {
    const el = evt.target;
    if (!el.matches || !el.matches('[data-nixsearch-input="ref"]')) return;
    resetQueryHistoryGrouping();
    resetSourceKeyboardHistoryGrouping();
    navigate(buildSearchUrlFromInputs({ activeRefSet: "", activeRefSetExplicit: false }));
  });

  document.addEventListener("click", (evt) => {
    if (evt.defaultPrevented) return;
    if (evt.button !== 0) return;
    if (evt.metaKey || evt.ctrlKey || evt.shiftKey || evt.altKey) return;

    const row = evt.target.closest("tr[data-href]");
    if (row) {
      const link = evt.target.closest("a[href]");
      if (!link) {
        evt.preventDefault();
        rememberResultLink(row.querySelector("a.entry-name"));
        const url = new URL(row.dataset.href, window.location.href);
        if (url.origin === window.location.origin) {
          resetQueryHistoryGrouping();
          resetSourceKeyboardHistoryGrouping();
          navigate(url.toString());
          return;
        }
      }
    }
  });

  document.addEventListener("click", (evt) => {
    if (evt.defaultPrevented) return;
    if (evt.button !== 0) return;
    if (evt.metaKey || evt.ctrlKey || evt.shiftKey || evt.altKey) return;

    const link = evt.target.closest("a[href]");
    if (!link) return;
    if (link.target === "_blank") return;
    if (link.hasAttribute("download")) return;

    const url = new URL(link.href, window.location.href);
    if (url.origin !== window.location.origin) return;
    if (link.rel && link.rel.includes("external")) return;

    evt.preventDefault();
    if (link.matches("#results-body a.entry-name")) rememberResultLink(link);
    resetQueryHistoryGrouping();
    resetSourceKeyboardHistoryGrouping();
    navigate(url.toString(), { syncInputs: true });
  });

  document.addEventListener("focusin", (evt) => {
    const target = evt.target;
    if (!(target instanceof Element)) return;

    const link = target.closest("#results-body a.entry-name");
    if (link) rememberResultLink(link);
  });

  document.addEventListener("click", (evt) => {
    const dialog = evt.target;
    if (!(dialog instanceof HTMLDialogElement)) return;
    if (dialog.id !== "entry-modal") return;

    if (!closeEntryModal(dialog)) return;
    evt.preventDefault();
  });

  const QUERY_NAVIGATION_DEBOUNCE_MS = 75;
  const QUERY_HISTORY_DEBOUNCE_MS = 1000;
  const SOURCE_KEYBOARD_HISTORY_DEBOUNCE_MS = 500;
  let queryNavigationDebounce;
  let queryHistoryDebounce;
  let nextQueryNavigationPushes = true;
  let sourceKeyboardHistoryDebounce;
  let nextSourceKeyboardNavigationPushes = true;

  function clearPendingQueryNavigation() {
    clearTimeout(queryNavigationDebounce);
    queryNavigationDebounce = null;
  }

  function resetQueryHistoryGrouping() {
    clearPendingQueryNavigation();
    clearTimeout(queryHistoryDebounce);
    queryHistoryDebounce = null;
    nextQueryNavigationPushes = true;
  }

  function scheduleQueryHistoryBoundary() {
    clearTimeout(queryHistoryDebounce);
    queryHistoryDebounce = setTimeout(() => {
      queryHistoryDebounce = null;
      nextQueryNavigationPushes = true;
    }, QUERY_HISTORY_DEBOUNCE_MS);
  }

  function resetSourceKeyboardHistoryGrouping() {
    clearTimeout(sourceKeyboardHistoryDebounce);
    sourceKeyboardHistoryDebounce = null;
    nextSourceKeyboardNavigationPushes = true;
  }

  function scheduleSourceKeyboardHistoryBoundary() {
    clearTimeout(sourceKeyboardHistoryDebounce);
    sourceKeyboardHistoryDebounce = setTimeout(() => {
      sourceKeyboardHistoryDebounce = null;
      nextSourceKeyboardNavigationPushes = true;
    }, SOURCE_KEYBOARD_HISTORY_DEBOUNCE_MS);
  }

  function navigateQueryFromInput() {
    const changed = navigate(buildSearchUrlFromInputs(), {
      push: nextQueryNavigationPushes,
    });
    if (changed) nextQueryNavigationPushes = false;
  }

  function isEditableTarget(target) {
    if (!(target instanceof Element)) return false;
    return !!target.closest("input, textarea, select, [contenteditable]");
  }

  document.addEventListener("keydown", (evt) => {
    if (
      evt.ctrlKey &&
      !evt.metaKey &&
      !evt.altKey &&
      !evt.shiftKey &&
      !evt.isComposing
    ) {
      const key = evt.key.toLowerCase();
      if (key === "n" || key === "p") {
        const dialog = document.getElementById("entry-modal");
        if (dialog && dialog.open) return;

        if (moveResultSelection(key === "n" ? 1 : -1)) evt.preventDefault();
        return;
      }
    }

    if (
      (evt.key === "[" || evt.key === "]") &&
      evt.ctrlKey &&
      !evt.metaKey &&
      !evt.altKey &&
      !evt.isComposing
    ) {
      const dialog = document.getElementById("entry-modal");
      if (dialog && dialog.open) return;

      if (cycleSourceFilter(evt.key === "]" ? 1 : -1)) evt.preventDefault();
      return;
    }

    if (evt.key !== "/") return;
    if (evt.metaKey || evt.ctrlKey || evt.altKey || evt.isComposing) return;
    if (isEditableTarget(evt.target)) return;

    const dialog = document.getElementById("entry-modal");
    if (dialog && dialog.open) return;

    const input = document.querySelector('[data-nixsearch-input="q"]');
    if (!input) return;

    evt.preventDefault();
    input.focus();
    input.select();
  });

  document.addEventListener("input", (evt) => {
    const el = evt.target;
    if (!el.matches || !el.matches('[data-nixsearch-input="q"]')) return;
    clearPendingQueryNavigation();
    resetSourceKeyboardHistoryGrouping();
    scheduleQueryHistoryBoundary();
    queryNavigationDebounce = setTimeout(() => {
      queryNavigationDebounce = null;
      navigateQueryFromInput();
    }, QUERY_NAVIGATION_DEBOUNCE_MS);
  });

  document.addEventListener("submit", (evt) => {
    const form = evt.target;
    if (!(form instanceof HTMLFormElement)) return;
    if (form.method && form.method.toLowerCase() !== "get") return;

    evt.preventDefault();
    resetQueryHistoryGrouping();
    resetSourceKeyboardHistoryGrouping();

    const q = form.querySelector('[data-nixsearch-input="q"]');
    if (q) q.blur();

    navigate(buildSearchUrlFromInputs());
  });

  window.addEventListener("popstate", () => {
    const previous = currentUrl;
    resetQueryHistoryGrouping();
    resetSourceKeyboardHistoryGrouping();
    syncInputsFromUrl();
    setLoading(shouldLoadResults(previous, currentPublicUrl()));
    syncTitle();
    reconcile(previous);
  });

  window.addEventListener("pageshow", (evt) => {
    if (!evt.persisted) return;

    syncInputsFromUrl();
    setLoading(false);
    syncTitle();
    currentUrl = currentPublicUrl();
  });

  window.nixsearchNavigate = navigate;

  const RESULTS_SLICE_URL = "__RESULTS_SLICE_URL__";

  function pageForOffset(offset) {
    return Math.floor(Math.max(0, offset) / PAGE_SIZE) + 1;
  }

  async function fetchResultSlice(
    offset,
    limit = PAGE_SIZE,
    pageUrl = currentPublicUrl(),
    signal = undefined,
  ) {
    const url = `${RESULTS_SLICE_URL}?url=${encodeURIComponent(pageUrl)}&offset=${offset}&limit=${limit}`;
    const res = await fetch(url, { signal });
    return await res.json();
  }

  function virtualSliceCacheKey(requestUrl, offset, limit) {
    return `${requestUrl}\n${offset}\n${limit}`;
  }

  function rememberVirtualSlice(key, data) {
    virtualSliceCache.set(key, data);
    if (virtualSliceCache.size > 32) {
      virtualSliceCache.delete(virtualSliceCache.keys().next().value);
    }
  }

  function initializeVirtualResults() {
    const results = document.getElementById("results");
    const tbody = document.getElementById("results-body");
    if (!results || !tbody || !results.dataset.total) {
      virtualResults = null;
      return false;
    }

    if (results.querySelector("[data-virtual-spacer]")) {
      return !!virtualResults;
    }

    const table = tbody.closest("table");
    if (!table || !table.parentNode) {
      virtualResults = null;
      return false;
    }

    const rows = Array.from(tbody.querySelectorAll("tr[data-result-page]"));
    const total = parseInt(results.dataset.total || "0", 10);
    const startOffset = parseInt(results.dataset.startOffset || "0", 10);
    const rowHeight = measureResultRowHeight(rows);

    if (
      !Number.isFinite(total) ||
      total <= 0 ||
      !rowHeight ||
      rows.length === 0
    ) {
      virtualResults = null;
      return false;
    }

    virtualResults = {
      results,
      table,
      tbody,
      total,
      rowHeight,
      startOffset,
      endOffset: Math.min(total, startOffset + rows.length),
      requestUrl: currentPublicUrl(),
      topSpacer: createVirtualSpacer("top"),
      bottomSpacer: createVirtualSpacer("bottom"),
      topSpacerHeight: startOffset * rowHeight,
      bottomSpacerHeight:
        (total - Math.min(total, startOffset + rows.length)) * rowHeight,
    };
    virtualLastTargetOffset = null;

    results.classList.add("virtual-results-active");
    applyVirtualSpacerRowHeight();
    table.parentNode.insertBefore(virtualResults.topSpacer, table);
    table.insertAdjacentElement("afterend", virtualResults.bottomSpacer);
    applyVirtualSpacers();
    return true;
  }

  function measureResultRowHeight(rows) {
    const resultRows =
      rows && rows.length
        ? rows
        : Array.from(
            document.querySelectorAll("#results-body tr[data-result-page]"),
          );
    if (!resultRows.length) return null;

    const height = measureRowsHeight(resultRows) / resultRows.length;
    return height > 0 ? height : null;
  }

  function createVirtualSpacer(position) {
    const spacer = document.createElement("div");
    spacer.className = `virtual-spacer virtual-${position}-spacer`;
    spacer.dataset.virtualSpacer = position;
    return spacer;
  }

  function applyVirtualSpacerRowHeight() {
    if (!virtualResults) return;

    const height = `${virtualResults.rowHeight}px`;
    virtualResults.topSpacer.style.setProperty("--row-height", height);
    virtualResults.bottomSpacer.style.setProperty("--row-height", height);
  }

  function setVirtualSpacerLoading(mode, active) {
    if (!virtualResults) return;

    const toggle = (spacer) => {
      spacer.classList.toggle("virtual-spacer-loading", active);
    };

    if (mode === "prepend" || mode === "replace") {
      toggle(virtualResults.topSpacer);
    }

    if (mode === "append" || mode === "replace") {
      toggle(virtualResults.bottomSpacer);
    }
  }

  function setSpacerHeight(spacer, height) {
    const px = Math.max(0, height);
    spacer.style.height = `${px}px`;
  }

  function applyVirtualSpacers() {
    if (!virtualResults) return;

    setSpacerHeight(virtualResults.topSpacer, virtualResults.topSpacerHeight);
    setSpacerHeight(
      virtualResults.bottomSpacer,
      virtualResults.bottomSpacerHeight,
    );
  }

  function resetVirtualSpacerHeights() {
    if (!virtualResults) return;

    virtualResults.topSpacerHeight =
      virtualResults.startOffset * virtualResults.rowHeight;
    virtualResults.bottomSpacerHeight =
      (virtualResults.total - virtualResults.endOffset) *
      virtualResults.rowHeight;
    applyVirtualSpacers();
  }

  function adjustVirtualSpacer(position, delta) {
    if (!virtualResults) return;

    if (position === "top") {
      virtualResults.topSpacerHeight = Math.max(
        0,
        virtualResults.topSpacerHeight + delta,
      );
    } else {
      virtualResults.bottomSpacerHeight = Math.max(
        0,
        virtualResults.bottomSpacerHeight + delta,
      );
    }
    applyVirtualSpacers();
  }

  function documentHeight() {
    const main = document.querySelector("main.main");
    return main
      ? main.getBoundingClientRect().height
      : document.documentElement.scrollHeight;
  }

  function runVirtualTransaction(spacer, anchor, mutate) {
    const beforeHeight = documentHeight();
    const beforeAnchorTop =
      anchor && document.contains(anchor)
        ? anchor.getBoundingClientRect().top
        : null;

    mutate();

    const heightDelta = documentHeight() - beforeHeight;
    if (heightDelta !== 0) {
      adjustVirtualSpacer(spacer, -heightDelta);
    }

    if (anchor && beforeAnchorTop !== null && document.contains(anchor)) {
      const anchorDelta = anchor.getBoundingClientRect().top - beforeAnchorTop;
      if (anchorDelta !== 0) window.scrollBy(0, anchorDelta);
    }
  }

  function measureRowsHeight(rows) {
    if (!rows.length) return 0;

    const first = rows[0].getBoundingClientRect();
    const last = rows[rows.length - 1].getBoundingClientRect();
    return Math.max(0, last.bottom - first.top);
  }

  function virtualOffsetAtViewport() {
    if (!virtualResults) return 0;

    const header = document.querySelector(".header");
    const viewportTop = window.scrollY + (header ? header.offsetHeight : 0) + 1;
    const tbodyTop =
      window.scrollY + virtualResults.tbody.getBoundingClientRect().top;
    const trackTop = tbodyTop - virtualResults.topSpacerHeight;
    const y = Math.max(0, viewportTop - trackTop);
    const offset = Math.floor(y / virtualResults.rowHeight);
    return Math.min(virtualResults.total - 1, Math.max(0, offset));
  }

  function scheduleVirtualLoad() {
    if (!virtualResults || virtualLoadScheduled) return;
    virtualLoadScheduled = true;
    requestAnimationFrame(() => {
      virtualLoadScheduled = false;
      loadVirtualRowsNearViewport();
    });
  }

  async function loadVirtualRowsNearViewport() {
    if (!virtualResults) return;

    const targetOffset = virtualOffsetAtViewport();
    const previousTargetOffset = virtualLastTargetOffset;
    virtualLastTargetOffset = targetOffset;

    const { startOffset, endOffset, total } = virtualResults;
    const jump = isVirtualJumpTarget(
      targetOffset,
      previousTargetOffset,
      startOffset,
      endOffset,
    );

    if (targetOffset < startOffset) {
      if (jump) {
        await loadVirtualSlice(
          replacementSliceOffset(targetOffset, total, VIRTUAL_REPLACE_LIMIT),
          "replace",
          {
            abortExisting: true,
            limit: VIRTUAL_REPLACE_LIMIT,
          },
        );
        return;
      }

      if (!virtualActiveRequest) {
        await loadVirtualSlice(Math.max(0, startOffset - PAGE_SIZE), "prepend");
      }
      return;
    }

    if (targetOffset >= endOffset) {
      if (jump) {
        await loadVirtualSlice(
          replacementSliceOffset(targetOffset, total, VIRTUAL_REPLACE_LIMIT),
          "replace",
          {
            abortExisting: true,
            limit: VIRTUAL_REPLACE_LIMIT,
          },
        );
        return;
      }

      if (!virtualActiveRequest) {
        await loadVirtualSlice(endOffset, "append");
      }
      return;
    }

    if (virtualActiveRequest) return;

    const margin = PAGE_SIZE * 2;
    if (targetOffset < startOffset + margin && startOffset > 0) {
      await loadVirtualSlice(Math.max(0, startOffset - PAGE_SIZE), "prepend");
      return;
    }

    if (targetOffset >= endOffset - margin && endOffset < total) {
      await loadVirtualSlice(endOffset, "append");
    }
  }

  function isVirtualJumpTarget(
    targetOffset,
    previousTargetOffset,
    startOffset,
    endOffset,
  ) {
    const gap = virtualGapOutsideWindow(targetOffset, startOffset, endOffset);
    const delta =
      previousTargetOffset === null
        ? 0
        : Math.abs(targetOffset - previousTargetOffset);

    return (
      delta > VIRTUAL_JUMP_DELTA ||
      (!virtualActiveRequest && gap > VIRTUAL_JUMP_GAP)
    );
  }

  function virtualGapOutsideWindow(targetOffset, startOffset, endOffset) {
    if (targetOffset < startOffset) return startOffset - targetOffset;
    if (targetOffset >= endOffset) return targetOffset - endOffset + 1;
    return 0;
  }

  function replacementSliceOffset(targetOffset, total, limit) {
    const centered = Math.max(0, targetOffset - Math.floor(limit / 2));
    const maxStart = Math.max(0, total - limit);
    return Math.floor(Math.min(centered, maxStart) / PAGE_SIZE) * PAGE_SIZE;
  }

  function cancelVirtualRequest() {
    if (!virtualActiveRequest) return;
    virtualActiveRequest.controller.abort();
    virtualActiveRequest = null;
  }

  async function loadVirtualSlice(offset, mode, options = {}) {
    if (!virtualResults) return;

    const state = virtualResults;
    const requestUrl = state.requestUrl;
    const limit = options.limit || PAGE_SIZE;
    const normalizedOffset = Math.max(
      0,
      Math.min(offset, Math.max(0, state.total - 1)),
    );
    const cacheKey = virtualSliceCacheKey(requestUrl, normalizedOffset, limit);
    const cached = virtualSliceCache.get(cacheKey);

    if (virtualActiveRequest && virtualActiveRequest.key === cacheKey) return;

    if (cached) {
      if (options.abortExisting || mode === "replace") cancelVirtualRequest();
      if (applyVirtualSlice(cached, mode, normalizedOffset)) {
        scheduleVisiblePageSync();
        scheduleVirtualLoad();
      }
      return;
    }

    if (virtualActiveRequest) {
      if (options.abortExisting || mode === "replace") {
        cancelVirtualRequest();
      } else {
        return;
      }
    }

    const requestId = ++virtualRequestSeq;
    const controller = new AbortController();
    virtualActiveRequest = { controller, id: requestId, key: cacheKey };
    setVirtualSpacerLoading(mode, true);

    try {
      const data = await fetchResultSlice(
        normalizedOffset,
        limit,
        requestUrl,
        controller.signal,
      );

      if (
        !virtualResults ||
        virtualResults.requestUrl !== requestUrl ||
        !virtualActiveRequest ||
        virtualActiveRequest.id !== requestId
      ) {
        return;
      }

      if (data.error) {
        console.error("Load virtual results failed:", data.error);
        return;
      }

      rememberVirtualSlice(cacheKey, data);
      if (applyVirtualSlice(data, mode, normalizedOffset)) scheduleVisiblePageSync();
    } catch (e) {
      if (e.name === "AbortError") return;
      console.error("Failed to load virtual results:", e);
    } finally {
      const ownsActiveRequest =
        virtualActiveRequest && virtualActiveRequest.id === requestId;
      if (ownsActiveRequest || !virtualActiveRequest) {
        setVirtualSpacerLoading(mode, false);
      }
      if (ownsActiveRequest) {
        virtualActiveRequest = null;
      }
      scheduleVirtualLoad();
    }
  }

  function applyVirtualSlice(data, mode, requestedOffset) {
    if (!virtualResults || typeof data.rows !== "string") return false;

    const state = virtualResults;
    const previousTotal = state.total;
    const sliceOffset = finiteNumber(data.offset, requestedOffset);
    const count = finiteNumber(data.count, null);
    const sliceEnd = finiteNumber(
      data.endOffset,
      sliceOffset + Math.max(0, count || 0),
    );

    if (typeof data.total === "number") state.total = data.total;

    if (mode === "replace") {
      state.tbody
        .querySelectorAll("tr[data-result-page]")
        .forEach((row) => row.remove());
      if (data.rows) state.tbody.insertAdjacentHTML("afterbegin", data.rows);
      state.startOffset = Math.min(sliceOffset, state.total);
      state.endOffset = Math.min(state.total, Math.max(sliceOffset, sliceEnd));
      resetVirtualSpacerHeights();
      return true;
    }

    const anchor = firstVisibleResultRow();
    const spacer = mode === "prepend" ? "top" : "bottom";

    runVirtualTransaction(spacer, anchor, () => {
      if (mode === "append") {
        if (data.rows) state.tbody.insertAdjacentHTML("beforeend", data.rows);
        state.endOffset = Math.min(state.total, Math.max(state.endOffset, sliceEnd));
      } else {
        if (data.rows) state.tbody.insertAdjacentHTML("afterbegin", data.rows);
        state.startOffset = Math.min(state.startOffset, sliceOffset);
      }
    });

    if (previousTotal !== state.total) resetVirtualSpacerHeights();
    return true;
  }

  function finiteNumber(value, fallback) {
    return typeof value === "number" && Number.isFinite(value) ? value : fallback;
  }

  function firstVisibleResultRow() {
    const header = document.querySelector(".header");
    const top = (header ? header.offsetHeight : 0) + 1;

    for (const row of document.querySelectorAll(
      "#results-body tr[data-result-page]",
    )) {
      const rect = row.getBoundingClientRect();
      if (rect.bottom > top && rect.top < window.innerHeight) {
        return row;
      }
    }

    return null;
  }

  function remeasureVirtualResults() {
    if (!virtualResults) return;

    const height = measureResultRowHeight();
    if (!height) return;

    virtualResults.rowHeight = height;
    applyVirtualSpacerRowHeight();
    resetVirtualSpacerHeights();
  }

  (() => {
    const dialog = document.getElementById("entry-modal");
    if (dialog && !dialog.open) dialog.showModal();
    syncModalState();
  })();

  syncHeaderHeight();
  syncFooterViewportState();

  document.addEventListener("focusin", (evt) => {
    if (!isEditableTarget(evt.target)) return;
    footerEditableFocused = true;
    scheduleFooterViewportStateSync();
  });

  document.addEventListener("focusout", (evt) => {
    if (!isEditableTarget(evt.target)) return;
    footerEditableFocused = false;
    scheduleFooterViewportStateSync();
    setTimeout(scheduleFooterViewportStateSync, 250);
  });

  if (window.visualViewport) {
    window.visualViewport.addEventListener(
      "resize",
      scheduleFooterViewportStateSync,
      { passive: true },
    );
  }

  const initialPage = currentPageFromUrl();
  initializeVirtualResults();
  scrollToResultPage(initialPage);
  scheduleVisiblePageSync();
  window.addEventListener(
    "scroll",
    () => {
      scheduleVisiblePageSync();
      scheduleVirtualLoad();
      scheduleFooterViewportStateSync();
    },
    { passive: true },
  );
  window.addEventListener("resize", () => {
    remeasureVirtualResults();
    scheduleVisiblePageSync();
    scheduleVirtualLoad();
    scheduleFooterViewportStateSync();
  });
  window.addEventListener(RECONCILE_EVENT, () => {
    setTimeout(() => {
      initializeVirtualResults();
      scheduleVisiblePageSync();
    }, 50);
  });
})();
