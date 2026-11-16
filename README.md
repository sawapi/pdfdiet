# pdfdiet

Put your PDFs on a diet. Measured-DPI image downsampling, lossless font and
structure optimization, and a design rule that anything it cannot decode with
confidence is left byte-for-byte alone. Runs from `npx`, or entirely in your
browser — nothing is ever uploaded.

Permissively licensed (MIT OR Apache-2.0), written in Rust, compiled to WASM.

## Why

Shrinking a PDF well means touching three different worlds — image codecs,
font programs, and the object graph itself — which is why the tools that do
it well are few, and why the permissive corner of the ecosystem mostly stops
at lossless structural cleanup.

pdfdiet takes the whole problem, under a permissive license, with one rule:
every transform is either provably lossless, or a bounded-quality image
recompression you asked for. Anything else — an encoding it cannot decode,
an object whose identity might matter, a document whose protection or
signature it would invalidate — is refused or passed through untouched, and
said out loud in the log rather than silently.

## Usage

Via npm (recommended — CLI + Node/browser library, no Rust toolchain needed):

```
npx @sawapi/pdfdiet inspect  file.pdf             # list images: size, colorspace, filters, measured DPI
npx @sawapi/pdfdiet compress in.pdf -o out.pdf    # defaults: --dpi 150 --quality 80
npx @sawapi/pdfdiet compress in.pdf -o out.pdf --subset-outlines   # also drop unused glyph outlines
npx @sawapi/pdfdiet compress in.pdf -o out.pdf --strip-jpeg-metadata   # remove EXIF/GPS etc. from JPEGs
```

`--subset-outlines` additionally empties the outlines of embedded glyphs
that no text in the document shows, in TrueType programs and in CID-keyed
CFF programs of Identity-encoded fonts (glyph numbering is kept, so the
document renders and extracts exactly as before). It is off by default
because it turns embedded fonts into true subsets: text added to the file
later can no longer use the dropped glyphs. On two Japanese government PDFs
with generous CJK subsets it took the result from 64% to 37% (TrueType) and
from 93% to 32% (CFF) of the original.

`--strip-jpeg-metadata` removes metadata from the embedded JPEG images that
remain in the output: EXIF (GPS position, camera, dates, thumbnails), XMP,
IPTC, comments and data appended after the image. It is lossless (the
image data and the color information, i.e. JFIF, ICC profile and Adobe
segments, are kept byte for byte) and PDF viewers never show this data, but
it is off by default because it discards what the author embedded. Photos
pasted from a phone often carry their GPS position this way.

See `npm/README.md` for the JS library API. The npm package is built with
`./build-npm.sh`. The Rust CLI is the same tool: `cargo run -- compress ...`.

On the project's own test corpus, a 300 dpi scanned page lands around 90%
smaller at default settings, and a font-heavy 15-page paper (a public arXiv
preprint) lands at 44% of its original size. Your mileage will differ — run
`inspect` first to see what a given file is actually made of.

## What it does

**Images** — every page's content stream is interpreted (CTM tracking,
nested Form XObjects, soft masks) to *measure* each image's rendered
resolution, so downsampling is exact, never guessed. Chroma handling adapts
per image: visually-grayscale scans drop to one channel, sharp colored art
keeps full chroma, photographs get standard subsampling. Bilevel images are
recompressed losslessly (CCITT G4). CMYK images (JPEG or raw) are resampled
and re-encoded as four channels, never converted to RGB, with each file's
stored values and Adobe marker carried over unchanged. Images with
premultiplied alpha (a soft mask carrying /Matte) are resampled together
with their mask, so the pair keeps matching dimensions and color components.
Palette (Indexed) images are expanded through their palette into the base
color space (gray, RGB or CMYK, ICC profile kept) and then treated like any
other image; once downsampled, a lossless encoding wins when it is smaller. JPEGs that can't be beaten are still
entropy-optimized losslessly. A size guard keeps the original bytes whenever
recompression would lose.

