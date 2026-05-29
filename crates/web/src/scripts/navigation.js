(() => {
  const RECONCILE_EVENT = "nixsearch-reconcile";
  const metadata = JSON.parse(
    document.getElementById("source-metadata").textContent,
  );
  const PAGE_SIZE = __DEFAULT_LIMIT__;
  let currentUrl = currentPublicUrl();

  if ("scrollRestoration" in history) {
    history.scrollRestoration = "manual";
  }

  function currentPublicUrl() {
    return window.location.pathname + window.location.search;
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
  let virtualLoading = false;

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
    const parts = parsed.pathname.split("/").filter(Boolean);
    const sourceAll = params.get("source") === "__SOURCE_ALL_VALUE__";
    const q = (params.get("q") || "").trim();
    const source = sourceAll
      ? ""
      : parts[0]
        ? decodeURIComponent(parts[0])
        : "";
    const ref = sourceAll ? "" : (params.get("ref") || "").trim();
    const refSet = sourceAll || !source ? (params.get("ref_set") || "").trim() : "";

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

  function defaultRefSet() {
    return metadata.defaultRefSet || "";
  }

  function populateRefRadios(sourceId) {
    const container = getRefContainer();
    if (!container) return;

    const source = sourceMetadata(sourceId);
    syncLogoAccent(sourceId);

    if (source) {
      container.style.setProperty("--source-color", source.color);
    } else {
      container.style.removeProperty("--source-color");
    }

    if (!sourceId) {
      if (!metadata.refSets || metadata.refSets.length === 0) {
        container.innerHTML = "";
        syncHeaderHeight();
        return;
      }

      container.innerHTML = metadata.refSets
        .map((r) => {
          const checked = r === defaultRefSet() ? " checked" : "";
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

    container.innerHTML = source.refs
      .map((r) => {
        const checked = r === source.defaultRef ? " checked" : "";
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

  function syncModalState() {
    const dialog = document.getElementById("entry-modal");

    document.documentElement.classList.toggle(
      "modal-open",
      !!dialog && dialog.open,
    );

    if (dialog && !dialog.dataset.modalStateBound) {
      dialog.dataset.modalStateBound = "true";
      dialog.addEventListener("close", syncModalState);
    }
  }

  window.nixsearchSyncModalState = syncModalState;

  function sourcePath(sourceId) {
    return sourceId ? "/" + encodeURIComponent(sourceId) : "/";
  }

  function buildSearchUrlFromInputs() {
    const sourceId = currentSourceFromTabs();
    const path = sourcePath(sourceId);
    const params = new URLSearchParams();

    const q = document.querySelector('[data-nixsearch-input="q"]');
    if (q && q.value.trim()) params.set("q", q.value.trim());

    if (sourceId) {
      const refValue = currentRefFromRadios();
      const source = sourceMetadata(sourceId);
      if (refValue && source && refValue !== source.defaultRef) {
        params.set("ref", refValue);
      }
    } else {
      const refSetValue = currentRefFromRadios();
      if (refSetValue && refSetValue !== defaultRefSet()) {
        params.set("ref_set", refSetValue);
      }
    }

    const qs = params.toString();
    return qs ? path + "?" + qs : path;
  }

  function selectSource(sourceId) {
    setActiveSourceTab(sourceId);
    populateRefRadios(sourceId);

    const dropdown = document.querySelector("[data-nixsearch-overflow-menu]");
    if (dropdown) dropdown.hidden = true;

    navigate(buildSearchUrlFromInputs());
  }

  function cycleSourceFilter(direction) {
    const sourceIds = sourceIdsFromTabs();
    if (sourceIds.length < 2) return false;

    const currentIndex = sourceIds.indexOf(currentSourceFromTabs());
    const startIndex = currentIndex >= 0 ? currentIndex : 0;
    const nextIndex =
      (startIndex + direction + sourceIds.length) % sourceIds.length;

    selectSource(sourceIds[nextIndex]);
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
      return;
    }

    const loadsResults = shouldLoadResults(current, target);

    if (push) {
      history.pushState(null, "", target);
    }

    if (syncInputs) {
      syncInputsFromUrl();
    }

    setLoading(loadsResults);
    if (loadsResults) {
      window.scrollTo(0, 0);
    }
    reconcile(current);
  }

  function syncInputsFromUrl() {
    const params = new URLSearchParams(window.location.search);
    const parts = window.location.pathname.split("/").filter(Boolean);
    const pathSource = parts.length > 0 ? decodeURIComponent(parts[0]) : "";
    const effectiveSource =
      params.get("source") === "__SOURCE_ALL_VALUE__" ? "" : pathSource;

    setActiveSourceTab(effectiveSource);
    populateRefRadios(effectiveSource);

    const refParam = effectiveSource
      ? params.get("ref") || ""
      : params.get("ref_set") || "";
    if (refParam) {
      const radio = document.querySelector(
        `[data-nixsearch-input="ref"][value="${CSS.escape(refParam)}"]`,
      );
      if (radio) radio.checked = true;
    }

    const q = document.querySelector('[data-nixsearch-input="q"]');
    if (q) q.value = params.get("q") || "";
  }

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
    navigate(buildSearchUrlFromInputs());
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
        const url = new URL(row.dataset.href, window.location.href);
        if (url.origin === window.location.origin) {
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
    navigate(url.toString(), { syncInputs: true });
  });

  document.addEventListener("click", (evt) => {
    const dialog = evt.target;
    if (!(dialog instanceof HTMLDialogElement)) return;
    if (dialog.id !== "entry-modal") return;

    const url = dialog.getAttribute("data-close-url");
    if (!url) return;

    evt.preventDefault();
    navigate(url);
  });

  let debounce;

  function clearPendingQueryNavigation() {
    clearTimeout(debounce);
    debounce = null;
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
        if (cycleSourceFilter(key === "n" ? 1 : -1)) evt.preventDefault();
        return;
      }
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
    debounce = setTimeout(() => {
      debounce = null;
      navigate(buildSearchUrlFromInputs());
    }, 75);
  });

  document.addEventListener("submit", (evt) => {
    const form = evt.target;
    if (!(form instanceof HTMLFormElement)) return;
    if (form.method && form.method.toLowerCase() !== "get") return;

    evt.preventDefault();
    clearPendingQueryNavigation();

    const q = form.querySelector('[data-nixsearch-input="q"]');
    if (q) q.blur();

    navigate(buildSearchUrlFromInputs());
  });

  window.addEventListener("popstate", () => {
    const previous = currentUrl;
    syncInputsFromUrl();
    setLoading(shouldLoadResults(previous, currentPublicUrl()));
    reconcile(previous);
  });

  window.nixsearchNavigate = navigate;

  const MORE_URL = "__MORE_RESULTS_URL__";

  function pageForOffset(offset) {
    return Math.floor(Math.max(0, offset) / PAGE_SIZE) + 1;
  }

  async function fetchMoreResults(offset, pageUrl = currentPublicUrl()) {
    let url = `${MORE_URL}?url=${encodeURIComponent(pageUrl)}&offset=${offset}`;
    const res = await fetch(url);
    return await res.json();
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
    if (!virtualResults || virtualLoading) return;

    const targetOffset = virtualOffsetAtViewport();
    const { startOffset, endOffset, total } = virtualResults;

    if (targetOffset < startOffset) {
      if (startOffset - targetOffset <= PAGE_SIZE) {
        await loadVirtualPage(Math.max(0, startOffset - PAGE_SIZE), "prepend");
        return;
      }

      await loadVirtualPage(
        Math.floor(targetOffset / PAGE_SIZE) * PAGE_SIZE,
        "replace",
      );
      return;
    }

    if (targetOffset >= endOffset) {
      if (targetOffset - endOffset <= PAGE_SIZE) {
        await loadVirtualPage(endOffset, "append");
        return;
      }

      await loadVirtualPage(
        Math.floor(targetOffset / PAGE_SIZE) * PAGE_SIZE,
        "replace",
      );
      return;
    }

    const margin = PAGE_SIZE * 2;
    if (targetOffset < startOffset + margin && startOffset > 0) {
      await loadVirtualPage(Math.max(0, startOffset - PAGE_SIZE), "prepend");
      return;
    }

    if (targetOffset >= endOffset - margin && endOffset < total) {
      await loadVirtualPage(endOffset, "append");
    }
  }

  async function loadVirtualPage(offset, mode) {
    if (!virtualResults || virtualLoading) return;

    virtualLoading = true;
    const state = virtualResults;
    const requestUrl = state.requestUrl;
    const normalizedOffset = Math.max(
      0,
      Math.min(offset, Math.max(0, state.total - 1)),
    );
    const anchor = firstVisibleResultRow();

    if (mode === "replace") {
      runVirtualTransaction("bottom", anchor, () => {
        state.tbody
          .querySelectorAll("tr[data-result-page]")
          .forEach((row) => row.remove());
        state.startOffset = normalizedOffset;
        state.endOffset = normalizedOffset;
        state.topSpacerHeight = normalizedOffset * state.rowHeight;
        state.bottomSpacerHeight =
          (state.total - normalizedOffset) * state.rowHeight;
        applyVirtualSpacers();
      });
    }

    setVirtualSpacerLoading(mode, true);

    try {
      const data = await fetchMoreResults(normalizedOffset, requestUrl);

      if (!virtualResults || virtualResults.requestUrl !== requestUrl) return;

      if (data.error) {
        console.error("Load virtual results failed:", data.error);
        return;
      }

      if (!data.rows) return;

      if (typeof data.total === "number") {
        state.total = data.total;
      }

      const spacer = mode === "prepend" ? "top" : "bottom";
      let insertedRows = [];

      runVirtualTransaction(spacer, anchor, () => {
        if (mode === "append") {
          state.tbody.insertAdjacentHTML("beforeend", data.rows);
        } else if (mode === "prepend") {
          state.tbody.insertAdjacentHTML("afterbegin", data.rows);
        } else {
          state.tbody
            .querySelectorAll("tr[data-result-page]")
            .forEach((row) => row.remove());
          state.tbody.insertAdjacentHTML("afterbegin", data.rows);
        }

        insertedRows = rowsForOffset(normalizedOffset);
        if (insertedRows.length === 0) return;

        state.startOffset =
          mode === "append" ? state.startOffset : normalizedOffset;
        state.endOffset =
          mode === "prepend"
            ? state.endOffset
            : Math.min(state.total, normalizedOffset + insertedRows.length);
      });

      if (insertedRows.length === 0) return;

      scheduleVisiblePageSync();
    } catch (e) {
      console.error("Failed to load virtual results:", e);
    } finally {
      setVirtualSpacerLoading(mode, false);
      virtualLoading = false;
      scheduleVirtualLoad();
    }
  }

  function rowsForOffset(offset) {
    const page = pageForOffset(offset);
    return Array.from(
      document.querySelectorAll(
        `#results-body tr[data-result-page="${CSS.escape(String(page))}"]`,
      ),
    );
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

  const initialPage = currentPageFromUrl();
  initializeVirtualResults();
  scrollToResultPage(initialPage);
  scheduleVisiblePageSync();
  window.addEventListener(
    "scroll",
    () => {
      scheduleVisiblePageSync();
      scheduleVirtualLoad();
    },
    { passive: true },
  );
  window.addEventListener("resize", () => {
    remeasureVirtualResults();
    scheduleVisiblePageSync();
    scheduleVirtualLoad();
  });
  window.addEventListener(RECONCILE_EVENT, () => {
    setTimeout(() => {
      initializeVirtualResults();
      scheduleVisiblePageSync();
    }, 50);
  });
})();
