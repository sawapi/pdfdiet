# pdfdiet

[![crates.io](https://img.shields.io/crates/v/pdfdiet.svg)](https://crates.io/crates/pdfdiet)
[![docs.rs](https://img.shields.io/docsrs/pdfdiet)](https://docs.rs/pdfdiet)
[![npm](https://img.shields.io/npm/v/@sawapi/pdfdiet.svg)](https://www.npmjs.com/package/@sawapi/pdfdiet)
[![CI](https://github.com/sawapi/pdfdiet/actions/workflows/ci.yml/badge.svg)](https://github.com/sawapi/pdfdiet/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/pdfdiet.svg)](#license)

Makes PDFs smaller: images are resampled to the resolution they are
actually drawn at, and fonts and structure are optimized losslessly.
Anything it cannot handle with confidence is left as it was. Written in
Rust; the same code runs as WebAssembly in Node and the browser.

[Website](https://sawapi.github.io/pdfdiet/) ·
[API docs](https://docs.rs/pdfdiet) ·
[npm package](https://www.npmjs.com/package/@sawapi/pdfdiet) ·
[Changelog](https://github.com/sawapi/pdfdiet/blob/main/CHANGELOG.md)

## Install

```sh
npx @sawapi/pdfdiet --help     # run the CLI without installing
npm install @sawapi/pdfdiet    # the CLI and the library for Node and the browser
cargo install pdfdiet          # the CLI, built from source
cargo add pdfdiet              # the Rust library
```

The [website](https://sawapi.github.io/pdfdiet/) runs it in your browser;
the file never leaves your device.

## Example

```sh
# what a file is made of: images, encodings, measured resolution
pdfdiet inspect report.pdf

# compress (defaults: --dpi 150 --quality 80)
pdfdiet compress report.pdf -o report.small.pdf
```

From Rust:

```rust
use pdfdiet::{CompressOptions, compress_bytes};

let input = std::fs::read("report.pdf")?;
let opts = CompressOptions::default().target_dpi(150.0);
let (output, stats) = compress_bytes(&input, &opts)?;
std::fs::write("report.small.pdf", output)?;
for line in &stats.log {
    println!("{line}"); // what was changed, and what was left alone and why
}
```

## Features

- **Measures** the resolution each image is drawn at on the page, and
  downsamples only images above the target.
- **Re-encodes** gray, RGB and CMYK images as JPEG, bilevel scans
  losslessly as CCITT G4, and keeps the original whenever it is smaller.
- **Optimizes fonts**: merges duplicate subsets, drops tables, glyphs and
  subroutines no page can reach, and converts Type 1 programs to the more
  compact CFF.
- **Cleans up structure**: merges identical streams, drops editing data
  and rewrites the file with compressed object streams.
- **Leaves alone** what it cannot decode exactly. It refuses encrypted
  documents, and owner-password-protected or signed ones unless told
  otherwise.
- **Handles untrusted files**: no `unsafe`, and every decompression and
  content walk runs under limits.

## Documentation

- [What it does](https://github.com/sawapi/pdfdiet/blob/main/docs/what-it-does.md):
  images, fonts and structure, step by step
- [Command line](https://github.com/sawapi/pdfdiet/blob/main/docs/cli.md):
  every command and flag
- [Safety](https://github.com/sawapi/pdfdiet/blob/main/docs/safety.md):
  what is refused or left alone, and how that is tested
- [Limitations](https://github.com/sawapi/pdfdiet/blob/main/docs/limitations.md):
  what passes through unchanged
- [API reference](https://docs.rs/pdfdiet) on docs.rs; the
  [JavaScript API](https://github.com/sawapi/pdfdiet/blob/main/docs/npm-api.md)
- [Development](https://github.com/sawapi/pdfdiet/blob/main/docs/development.md):
  building the npm package and the website, running the tests
- [Provenance](https://github.com/sawapi/pdfdiet/blob/main/THIRD_PARTY.md):
  the specifications it is written from, and the dependency licenses

## Requirements

Rust 1.88 or later. The npm package needs Node 22.12 or later, or a
current browser.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/sawapi/pdfdiet/blob/main/LICENSE-APACHE)
or [MIT license](https://github.com/sawapi/pdfdiet/blob/main/LICENSE-MIT) at your option.
