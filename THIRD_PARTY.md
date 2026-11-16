# Provenance and third-party record

This file is a factual record kept for provenance and license-hygiene
purposes.

## Clean-room statement

pdfdiet's transforms are implemented from public specifications and observed
input/output behavior only:

- PDF: ISO 32000
- Bilevel image coding: ITU-T T.6 (Group 4)
- JPEG: ITU-T T.81
- Fonts: OpenType/TrueType specifications; Adobe Type 1 Font Format; Adobe
  CFF (Type 2 charstring) specifications

No source code of any copyleft PDF or image tool was consulted during
development. Copyleft tools were run as black boxes for benchmarking and
output comparison; only their inputs and outputs were observed.

## Direct dependencies

| crate | license | role |
|---|---|---|
| lopdf | MIT | PDF object model, parsing and serialization |
| image | MIT OR Apache-2.0 | JPEG decoding (kept deliberately on a widely-used decoder) |
| clap | MIT OR Apache-2.0 | CLI argument parsing (CLI builds only) |
| smallpeg | MIT OR Apache-2.0 | JPEG encoding, lossless JPEG re-optimization, resampling |
| wasm-bindgen | MIT OR Apache-2.0 | WASM bindings (browser builds only) |

The full transitive inventory of everything linked into the published
WebAssembly module, with licences, is generated into `npm/THIRD_PARTY.md`
by `tools/gen_licenses.sh` and ships inside the npm package. No crate in
the tree is under a copyleft licence.
