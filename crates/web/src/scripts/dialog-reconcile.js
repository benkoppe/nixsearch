(() => {
  const dialog = document.getElementById("entry-modal");

  if (dialog) {
    if (!dialog.open) dialog.showModal();
  } else {
    document.querySelectorAll("dialog[open]").forEach((d) => d.close());
  }

  if (window.nixsearchSyncModalState) {
    window.nixsearchSyncModalState();
  } else {
    const root = document.documentElement;
    if (dialog && dialog.open) {
      if (!root.classList.contains("modal-open")) {
        root.classList.toggle(
          "modal-scrollbar-gutter",
          window.innerWidth > root.clientWidth
        );
      }
      root.classList.add("modal-open");
    } else {
      root.classList.remove("modal-open");
      root.classList.remove("modal-scrollbar-gutter");
    }
  }

  if (!dialog && window.nixsearchRestoreResultFocus) {
    window.nixsearchRestoreResultFocus();
  }
})();
