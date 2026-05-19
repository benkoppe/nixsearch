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

pub fn navigation_script() -> &'static str {
    r#"
      (() => {
        const RECONCILE_EVENT = "nix-search-reconcile";
        const metadata = JSON.parse(
          document.getElementById("source-metadata").textContent
        );

        function getSourceSelect() {
          return document.querySelector('[data-nix-search-input="source-path"]');
        }

        function getRefSelect() {
          return document.querySelector('[data-nix-search-input="ref"]');
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
            newSelect.setAttribute("data-nix-search-input", "ref");
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

          const q = document.querySelector('[data-nix-search-input="q"]');
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
          const current = window.location.pathname + window.location.search;

          if (push && current !== target) {
            history.pushState(null, "", target);
          }

          window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
        }

        function syncInputsFromUrl() {
          const params = new URLSearchParams(window.location.search);
          const parts = window.location.pathname.split("/").filter(Boolean);
          const pathSource = parts.length > 0 ? decodeURIComponent(parts[0]) : "";

          const sourceSelect = getSourceSelect();
          if (sourceSelect) sourceSelect.value = pathSource;

          populateRefSelect(pathSource);

          const refSelect = getRefSelect();
          if (refSelect && refSelect.type !== "hidden") {
            const refParam = params.get("ref") || "";
            if (refParam) {
              refSelect.value = refParam;
            }
          }

          const q = document.querySelector('[data-nix-search-input="q"]');
          if (q) q.value = params.get("q") || "";
        }

        document.addEventListener("change", (evt) => {
          const el = evt.target;
          if (!el.matches) return;

          if (el.matches('[data-nix-search-input="source-path"]')) {
            populateRefSelect(el.value);
            navigate(buildSearchUrlFromInputs());
            return;
          }

          if (el.matches('[data-nix-search-input="ref"]')) {
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
          if (!el.matches || !el.matches('[data-nix-search-input="q"]')) return;
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
          syncInputsFromUrl();
          window.dispatchEvent(new CustomEvent(RECONCILE_EVENT));
        });

        window.nixSearchNavigate = navigate;

        (() => {
          const dialog = document.getElementById("entry-modal");
          if (dialog && !dialog.open) dialog.showModal();
        })();
      })();
      "#
}
