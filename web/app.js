const $ = (id) => document.getElementById(id);
const stage = $("stage");

const fmt = (n) => {
  if (n >= 1048576) return (n / 1048576).toFixed(n >= 104857600 ? 0 : 1) + " MB";
  if (n >= 1024) return Math.round(n / 1024) + " KB";
  return n + " B";
};
const setState = (s) => (stage.dataset.state = s);

// ---- worker lifecycle ----------------------------------------------------
// One worker, one job at a time. Cancelling (or a settings change while a
// job runs) terminates it and starts a fresh one: wasm cannot be interrupted.
let worker = null;
let engineReady = false;
let jobId = 0;
let running = null; // { id, name, size, t0, timer }

function spawn() {
  engineReady = false;
  worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
  worker.onmessage = ({ data }) => {
    if ("ready" in data) {
      engineReady = data.ready;
      $("engine").textContent = data.ready
        ? "Processed on this device — nothing is uploaded."
        : "Couldn't load the engine: " + data.error;
      if (data.ready && pendingRun) {
        pendingRun = false;
        run();
      }
      return;
    }
    if (!running || data.id !== running.id) return;
    finish(data);
  };
  worker.onerror = (e) => {
    if (running) {
      stopTimer();
      showError(running.name, "The engine stopped unexpectedly" + (e.message ? `: ${e.message}` : "") + ". The file may be too large for this browser.");
      running = null;
    }
    restart();
  };
}
function restart() {
  worker?.terminate();
  spawn();
}
spawn();

// ---- input --------------------------------------------------------------
let file = null; // { name, bytes: ArrayBuffer }
let pendingRun = false;

async function takeFile(f) {
  if (!f) return;
  const looksPdf = f.type === "application/pdf" || /\.pdf$/i.test(f.name);
  if (!looksPdf) {
    showError(f.name, "That doesn't look like a PDF.");
    return;
  }
  file = { name: f.name, bytes: await f.arrayBuffer() };
  run();
}

const drop = $("drop");
$("file").addEventListener("change", (e) => {
  takeFile(e.target.files[0]);
  e.target.value = "";
});
// Accept drops anywhere on the page, highlighting the drop zone.
let dragDepth = 0;
addEventListener("dragenter", (e) => {
  if (![...e.dataTransfer.types].includes("Files")) return;
  dragDepth++;
  drop.classList.add("over");
  if (stage.dataset.state !== "working") setState("idle");
});
addEventListener("dragleave", () => {
  if (--dragDepth <= 0) {
    dragDepth = 0;
    drop.classList.remove("over");
  }
});
addEventListener("dragover", (e) => e.preventDefault());
addEventListener("drop", (e) => {
  e.preventDefault();
  dragDepth = 0;
  drop.classList.remove("over");
  takeFile(e.dataTransfer.files[0]);
});

// ---- settings -----------------------------------------------------------
const settings = () => ({
  dpi: Number($("dpi").value),
  quality: Number($("quality").value),
  subsetOutlines: $("subsetOutlines").checked,
  stripJpegMetadata: $("stripMeta").checked,
});
let rerunTimer = 0;
function settingsChanged() {
  $("dpiOut").textContent = $("dpi").value + " dpi";
  $("qOut").textContent = $("quality").value;
  if (!file) return;
  clearTimeout(rerunTimer);
  rerunTimer = setTimeout(run, 350);
}
for (const id of ["dpi", "quality"]) $(id).addEventListener("input", settingsChanged);
for (const id of ["subsetOutlines", "stripMeta"]) $(id).addEventListener("change", settingsChanged);

// ---- running ------------------------------------------------------------
function run() {
  if (!file) return;
  if (running) {
    // Settings changed mid-job: abandon it.
    stopTimer();
    running = null;
    restart();
  }
  $("workName").textContent = file.name;
  setState("working");
  if (!engineReady) {
    pendingRun = true;
    return;
  }
  const id = ++jobId;
  const t0 = performance.now();
  running = {
    id, name: file.name, size: file.bytes.byteLength, t0,
    timer: setInterval(() => ($("elapsed").textContent = ((performance.now() - t0) / 1000).toFixed(1)), 100),
  };
  $("elapsed").textContent = "0.0";
  // Copy: the original stays here so settings can be changed and re-run.
  const input = file.bytes.slice(0);
  worker.postMessage({ id, input, ...settings() }, [input]);
}
function stopTimer() {
  if (running) clearInterval(running.timer);
}

