# Development

## Rust

```sh
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

The crate builds against `smallpeg` from a sibling checkout
(`path = "../smallpeg"`), falling back to the crates.io release when
packaged. Clone both side by side:

```
work/
  pdfdiet/
  smallpeg/
```

The library builds without the command line tool's dependencies with
`--no-default-features`.

## npm package

```sh
cd npm && npm ci && cd ..   # once: the TypeScript compiler
./build-npm.sh              # wasm for Node (npm/node) and browsers (npm/web), then npm/dist
cd npm && npm test          # smoke tests, init forms, type tests
```

`build-npm.sh` needs the `wasm-bindgen` CLI at the version in
`Cargo.lock` (it says which if not), and runs Binaryen's `wasm-opt` when
installed. The typed API is in `npm/src/*.mts`; the wasm-bindgen output in
`npm/node` and `npm/web` is internal to the package.

## Website

The landing page in `web/` doubles as the browser version: it compresses
in a Web Worker through the package's browser build.

```sh
./build-site.sh                      # builds the npm package, then the site into web/dist
python3 -m http.server -d web/dist   # any static server works
```

`SKIP_WASM=1 ./build-site.sh` reuses an existing npm build.

## Robustness runs

```sh
cargo run --release --example mutpdf -- <in.pdf> <outdir> <n> <seed>
```

writes `n` loadable copies of a PDF with corrupted streams. pdfdiet must
neither panic nor hang on them, and its output must open and extract the
same text as the corrupted input. The font parsers have a fuzz test of
their own, ignored by default, that mutates the fonts embedded in the PDFs
you give it:

```sh
FONTFUZZ_SEEDS="a.pdf b.pdf" cargo test --release fontfuzz -- --ignored
```

## Licenses

`tools/gen_licenses.sh` regenerates `npm/THIRD_PARTY.md`, the license
inventory of everything linked into the WebAssembly module.
