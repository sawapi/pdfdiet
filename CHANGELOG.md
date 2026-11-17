# Changelog

## 0.2.0

### npm

- A typed JavaScript API replaces the raw bindings. `compress(input,
  options)` takes an options object (`dpi`, `quality`, `subsetOutlines`,
  `stripJpegMetadata`, `stripProtection`, `breakSignatures`) and returns a
  plain object (`output`, `log` as lines, `imagesTotal`,
  `imagesRecompressed`); `inspect(input)` returns the images as data;
  documents that are not processed throw `PdfdietError` with a `reason`.
  The browser build is `import { init, compress } from
  "@sawapi/pdfdiet/web"`. This replaces `compress_pdf` and `inspect_pdf`.
- The browser build loads with `await init()` (the default export) or
  `initSync(bytes)`; the 0.1.x forms `init({ module_or_path })` and
  `initSync({ module })` are still accepted. The Node build has the same
  three as no-ops, so browser code also runs under Node.
- The package is ESM with type declarations (TypeScript 5.7 or later);
  `require()` needs Node 22.12 or later.
- CLI: `--strip-protection` and `--break-signatures`, as in the Rust CLI.
  A refused or unparsable document, or a file that cannot be read or
  written, prints a one-line error; `--dpi` and `--quality` are checked (an
  out-of-range quality used to wrap around silently).

### Rust

- `CompressOptions`, `CompressStats`, `Error` and `Refusal` are
  `#[non_exhaustive]`, so later options and outcomes are not breaking
  changes. Build options with the setters:
  `CompressOptions::default().target_dpi(200.0).subset_outlines(true)`.
- lopdf types are no longer part of the API: `Error::Pdf` holds an opaque
  `PdfError` (with no `From<lopdf::Error>`), `inspect_bytes` returns
  `Error`, and `compress_document`,
  `collect_images`, `load_options`, `compact_save_options`, `Extent` and
  the `ccitt` module are private. `compress_bytes` and `inspect_bytes` are
  the entry points.
- `ImageInfo` has typed accessors (`object_id`, `width`, `height`,
  `bits_per_component`, `color_space`, `components`, `filters`,
  `stream_len`, `drawn_size`) instead of public fields; `dpi()` returns
  `None` when the pixel size is missing or invalid; `dpi_label` is private.
- `Refusal` messages no longer name CLI flags (the CLI adds the hint), so
  they read right from the library too.
- CLI: `--quality` must be 1-100 and `--dpi` positive.
- Output PDFs are byte-for-byte the same as 0.1.1.

## 0.1.1

- The npm package is `@sawapi/pdfdiet` (`npx @sawapi/pdfdiet ...`); the
  command it installs is still `pdfdiet`. The README now says so.
- Package metadata points to the landing page,
  https://sawapi.github.io/pdfdiet/.
- The WebAssembly module is size-optimized: 1.8 MB down to 1.4 MB.
- Releases are published from CI with trusted publishing.
- The library and CLI forbid unsafe code.

## 0.1.0

First public release.

- `pdfdiet compress`: measured-DPI image downsampling and recompression
  (gray, RGB, CMYK, palette and bilevel images; soft masks and
  premultiplied-alpha pairs kept consistent), lossless JPEG re-optimization,
  font subset merging and union, unused font table and glyph removal,
  Type 1 to CFF conversion, structural deduplication and object streams.
- Opt-ins: `--subset-outlines` (drop unused glyph outlines),
  `--strip-jpeg-metadata` (remove EXIF/GPS, XMP and other JPEG metadata
  losslessly), `--strip-protection`, `--break-signatures`.
- `pdfdiet inspect`: list images with their measured effective DPI.
- Library API (`compress_bytes`, `inspect_bytes`), usable without the CLI
  via `default-features = false`.
- Hard limits on decompression, image headers, content walks and font
  program execution for untrusted input.
