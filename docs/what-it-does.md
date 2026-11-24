# What pdfdiet does

pdfdiet makes three kinds of change to a PDF: it resamples and re-encodes
images, optimizes embedded fonts, and cleans up the file structure. Every
change is either lossless, or a bounded-quality image re-encoding you asked
for with `--dpi` and `--quality`. The log (`stats.log` in the library,
printed by the CLI) lists what was changed, and what was left alone and why.

## Images

**Measured resolution.** Each page's content stream is interpreted
(transformation matrices, nested Form XObjects, soft masks), so pdfdiet
knows the largest size each image is drawn at. Its effective resolution is
pixels divided by that size. Only images more than 5% above the target
(`--dpi`, default 150) are downsampled, and only on the axis that exceeds
it.
`pdfdiet inspect` prints these measurements. If a page's content cannot be
interpreted, the images it uses are treated as drawn at the full page size.

**Re-encoding.** A downsampled image is re-encoded as JPEG at `--quality`
(default 80). How color is kept depends on the image: scans that are gray
in practice become one channel, sharp colored artwork keeps full color
resolution, photographs get standard chroma subsampling.

- **CMYK** images (JPEG or raw) are resampled and re-encoded as four
  channels, never converted to RGB; the file's stored values and Adobe
  marker are carried over.
- **Bilevel** images (1 bit per pixel) are re-coded losslessly as CCITT
  Group 4.
- **Two-color** images become exact 1-bit palettes.
- **Palette (Indexed)** images are expanded through their palette into the
  base color space (gray, RGB or CMYK, ICC profile kept) and treated like
  any other image; after downsampling, a lossless encoding is used when it
  is smaller.
- **Premultiplied alpha** (a soft mask carrying `/Matte`): the image and
  its mask are resampled together, so they keep matching sizes.
- **JPEGs that are not downsampled** are still re-coded losslessly (same
  pixels, smaller entropy coding) when that saves bytes.

Whatever the path, the original bytes are kept when the new encoding would
not be smaller.

**Metadata** (`--strip-jpeg-metadata`, off by default). Removes EXIF (GPS
position, camera, dates, thumbnails), XMP, IPTC, comments and data after
the image from the JPEGs that remain in the output. It is lossless: the
image data and the color information (JFIF, ICC profile, Adobe marker) are
kept byte for byte. PDF viewers never show this metadata, but it is off by
default because it discards what the author embedded. Photos pasted from a
phone often carry their GPS position this way.

## Fonts

- **Duplicate subsets** (a typical paper embeds one per included figure)
  are merged when they are provably identical.
- **Subsets of the same font** that keep its original glyph numbering
  (office suites embed one per section) are merged into one program holding
  the union of their outlines, when every outline, hinting program and
  metric that matters agrees.
- **OpenType layout tables** (GSUB, GPOS and friends) are dropped from
  embedded TrueType programs: a PDF names and positions every glyph itself,
  so they are never used.
- **Glyph slots** past the last one the document can reach are dropped,
  without renumbering anything.
- **Subroutines** that no glyph calls are emptied in CFF programs (subsets
  often keep all of the full font's). Every subroutine keeps its number,
  so each glyph draws the same.
- **Type 1** programs are converted to CFF, which is more compact, with the
  numeric precision of the outlines kept.

**Unused outlines** (`--subset-outlines`, off by default). Also empties the
outlines of embedded glyphs that no text in the document shows, in
TrueType programs and in CID-keyed CFF programs of Identity-encoded fonts.
Glyph numbering is kept, so the document renders and its text extracts as
before. It is off by default because it turns embedded fonts into true
subsets: text added to the file later can no longer use the dropped glyphs.

## Structure

- Byte-identical streams, and dictionaries that are pure values (fonts,
  font descriptors, encodings, graphics states, transparency groups), are
  merged into one copy.
  Dictionaries whose identity matters, such as pages, annotations or
  optional-content groups, are never merged.
- Application-private editing data (`/PieceInfo`, which can hold a
  figure's full editable source), page thumbnails (viewers regenerate
  them) and alternate image versions are removed.
- Uncompressed streams are compressed with zlib, and the file is written
  with compressed object streams and a cross-reference stream.

## What it never does

It does not flatten form fields or transparency, and does not remove
accessibility structure. See [Safety](safety.md) for what is refused, and
[Limitations](limitations.md) for what passes through unchanged.

## Typical results

Measured with the pdfdiet 0.2.0 CLI at default settings (output size as a
share of the input):

| Document | Result |
|---|---|
| A one-page 300 dpi color scan | 10% |
| arXiv:1706.03762 (15 pages, text and figures) | 42% |
| arXiv:2006.11239 (photographic figures) | 28% |
| arXiv:2112.10752 (many photographic figures) | 16% |

On two Japanese government PDFs with large embedded CJK fonts,
`--subset-outlines` took the result from 64% to 37% (TrueType fonts) and
from 96% to 32% (CFF fonts) of the original.

Results depend entirely on what a file is made of: run `pdfdiet inspect`
first. A file that is mostly vector graphics and text that is already
compressed may hardly shrink.
