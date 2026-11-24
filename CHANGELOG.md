# Changelog

## 0.5.0

### All builds

- Subroutines no glyph calls are emptied in embedded bare CFF font
  programs (Type1C and CID-keyed). Subset fonts often keep the whole
  font's subroutines although their few glyphs call a handful: a music
  score whose 14 such fonts carry them comes out at 78% of its original
  size instead of 95%. Every glyph draws the same (subroutine numbers and
  charstrings are unchanged); a font this cannot follow exactly is left
  as it is. The log reports each program as
  `font program N G: CFF K uncalled subroutines emptied (A -> B bytes uncompressed)`.

## 0.4.0

### Rust

- Images are processed on several threads (up to 8) in the native
  library and CLI, with the same output as before: about twice as fast
  on image-heavy documents. Images run together only while their pixels
  add up to at most 100 million, the limit for a single image (a JPEG
  counts its own frame size if larger than declared), so peak memory
  stays close to what one large image already needs. Images that read
  other images (a soft mask, a palette stored in an image) wait for them
  as before. The WebAssembly build is unchanged (one thread).

### All builds

- A document whose object stream cannot be read (it does not parse, or
  inflates past the limit) is no longer compressed. The loader drops such
  a stream with every object in it, so the output would have lost them,
  and a signature stored there would have gone unnoticed. It now fails as
  an invalid PDF (`invalid-pdf` in npm).
- An image that also serves as the lookup table of an `/Indexed` color
  space (its bytes are another image's palette) is now left as it is.
  Recompressing it changed that palette, so the other image was drawn in
  the wrong colors. The log reports it as
  `skip obj N G: also an Indexed lookup table`.
- smallpeg 0.16. Output is unchanged.

### Command-line tools

- Both command-line tools write the output to a temporary file next to it
  and rename it into place, so a failed write never leaves a truncated
  file, even when `-o` names the input. A symlinked output is followed,
  and an existing file keeps its permissions.

## 0.3.0

### npm

- `@sawapi/pdfdiet/worker` runs `compress` and `inspect` in a Web
  Worker. `createWorker()` returns the same functions returning promises,
  plus `ready` and `terminate()`; errors arrive as the same classes,
  `PdfdietError` with its `reason`. The website uses it.
- After a WebAssembly error (a trap, such as running out of memory) the
  worker stops and rejects later calls, instead of reusing a module that
  may be in an inconsistent state.

### All builds

- Signed documents: usage-rights and certification signatures referenced
  only from the catalog's `/Perms`, signature dictionaries without `/Type`,
  and an indirect `/SigFlags` are now recognized, so such documents are
  refused unless signatures may be broken.
- Hardening against hostile documents, from a source review. Output for
  ordinary documents is unchanged.
  - 1-bit images wider or taller than 65535 pixels are left alone, as on
    every other image path, and CCITT encoding stops as soon as it is no
    smaller than the original.
  - Embedded TrueType fonts whose table directory repeats a tag, or whose
    tables claim more bytes than the font holds, are not rebuilt.
  - With `subsetOutlines`, a TrueType font whose glyph offsets go
    backwards is left whole.
  - Type 1 glyphs over the Type 2 limit of 96 hints are rejected without
    quadratic work, and glyph ordering is linear.
  - Content walks count the bytes inflated from forms (1 GiB per walk),
    not only their parse cost.
  - Offset checks in the CFF and TrueType readers no longer rely on
    additions that can wrap in the 32-bit WebAssembly build.
- Both command-line tools escape control characters in what they print
  (names in the log and the image table come from the document), so a
  crafted PDF cannot send escape sequences to the terminal.
- smallpeg 0.15: fixes a panic on crafted Huffman frequency patterns,
  rejects progressive JPEGs whose successive-approximation scans are out of
  order (such images are now left as they are), and keeps APPn/COM segments
  that follow the frame header in place. Output for ordinary documents is
  unchanged.

### Website

- A document refused for its owner password (restrictions on printing,
  copying or editing) now offers a button to remove the restrictions and
  compress it. The choice is per file, and the result says the
  restrictions were removed. Encrypted and signed documents are still
  never processed on the page.
- A Content-Security-Policy limits the page to its own scripts, styles
  and workers.

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

### Documentation

- The READMEs follow the usual crate and npm layout (install, example,
  features, requirements), with the details moved to `docs/`: what it
  does, the command line, safety, limitations, the JavaScript API and
  development. The website's code samples are syntax-colored.

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
- Built on smallpeg 0.14.
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
