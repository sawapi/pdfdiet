// Smoke test for both builds and the CLI, through the published entry
// points: run `./build-npm.sh` first, then `node npm/test.mjs`.
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// By package name, so the exports map is exercised too. The Node build
// is tested before the browser build is even loaded.
const node = await import("@sawapi/pdfdiet");
await node.init();
// require() reaches the same build (the reason for Node 22.12 or later).
const required = createRequire(import.meta.url)("@sawapi/pdfdiet");
assert.equal(required.compress, node.compress, "require() gives the same module");

// A one-page PDF holding a 600x600 RGB photo-like gradient drawn 2 inches
// wide: 300 dpi on the page, so the default 150 dpi target halves it.
function samplePdf() {
  const w = 600, h = 600;
  const px = Buffer.alloc(w * h * 3);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      const i = (y * w + x) * 3;
      px[i] = (x * 255) / w;
      px[i + 1] = (y * 255) / h;
      px[i + 2] = ((x ^ y) & 255) / 2 + 64;
    }
  }
  const content = Buffer.from("q 144 0 0 144 72 576 cm /Im0 Do Q\n");
  const objs = [
    Buffer.from("<< /Type /Catalog /Pages 2 0 R >>"),
    Buffer.from("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
    Buffer.from("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"),
    Buffer.concat([Buffer.from(`<< /Length ${content.length} >>\nstream\n`), content, Buffer.from("\nendstream")]),
    Buffer.concat([
      Buffer.from(`<< /Type /XObject /Subtype /Image /Width ${w} /Height ${h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length ${px.length} >>\nstream\n`),
      px,
      Buffer.from("\nendstream"),
    ]),
  ];
  const parts = [Buffer.from("%PDF-1.7\n")];
  const offsets = [];
  let pos = parts[0].length;
  objs.forEach((body, i) => {
    offsets.push(pos);
    const chunk = Buffer.concat([Buffer.from(`${i + 1} 0 obj\n`), body, Buffer.from("\nendobj\n")]);
    parts.push(chunk);
    pos += chunk.length;
  });
  let xref = `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
  for (const o of offsets) xref += `${String(o).padStart(10, "0")} 00000 n \n`;
  xref += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${pos}\n%%EOF\n`;
  parts.push(Buffer.from(xref));
  return Buffer.concat(parts);
}

const input = samplePdf();

// The same document with a signature field: refused unless allowed.
function signedPdf() {
  const text = input.toString("latin1").replace(
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Catalog /Pages 2 0 R /AcroForm << /SigFlags 3 /Fields [] >> >>",
  );
  return fixXref(Buffer.from(text, "latin1"));
}

// Recompute the xref offsets after an edit that shifted the objects.
function fixXref(pdf) {
  const text = pdf.toString("latin1");
  const head = text.slice(0, text.indexOf("xref\n"));
  const offsets = [...head.matchAll(/(\d+) 0 obj\n/g)].map((m) => m.index);
  let xref = `xref\n0 ${offsets.length + 1}\n0000000000 65535 f \n`;
  for (const o of offsets) xref += `${String(o).padStart(10, "0")} 00000 n \n`;
  xref += `trailer\n<< /Size ${offsets.length + 1} /Root 1 0 R >>\nstartxref\n${head.length}\n%%EOF\n`;
  return Buffer.from(head + xref, "latin1");
}

