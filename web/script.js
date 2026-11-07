/* ============================================================================
   FileOptimize — client
   No framework, no build step: ~200 lines of progressive-enhancement JS that
   talks to POST /api/process and renders the result.
   ========================================================================== */
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);

  const el = {
    root: document.documentElement,
    theme: $("theme-toggle"),
    form: $("upload-form"),
    drop: $("dropzone"),
    input: $("file-input"),
    fileInfo: $("file-info"),
    fileName: $("file-name"),
    fileSize: $("file-size"),
    fileClear: $("file-clear"),
    pills: Array.from(document.querySelectorAll(".pill")),
    quality: $("quality-options"),
    slider: $("quality-slider"),
    qualityValue: $("quality-value"),
    submit: $("execute-btn"),
    submitLabel: $("execute-btn").querySelector(".btn-label"),
    spinner: $("execute-btn").querySelector(".btn-spinner"),
    reset: $("reset-btn"),
    progress: $("progress"),
    fill: $("progress-fill"),
    progressText: $("progress-text"),
    result: $("result"),
    badge: $("result-badge"),
    resultName: $("result-filename"),
    statOriginal: $("stat-original"),
    statOptimized: $("stat-optimized"),
    statSavings: $("stat-savings"),
    download: $("download-link"),
    fbBtn: $("feedback-btn"),
    fbModal: $("feedback-modal"),
    fbClose: $("feedback-close"),
    fbCancel: $("feedback-cancel"),
    fbForm: $("feedback-form"),
    fbText: $("feedback-text"),
    fbEmail: $("feedback-email"),
    toast: $("toast"),
    hint: document.querySelector(".dropzone-hint"),
  };

  const state = { action: "compress", file: null, objectUrl: null, xhr: null, info: null };

  const IMGS = ["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

  /* ------------------------------------------------------------------ theme */
  const stored = (() => {
    try { return localStorage.getItem("fo-theme"); } catch { return null; }
  })();
  const prefersLight = window.matchMedia("(prefers-color-scheme: light)").matches;
  el.root.dataset.theme = stored || (prefersLight ? "light" : "dark");

  el.theme.addEventListener("click", () => {
    el.root.dataset.theme = el.root.dataset.theme === "dark" ? "light" : "dark";
    try { localStorage.setItem("fo-theme", el.root.dataset.theme); } catch { /* private mode */ }
  });

  /* ------------------------------------------------------------------- info */
  fetch("/api/info")
    .then((r) => (r.ok ? r.json() : null))
    .then((info) => {
      if (!info) return;
      state.info = info;
      const mb = (info.max_upload_size / 1024 / 1024).toFixed(0);
      const missing = [];
      if (!info.tools.ghostscript) missing.push("PDF compress");
      if (!info.tools.poppler) missing.push("PDF→image");
      el.hint.textContent = missing.length
        ? `images & PDFs · up to ${mb} MB · ${missing.join(" & ")} unavailable on this host`
        : `images & PDFs · up to ${mb} MB`;
      if (info.default_quality) setQuality(info.default_quality);
    })
    .catch(() => { /* hint stays generic if /api/info is unreachable */ });

  /* ------------------------------------------------------------------ setup */
  el.pills.forEach((p) =>
    p.addEventListener("click", () => {
      state.action = p.dataset.action;
      el.pills.forEach((o) => {
        const on = o === p;
        o.classList.toggle("active", on);
        o.setAttribute("aria-checked", String(on));
      });
      el.quality.classList.toggle("hidden", state.action !== "compress");
      validate();
    })
  );

  function setQuality(v) {
    el.slider.value = String(v);
    el.qualityValue.textContent = `${v}%`;
    el.slider.style.setProperty("--fill", `${v}%`);
  }
  el.slider.addEventListener("input", () => setQuality(Number(el.slider.value)));
  setQuality(Number(el.slider.value));

  el.input.addEventListener("change", () => adopt(el.input.files[0]));

  ["dragenter", "dragover"].forEach((ev) =>
    el.drop.addEventListener(ev, (e) => { e.preventDefault(); el.drop.classList.add("dragover"); })
  );
  ["dragleave", "drop"].forEach((ev) =>
    el.drop.addEventListener(ev, (e) => { e.preventDefault(); el.drop.classList.remove("dragover"); })
  );
  el.drop.addEventListener("drop", (e) => adopt(e.dataTransfer.files[0]));
  el.drop.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") { e.preventDefault(); el.input.click(); }
  });
  el.fileClear.addEventListener("click", () => adopt(null));

  function adopt(file) {
    state.file = file || null;
    const has = Boolean(file);
    el.fileInfo.classList.toggle("hidden", !has);
    el.drop.classList.toggle("hidden", has);
    if (has) {
      el.fileName.textContent = file.name;
      el.fileSize.textContent = human(file.size);
    } else {
      el.input.value = "";
    }
    validate();
  }

  /* ------------------------------------------------------------- validation */
  function extOf(name) { return (name.split(".").pop() || "").toLowerCase(); }
  function isImage(name) { return IMGS.includes(extOf(name)); }
  function isPdf(name) { return extOf(name) === "pdf"; }

  function problem() {
    const f = state.file;
    if (!f) return "Choose a file first.";
    const max = state.info ? state.info.max_upload_size : 50 * 1024 * 1024;
    if (f.size === 0) return "That file is empty.";
    if (f.size > max) return `Too large — the limit is ${human(max)}.`;
    if (state.action === "compress" && !isImage(f.name) && !isPdf(f.name))
      return "Compress needs an image or a PDF.";
    if (state.action === "image_to_pdf" && !isImage(f.name))
      return "Image → PDF needs an image file.";
    if (state.action === "pdf_to_image" && !isPdf(f.name))
      return "PDF → Image needs a PDF file.";
    return null;
  }

  function validate() {
    const why = state.file ? problem() : null;
    el.submit.disabled = !state.file || Boolean(why);
    if (why && state.file) toast(why);
    return !why && state.file;
  }

  /* ---------------------------------------------------------------- submit */
  el.form.addEventListener("submit", (e) => {
    e.preventDefault();
    if (!validate()) return;

    const data = new FormData();
    data.append("action", state.action);
    data.append("file", state.file, state.file.name);
    if (state.action === "compress") data.append("quality", String(el.slider.value));

    setBusy(true);
    const xhr = new XMLHttpRequest();
    state.xhr = xhr;
    xhr.open("POST", "/api/process");
    xhr.responseType = "blob";
    xhr.onprogress = (event) => {
      if (event.lengthComputable) {
        const pct = Math.round((event.loaded / event.total) * 100);
        el.fill.classList.remove("indeterminate");
        el.fill.style.width = `${pct}%`;
        el.progressText.textContent = pct < 100 ? `Uploading… ${pct}%` : "Processing…";
      }
    };
    xhr.upload.onprogress = (event) => {
      if (!event.lengthComputable) return;
      const pct = Math.round((event.loaded / event.total) * 100);
      el.fill.classList.remove("indeterminate");
      el.fill.style.width = `${Math.min(pct, 99)}%`;
      el.progressText.textContent = pct < 100 ? `Uploading… ${pct}%` : "Processing…";
    };
    xhr.onload = async () => {
      setBusy(false);
      state.xhr = null;
      if (xhr.status >= 200 && xhr.status < 300) return finish(xhr);
      let msg = `Something went wrong (${xhr.status}).`;
      try { msg = JSON.parse(await xhr.response.text()).error || msg; } catch { /* non-JSON body */ }
      toast(msg, true);
    };
    xhr.onerror = () => { setBusy(false); state.xhr = null; toast("Network error — the upload did not complete.", true); };
    xhr.onabort = () => { setBusy(false); state.xhr = null; toast("Upload cancelled."); };
    xhr.send(data);
  });

  function finish(xhr) {
    if (state.objectUrl) URL.revokeObjectURL(state.objectUrl);
    const blob = xhr.response;
    state.objectUrl = URL.createObjectURL(blob);

    const name = header(xhr, "content-disposition")?.match(/filename="?([^";]+)"?/)?.[1]
      || `${state.file.name.replace(/\.[^.]+$/, "")}_result`;
    const original = Number(header(xhr, "x-original-size") || blob.size) || blob.size;
    const optimized = Number(header(xhr, "x-output-size") || blob.size) || blob.size;

    el.resultName.textContent = name;
    el.statOriginal.textContent = human(original);
    el.statOptimized.textContent = human(optimized);

    const delta = original > 0 ? Math.round(((original - optimized) / original) * 100) : 0;
    if (delta > 0) {
      el.badge.textContent = `−${delta}%`;
      el.statSavings.textContent = `${human(original - optimized)} saved`;
    } else if (delta < 0) {
      el.badge.textContent = "converted";
      el.statSavings.textContent = `+${human(optimized - original)}`;
    } else {
      el.badge.textContent = "ready";
      el.statSavings.textContent = "unchanged";
    }

    el.download.href = state.objectUrl;
    el.download.download = name;
    el.result.classList.remove("hidden");
    el.form.classList.add("hidden");
    el.reset.classList.remove("hidden");

    // Auto-save the result once, then leave the choice to the user.
    const a = document.createElement("a");
    a.href = state.objectUrl;
    a.download = name;
    a.click();
  }

  el.download.addEventListener("click", (e) => { e.stopPropagation(); });

  el.reset.addEventListener("click", () => {
    if (state.xhr) state.xhr.abort();
    if (state.objectUrl) { URL.revokeObjectURL(state.objectUrl); state.objectUrl = null; }
    adopt(null);
    el.result.classList.add("hidden");
    el.form.classList.remove("hidden");
    el.progress.classList.add("hidden");
    el.reset.classList.add("hidden");
    el.fill.style.width = "0%";
  });

  function setBusy(busy) {
    el.submit.disabled = busy;
    el.spinner.classList.toggle("hidden", !busy);
    el.submitLabel.textContent = busy ? "Working…" : "Execute";
    el.progress.classList.toggle("hidden", !busy);
    el.fill.style.width = busy ? "2%" : "0%";
    if (busy) el.fill.classList.add("indeterminate");
    else el.fill.classList.remove("indeterminate");
  }

  /* --------------------------------------------------------------- feedback */
  const openModal = () => { el.fbModal.classList.remove("hidden"); el.fbText.focus(); };
  const closeModal = () => el.fbModal.classList.add("hidden");
  el.fbBtn.addEventListener("click", openModal);
  el.fbClose.addEventListener("click", closeModal);
  el.fbCancel.addEventListener("click", closeModal);
  el.fbModal.addEventListener("click", (e) => { if (e.target === el.fbModal) closeModal(); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") closeModal(); });

  el.fbForm.addEventListener("submit", (e) => {
    e.preventDefault();
    const body = el.fbText.value.trim();
    if (!body) { toast("Write a note first.", true); return; }
    const subject = encodeURIComponent("[FileOptimize] feedback");
    const from = el.fbEmail.value.trim() ? `\n\n— ${el.fbEmail.value.trim()}` : "";
    window.location.href = `mailto:hello@fileoptimize.com?subject=${subject}&body=${encodeURIComponent(body + from)}`;
    el.fbText.value = "";
    closeModal();
    toast("Thanks — your feedback is ready to send.");
  });

  /* ------------------------------------------------------------------ utils */
  let toastTimer = 0;
  function toast(msg, isError = false) {
    clearTimeout(toastTimer);
    el.toast.textContent = msg;
    el.toast.classList.toggle("error", Boolean(isError));
    el.toast.classList.remove("hidden");
    requestAnimationFrame(() => el.toast.classList.add("show"));
    toastTimer = setTimeout(() => {
      el.toast.classList.remove("show");
      setTimeout(() => el.toast.classList.add("hidden"), 260);
    }, 3400);
  }

  function header(xhr, name) {
    const v = xhr.getResponseHeader(name);
    return v === "" ? null : v;
  }

  function human(bytes) {
    const n = Number(bytes);
    if (!Number.isFinite(n)) return "—";
    const units = ["B", "KB", "MB", "GB"];
    let i = 0;
    let v = n;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
    return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
  }
})();
