# Safety

pdfdiet's rule is that every change is either lossless, or an image
re-encoding at the resolution and quality you asked for. Anything else is
refused or passed through unchanged, and the log says so.

## Refused documents

These are refused unless you explicitly allow it:

| Document | Why | To go ahead anyway |
|---|---|---|
| Password-protected (encrypted) | The contents cannot be read; any output would be unreadable | Decrypt it first |
| Protected by an owner password only (no-print, no-copy and similar) | pdfdiet cannot re-encrypt, so the output would silently lose the restrictions | `--strip-protection` (`stripProtection`, `strip_protection`) |
| Digitally signed | Any rewrite invalidates the signatures | `--break-signatures` (`breakSignatures`, `break_signatures`) |
| No readable pages | There is nothing to work on | — |

On the [website](https://sawapi.github.io/pdfdiet/), a document refused
for its owner password offers a button to remove the restrictions and
compress it; nothing else there can be overridden.

The signed case covers more than it first appears. Many fillable
government forms carry a usage-rights signature, which is what lets a free
PDF reader save your entries at all. Recompressing such a file would break
it while leaving the form fields visibly intact, so those are refused too.

## Left alone

Within a document that is processed, anything pdfdiet cannot decode
exactly is left as it is: see [Limitations](limitations.md). Objects whose
identity may carry meaning (pages, annotations, optional-content groups and
the like) are never merged with look-alikes. Form fields, transparency and accessibility structure are
never flattened or removed.

## Untrusted files

The library and CLI contain no `unsafe` code. Every decompression, image
header and content-stream walk runs under fixed limits, checked before
memory is allocated, so a small hostile file cannot exhaust memory or
time. A file that cannot be parsed is reported as an error, not a crash.

## How this is tested

During development, every change is checked against a fixed corpus of real
documents (scans, papers, office exports, government forms, CJK documents):

- Each page is rendered before and after with several independent
  renderers, and the pixels are compared. The lossless changes must render
  bit-identically. The Type 1 to CFF conversion is also checked glyph by
  glyph against documented error bounds.
- Extracted text must be byte-identical before and after.
- Re-encoded images must stay within a per-tile difference bound that
  ordinary JPEG re-encoding stays well inside, and that a decoding bug (an
  inverted or blackened image) exceeds many times over. QR codes on the
  page must still decode.

Robustness is exercised with `examples/mutpdf.rs`, which corrupts the
streams of loadable PDFs: pdfdiet must neither panic nor hang on them, and
its output must open and extract the same text as the corrupted input. The
font parsers are fuzzed the same way (an ignored test, `fontfuzz`).

### What that does not establish

It is a regression check over a fixed corpus during development, not a
guarantee for any particular file, and it does not run on your document. It
compares rendered pages and extracted text, so it says nothing about
behavior a renderer does not exercise: form logic, embedded JavaScript,
non-default optional-content states, the order of tagged structure, or
printing. Changes that could disturb those are prevented by the refusal
and pass-through rules in the code, not by this check.

Bug reports with a file that reproduces the problem are the most useful
thing you can send.
