// Smoke test for both builds and the CLI: run `./build-npm.sh` first, then
// `node npm/test.mjs`.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);
const node = require("./node/pdfdiet_wasm.js");
const web = await import("./web/pdfdiet_wasm.js");
web.initSync({ module: readFileSync(new URL("./web/pdfdiet_wasm_bg.wasm", import.meta.url)) });

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

function run(lib, name) {
  const res = lib.compress_pdf(new Uint8Array(input), 150, 80);
  const out = res.output;
  assert.equal(Buffer.from(out.subarray(0, 5)).toString(), "%PDF-", `${name}: output is a PDF`);
  assert.equal(res.images_total, 1, `${name}: one image`);
  assert.equal(res.images_recompressed, 1, `${name}: image recompressed`);
  assert.ok(out.length < input.length / 10, `${name}: ${input.length} -> ${out.length} bytes`);
  assert.match(lib.inspect_pdf(new Uint8Array(input)), /600x600/, `${name}: inspect lists the image`);
  // Opt-in flags are accepted.
  lib.compress_pdf(new Uint8Array(input), 150, 80, false, false, true, true);
  // Garbage is refused with an error, not a crash.
  assert.throws(() => lib.compress_pdf(new Uint8Array([1, 2, 3]), 150, 80), undefined, `${name}: garbage throws`);
  console.log(`${name}: ${input.length} -> ${out.length} bytes`);
}

run(node, "node");
run(web, "web");

// The CLI, as `npx` would run it.
const dir = mkdtempSync(join(tmpdir(), "pdfdiet-test-"));
try {
  const inPath = join(dir, "in.pdf");
  const outPath = join(dir, "out.pdf");
  writeFileSync(inPath, input);
  const cli = new URL("./bin/pdfdiet.mjs", import.meta.url).pathname;
  execFileSync(process.execPath, [cli, "compress", inPath, "-o", outPath], { stdio: "pipe" });
  const out = readFileSync(outPath);
  assert.ok(out.length < input.length / 10, `cli: ${input.length} -> ${out.length} bytes`);
  console.log(`cli: ${input.length} -> ${out.length} bytes`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
console.log("ok");
