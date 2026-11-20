#!/usr/bin/env node
import { chmodSync, readFileSync, realpathSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { compress, PdfdietError } from "../dist/node.mjs";
import { inspect_table } from "../node/pdfdiet_wasm.js";

function usage(code) {
  console.error(`pdfdiet — squeeze PDFs by downsampling and recompressing embedded images

usage:
  pdfdiet inspect  <in.pdf>
  pdfdiet compress <in.pdf> -o <out.pdf> [--dpi <n>] [--quality <n>] [--subset-outlines]
                   [--strip-jpeg-metadata] [--strip-protection] [--break-signatures]

options:
  --dpi <n>          target effective DPI (default: 150)
  --quality <n>      JPEG quality 1-100 (default: 80)
  --subset-outlines  also drop embedded glyph outlines (TrueType, CID-keyed CFF) no text uses
                     (smaller; the fonts can no longer show text added later)
  --strip-jpeg-metadata
                     remove EXIF (GPS, camera, dates), XMP and other metadata
                     from embedded JPEG images, losslessly
  --strip-protection process a document protected only by an owner password,
                     stripping its permission restrictions
  --break-signatures process a digitally signed document, invalidating its
                     signatures`);
  process.exit(code);
}

// The library's refusals say what would happen; the CLI also says which
// flag allows it.
const hints = {
  protected: " — pass --strip-protection to do that explicitly",
  signed: " — pass --break-signatures to do that explicitly",
};

// Text for the terminal with control characters (other than newline and
// tab) escaped: names in the log and the table come from the document, and
// a PDF name can hold any byte, including escape sequences.
// Write through a temporary file in the same directory, renamed over the
// target only once complete, so a failed write never leaves a truncated
// file (even when the output is the input). A symlink is followed and the
// target keeps its permissions.
function writeReplacing(path, data) {
  let target = path;
  try {
    target = realpathSync(path);
  } catch {}
  const tmp = join(dirname(target), `.${basename(target)}.pdfdiet-${process.pid}.tmp`);
  try {
    writeFileSync(tmp, data, { flag: "wx", flush: true });
    try {
      chmodSync(tmp, statSync(target).mode & 0o7777);
    } catch (e) {
      if (e.code !== "ENOENT") throw e;
    }
    renameSync(tmp, target);
  } catch (e) {
    rmSync(tmp, { force: true });
    throw e;
  }
}

const printable = (s) =>
  String(s).replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, (c) => `\\u{${c.charCodeAt(0).toString(16)}}`);

// Report a refused or unreadable document, or a file that cannot be read or
// written, as a one-line error rather than a stack trace.
function run(f) {
  try {
    return f();
  } catch (e) {
    if (e instanceof PdfdietError) {
      console.error(`pdfdiet: ${printable(e.message)}${hints[e.reason] ?? ""}`);
    } else if (typeof e?.reason === "string" && typeof e.free === "function") {
      // inspect_table comes straight from the bindings and throws their Failure.
      console.error(`pdfdiet: ${printable(e.message)}`);
      e.free();
    } else if (typeof e?.code === "string" && typeof e.syscall === "string") {
      console.error(`pdfdiet: ${e.message}`); // a file system error
    } else {
      throw e;
    }
    process.exit(1);
  }
}

const args = process.argv.slice(2);
const cmd = args.shift();

if (cmd === "inspect") {
  const input = args[0];
  if (!input) usage(1);
  const data = run(() => readFileSync(input));
  run(() => console.log(printable(inspect_table(data))));
} else if (cmd === "compress") {
  let input = null;
  let output = null;
  let dpi = 150;
  let quality = 80;
  let subsetOutlines = false;
  let stripJpegMetadata = false;
  let stripProtection = false;
  let breakSignatures = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "-o" || a === "--output") output = args[++i];
    else if (a === "--dpi") dpi = Number(args[++i]);
    else if (a === "--quality") quality = Number(args[++i]);
    else if (a === "--subset-outlines") subsetOutlines = true;
    else if (a === "--strip-jpeg-metadata") stripJpegMetadata = true;
    else if (a === "--strip-protection") stripProtection = true;
    else if (a === "--break-signatures") breakSignatures = true;
    else if (!a.startsWith("-") && !input) input = a;
    else usage(1);
  }
  if (!input || !output) usage(1);
  if (!(Number.isFinite(dpi) && dpi > 0)) {
    console.error("pdfdiet: --dpi must be a positive number");
    process.exit(1);
  }
  if (!(Number.isInteger(quality) && quality >= 1 && quality <= 100)) {
    console.error("pdfdiet: --quality must be an integer from 1 to 100");
    process.exit(1);
  }

  const data = run(() => readFileSync(input));
  const res = run(() =>
    compress(data, { dpi, quality, subsetOutlines, stripJpegMetadata, stripProtection, breakSignatures }),
  );
  run(() => writeReplacing(output, res.output));
  for (const line of res.log) console.log(printable(line));
  console.log(`\n${res.imagesRecompressed}/${res.imagesTotal} images recompressed`);
  const outLen = res.output.length;
  console.log(
    `${data.length} -> ${outLen} bytes (${((outLen / data.length) * 100).toFixed(1)}%)`
  );
} else {
  usage(cmd === undefined || cmd === "--help" || cmd === "-h" ? 0 : 1);
}
