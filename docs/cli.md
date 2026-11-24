# Command line

The same command comes from npm (`npx @sawapi/pdfdiet`) and from Rust
(`cargo install pdfdiet`). Both produce the same output files; the Rust
build prints a few more counts in its summary.

## `pdfdiet inspect <in.pdf>`

Lists the images in a document, with the resolution each is drawn at:

```
     obj     size(px)   bpc  colorspace      filters                      bytes        dpi
     5 0   2480x3508      8  ICCBased(N=3)   DCTDecode                  2561176        300

1 images, 2561176 bytes total in image streams
```

| Column | Meaning |
|---|---|
| `obj` | PDF object number and generation |
| `size(px)` | Width and height in pixels |
| `bpc` | Bits per color component |
| `colorspace` | Color space; `ICCBased(N=3)` is an ICC profile with three components |
| `filters` | How the image is encoded (`DCTDecode` is JPEG, `CCITTFaxDecode` fax, `FlateDecode` zlib) |
| `bytes` | Encoded size of the image |
| `dpi` | Effective resolution at the largest size any page draws it; two numbers when the axes differ by more than 5%, `?` when no page draws it |

Images above the target resolution are the ones `compress` downsamples.

## `pdfdiet compress <in.pdf> -o <out.pdf> [options]`

```
obj 5 0: 2561176 -> 266632 bytes (downsampled to 1240x1754) [gray]

1/1 images recompressed
2565124 -> 267598 bytes (10.4%)
```

The first lines are the log: one line per change, and per image or font
that was left alone, with the reason. The output is written only when
compression succeeds, through a temporary file renamed into place, so
`-o` may name the input.

| Option | Default | |
|---|---|---|
| `-o`, `--output <file>` | (required) | Where to write the result |
| `--dpi <n>` | 150 | Target effective resolution. Images drawn above it are downsampled to it; others keep their size. Must be positive. |
| `--quality <n>` | 80 | JPEG quality, 1 to 100, for images that are re-encoded |
| `--subset-outlines` | off | Also drop the outlines of embedded glyphs that no text in the document shows. Smaller, but text added to the file later can no longer use them. |
| `--strip-jpeg-metadata` | off | Remove EXIF (GPS position, camera, dates), XMP and other metadata from embedded JPEGs, losslessly |
| `--strip-protection` | off | Process a document protected only by an owner password, removing its permission restrictions |
| `--break-signatures` | off | Process a digitally signed document, invalidating its signatures |

Details of each change are in [What it does](what-it-does.md).

## Errors

A document that is not processed exits with status 1 and a one-line
message on standard error:

```
error: document is digitally signed; compressing would invalidate the signatures — pass --break-signatures to do that explicitly
```

The npm CLI prints the same messages prefixed `pdfdiet:` instead of
`error:`. Documents that are refused, and why, are described in
[Safety](safety.md#refused-documents).
