# JavaScript API

The npm package `@sawapi/pdfdiet`. TypeScript types are included, and every
function's documentation shows up in editors.

## Entry points

| Import | For |
|---|---|
| `"@sawapi/pdfdiet"` | Node, and browsers through a bundler (resolves to the browser build there) |
| `"@sawapi/pdfdiet/web"` | Browsers and bundlers, explicitly |
| `"@sawapi/pdfdiet/worker"` | Browsers and bundlers: the same functions run in a Web Worker ([below](#in-a-web-worker)) |

The first two export the same functions. The browser build has to load its `.wasm`
file before use:

```js
import { init, initSync, compress } from "@sawapi/pdfdiet/web";

await init();     // fetches web/pdfdiet_wasm_bg.wasm from the package
await init(url);  // or a URL, a Response, a fetch() promise, bytes, a WebAssembly.Module
initSync(bytes);  // synchronously, from bytes or a WebAssembly.Module (workers)
```

`init` is also the default export. Calling it again is harmless, and a
failed load can be retried. `initSync` throws while an `init()` is still
loading, and does nothing once the module is loaded. Calling `compress` or
`inspect` before the module has loaded throws an `Error`. The 0.1.x forms
`init({ module_or_path })` and `initSync({ module })` still work; any
other plain object passed to `init` is rejected with a `TypeError`.

In Node the module loads on import. `init`, `initSync` and the default
export exist there too and do nothing, so code shared between Node and the
browser can always call them.

`require("@sawapi/pdfdiet")` works in Node 22.12 or later. The package is ES
modules, so TypeScript code compiled to CommonJS needs `"module":
"nodenext"` and TypeScript 5.8 or later. The type declarations need
TypeScript 5.7 or later.

## `compress(input, options?)`

Shrinks a PDF. `input` is the file's bytes as a `Uint8Array` (a Node
`Buffer` is one).

```js
import { compress } from "@sawapi/pdfdiet";
import { readFileSync, writeFileSync } from "node:fs";

const res = compress(readFileSync("in.pdf"), { dpi: 150, subsetOutlines: true });
writeFileSync("out.pdf", res.output);
console.log(res.log.join("\n"));
console.log(`${res.imagesRecompressed} of ${res.imagesTotal} images recompressed`);
```

| Option | Default | |
|---|---|---|
| `dpi` | 150 | Target effective resolution; images drawn above it are downsampled |
| `quality` | 80 | JPEG quality for re-encoded images, an integer from 1 to 100 |
| `subsetOutlines` | `false` | Also drop the outlines of embedded glyphs (TrueType, and CID-keyed CFF) that no text uses: smaller, but the fonts can no longer show text added later |
| `stripJpegMetadata` | `false` | Remove EXIF (GPS, camera, dates), XMP and other metadata from embedded JPEGs, losslessly |
| `stripProtection` | `false` | Process a document protected only by an owner password, removing its permission restrictions |
| `breakSignatures` | `false` | Process a digitally signed document, invalidating its signatures |

`options` may be left out or `null`; a field left out or `undefined` takes
its default. See [What it does](what-it-does.md) for what each one changes.

Returns:

| Field | |
|---|---|
| `output` | The new PDF, a `Uint8Array` on its own `ArrayBuffer` (it can go straight into a `Blob` or be transferred) |
| `log` | What was changed, and what was left alone and why, one string per line |
| `imagesTotal` | Images in the document |
| `imagesRecompressed` | Images downsampled, re-encoded or losslessly re-optimized |

## `inspect(input)`

Lists the images in a PDF:

```js
for (const img of inspect(bytes)) {
  console.log(img.object, img.width, img.height, img.filters, img.dpi);
}
```

Each entry:

| Field | |
|---|---|
| `object` | `[objectNumber, generation]` |
| `width`, `height` | Size in pixels (0 if missing or invalid) |
| `bitsPerComponent` | Bits per color component (0 if missing or invalid) |
| `colorSpace` | For example `"DeviceRGB"`, `"ICCBased(N=3)"`, `"Indexed"` |
| `components` | Color components per pixel, or `null` when not known |
| `filters` | Stream filters in order, for example `["DCTDecode"]` |
| `bytes` | Encoded size of the image |
| `drawnSize` | `{ width, height }` in inches: the largest size any page draws it at, or `null` |
| `dpi` | `{ x, y }`: the effective resolution at that size, or `null` |

## Errors

| Thrown | When |
|---|---|
| `PdfdietError` | The document was not processed; `reason` says why |
| `TypeError` | An argument has the wrong type (for example a string instead of bytes, or a non-boolean flag) |
| `RangeError` | `dpi` is not positive, or `quality` is not an integer from 1 to 100 |
| `Error` | The browser build was used before `init()` finished |

`PdfdietError.reason` is one of:

| `reason` | |
|---|---|
| `"invalid-pdf"` | The input could not be parsed as a PDF |
| `"encrypted"` | Password-protected; the contents cannot be read |
| `"protected"` | Owner-password protected; pass `stripProtection: true` to go ahead |
| `"signed"` | Digitally signed; pass `breakSignatures: true` to go ahead |
| `"empty"` | No readable pages |

```js
import { compress, PdfdietError } from "@sawapi/pdfdiet";

try {
  compress(input);
} catch (e) {
  if (e instanceof PdfdietError && e.reason === "signed") {
    // compress(input, { breakSignatures: true }) to go ahead anyway
  }
}
```

Anything else, such as a `WebAssembly.RuntimeError` from running out of
memory on a very large file, means the module may be left in an
inconsistent state: in Node restart the process, and in the browser
prefer the [worker](#in-a-web-worker), which stops itself and can be
replaced.

`TypeError` and `RangeError` messages start with `pdfdiet: `.
`PdfdietError` messages do not; its class tells where it came from.

## In a Web Worker

Compression is synchronous and can take a few seconds on a large file, so
in a page, run it off the main thread. `createWorker` starts a Web Worker
that loads the wasm by itself, and returns `compress` and `inspect` as
functions taking the same arguments and returning promises:

```js
import { createWorker, PdfdietError } from "@sawapi/pdfdiet/worker";

const pdfdiet = createWorker();
try {
  const res = await pdfdiet.compress(new Uint8Array(await file.arrayBuffer()), { dpi: 150 });
  const blob = new Blob([res.output], { type: "application/pdf" });
} catch (e) {
  if (e instanceof PdfdietError) console.log(e.reason);
}
pdfdiet.terminate(); // when done
```

- Arguments are copied to the worker, so your buffers stay usable (a
  `Uint8Array` that views part of a larger buffer is copied on its own,
  not with the whole buffer). The `output` of `compress` is moved back
  without a copy.
- Calls on one worker run one at a time, in order. Create several workers
  to run in parallel.
- Errors reject the promise with the same class and message the function
  throws: `PdfdietError` with its `reason`, `TypeError`, `RangeError`. An
  argument that cannot be sent to a worker (such as a function) is a
  `TypeError`.
- `ready` settles once the wasm has loaded, and rejects if the worker
  could not start or load it; every call then rejects with that error.
  Calls wait for it themselves, so awaiting it is optional.
- `terminate()` stops the worker at once, which is also the way to cancel
  a call: calls still running or waiting, and any made later, reject with
  `Error("pdfdiet: worker terminated")`. There is no progress reporting.
- `createWorker({ wasm })` loads the `.wasm` from elsewhere: a URL (string
  or `URL`; a relative one resolves against the page), an `ArrayBuffer` or
  `Uint8Array`, or a `WebAssembly.Module`. Unlike `init`, it cannot take a
  `Response` or a promise, which cannot be sent to a worker; anything else
  makes `ready` and every call reject with a `TypeError`.
- The worker script is `dist/worker-main.mjs`, next to the entry point,
  started as `new Worker(new URL("./worker-main.mjs", import.meta.url))`:
  bundlers that understand that form bundle it. Bundlers that pre-bundle
  dependencies, such as Vite's development server, can break such
  relative URLs (for the `.wasm` file too); excluding the package from
  that step (`optimizeDeps.exclude` in Vite) avoids it.
- Loaded from another origin without a bundler (a CDN, for example), the
  module starts a same-origin worker that imports the script, which
  needs the other origin to allow it with CORS.
- `createWorker` throws a `TypeError` where Web Workers are not available,
  such as in Node.
- A call that fails inside the wasm with something other than the errors
  above (running out of memory on a very large file, for example) rejects
  with a plain `Error` whose `name` is the original one (such as
  `"RuntimeError"`). The worker then stops, since the module may be left
  in an inconsistent state: later calls reject, and `terminate()` it and
  create a new one to go on.

The [website](https://sawapi.github.io/pdfdiet/) works this way; see
[`web/app.js`](https://github.com/sawapi/pdfdiet/blob/main/web/app.js).
