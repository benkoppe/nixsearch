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

        function getSourceSelect() {
          return document.querySelector('[data-nixsearch-input="source-path"]');
        }

        function getRefSelect() {
          return document.querySelector('[data-nixsearch-input="ref"]');
        }

        function sourceMetadata(sourceId) {
          return metadata.find((s) => s.id === sourceId);
        }

        function populateRefSelect(sourceId) {
          const refSelect = getRefSelect();
          if (!refSelect) return;

          if (refSelect.type === "hidden") {
            const source = sourceMetadata(sourceId);
            if (!source || source.refs.length === 0) {
              refSelect.value = "";
              return;
            }

            const wrapper = refSelect.closest(".header-filters") || refSelect.parentElement;
            const newSelect = document.createElement("select");
            newSelect.name = "ref";
            newSelect.setAttribute("data-nixsearch-input", "ref");
            newSelect.innerHTML = source.refs
              .map((r) => {
                const selected = r === source.defaultRef ? " selected" : "";
                return `<option value="${r}"${selected}>${r}</option>`;
              })
              .join("");
            refSelect.replaceWith(newSelect);
            return;
          }

          const source = sourceMetadata(sourceId);

          if (!source || source.refs.length === 0) {
            refSelect.innerHTML = "";
            refSelect.style.display = "none";
            return;
          }

          refSelect.style.display = "";
          refSelect.innerHTML = source.refs
            .map((r) => {
              const selected = r === source.defaultRef ? " selected" : "";
              return `<option value="${r}"${selected}>${r}</option>`;
            })
            .join("");
        }

        function currentSourceFromSelect() {
          const sel = getSourceSelect();
          return sel ? sel.value : "";
        }

        function currentRefFromSelect() {
          const sel = getRefSelect();
          return sel ? sel.value : "";
        }

        function sourcePath(sourceId) {
          return sourceId ? "/" + encodeURIComponent(sourceId) : "/";
        }

        function buildSearchUrlFromInputs() {
          const sourceId = currentSourceFromSelect();
          const path = sourcePath(sourceId);
          const params = new URLSearchParams();

          const q = document.querySelector('[data-nixsearch-input="q"]');
          if (q && q.value.trim()) params.set("q", q.value.trim());

          if (sourceId) {
            const refValue = currentRefFromSelect();
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

          const sourceSelect = getSourceSelect();
          if (sourceSelect) sourceSelect.value = effectiveSource;

          populateRefSelect(effectiveSource);

          const refSelect = getRefSelect();
          if (refSelect && refSelect.type !== "hidden") {
            const refParam = params.get("ref") || "";
            if (refParam) {
              refSelect.value = refParam;
            }
          }

          const q = document.querySelector('[data-nixsearch-input="q"]');
          if (q) q.value = params.get("q") || "";
        }

        document.addEventListener("change", (evt) => {
          const el = evt.target;
          if (!el.matches) return;

          if (el.matches('[data-nixsearch-input="source-path"]')) {
            populateRefSelect(el.value);
            navigate(buildSearchUrlFromInputs());
            return;
          }

          if (el.matches('[data-nixsearch-input="ref"]')) {
            navigate(buildSearchUrlFromInputs());
            return;
          }
        });

        // Handle clicks on table rows
        document.addEventListener("click", (evt) => {
          if (evt.defaultPrevented) return;
          if (evt.button !== 0) return;
          if (evt.metaKey || evt.ctrlKey || evt.shiftKey || evt.altKey) return;

          // Check if click is on a table row with data-href
          const row = evt.target.closest("tr[data-href]");
          if (row) {
            const link = evt.target.closest("a[href]");
            if (!link) {
              // Click was on the row but not the link itself
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
          navigate(url.toString());
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
        document.addEventListener("input", (evt) => {
          const el = evt.target;
          if (!el.matches || !el.matches('[data-nixsearch-input="q"]')) return;
          clearTimeout(debounce);
          debounce = setTimeout(() => {
            navigate(buildSearchUrlFromInputs());
          }, 200);
        });

        document.addEventListener("submit", (evt) => {
          const form = evt.target;
          if (!(form instanceof HTMLFormElement)) return;
          if (form.method && form.method.toLowerCase() !== "get") return;

          evt.preventDefault();
          navigate(buildSearchUrlFromInputs());
        });

        window.addEventListener("popstate", () => {
          const previous = currentUrl;
          syncInputsFromUrl();
          reconcile(previous);
        });

        window.nixsearchNavigate = navigate;

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
          setTimeout(observeSentinel, 50);
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
