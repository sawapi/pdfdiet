# pdfdiet

[![npm](https://img.shields.io/npm/v/@sawapi/pdfdiet.svg)](https://www.npmjs.com/package/@sawapi/pdfdiet)
[![CI](https://github.com/sawapi/pdfdiet/actions/workflows/ci.yml/badge.svg)](https://github.com/sawapi/pdfdiet/actions/workflows/ci.yml)
[![license](https://img.shields.io/npm/l/@sawapi/pdfdiet.svg)](#license)

Makes PDFs smaller, from the command line, in Node or in the browser:
images are resampled to the resolution they are actually drawn at, and
fonts and structure are optimized losslessly. Anything it cannot handle
with confidence is left as it was. WebAssembly compiled from a Rust
library, with TypeScript types; nothing is uploaded anywhere.

[Website](https://sawapi.github.io/pdfdiet/) ·
[API](https://github.com/sawapi/pdfdiet/blob/main/docs/npm-api.md) ·
[GitHub](https://github.com/sawapi/pdfdiet) ·
[Changelog](https://github.com/sawapi/pdfdiet/blob/main/CHANGELOG.md)

## Install

```sh
npx @sawapi/pdfdiet --help     # run without installing
npm install @sawapi/pdfdiet    # the library (and the CLI, for npx and package scripts)
```

## Command line

```sh
npx @sawapi/pdfdiet inspect report.pdf                        # images, encodings, measured resolution
npx @sawapi/pdfdiet compress report.pdf -o report.small.pdf   # defaults: --dpi 150 --quality 80
```

Options: `--dpi`, `--quality`, `--subset-outlines`,
`--strip-jpeg-metadata`, `--strip-protection`, `--break-signatures`. See
the [command line guide](https://github.com/sawapi/pdfdiet/blob/main/docs/cli.md).

## Node

```js
import { compress } from "@sawapi/pdfdiet";
import { readFileSync, writeFileSync } from "node:fs";

const res = compress(readFileSync("report.pdf"), { dpi: 150 });
writeFileSync("report.small.pdf", res.output);
console.log(res.log.join("\n")); // what was changed, and what was left alone and why
```

## Browser

```js
import { init, compress } from "@sawapi/pdfdiet/web";

await init(); // loads the .wasm file once
const res = compress(new Uint8Array(await file.arrayBuffer()));
const blob = new Blob([res.output], { type: "application/pdf" });
```

Compression is synchronous; for large files, run it in a Web Worker.

## Features

- **Measures** the resolution each image is drawn at, and downsamples only
  images above the target.
- **Re-encodes** gray, RGB and CMYK images as JPEG and bilevel scans
  losslessly, keeping the original whenever it is smaller.
- **Optimizes fonts and structure**: merges duplicates, drops what no page
  can reach.
- **Leaves alone** what it cannot decode exactly. It refuses encrypted
  documents, and owner-password-protected or signed ones unless told
  otherwise.

| Function | Returns |
|---|---|
| `compress(pdf, { dpi, quality, subsetOutlines, stripJpegMetadata, stripProtection, breakSignatures })` | `{ output, log, imagesTotal, imagesRecompressed }` |
| `inspect(pdf)` | One entry per image: size, color space, filters, bytes, drawn size, `dpi` |
| `init(wasm?)`, `initSync(bytes)` | Load the browser build (no-ops in Node) |

Documents that are not processed throw a `PdfdietError` whose `reason` is
`"invalid-pdf"`, `"encrypted"`, `"protected"`, `"signed"` or `"empty"`.
Details, options and errors are in the
[API documentation](https://github.com/sawapi/pdfdiet/blob/main/docs/npm-api.md).

## Requirements

Node 22.12 or later, or a current browser (directly or through a bundler).
TypeScript types need TypeScript 5.7 or later.

## License

MIT OR Apache-2.0, at your option. The Rust library is
[`pdfdiet` on crates.io](https://crates.io/crates/pdfdiet).
