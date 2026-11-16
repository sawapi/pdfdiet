# pdfdiet

Put your PDFs on a diet — measured-DPI image downsampling, lossless font and
structure optimization, and a hard rule that anything it cannot prove safe is
left byte-for-byte untouched. Permissively licensed (MIT OR Apache-2.0),
written in Rust, shipped as WASM.

Works as a CLI, in Node, and in the browser. All processing is local —
nothing is uploaded anywhere.

## CLI

```sh
npx @sawapi/pdfdiet compress scanned.pdf -o small.pdf          # defaults: --dpi 150 --quality 80
npx @sawapi/pdfdiet inspect scanned.pdf                        # list images with measured effective DPI
```

On the project's own test corpus, a 300 dpi scanned page lands around 90%
smaller at default settings, and a font-heavy 15-page paper lands at 44% of
its original size. Run `inspect` first to see what a given file is made of.

## Node API

```js
import { compress_pdf, inspect_pdf } from "@sawapi/pdfdiet";
import { readFileSync, writeFileSync } from "node:fs";

const input = readFileSync("in.pdf");
const res = compress_pdf(input, 150, 80); // (bytes, dpi, quality)
// Optional trailing flags: strip_protection, break_signatures,
// subset_outlines, strip_jpeg_metadata
// e.g. compress_pdf(input, 150, 80, false, false, true) also drops unused
// glyph outlines (smaller; the fonts can no longer show text added later),
// and compress_pdf(input, 150, 80, false, false, false, true) removes EXIF
// (GPS, camera, dates), XMP and other metadata from embedded JPEGs.
writeFileSync("out.pdf", res.output);
console.log(res.log, res.images_recompressed, res.images_total);
```

## Browser API

The browser build is WASM behind an explicit async init:

```js
import init, { compress_pdf } from "@sawapi/pdfdiet/web";

await init(); // fetches and instantiates the .wasm
const res = compress_pdf(new Uint8Array(await file.arrayBuffer()), 150, 80);
const blob = new Blob([res.output], { type: "application/pdf" });
```

Bundlers that understand the `exports` map will also resolve the bare
`"@sawapi/pdfdiet"` import to the browser build outside Node.

## Safety model

- Effective DPI is **measured**, not guessed: every page's content stream is
  interpreted (transform tracking, nested Form XObjects, soft-mask
  propagation), so an image placed small on the page is downsampled
  correctly and no more.
- Bilevel scans are recompressed **losslessly** (CCITT G4); JPEGs that can't
  be beaten are entropy-optimized losslessly; a size guard keeps original
  bytes whenever recompression would lose.
- Fonts are only rewritten under proof: provably-identical subsets merge,
  unreachable glyph outlines drop without renumbering, Type 1 programs
  convert to CFF with documented numeric fidelity.
- Forms, annotations and accessibility structure are never flattened or
  dropped. Anything undecodable (JPX, image masks, TIFF-predictor-filtered
  or color-key-masked images) passes through byte-for-byte.
- Encrypted, permission-protected and digitally signed documents are refused
  rather than quietly stripped of their protection or their signature.
- In development, the lossless transforms are held to bit-identical page
  renders across several renderers over a fixed corpus. That is a
  regression check, not a per-file guarantee — see the repository README
  for what it does and does not cover.

## Clean-room note

Written from public specifications (ISO 32000, ITU-T T.6/T.81, OpenType,
Adobe Type 1/CFF) and observed behavior only. See THIRD_PARTY.md in the
repository for the full provenance record.

## License

MIT OR Apache-2.0, at your option.