function finish(res) {
  const job = running;
  stopTimer();
  running = null;
  if (!res.ok) {
    showError(job.name, friendlyError(res.error));
    return;
  }
  const before = job.size;
  let out = res.output;
  let after = out.byteLength;
  // Never hand back something bigger.
  const kept = after >= before;
  if (kept) {
    out = file.bytes;
    after = before;
  }
  const saved = 1 - after / before;
  $("doneName").textContent = job.name;
  $("pct").textContent = kept ? "0%" : "−" + (saved * 100).toFixed(saved >= 0.1 ? 0 : 1) + "%";
  $("pctLabel").textContent = kept ? "already lean — original kept" : "smaller";
  $("sizeBefore").textContent = fmt(before);
  $("sizeAfter").textContent = fmt(after);
  $("barBefore").style.width = "0";
  $("barAfter").style.width = "0";
  requestAnimationFrame(() => requestAnimationFrame(() => {
    $("barBefore").style.width = "100%";
    $("barAfter").style.width = Math.max(1, (after / before) * 100) + "%";
  }));
  const imgs = res.imagesTotal
    ? `${res.imagesRecompressed} of ${res.imagesTotal} image${res.imagesTotal === 1 ? "" : "s"} recompressed`
    : "no images";
  $("meta").textContent = `${imgs} · ${(res.ms / 1000).toFixed(1)} s`;
  $("log").textContent = res.log || "(nothing to report)";

  const a = $("download");
  if (a.href) URL.revokeObjectURL(a.href);
  a.href = URL.createObjectURL(new Blob([out], { type: "application/pdf" }));
  a.download = job.name.replace(/\.pdf$/i, "") + (kept ? ".pdf" : ".diet.pdf");
  setState("done");
}

function friendlyError(msg) {
  if (/encrypt|password|protect/i.test(msg)) return "This PDF is password-protected or encrypted, so it was left alone.";
  if (/signature|signed/i.test(msg)) return "This PDF is digitally signed; changing it would invalidate the signature, so it was left alone.";
  return msg ? "Couldn't process this PDF: " + msg : "Something went wrong.";
}
function showError(name, msg) {
  $("errName").textContent = name;
  $("errMsg").textContent = msg;
  setState("error");
}

$("cancel").addEventListener("click", () => {
  stopTimer();
  running = null;
  pendingRun = false;
  file = null;
  restart();
  setState("idle");
});
for (const id of ["again", "errAgain"]) {
  $(id).addEventListener("click", () => {
    file = null;
    setState("idle");
    $("file").click();
  });
}

// ---- code tabs ----------------------------------------------------------
const tabs = [...document.querySelectorAll('.tabs [role="tab"]')];
function select(tab) {
  for (const t of tabs) {
    const on = t === tab;
    t.setAttribute("aria-selected", on);
    t.tabIndex = on ? 0 : -1;
    $(t.getAttribute("aria-controls")).hidden = !on;
  }
}
tabs.forEach((t, i) => {
  t.addEventListener("click", () => select(t));
  t.addEventListener("keydown", (e) => {
    const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
    if (!d) return;
    const next = tabs[(i + d + tabs.length) % tabs.length];
    select(next);
    next.focus();
  });
});
$("copy").addEventListener("click", async () => {
  const panel = document.querySelector('.code-card [role="tabpanel"]:not([hidden])');
  try {
    await navigator.clipboard.writeText(panel.textContent);
    $("copy").textContent = "Copied";
  } catch {
    $("copy").textContent = "Select & copy";
  }
  setTimeout(() => ($("copy").textContent = "Copy"), 1500);
});

// ---- theme --------------------------------------------------------------
// Follows the OS until the button is used; the choice is remembered.
const root = document.documentElement;
const osDark = matchMedia("(prefers-color-scheme: dark)");
const isDark = () => (root.dataset.theme ? root.dataset.theme === "dark" : osDark.matches);
function syncThemeButton() {
  $("theme").setAttribute("aria-label", isDark() ? "Switch to light theme" : "Switch to dark theme");
}
$("theme").addEventListener("click", () => {
  const next = isDark() ? "light" : "dark";
  // Back to "follow the OS" when the choice matches it.
  if ((next === "dark") === osDark.matches) delete root.dataset.theme;
  else root.dataset.theme = next;
  try {
    if (root.dataset.theme) localStorage.setItem("theme", root.dataset.theme);
    else localStorage.removeItem("theme");
  } catch {}
  syncThemeButton();
});
osDark.addEventListener("change", syncThemeButton);
syncThemeButton();
