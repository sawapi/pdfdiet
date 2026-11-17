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
let file = null; // { name, bytes: ArrayBuffer, stripProtection: boolean }
let pendingRun = false;

async function takeFile(f) {
  if (!f) return;
  const looksPdf = f.type === "application/pdf" || /\.pdf$/i.test(f.name);
  if (!looksPdf) {
    showError(f.name, "That doesn't look like a PDF.");
    return;
  }
  // Removing owner-password restrictions is chosen per file, after a refusal.
  file = { name: f.name, bytes: await f.arrayBuffer(), stripProtection: false };
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
    id, name: file.name, size: file.bytes.byteLength, t0, stripped: file.stripProtection,
    timer: setInterval(() => ($("elapsed").textContent = ((performance.now() - t0) / 1000).toFixed(1)), 100),
  };
  $("elapsed").textContent = "0.0";
  // Copy: the original stays here so settings can be changed and re-run.
  const input = file.bytes.slice(0);
  worker.postMessage({ id, input, ...settings(), stripProtection: file.stripProtection }, [input]);
}
function stopTimer() {
  if (running) clearInterval(running.timer);
}

function finish(res) {
  const job = running;
  stopTimer();
  running = null;
  if (!res.ok) {
    showError(job.name, friendlyError(res.reason, res.error), res.reason === "protected");
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
  $("meta").textContent =
    `${imgs} · ${(res.ms / 1000).toFixed(1)} s` + (job.stripped && !kept ? " · restrictions removed" : "");
  $("log").textContent = res.log || "(nothing to report)";

  const a = $("download");
  if (a.href) URL.revokeObjectURL(a.href);
  a.href = URL.createObjectURL(new Blob([out], { type: "application/pdf" }));
  a.download = job.name.replace(/\.pdf$/i, "") + (kept ? ".pdf" : ".diet.pdf");
  setState("done");
}

function friendlyError(reason, msg) {
  if (reason === "encrypted") return "This PDF is encrypted with a password, so it can't be read and was left alone.";
  if (reason === "protected") {
    return "This PDF has an owner password that may restrict printing, copying or editing, so it was left alone.";
  }
  if (reason === "signed") return "This PDF is digitally signed; changing it would invalidate the signature, so it was left alone.";
  if (reason === "empty") return "This PDF has no readable pages.";
  return msg ? "Couldn't process this PDF: " + msg : "Something went wrong.";
}
function showError(name, msg, offerStrip = false) {
  $("errName").textContent = name;
  $("errMsg").textContent = msg;
  $("errStrip").hidden = !(offerStrip && file);
  setState("error");
}
$("stripRun").addEventListener("click", () => {
  if (!file) return;
  file.stripProtection = true;
  run();
});

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
    $("file").focus();
  });
}

// ---- syntax colors --------------------------------------------------------
// A small tokenizer for the samples: enough to color comments, strings,
// numbers, keywords, function names and types in the Rust and JavaScript
// ones, and comments, commands and flags in the shell ones. The text itself
// is unchanged, so copying still gets the plain code.
const KEYWORDS = new Set(
  ("as async await break const continue else enum fn for from if impl import in let " +
    "loop match mod move mut new pub return self static struct true false use where while " +
    "export function type typeof void null undefined").split(" "),
);
const TOKEN =
  /(\/\/[^\n]*|\/\*[\s\S]*?\*\/)|("(?:[^"\\\n]|\\.)*"|`(?:[^`\\]|\\.)*`|'(?:[^'\\\n]|\\.)*'(?!\w))|(\b(?:0x[\da-fA-F_]+|\d[\d_]*(?:\.\d+)?(?:[eE][+-]?\d+)?)\b)|([A-Za-z_]\w*)(?=\s*(?:!\s*)?[(\[])|([A-Za-z_]\w*)/g;
// Rust's primitive types read as types, like the capitalized ones.
const PRIMITIVES = new Set("u8 u16 u32 u64 usize i8 i16 i32 i64 isize f32 f64 bool char str".split(" "));
// Shell: a comment (# at the start of a word), the command at the start of
// a line, a --flag or -o.
const SHELL = /((?:^|(?<=\s))#[^\n]*)|(^[\w@./-]+)|((?<=\s)--?[A-Za-z][\w-]*)/gm;
const esc = (t) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
function highlight(code, shell) {
  const text = code.textContent;
  let html = "";
  let last = 0;
  for (const m of text.matchAll(shell ? SHELL : TOKEN)) {
    const tok = m[0];
    let cls;
    if (shell) {
      const [, comment, command] = m;
      cls = comment ? "c" : command ? "f" : "k";
    } else {
      const [, comment, str, num, call, word] = m;
      const name = call || word;
      cls = comment ? "c" : str ? "s" : num ? "n"
        : /^[A-Z]/.test(name) || PRIMITIVES.has(name) ? "t" // types, and Some(...) / Ok(...)
        : KEYWORDS.has(name) && !(call && name === "new") && !/^\s*:(?!:)/.test(text.slice(m.index + tok.length))
          ? "k" // not Foo::new() (a call) or { type: ... } (a key)
        : call ? "f" : "";
    }
    html += esc(text.slice(last, m.index));
    html += cls ? `<span class="${cls}">${esc(tok)}</span>` : esc(tok);
    last = m.index + tok.length;
  }
  code.innerHTML = html + esc(text.slice(last));
}
for (const code of document.querySelectorAll(".code-card pre code")) {
  highlight(code, code.parentElement.id.endsWith("-cli"));
}

// ---- code tabs ----------------------------------------------------------
// Each code card has its own tab set.
for (const card of document.querySelectorAll(".code-card")) {
  const tabs = [...card.querySelectorAll('[role="tab"]')];
  const select = (tab) => {
    for (const t of tabs) {
      const on = t === tab;
      t.setAttribute("aria-selected", on);
      t.tabIndex = on ? 0 : -1;
      $(t.getAttribute("aria-controls")).hidden = !on;
    }
  };
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
  const copy = card.querySelector(".copy");
  copy.addEventListener("click", () => copyText(copy, card.querySelector('[role="tabpanel"]:not([hidden])').textContent));
}
for (const b of document.querySelectorAll(".copy-cmd")) {
  b.addEventListener("click", () => copyText(b, b.dataset.copy));
}
async function copyText(button, text) {
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "Copied";
  } catch {
    button.textContent = "Select & copy";
  }
  setTimeout(() => (button.textContent = "Copy"), 1500);
}

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
