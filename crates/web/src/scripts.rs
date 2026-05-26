use crate::MORE_RESULTS_URL;
use crate::request::LinkOrigin;

pub fn dialog_reconcile_script() -> &'static str {
    r#"
      (() => {
        const dialog = document.getElementById("entry-modal");

        if (dialog) {
          if (!dialog.open) dialog.showModal();
        } else {
          document.querySelectorAll("dialog[open]").forEach((d) => d.close());
        }
      })();
      "#
}

pub fn navigation_script() -> String {
    r##"
      (() => {
        const RECONCILE_EVENT = "nixsearch-reconcile";
        const metadata = JSON.parse(
          document.getElementById("source-metadata").textContent
        );
        let currentUrl = currentPublicUrl();

        function currentPublicUrl() {
          return window.location.pathname + window.location.search;
        }

        function reconcile(previousUrl) {
          window.nixsearchPreviousUrl = previousUrl || "";
          window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
          currentUrl = currentPublicUrl();
        }

        // ─── Source tabs ───

        function getActiveSourceTab() {
          return document.querySelector('.source-tab[data-active]');
        }

        function currentSourceFromTabs() {
          const tab = getActiveSourceTab();
          return tab ? (tab.dataset.nixsearchSource || "") : "";
        }

        function setActiveSourceTab(sourceId) {
          document.querySelectorAll('.source-tab').forEach((tab) => {
            if (tab.dataset.nixsearchSource === sourceId) {
              tab.setAttribute("data-active", "");
            } else {
              tab.removeAttribute("data-active");
            }
          });
          // Also update overflow dropdown items
          document.querySelectorAll('.source-tabs-dropdown button').forEach((btn) => {
            if (btn.dataset.nixsearchSource === sourceId) {
              btn.setAttribute("data-active", "");
            } else {
              btn.removeAttribute("data-active");
            }
          });
        }

        // ─── Ref radios ───

        function getRefContainer() {
          return document.querySelector('[data-nixsearch-ref-container]');
        }

        function currentRefFromRadios() {
          const checked = document.querySelector('[data-nixsearch-input="ref"]:checked');
          return checked ? checked.value : "";
        }

        function sourceMetadata(sourceId) {
          return metadata.find((s) => s.id === sourceId);
        }

        function populateRefRadios(sourceId) {
          const container = getRefContainer();
          if (!container) return;

          const source = sourceMetadata(sourceId);

          if (source) {
            container.style.setProperty("--source-color", source.color);
          } else {
            container.style.removeProperty("--source-color");
          }

          if (!source || source.refs.length === 0) {
            container.innerHTML = "";
            return;
          }

          container.innerHTML = source.refs
            .map((r) => {
              const checked = r === source.defaultRef ? " checked" : "";
              return `<label class="ref-radio-label"><input type="radio" name="ref" value="${r}"${checked} data-nixsearch-input="ref"><span>${r}</span></label>`;
            })
            .join("");
        }

        // ─── URL building ───

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
          }

          const qs = params.toString();
          return qs ? path + "?" + qs : path;
        }

        function navigate(url, { push = true } = {}) {
          const next = new URL(url, window.location.href);
          const target = next.pathname + next.search;
          const current = currentPublicUrl();

          if (push && current !== target) {
            history.pushState(null, "", target);
          }

          reconcile(current);
        }

        function syncInputsFromUrl() {
          const params = new URLSearchParams(window.location.search);
          const parts = window.location.pathname.split("/").filter(Boolean);
          const pathSource = parts.length > 0 ? decodeURIComponent(parts[0]) : "";
          const effectiveSource = params.get("source") === "__SOURCE_ALL_VALUE__" ? "" : pathSource;

          setActiveSourceTab(effectiveSource);
          populateRefRadios(effectiveSource);

          const refParam = params.get("ref") || "";
          if (refParam) {
            const radio = document.querySelector(`[data-nixsearch-input="ref"][value="${CSS.escape(refParam)}"]`);
            if (radio) radio.checked = true;
          }

          const q = document.querySelector('[data-nixsearch-input="q"]');
          if (q) q.value = params.get("q") || "";
        }

        // ─── Tab click handler ───

        document.addEventListener("click", (evt) => {
          const tab = evt.target.closest('.source-tab, .source-tabs-dropdown button');
          if (!tab) return;
          if (!tab.hasAttribute("data-nixsearch-source")) return;

          evt.preventDefault();
          const sourceId = tab.dataset.nixsearchSource || "";
          setActiveSourceTab(sourceId);
          populateRefRadios(sourceId);

          // Close dropdown if it was open
          const dropdown = document.querySelector('[data-nixsearch-overflow-menu]');
          if (dropdown) dropdown.hidden = true;

          navigate(buildSearchUrlFromInputs());
        });

        // ─── Ref radio change handler ───

        document.addEventListener("change", (evt) => {
          const el = evt.target;
          if (!el.matches || !el.matches('[data-nixsearch-input="ref"]')) return;
          navigate(buildSearchUrlFromInputs());
        });

        // ─── Row clicks ───

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

        // ─── Internal link clicks ───

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
          navigate(url.toString());
        });

        // ─── Modal backdrop click ───

        document.addEventListener("click", (evt) => {
          const dialog = evt.target;
          if (!(dialog instanceof HTMLDialogElement)) return;
          if (dialog.id !== "entry-modal") return;

          const url = dialog.getAttribute("data-close-url");
          if (!url) return;

          evt.preventDefault();
          navigate(url);
        });

        // ─── Search input debounce ───

        let debounce;
        document.addEventListener("input", (evt) => {
          const el = evt.target;
          if (!el.matches || !el.matches('[data-nixsearch-input="q"]')) return;
          clearTimeout(debounce);
          debounce = setTimeout(() => {
            navigate(buildSearchUrlFromInputs());
          }, 75);
        });

        // ─── Form submit ───

        document.addEventListener("submit", (evt) => {
          const form = evt.target;
          if (!(form instanceof HTMLFormElement)) return;
          if (form.method && form.method.toLowerCase() !== "get") return;

          evt.preventDefault();
          navigate(buildSearchUrlFromInputs());
        });

        // ─── Popstate ───

        window.addEventListener("popstate", () => {
          const previous = currentUrl;
          syncInputsFromUrl();
          reconcile(previous);
        });

        window.nixsearchNavigate = navigate;

        // ─── Source tabs overflow detection ───

        function computeTabOverflow() {
          const tabsNav = document.querySelector('.source-tabs');
          const overflowSelect = document.querySelector('[data-nixsearch-overflow-select]');
          if (!tabsNav || !overflowSelect) return;

          const tabs = Array.from(tabsNav.querySelectorAll('.source-tab'));
          if (tabs.length === 0) return;

          // Reset: show all tabs, hide overflow select
          tabs.forEach((tab) => { tab.style.display = ""; });
          overflowSelect.hidden = true;
          overflowSelect.innerHTML = "";

          // Check if tabs actually overflow the container
          if (tabsNav.scrollWidth <= tabsNav.clientWidth) return;

          // Tabs overflow — figure out which ones fit.
          // Show the select first so we can account for its width.
          overflowSelect.hidden = false;
          const selectWidth = overflowSelect.offsetWidth + 8; // 8px breathing room
          const availableWidth = tabsNav.clientWidth - selectWidth;

          let totalWidth = 0;
          let overflowIndex = -1;

          for (let i = 0; i < tabs.length; i++) {
            totalWidth += tabs[i].offsetWidth + 2; // 2px gap
            if (totalWidth > availableWidth && i > 0) {
              overflowIndex = i;
              break;
            }
          }

          if (overflowIndex === -1) {
            // Everything fits after all (edge case)
            overflowSelect.hidden = true;
            return;
          }

          // Hide overflowing tabs and populate native select
          const overflowTabs = tabs.slice(overflowIndex);
          overflowTabs.forEach((tab) => { tab.style.display = "none"; });

          const activeSource = currentSourceFromTabs();
          let hasActiveInOverflow = false;

          overflowSelect.innerHTML = '<option value="" disabled selected>More…</option>' +
            overflowTabs
              .map((tab) => {
                const sourceId = tab.dataset.nixsearchSource || "";
                const label = tab.textContent.trim();
                const isActive = tab.hasAttribute("data-active");
                if (isActive) hasActiveInOverflow = true;
                return `<option value="${sourceId}"${isActive ? " selected" : ""}>${label}</option>`;
              })
              .join("");

          // If active source is in overflow, select it
          if (hasActiveInOverflow) {
            overflowSelect.value = activeSource;
          }
        }

        // ─── Overflow select change handler ───

        document.addEventListener("change", (evt) => {
          const el = evt.target;
          if (!el.matches || !el.matches('[data-nixsearch-overflow-select]')) return;

          const sourceId = el.value;
          setActiveSourceTab(sourceId);
          populateRefRadios(sourceId);
          navigate(buildSearchUrlFromInputs());

          // Reset the select label after navigation
          setTimeout(computeTabOverflow, 60);
        });

        // Run overflow detection on load and resize
        computeTabOverflow();
        let resizeRaf;
        window.addEventListener("resize", () => {
          cancelAnimationFrame(resizeRaf);
          resizeRaf = requestAnimationFrame(computeTabOverflow);
        });

        // ─── Infinite scroll ───
        const MORE_URL = "__MORE_RESULTS_URL__";
        let loadingMore = false;
        let loadMoreObserver = null;

        function observeSentinel() {
          if (loadMoreObserver) {
            loadMoreObserver.disconnect();
            loadMoreObserver = null;
          }

          const sentinel = document.querySelector("#load-more-sentinel .load-more-trigger");
          if (!sentinel) return;

          loadMoreObserver = new IntersectionObserver((entries) => {
            for (const entry of entries) {
              if (entry.isIntersecting && !loadingMore) {
                loadMoreObserver.disconnect();
                loadMoreObserver = null;
                loadMore(entry.target);
              }
            }
          }, { rootMargin: "200px" });

          loadMoreObserver.observe(sentinel);
        }

        async function loadMore(trigger) {
          const offset = trigger.dataset.offset;
          if (!offset) return;

          loadingMore = true;
          const pageUrl = location.pathname + location.search;
          const url = `${MORE_URL}?url=${encodeURIComponent(pageUrl)}&offset=${offset}`;

          try {
            const res = await fetch(url);
            const data = await res.json();

            if (data.error) {
              console.error("Load more failed:", data.error);
              return;
            }

            // Preserve scroll position during DOM mutations
            const scrollY = window.scrollY;

            const tbody = document.getElementById("results-body");
            if (tbody && data.rows) {
              tbody.insertAdjacentHTML("beforeend", data.rows);
            }

            const sentinelEl = document.getElementById("load-more-sentinel");
            if (sentinelEl) {
              if (data.sentinel) {
                sentinelEl.outerHTML = data.sentinel;
              } else {
                sentinelEl.remove();
              }
            }

            window.scrollTo(0, scrollY);
          } catch (e) {
            console.error("Failed to load more results:", e);
          } finally {
            loadingMore = false;
            observeSentinel();
          }
        }

        // Start observing on load and after each reconcile
        observeSentinel();
        window.addEventListener(RECONCILE_EVENT, () => {
          setTimeout(() => {
            observeSentinel();
            computeTabOverflow();
          }, 50);
        });

        (() => {
          const dialog = document.getElementById("entry-modal");
          if (dialog && !dialog.open) dialog.showModal();
        })();
      })();
      "##
    .replace("__MORE_RESULTS_URL__", MORE_RESULTS_URL)
    .replace("__SOURCE_ALL_VALUE__", LinkOrigin::All.as_str())
}
