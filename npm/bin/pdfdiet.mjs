#!/usr/bin/env node
import { createRequire } from "node:module";
import { readFileSync, writeFileSync, statSync } from "node:fs";

const require = createRequire(import.meta.url);
const wasm = require("../node/pdfdiet_wasm.js");

function usage(code) {
  console.error(`pdfdiet — squeeze PDFs by downsampling and recompressing embedded images

usage:
  pdfdiet inspect  <in.pdf>
  pdfdiet compress <in.pdf> -o <out.pdf> [--dpi <n>] [--quality <n>] [--subset-outlines]
                   [--strip-jpeg-metadata]

options:
  --dpi <n>          target effective DPI (default: 150)
  --quality <n>      JPEG quality 1-100 (default: 80)
  --subset-outlines  also drop embedded TrueType glyph outlines no text uses
                     (smaller; the fonts can no longer show text added later)
  --strip-jpeg-metadata
                     remove EXIF (GPS, camera, dates), XMP and other metadata
                     from embedded JPEG images, losslessly`);
  process.exit(code);
}

const args = process.argv.slice(2);
const cmd = args.shift();

if (cmd === "inspect") {
  const input = args[0];
  if (!input) usage(1);
  console.log(wasm.inspect_pdf(readFileSync(input)));
} else if (cmd === "compress") {
  let input = null;
  let output = null;
  let dpi = 150;
  let quality = 80;
  let subsetOutlines = false;
  let stripJpegMetadata = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "-o" || a === "--output") output = args[++i];
    else if (a === "--dpi") dpi = Number(args[++i]);
    else if (a === "--quality") quality = Number(args[++i]);
    else if (a === "--subset-outlines") subsetOutlines = true;
    else if (a === "--strip-jpeg-metadata") stripJpegMetadata = true;
    else if (!a.startsWith("-") && !input) input = a;
    else usage(1);
  }
  if (!input || !output || !Number.isFinite(dpi) || !Number.isFinite(quality)) usage(1);

  const data = readFileSync(input);
  const res = wasm.compress_pdf(
    data, dpi, quality, undefined, undefined, subsetOutlines, stripJpegMetadata
  );
  writeFileSync(output, res.output);
  if (res.log) console.log(res.log);
  console.log(
    `\n${res.images_recompressed}/${res.images_total} images recompressed`
  );
  const outLen = statSync(output).size;
  console.log(
    `${data.length} -> ${outLen} bytes (${((outLen / data.length) * 100).toFixed(1)}%)`
  );
} else {
  usage(cmd === undefined || cmd === "--help" || cmd === "-h" ? 0 : 1);
}