function run(lib, name) {
  const res = lib.compress(input);
  assert.deepEqual(Object.keys(res).sort(), ["imagesRecompressed", "imagesTotal", "log", "output"]);
  const out = res.output;
  assert.ok(out instanceof Uint8Array);
  assert.equal(Buffer.from(out.subarray(0, 5)).toString(), "%PDF-", `${name}: output is a PDF`);
  assert.equal(res.imagesTotal, 1, `${name}: one image`);
  assert.equal(res.imagesRecompressed, 1, `${name}: image recompressed`);
  assert.ok(Array.isArray(res.log) && res.log.every((l) => typeof l === "string"), `${name}: log lines`);
  assert.ok(out.length < input.length / 10, `${name}: ${input.length} -> ${out.length} bytes`);
  // Options: a lower target and the opt-in flags are accepted.
  const small = lib.compress(input, { dpi: 72, quality: 50, subsetOutlines: true, stripJpegMetadata: true });
  assert.ok(small.output.length < out.length, `${name}: dpi 72 is smaller`);
  assert.throws(() => lib.compress(input, { quality: 0 }), RangeError);
  assert.throws(() => lib.compress(input, { quality: 80.5 }), RangeError);
  assert.throws(() => lib.compress(input, { dpi: -1 }), RangeError);
  assert.throws(() => lib.compress(input, { dpi: "150" }), TypeError);
  assert.throws(() => lib.compress(input, { dpi: null }), TypeError);
  assert.throws(() => lib.compress(input, { quality: NaN }), /^RangeError: pdfdiet: quality .* got NaN$/);
  assert.throws(() => lib.compress(input, { dpi: 10n }), /^TypeError: pdfdiet: dpi .* got 10$/);
  assert.throws(() => lib.compress(input, []), TypeError);
  assert.throws(() => lib.compress(input, { dpi: Object.create(null) }), /^TypeError: pdfdiet: dpi .* got \[object Object\]$/);
  assert.throws(() => lib.compress(input, { subsetOutlines: "yes" }), TypeError);
  assert.throws(() => lib.compress(input, { breakSignatures: null }), /^TypeError: pdfdiet: breakSignatures/);
  // The input is checked before the options.
  assert.throws(() => lib.compress("in.pdf", { quality: 0 }), /^TypeError: pdfdiet: input/);
  assert.throws(() => lib.compress("in.pdf"), TypeError);
  assert.equal(lib.compress(input, null).output.length, out.length, `${name}: null options are defaults`);

  // The image list is plain data.
  const images = lib.inspect(input);
  assert.deepEqual(images, [
    {
      object: [5, 0],
      width: 600,
      height: 600,
      bitsPerComponent: 8,
      colorSpace: "DeviceRGB",
      components: 3,
      filters: [],
      bytes: 600 * 600 * 3,
      drawnSize: { width: 2, height: 2 },
      dpi: { x: 300, y: 300 },
    },
  ]);

  // Garbage is a PdfdietError with a reason, not a crash or a wasm object.
  assert.throws(
    () => lib.compress(new Uint8Array([1, 2, 3])),
    (e) => e instanceof lib.PdfdietError && e instanceof Error && e.reason === "invalid-pdf" && e.message.length > 0,
    `${name}: garbage throws PdfdietError`,
  );
  assert.throws(() => lib.inspect(new Uint8Array([1, 2, 3])), (e) => e.reason === "invalid-pdf");
  // A refusal carries its own reason.
  assert.throws(() => lib.compress(signedPdf()), (e) => e.reason === "signed" && /signed/.test(e.message));
  assert.ok(lib.compress(signedPdf(), { breakSignatures: true }).output.length > 0);
  console.log(`${name}: ${input.length} -> ${out.length} bytes`);
}

run(node, "node");

const web = await import("@sawapi/pdfdiet/web");
assert.equal(node.PdfdietError, web.PdfdietError, "one error class for both builds");
// Node has no fetch for file: URLs, so hand the browser build the bytes.
await web.init(readFileSync(new URL("./web/pdfdiet_wasm_bg.wasm", import.meta.url)));
await web.init(); // already loaded: same promise, no refetch
run(web, "web");

// Both builds export the same names, and document them the same way.
const names = (m) => Object.keys(m).sort();
assert.deepEqual(names(web), names(node));
const decls = (f) => {
  const text = readFileSync(new URL(`./dist/${f}`, import.meta.url), "utf8");
  const end = 'export declare const inspect: Api["inspect"];';
  return text.slice(text.indexOf("/**\n * Shrink a PDF"), text.indexOf(end) + end.length);
};
assert.equal(decls("node.d.mts"), decls("web.d.mts"), "compress/inspect declarations match");

// The worker entry point's protocol, over a MessageChannel.
const { testWorker } = await import("./test-worker.mjs");
await testWorker(web, input);

