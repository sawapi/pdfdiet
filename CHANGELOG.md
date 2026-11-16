# Changelog

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