**Fonts** — duplicated font subsets (one per included figure) are merged when
provably identical; subsets that keep the font's original glyph numbering
(office-suite exports embed one per section) are merged into a single program
holding the union of their outlines, when every outline, hinting program
and metric that matters agrees; OpenType layout tables (GSUB/GPOS and
friends), which a PDF never applies since it names glyphs and positions
them itself, are dropped from embedded TrueType programs; glyph slots past
the last one the document reaches are dropped without renumbering anything
(and, with `--subset-outlines`, unused outlines too); and Type 1 font
programs are converted to compact CFF with numeric fidelity preserved.

**Structure** — byte-identical streams and dictionaries are deduplicated,
application-private editing data and regenerable baggage are stripped, and
the file is rewritten with compressed object streams.

**What it never does** — flatten form fields, discard accessibility
structure, or flatten transparency. Encodings it cannot decode with
confidence pass through byte-for-byte.

## How the safety claims are tested

During development, every change is checked against a fixed corpus of real
documents (scans, papers, office exports, government forms, CJK documents)
by rendering each page before and after with several independent renderers
and comparing the pixels, alongside extracted text compared byte for byte.
The lossless transforms are held to bit-identical renders; the Type 1 to
CFF conversion is additionally checked glyph by glyph against documented
error bounds. Lossy output must keep identical extracted text and stay
within a per-tile render-difference bound that ordinary recompression
stays well inside and a decoding bug (an inverted or blackened image)
exceeds many times over; QR codes on the page must still decode.

Robustness is exercised with `examples/mutpdf.rs`, which corrupts the
streams of loadable PDFs: pdfdiet must neither panic nor hang on them, and
its output must open and extract the same text as the corrupted input. The
font parsers are fuzzed the same way (an ignored test, `fontfuzz`), and
every decompression, image header and content walk runs under hard limits
so that a small hostile file cannot exhaust memory or time.

Be clear about what that does and does not establish. It is a
development-time regression check over a fixed corpus, not a per-file
guarantee, and it does not run on your document. It compares rendered pages
and extracted text, so it says nothing about behavior a renderer does not
exercise: form logic, embedded JavaScript, non-default optional-content
states, tagged-structure order, or print-path output. Transforms that would
disturb those are gated by refusal rules in the code rather than by this
check. Bug reports with a reproducing file are the most useful thing you can
send.

## Browser version

The landing page in `web/` doubles as the browser version: drop a PDF, tune
resolution, quality and the opt-ins, download the result. All processing
happens locally in WASM, in a Web Worker.

```
./build-site.sh                      # builds the wasm, then the site into web/dist
python3 -m http.server -d web/dist   # any static server works
```

`SKIP_WASM=1 ./build-site.sh` reuses an existing `npm/web` build.

## Limitations

JPX-encoded images, image masks, palettes over other color spaces (Lab,
Separation, ...), and images with a TIFF predictor, a color-key mask or a
non-default /Decode are left exactly as-is. (PNG predictors matching the
image, a default /Decode and empty /DecodeParms are understood.)

Rewriting a PDF necessarily invalidates a digital signature, and stripping
encryption would silently remove a document's protection, so encrypted,
permission-protected and signed documents are refused rather than quietly
altered. `--strip-protection` and `--break-signatures` proceed anyway, if
that is what you want.

Note that this catches more than it first appears. Many fillable government
forms carry a usage-rights signature — the thing that lets a free PDF reader
save your entries at all — and recompressing the file would break it while
leaving the form fields visibly intact. Those documents are refused too.

## Provenance

Written from the public specifications (ISO 32000, ITU-T T.6/T.81, OpenType,
Adobe Type 1/CFF). See `THIRD_PARTY.md` for the provenance statement and the
dependency licenses.

## License

MIT OR Apache-2.0, at your option.
