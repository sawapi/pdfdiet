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
import { compress, inspect, PdfdietError } from "@sawapi/pdfdiet";
import { readFileSync, writeFileSync } from "node:fs";

const input = readFileSync("in.pdf");
const res = compress(input); // defaults: { dpi: 150, quality: 80 }
writeFileSync("out.pdf", res.output);
console.log(res.log.join("\n")); // what changed, and what was left alone and why
console.log(`${res.imagesRecompressed} of ${res.imagesTotal} images recompressed`);
```

Options (all optional):

| option | default | |
| --- | --- | --- |
| `dpi` | 150 | target effective DPI; images drawn at a higher resolution are downsampled |
| `quality` | 80 | JPEG quality for re-encoded images, 1-100 |
| `subsetOutlines` | false | also drop embedded TrueType glyph outlines no text uses (smaller; the fonts can no longer show text added later) |
| `stripJpegMetadata` | false | remove EXIF (GPS, camera, dates), XMP and other metadata from embedded JPEGs, losslessly |
| `stripProtection` | false | process a document protected only by an owner password, removing its permission restrictions |
| `breakSignatures` | false | process a digitally signed document, invalidating its signatures |

`inspect(input)` lists the images, each with its pixel size, color space,
filters, encoded size, the largest size it is drawn at (`drawnSize`, in
inches) and the resulting effective resolution (`dpi: { x, y }`).

A wrong argument type throws a `TypeError` and an out-of-range `dpi` or
`quality` a `RangeError`. Documents that are not processed throw a
`PdfdietError` whose `reason` is
one of `"invalid-pdf"`, `"encrypted"`, `"protected"`, `"signed"` or
`"empty"`:

```js
try {
  compress(input);
} catch (e) {
  if (e instanceof PdfdietError && e.reason === "signed") {
    // compress(input, { breakSignatures: true }) to go ahead anyway
  }
}
```

Type declarations are included (they need TypeScript 5.7 or later, for
`Uint8Array<ArrayBuffer>`). The package is ESM; `require()` works
too on Node 22.12 or later. TypeScript resolves the types under any
`node16`-or-later or `bundler` module resolution; CommonJS (`.cts`) code
that `require`s it needs `"module": "nodenext"` (TypeScript 5.8 or later).

## Browser API

The browser build loads its WASM asynchronously, once:

```js
import { init, compress } from "@sawapi/pdfdiet/web";

await init(); // fetches and instantiates the package's web/pdfdiet_wasm_bg.wasm
const res = compress(new Uint8Array(await file.arrayBuffer()));
const blob = new Blob([res.output], { type: "application/pdf" });
```

`init()` also takes a URL, a `Response` (or a promise of one, such as
`fetch(url)`), the bytes or a compiled `WebAssembly.Module` if the `.wasm`
file is served from elsewhere. `initSync(bytes)` loads it synchronously
(best in a worker; it throws while an `init()` is still loading), and
`init` is also the default export. The 0.1.x forms
`init({ module_or_path })` and `initSync({ module })` still work.

Bundlers that understand the `exports` map resolve the bare
`"@sawapi/pdfdiet"` import to the browser build outside Node. The Node
build exports the same `init`, `initSync` and default export as no-ops, so
code written for the browser also runs under Node (server-side rendering,
tests).

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