// Each way of loading the wasm, in a fresh process: once the glue has
// loaded, it ignores later loads, so one process would hide a broken form.
const initCases = [
  "web:before-init",
  "web:init-bytes",
  "web:init-module",
  "web:init-response",
  "web:init-promise",
  "web:init-legacy-object",
  "web:init-default-export",
  "web:initSync-bytes",
  "web:initSync-module",
  "web:initSync-legacy-object",
  "web:init-retry-after-failure",
  "web:init-after-initSync",
  "web:initSync-after-init",
  "web:initSync-while-loading",
  "web:init-wrong-object",
  "web:require",
  "node:browser-style-calls",
];
for (const c of initCases) {
  const script = fileURLToPath(new URL("./test-init.mjs", import.meta.url));
  const r = spawnSync(process.execPath, [script, c], { encoding: "utf8" });
  assert.equal(r.status, 0, `${c}: ${r.stderr}`);
  assert.equal(r.stdout.trim(), `${c} ok`);
  // The glue still accepts its deprecated bare argument, with only a
  // warning: an argument passed to it unwrapped shows up here.
  assert.equal(r.stderr, "", `${c}: nothing on stderr`);
}
console.log(`init forms: ${initCases.length} ok`);

// The CLI, as `npx` would run it.
const dir = mkdtempSync(join(tmpdir(), "pdfdiet-test-"));
try {
  const inPath = join(dir, "in.pdf");
  const outPath = join(dir, "out.pdf");
  writeFileSync(inPath, input);
  const cli = fileURLToPath(new URL("./bin/pdfdiet.mjs", import.meta.url));
  execFileSync(process.execPath, [cli, "compress", inPath, "-o", outPath], { stdio: "pipe" });
  const out = readFileSync(outPath);
  assert.ok(out.length < input.length / 10, `cli: ${input.length} -> ${out.length} bytes`);
  const table = execFileSync(process.execPath, [cli, "inspect", inPath], { encoding: "utf8" });
  assert.match(table, /600x600/, "cli: inspect lists the image");
  // A refused document: exit status 1 and a one-line message, no stack trace.
  const signedPath = join(dir, "signed.pdf");
  writeFileSync(signedPath, signedPdf());
  let failed;
  try {
    execFileSync(process.execPath, [cli, "compress", signedPath, "-o", outPath], { stdio: "pipe" });
  } catch (e) {
    failed = e;
  }
  assert.equal(failed?.status, 1, "cli: refusal exits 1");
  assert.match(
    String(failed.stderr),
    /^pdfdiet: document is digitally signed; compressing would invalidate the signatures — pass --break-signatures to do that explicitly\n$/,
  );
  execFileSync(process.execPath, [cli, "compress", signedPath, "-o", outPath, "--break-signatures"], { stdio: "pipe" });
  // So is a file that is not a PDF, for inspect (which uses the bindings directly).
  const junkPath = join(dir, "junk.pdf");
  writeFileSync(junkPath, "not a pdf");
  const junk = spawnSync(process.execPath, [cli, "inspect", junkPath], { encoding: "utf8" });
  assert.equal(junk.status, 1);
  assert.match(junk.stderr, /^pdfdiet: [^\n]+\n$/);
  // Names from the document reach the terminal with control characters
  // escaped: a color space named with an ESC byte is shown as \u{1b}.
  const evilPath = join(dir, "evil.pdf");
  writeFileSync(evilPath, fixXref(Buffer.from(input.toString("latin1").replace("/ColorSpace /DeviceRGB", "/ColorSpace /Evil#1b#5b2J"), "latin1")));
  const evil = execFileSync(process.execPath, [cli, "inspect", evilPath], { encoding: "utf8" });
  assert.ok(!evil.includes("\x1b") && evil.includes("Evil\\u{1b}[2J"), `cli: control characters escaped: ${JSON.stringify(evil)}`);
  // A missing file is a one-line error too.
  let missing;
  try {
    execFileSync(process.execPath, [cli, "inspect", join(dir, "missing.pdf")], { stdio: "pipe" });
  } catch (e) {
    missing = e;
  }
  assert.equal(missing?.status, 1);
  assert.match(String(missing.stderr), /^pdfdiet: ENOENT[^\n]*\n$/);
  console.log(`cli: ${input.length} -> ${out.length} bytes`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
console.log("ok");
