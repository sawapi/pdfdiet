//! pdfdiet — squeeze PDFs by downsampling and recompressing embedded images,
//! and by optimizing fonts and structure losslessly.
//!
//! The core API is filesystem-agnostic (`&[u8]` in, `Vec<u8>` out):
//!
//! ```no_run
//! use pdfdiet::{compress_bytes, CompressOptions};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let input = std::fs::read("in.pdf")?;
//! let opts = CompressOptions::default().target_dpi(150.0).jpeg_quality(80);
//! let (output, stats) = compress_bytes(&input, &opts)?;
//! std::fs::write("out.pdf", output)?;
//! // what was changed, and what was left alone and why
//! for line in &stats.log {
//!     println!("{line}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Documents that cannot be changed safely (encrypted, owner-password
//! protected or digitally signed) are refused with [`Error::Refused`]
//! unless [`CompressOptions`] explicitly allows it.
//!
//! The command-line tool is behind the default `cli` feature; library users
//! can depend on the crate with `default-features = false`.

// Style lints whose suggested rewrites would not make this code clearer:
// the font passes index parallel tables by glyph id, and a few internal
// helpers return tuples or take many plain parameters.
#![forbid(unsafe_code)]
#![allow(
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::too_many_arguments
)]

use image::DynamicImage;
use lopdf::{Dictionary, Document, Object, ObjectId};

mod ccitt;
mod cffsubrs;
mod cffsubset;
mod decode;
mod dedup;
#[cfg(test)]
mod fontfuzz;
mod fontmerge;
mod fontunion;
mod images;
mod jpeg;
mod limits;
mod parallel;
mod recompress;
mod subset;
mod type1;

pub use images::ImageInfo;

use images::{collect_images, indexed_lookups};
use limits::load_options;

use decode::{decode_image, indexed_palette};
use dedup::dedup_streams;
use fontmerge::merge_font_subsets;
use images::target_size;
use jpeg::{
    ChromaClass, CmykOutcome, classify_chroma, encode_jpeg, recompress_cmyk_jpeg,
    strip_jpeg_metadata, try_lossless_jpeg_optimize,
};
use recompress::{
    BilevelOutcome, MatteHalf, Staged, TwoColor, bleed_under_soft_mask, recompress_bilevel,
    recompress_matte_pairs, recompress_two_color, resize_image, smallest_encoding,
};

// Shared with the font modules, which reach them through the crate root.
pub(crate) use images::page_resource_dicts;
pub(crate) use limits::{MAX_INFLATE, MAX_PASS_BYTES, Walk, inflate, page_content_bounded};

/// Settings for [`compress_bytes`]. Start from [`Default`] and change what
/// you need with the setters:
///
/// ```
/// let opts = pdfdiet::CompressOptions::default()
///     .target_dpi(200.0)
///     .subset_outlines(true);
/// assert_eq!((opts.target_dpi, opts.jpeg_quality), (200.0, 80));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CompressOptions {
    /// Target effective DPI; images rendered above this are downsampled.
    /// Default 150.
    pub target_dpi: f64,
    /// JPEG quality for re-encoded images, 1-100. Default 80.
    pub jpeg_quality: u8,
    /// Process a document whose only protection is an owner password
    /// (permission restrictions such as no-print / no-copy). pdfdiet cannot
    /// re-encrypt, so its output would be an unprotected file — refused
    /// unless explicitly allowed here.
    pub strip_protection: bool,
    /// Process a digitally signed document. Any rewrite invalidates the
    /// signatures (their ByteRange covers the original bytes) — refused
    /// unless explicitly allowed here.
    pub break_signatures: bool,
    /// Also empty the outlines of embedded glyphs no text in the document
    /// shows (keeping glyph numbering): TrueType programs, and CID-keyed CFF
    /// programs of Identity-encoded Type0 fonts. Off by default: it turns an
    /// embedded font into a true subset, so text added later (by editing the
    /// file) can no longer use the glyphs dropped. Rendering of the document
    /// as it is does not change, and anything the usage walker cannot
    /// interpret disables it, as for all subsetting.
    pub subset_outlines: bool,
    /// Remove metadata from every embedded JPEG that remains in the output:
    /// EXIF (GPS position, camera, dates, thumbnails), XMP, IPTC, comments
    /// and data after the image (smallpeg::strip_metadata). Lossless: the
    /// pixels and the color information (JFIF, ICC, Adobe) are kept byte
    /// for byte. Off by default, as it discards information the author
    /// embedded; PDF viewers never show it.
    pub strip_jpeg_metadata: bool,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            target_dpi: 150.0,
            jpeg_quality: 80,
            strip_protection: false,
            break_signatures: false,
            subset_outlines: false,
            strip_jpeg_metadata: false,
        }
    }
}

impl CompressOptions {
    /// Set [`target_dpi`](CompressOptions::target_dpi).
    pub fn target_dpi(mut self, dpi: f64) -> Self {
        self.target_dpi = dpi;
        self
    }

    /// Set [`jpeg_quality`](CompressOptions::jpeg_quality).
    pub fn jpeg_quality(mut self, quality: u8) -> Self {
        self.jpeg_quality = quality;
        self
    }

    /// Set [`strip_protection`](CompressOptions::strip_protection).
    pub fn strip_protection(mut self, on: bool) -> Self {
        self.strip_protection = on;
        self
    }

    /// Set [`break_signatures`](CompressOptions::break_signatures).
    pub fn break_signatures(mut self, on: bool) -> Self {
        self.break_signatures = on;
        self
    }

    /// Set [`subset_outlines`](CompressOptions::subset_outlines).
    pub fn subset_outlines(mut self, on: bool) -> Self {
        self.subset_outlines = on;
        self
    }

    /// Set [`strip_jpeg_metadata`](CompressOptions::strip_jpeg_metadata).
    pub fn strip_jpeg_metadata(mut self, on: bool) -> Self {
        self.strip_jpeg_metadata = on;
        self
    }
}

/// Why a document was refused rather than compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// Password-protected: the contents could not be decrypted, so any
    /// output would be an unreadable stub.
    Encrypted,
    /// Protected by an owner password only; compressing would silently strip
    /// the permission restrictions (see `CompressOptions::strip_protection`).
    Protected,
    /// Digitally signed; compressing would invalidate the signatures (see
    /// `CompressOptions::break_signatures`).
    Signed,
    /// Parsing produced no usable pages.
    Empty,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Encrypted => {
                "document is password-protected; decrypt it first (the contents cannot be read)"
            }
            Refusal::Protected => {
                "document is protected by an owner password; compressing would strip its \
                 permission restrictions"
            }
            Refusal::Signed => {
                "document is digitally signed; compressing would invalidate the signatures"
            }
            Refusal::Empty => "document has no readable pages",
        })
    }
}

impl std::error::Error for Refusal {}

// Refusal is non_exhaustive, so other crates match it with a wildcard and
// would not notice a new variant. This exhaustive match does: adding one
// fails to compile here until the reason strings in wasm/src/lib.rs and
// npm/src/core.mts (FailureReason) are updated too.
#[cfg(test)]
#[allow(dead_code)]
fn refusal_reasons_are_mapped(r: Refusal) -> &'static str {
    match r {
        Refusal::Encrypted => "encrypted",
        Refusal::Protected => "protected",
        Refusal::Signed => "signed",
        Refusal::Empty => "empty",
    }
}

/// Errors from [`compress_bytes`] and [`inspect_bytes`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The input could not be read or written as a PDF.
    Pdf(PdfError),
    /// The document was readable but deliberately left alone.
    Refused(Refusal),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Pdf(e) => e.fmt(f),
            Error::Refused(r) => r.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// A PDF that could not be parsed (or serialized). The message says what
/// went wrong; the details are not part of the API.
#[derive(Debug)]
pub struct PdfError(lopdf::Error);

impl std::fmt::Display for PdfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for PdfError {}

// Not a From impl: that would put lopdf's error type in the public API.
fn pdf_error(e: lopdf::Error) -> Error {
    Error::Pdf(PdfError(e))
}

impl From<Refusal> for Error {
    fn from(r: Refusal) -> Self {
        Error::Refused(r)
    }
}

/// What [`compress_bytes`] did.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CompressStats {
    /// Image XObjects in the document.
    pub images_total: usize,
    /// Images downsampled, re-encoded or losslessly re-optimized.
    pub images_recompressed: usize,
    /// Duplicate streams merged into one copy.
    pub streams_deduped: usize,
    /// Duplicate font subsets merged.
    pub fonts_merged: usize,
    /// Fonts whose unused tables or glyphs were dropped.
    pub fonts_subset: usize,
    /// Type 1 fonts converted to CFF.
    pub fonts_converted: usize,
    /// Bytes saved in stream data (before the final write's own savings).
    pub stream_bytes_saved: i64,
    /// What was changed, and what was left alone and why, one line each.
    pub log: Vec<String>,
}

/// Compress a PDF byte buffer and return the new PDF bytes.
pub fn compress_bytes(
    input: &[u8],
    opts: &CompressOptions,
) -> Result<(Vec<u8>, CompressStats), Error> {
    let mut doc = Document::load_mem_with_options(input, load_options()).map_err(pdf_error)?;
    check_object_streams(&doc).map_err(pdf_error)?;
    let stats = compress_document(&mut doc, opts)?;
    let mut out = Vec::new();
    doc.save_with_options(&mut out, compact_save_options())
        .map_err(|e| pdf_error(e.into()))?;
    Ok((out, stats))
}

/// Serialization settings for the final write: pack non-stream objects into
/// object streams and use a cross-reference stream (PDF 1.5). On dictionary-
/// heavy documents the plain-text structure can be nearly half the file.
fn compact_save_options() -> lopdf::SaveOptions {
    lopdf::SaveOptions {
        use_object_streams: true,
        use_xref_streams: true,
        linearize: false,
        object_stream_config: Default::default(),
    }
}

/// List the images embedded in a PDF byte buffer.
pub fn inspect_bytes(input: &[u8]) -> Result<Vec<ImageInfo>, Error> {
    let doc = Document::load_mem_with_options(input, load_options()).map_err(pdf_error)?;
    Ok(collect_images(&doc))
}

/// Format inspect results as the same table string the CLI prints.
pub fn format_inspect_table(images: &[ImageInfo]) -> String {
    let mut out = format!(
        "{:>8}  {:>11}  {:>4}  {:<14}  {:<22}  {:>10}  {:>9}\n",
        "obj", "size(px)", "bpc", "colorspace", "filters", "bytes", "dpi"
    );
    for info in images {
        out.push_str(&format!(
            "{:>8}  {:>5}x{:<5}  {:>4}  {:<14}  {:<22}  {:>10}  {:>9}\n",
            format!("{} {}", info.id.0, info.id.1),
            info.width,
            info.height,
            info.bpc,
            info.color,
            info.filters.join("+"),
            info.stream_len,
            info.dpi_label(),
        ));
    }
    let total: usize = images.iter().map(|i| i.stream_len).sum();
    out.push_str(&format!(
        "\n{} images, {} bytes total in image streams",
        images.len(),
        total
    ));
    out
}

/// Recompress the images inside a document (in place).
pub(crate) fn compress_document(
    doc: &mut Document,
    opts: &CompressOptions,
) -> Result<CompressStats, Refusal> {
    check_document(doc, opts)?;
    let images = collect_images(doc);
    let mut stats = CompressStats {
        images_total: images.len(),
        images_recompressed: 0,
        streams_deduped: 0,
        fonts_merged: 0,
        fonts_subset: 0,
        fonts_converted: 0,
        stream_bytes_saved: 0,
        log: Vec::new(),
    };
    if doc.encryption_state.is_some() {
        stats
            .log
            .push("stripping owner-password protection (explicitly allowed)".into());
    }
    if opts.break_signatures && document_is_signed(doc) {
        stats.log.push(
            "document is signed; the signatures will not survive (explicitly allowed)".into(),
        );
    }

    // An image that is also some /Indexed color space's lookup table holds
    // that color space's colors: recompressing it would change them.
    let lookups = indexed_lookups(doc);
    let images: Vec<ImageInfo> = images
        .into_iter()
        .filter(|info| {
            let keep = !lookups.contains(&info.id);
            if !keep {
                stats.log.push(format!(
                    "skip obj {} {}: also an Indexed lookup table",
                    info.id.0, info.id.1
                ));
            }
            keep
        })
        .collect();

    // Premultiplied-alpha pairs must be resampled together; see
    // recompress_matte_pairs. Whatever it handled is done.
    let matte_done = recompress_matte_pairs(doc, &images, &lookups, opts, &mut stats);
    // Images serving as some image's soft mask: these must stay DeviceGray.
    let soft_masks: std::collections::HashSet<ObjectId> = doc
        .objects
        .values()
        .filter_map(|o| match o {
            Object::Stream(s) => match s.dict.get(b"SMask") {
                Ok(Object::Reference(id)) => Some(*id),
                _ => None,
            },
            _ => None,
        })
        .collect();

    let outcomes = process_images(doc, &images, opts, &matte_done, &soft_masks);
    for outcome in outcomes.into_iter().flatten() {
        stats.log.extend(outcome.log);
        stats.stream_bytes_saved += outcome.saved;
        stats.images_recompressed += outcome.recompressed;
    }

    if opts.strip_jpeg_metadata {
        let (stripped, saved) = strip_jpeg_metadata(doc);
        if stripped > 0 {
            stats.log.push(format!(
                "stripped metadata from {stripped} JPEG images ({saved} bytes)"
            ));
        }
        stats.stream_bytes_saved += saved;
    }

    let (fonts_merged, font_saved) = merge_font_subsets(doc, &mut stats.log);
    stats.fonts_merged = fonts_merged;
    stats.stream_bytes_saved += font_saved;

    // Layout tables PDF never consults go first, which also lets more
    // subsets agree table-for-table in the union pass below.
    let (stripped_fonts, strip_saved) = fontunion::strip_unused_font_tables(doc, &mut stats.log);
    let _ = stripped_fonts;
    stats.stream_bytes_saved += strip_saved;

    // Subsets that keep the original glyph numbering become one union
    // program per font (before truncation, which can then trim the union).
    let (unioned, union_saved) = fontunion::union_cid_truetype_subsets(doc, &mut stats.log);
    stats.fonts_merged += unioned;
    stats.stream_bytes_saved += union_saved;

    let (fonts_subset, subset_saved) =
        subset::subset_truetype_fonts(doc, &mut stats.log, opts.subset_outlines);
    stats.fonts_subset = fonts_subset;
    stats.stream_bytes_saved += subset_saved;
    if opts.subset_outlines {
        // CID-keyed CFF programs: the same, for Identity-encoded Type0 fonts.
        // (Their stored saving shows in the final file size: the programs
        // are compressed by the document-wide pass.)
        let (cff_subset, _) = cffsubset::subset_cff_cid_fonts(doc, &mut stats.log);
        stats.fonts_subset += cff_subset;
    }

    let (fonts_converted, cff_saved) = type1::convert_type1_fonts(doc, &mut stats.log);
    stats.fonts_converted = fonts_converted;
    stats.stream_bytes_saved += cff_saved;

    // Every CFF program: subroutines no charstring calls are emptied
    // (after outline subsetting and Type 1 conversion, which change what
    // is called and what is CFF).
    // (Like the CFF subsetting above, the saving shows in the final file
    // size: the programs are compressed by the document-wide pass.)
    cffsubrs::prune_cff_subrs(doc, &mut stats.log);

    let (deduped, dedup_saved) = dedup_streams(doc);
    if deduped > 0 {
        stats.log.push(format!(
            "merged {deduped} duplicate streams ({dedup_saved} bytes)"
        ));
        stats.streams_deduped = deduped;
        stats.stream_bytes_saved += dedup_saved;
    }

    let stripped = strip_piece_info(doc);
    if stripped > 0 {
        stats.log.push(format!(
            "stripped {stripped} PieceInfo private-data entries"
        ));
    }
    let regen = strip_regenerable(doc);
    if regen > 0 {
        stats
            .log
            .push(format!("stripped {regen} thumbnail/alternate entries"));
    }

    // Drop objects orphaned by the merges above, then recompress streams.
    doc.prune_objects();
    doc.compress();
    // Cross-reference streams require PDF 1.5.
    if doc.version.parse::<f32>().unwrap_or(1.5) < 1.5 {
        doc.version = "1.5".into();
    }
    Ok(stats)
}

/// What processing one image produced: its log lines and savings, and the
/// stream it replaces (written back by the caller).
#[derive(Default)]
struct ImageOutcome {
    log: Vec<String>,
    saved: i64,
    recompressed: usize,
    staged: Staged,
}

/// Recompress one image, reading the document only. `matte` says what
/// recompress_matte_pairs did with it, if it is half of a /Matte pair.
fn process_image(
    doc: &Document,
    info: &ImageInfo,
    opts: &CompressOptions,
    matte: Option<&MatteHalf>,
    is_soft_mask: bool,
) -> ImageOutcome {
    let mut out = ImageOutcome::default();
    match matte {
        Some(MatteHalf::Written) => return out,
        Some(MatteHalf::Kept) => {
            // Pixels stay as they are; a DCT half can still be
            // entropy-optimized, which changes nothing the pair relies on.
            if let Some((from, to)) = try_lossless_jpeg_optimize(doc, &mut out.staged, info) {
                out.log.push(format!(
                    "obj {} {}: {} -> {} bytes [lossless entropy]",
                    info.id.0, info.id.1, from, to
                ));
                out.saved += from as i64 - to as i64;
                out.recompressed += 1;
            }
            return out;
        }
        None => {}
    }
    // Bilevel images take the lossless CCITT G4 path; the JPEG pipeline
    // below is 8-bit only and would be lossy on scanned text anyway.
    if info.bpc == 1 {
        match recompress_bilevel(doc, &mut out.staged, info) {
            BilevelOutcome::Recompressed { to } => {
                out.log.push(format!(
                    "obj {} {}: {} -> {} bytes [G4 lossless]",
                    info.id.0, info.id.1, info.stream_len, to
                ));
                out.saved += info.stream_len as i64 - to as i64;
                out.recompressed += 1;
            }
            BilevelOutcome::Larger => {
                out.log.push(format!(
                    "keep obj {} {}: G4 >= original {}",
                    info.id.0, info.id.1, info.stream_len
                ));
            }
            BilevelOutcome::Unsupported => {
                out.log.push(format!(
                    "skip obj {} {} ({}, {}): unsupported bilevel variant",
                    info.id.0,
                    info.id.1,
                    info.color,
                    info.filters.join("+")
                ));
            }
        }
        return out;
    }

    let decoded = match decode_image(doc, info.id) {
        Ok(img) => img,
        Err(reason) => {
            // CMYK JPEGs never go through RGB: their four stored channels
            // are resampled and re-encoded as they are (see
            // recompress_cmyk_jpeg).
            let mut cmyk_larger = None;
            match recompress_cmyk_jpeg(doc, &mut out.staged, info, opts) {
                CmykOutcome::Recompressed { to, resized, flate } => {
                    out.log.push(format!(
                        "obj {} {}: {} -> {} bytes{} [cmyk{}]",
                        info.id.0,
                        info.id.1,
                        info.stream_len,
                        to,
                        resized
                            .map(|(w, h)| format!(" (downsampled to {w}x{h})"))
                            .unwrap_or_default(),
                        if flate { ", flate" } else { "" }
                    ));
                    out.saved += info.stream_len as i64 - to as i64;
                    out.recompressed += 1;
                    return out;
                }
                // Fall through to the lossless attempt below; if that
                // fails too, the image is reported as kept, not skipped.
                CmykOutcome::Larger { to } => cmyk_larger = Some(to),
                CmykOutcome::NotApplicable => {}
            }
            // Even when the pixel pipeline can't handle this image (CMYK,
            // Decode arrays, ...), a DCT stream can still be
            // entropy-optimized losslessly: the coefficients are
            // untouched, so every PDF-level property stays valid.
            if let Some((from, to)) = try_lossless_jpeg_optimize(doc, &mut out.staged, info) {
                out.log.push(format!(
                    "obj {} {}: {} -> {} bytes [lossless entropy]",
                    info.id.0, info.id.1, from, to
                ));
                out.saved += from as i64 - to as i64;
                out.recompressed += 1;
            } else if let Some(to) = cmyk_larger {
                out.log.push(format!(
                    "keep obj {} {}: cmyk re-encode {} >= original {}",
                    info.id.0, info.id.1, to, info.stream_len
                ));
            } else {
                out.log.push(format!(
                    "skip obj {} {} ({}, {}): {reason}",
                    info.id.0,
                    info.id.1,
                    info.color,
                    info.filters.join("+")
                ));
            }
            return out;
        }
    };

    // Images made of at most two colors (QR codes, binarized line art
    // stored as 8-bit) become 1-bit palette images at full resolution:
    // exact, and nothing to blur.
    match recompress_two_color(doc, &mut out.staged, info, &decoded, is_soft_mask) {
        TwoColor::Recompressed { to } => {
            out.log.push(format!(
                "obj {} {}: {} -> {} bytes [2-color, lossless]",
                info.id.0, info.id.1, info.stream_len, to
            ));
            out.saved += info.stream_len as i64 - to as i64;
            out.recompressed += 1;
            return out;
        }
        TwoColor::Larger { to } => {
            out.log.push(format!(
                "keep obj {} {}: 2-color lossless {} >= original {}",
                info.id.0, info.id.1, to, info.stream_len
            ));
            return out;
        }
        TwoColor::MoreColors => {}
    }
    let mut img = decoded;
    // Colors under fully transparent pixels are never shown, but
    // resampling and JPEG (of this image and of its mask) spread them
    // into the visible edge; replace them with nearby visible colors.
    let bled = bleed_under_soft_mask(doc, info.id, &mut img);
    let (new_w, new_h) = target_size(info, opts);
    let resized = new_w != info.width as u32 || new_h != info.height as u32;
    if resized {
        img = resize_image(&img, new_w, new_h);
    }

    // Decide how to treat color: RGB images that are visually grayscale
    // become single-channel; sharp chroma edges (stamps, colored line art)
    // disable chroma subsampling, which visibly bleeds there.
    let mut sharp_chroma = false;
    let mut mode = "";
    if let DynamicImage::ImageRgb8(ref rgb) = img {
        match classify_chroma(rgb) {
            ChromaClass::Gray => {
                img = DynamicImage::ImageLuma8(image::imageops::grayscale(&img));
                mode = " [gray]";
            }
            ChromaClass::SharpColor => {
                sharp_chroma = true;
                mode = " [4:4:4]";
            }
            ChromaClass::SmoothColor => {}
        }
    }

    let gray = matches!(img, DynamicImage::ImageLuma8(_));
    let decoded_components: u8 = if gray { 1 } else { 3 };
    // If pixel components are preserved, the original ColorSpace (ICC included) can be kept.
    let keep_colorspace = info.components == Some(decoded_components);
    // An expanded Indexed image takes its palette's base color space
    // (ICC profile included), unless it was reduced to gray.
    let indexed_base = match doc.get_object(info.id) {
        Ok(Object::Stream(s)) => indexed_palette(doc, &s.dict).ok().flatten(),
        _ => None,
    };
    let is_indexed = indexed_base.is_some();
    if is_indexed && mode.is_empty() {
        mode = " [indexed]";
    }
    let mode = if bled {
        format!("{mode} [bled]")
    } else {
        mode.to_string()
    };
    let new_colorspace = match indexed_base {
        Some((c, _, base)) if c == decoded_components => Some(base),
        _ if keep_colorspace => None,
        _ => Some(Object::Name(if gray {
            b"DeviceGray".to_vec()
        } else {
            b"DeviceRGB".to_vec()
        })),
    };

    // Palette images are mostly flat-color art: once resampled, a
    // lossless Flate encoding of it can beat JPEG, and then it wins.
    let encoded = if is_indexed && resized {
        smallest_encoding(&img, opts.jpeg_quality, sharp_chroma)
    } else {
        encode_jpeg(&img, opts.jpeg_quality, sharp_chroma)
            .map(|j| (j, Some(b"DCTDecode".as_slice())))
    };
    let Some((jpeg, filter)) = encoded else {
        out.log.push(format!(
            "keep obj {} {}: jpeg encode failed or not baseline-conformant",
            info.id.0, info.id.1
        ));
        return out;
    };

    if jpeg.len() >= info.stream_len {
        // Recompressing lost; if the original is DCT, still try the
        // lossless entropy re-optimization before keeping it as-is.
        if let Some((from, to)) = try_lossless_jpeg_optimize(doc, &mut out.staged, info) {
            out.log.push(format!(
                "obj {} {}: {} -> {} bytes [lossless entropy]",
                info.id.0, info.id.1, from, to
            ));
            out.saved += from as i64 - to as i64;
            out.recompressed += 1;
        } else {
            out.log.push(format!(
                "keep obj {} {}: recompressed {} >= original {}",
                info.id.0,
                info.id.1,
                jpeg.len(),
                info.stream_len
            ));
        }
        return out;
    }

    let Some(stream) = out.staged.stream(doc, info.id) else {
        return out;
    };
    let dict = &mut stream.dict;
    dict.set("Width", img.width() as i64);
    dict.set("Height", img.height() as i64);
    dict.set("BitsPerComponent", 8i64);
    if let Some(cs) = new_colorspace {
        dict.set("ColorSpace", cs);
    }
    match filter {
        Some(f) => dict.set("Filter", Object::Name(f.to_vec())),
        None => {
            dict.remove(b"Filter");
        }
    }
    dict.remove(b"DecodeParms");
    dict.remove(b"Decode");
    dict.set("Length", jpeg.len() as i64);
    stream.set_content(jpeg.clone());
    stream.allows_compression = false;

    out.log.push(format!(
        "obj {} {}: {} -> {} bytes{}{}{}",
        info.id.0,
        info.id.1,
        info.stream_len,
        jpeg.len(),
        if resized {
            format!(" (downsampled to {}x{})", img.width(), img.height())
        } else {
            String::new()
        },
        mode,
        if filter == Some(b"FlateDecode".as_slice()) {
            " [flate]"
        } else {
            ""
        }
    ));
    out.saved += info.stream_len as i64 - jpeg.len() as i64;
    out.recompressed += 1;
    out
}

/// Run process_image over every image not already written by the /Matte
/// pass, on several threads, with the same result as one image after
/// another in `images` order. An image can read other images (its soft
/// mask, to bleed colors under it; an /Indexed lookup table stored in an
/// image stream), so images run in batches: see parallel::batches.
/// Returns the outcomes in order.
fn process_images(
    doc: &mut Document,
    images: &[ImageInfo],
    opts: &CompressOptions,
    matte_done: &std::collections::HashMap<ObjectId, MatteHalf>,
    soft_masks: &std::collections::HashSet<ObjectId>,
) -> Vec<Option<ImageOutcome>> {
    let mut outcomes: Vec<Option<ImageOutcome>> = (0..images.len()).map(|_| None).collect();
    let units: Vec<usize> = (0..images.len())
        .filter(|&i| !matches!(matte_done.get(&images[i].id), Some(MatteHalf::Written)))
        .collect();
    let batches = parallel::batches(
        &units,
        |i| parallel::reachable(doc, images[i].id, parallel::REACH_LIMIT),
        |i| vec![images[i].id],
    );
    for batch in batches {
        let snapshot: &Document = doc;
        let results = parallel::map_bounded(
            &batch,
            |i| work_pixels(snapshot, &images[i]),
            |i| {
                let info = &images[i];
                process_image(
                    snapshot,
                    info,
                    opts,
                    matte_done.get(&info.id),
                    soft_masks.contains(&info.id),
                )
            },
        );
        for (i, mut outcome) in batch.into_iter().zip(results) {
            std::mem::take(&mut outcome.staged).apply(doc);
            outcomes[i] = Some(outcome);
        }
    }
    outcomes
}

/// Roughly how many pixels processing the image may hold at once: its own
/// and its soft mask's (decoded to bleed colors under it, or as the other
/// half of a /Matte pair). See stream_pixels.
pub(crate) fn work_pixels(doc: &Document, info: &ImageInfo) -> u64 {
    let Some(Object::Stream(s)) = doc.objects.get(&info.id) else {
        return 1;
    };
    let mask = match s.dict.get(b"SMask") {
        // Resolved the way bleeding and the /Matte pass resolve it, through
        // chains of references.
        Ok(Object::Reference(m)) => match doc.get_object(*m) {
            Ok(Object::Stream(m)) => stream_pixels(m),
            _ => 0,
        },
        _ => 0,
    };
    stream_pixels(s).saturating_add(mask)
}

/// An image stream's declared size, or a JPEG's own frame size when larger
/// (the lossless optimizer allocates from the frame header, which need not
/// match the dictionary). A JPEG inside another filter is not inspected
/// and counts as the most an image may have.
fn stream_pixels(s: &lopdf::Stream) -> u64 {
    let side = |key: &[u8]| dict_int(&s.dict, key).unwrap_or(1).max(1) as u64;
    let declared = side(b"Width").saturating_mul(side(b"Height"));
    let filters = filter_names(&s.dict);
    if !filters.iter().any(|f| f == "DCTDecode") {
        return declared;
    }
    if filters.len() > 1 {
        return decode::MAX_PIXELS;
    }
    match smallpeg::jpeg_info(&s.content) {
        Some(j) => declared.max(j.width as u64 * j.height as u64),
        None => declared,
    }
}

/// Refuse documents that compression would damage rather than shrink.
fn check_document(doc: &Document, opts: &CompressOptions) -> Result<(), Refusal> {
    // /Encrypt still present in the trailer means the loader could not
    // decrypt (a user password is required): the objects are unreadable
    // stubs, and writing them out would produce a broken file that opens.
    if doc.is_encrypted() {
        return Err(Refusal::Encrypted);
    }
    if doc.get_pages().is_empty() {
        return Err(Refusal::Empty);
    }
    // The loader decrypted transparently (empty user password): the only
    // barrier was the owner password, i.e. permission restrictions, and our
    // output would carry none of them.
    if doc.encryption_state.is_some() && !opts.strip_protection {
        return Err(Refusal::Protected);
    }
    if !opts.break_signatures && document_is_signed(doc) {
        return Err(Refusal::Signed);
    }
    Ok(())
}

/// The loader drops an object stream it cannot parse (or that inflates past
/// the limit) together with every object inside it, without an error. The
/// output would then lose those objects, and a signature stored there would
/// go unnoticed, so such a document is not processed.
fn check_object_streams(doc: &Document) -> Result<(), lopdf::Error> {
    let mut lost = std::collections::BTreeMap::<u32, usize>::new();
    for (&id, entry) in &doc.reference_table.entries {
        if let lopdf::xref::XrefEntry::Compressed { container, .. } = *entry
            && !doc.objects.contains_key(&(id, 0))
        {
            *lost.entry(container).or_default() += 1;
        }
    }
    match lost.iter().next() {
        None => Ok(()),
        Some((container, count)) => Err(lopdf::Error::InvalidObjectStream(format!(
            "object stream {container} could not be read ({count} objects in it, {} in all, would be lost)",
            lost.values().sum::<usize>()
        ))),
    }
}

/// Whether the document carries digital signatures (which any rewrite would
/// invalidate — their ByteRange refers to the original bytes).
fn document_is_signed(doc: &Document) -> bool {
    if let Ok(catalog) = doc.catalog() {
        if let Ok(acro) = catalog.get(b"AcroForm").and_then(|o| resolve_dict(doc, o))
            && acro
                .get(b"SigFlags")
                .ok()
                .and_then(|o| resolve(doc, o).ok())
                .and_then(|o| o.as_i64().ok())
                .unwrap_or(0)
                != 0
        {
            return true;
        }
        // Usage-rights (UR3) and certification (DocMDP) signatures hang
        // off the catalog's /Perms, with no form field and possibly no
        // /Type on the signature dictionary.
        if let Ok(perms) = catalog.get(b"Perms").and_then(|o| resolve_dict(doc, o))
            && !perms.is_empty()
        {
            return true;
        }
    }
    doc.objects.values().any(|obj| {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            _ => return false,
        };
        matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Sig")
            || matches!(dict.get(b"FT"), Ok(Object::Name(n)) if n == b"Sig")
            // A signature dictionary's own keys, /Type being optional.
            || (dict.has(b"ByteRange") && dict.has(b"Contents") && dict.has(b"Filter"))
    })
}

/// Remove application-private and regenerable baggage that the spec allows
/// readers to ignore: PieceInfo (see below), page thumbnails (/Thumb —
/// viewers regenerate these), and alternate image versions (/Alternates).
fn strip_regenerable(doc: &mut Document) -> usize {
    let mut stripped = 0;
    for obj in doc.objects.values_mut() {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &mut s.dict,
            _ => continue,
        };
        for key in [b"Thumb".as_slice(), b"Alternates".as_slice()] {
            if dict.remove(key).is_some() {
                stripped += 1;
            }
        }
    }
    stripped
}

/// Remove PieceInfo entries: application-private editing data (a
/// drawing application may embed the full editable source of a figure) that the spec
/// says readers must ignore. The referenced payload streams become orphans
/// and are collected by the prune pass.
fn strip_piece_info(doc: &mut Document) -> usize {
    let mut stripped = 0;
    for obj in doc.objects.values_mut() {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &mut s.dict,
            _ => continue,
        };
        if dict.remove(b"PieceInfo").is_some() {
            stripped += 1;
        }
    }
    stripped
}

pub(crate) fn filter_names(dict: &Dictionary) -> Vec<String> {
    match dict.get(b"Filter") {
        Ok(Object::Name(n)) => vec![String::from_utf8_lossy(n).into_owned()],
        Ok(Object::Array(a)) => a
            .iter()
            .filter_map(|o| match o {
                Object::Name(n) => Some(String::from_utf8_lossy(n).into_owned()),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

pub(crate) fn dict_int(dict: &Dictionary, key: &[u8]) -> Option<i64> {
    match dict.get(key) {
        Ok(Object::Integer(i)) => Some(*i),
        _ => None,
    }
}

fn as_f64(obj: &Object) -> Option<f64> {
    match obj {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

pub(crate) fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> lopdf::Result<&'a Object> {
    match obj {
        Object::Reference(id) => doc.get_object(*id),
        other => Ok(other),
    }
}

pub(crate) fn resolve_dict<'a>(
    doc: &'a Document,
    obj: &'a Object,
) -> lopdf::Result<&'a Dictionary> {
    match resolve(doc, obj)? {
        Object::Dictionary(d) => Ok(d),
        Object::Stream(s) => Ok(&s.dict),
        _ => Err(lopdf::Error::ObjectType {
            expected: "Dictionary",
            found: "other",
        }),
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use lopdf::dictionary;

    fn doc_with_catalog(build: impl FnOnce(&mut Document, &mut Dictionary)) -> Document {
        let mut doc = Document::with_version("1.7");
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        build(&mut doc, &mut catalog);
        let id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(id));
        doc
    }

    #[test]
    fn usage_rights_without_a_field_or_type_count_as_signed() {
        // A UR3 signature: referenced from /Perms only, no /Type key.
        let doc = doc_with_catalog(|doc, catalog| {
            let mut sig = Dictionary::new();
            sig.set("Filter", Object::Name(b"Adobe.PPKLite".to_vec()));
            sig.set(
                "ByteRange",
                Object::Array(vec![0.into(), 10.into(), 20.into(), 10.into()]),
            );
            sig.set("Contents", Object::string_literal(vec![0u8; 8]));
            let sig = doc.add_object(sig);
            let mut perms = Dictionary::new();
            perms.set("UR3", Object::Reference(sig));
            catalog.set("Perms", perms);
        });
        assert!(document_is_signed(&doc));
    }

    #[test]
    fn indirect_sigflags_count_as_signed() {
        let doc = doc_with_catalog(|doc, catalog| {
            let flags = doc.add_object(Object::Integer(3));
            let mut acro = Dictionary::new();
            acro.set("Fields", Object::Array(vec![]));
            acro.set("SigFlags", Object::Reference(flags));
            catalog.set("AcroForm", acro);
        });
        assert!(document_is_signed(&doc));
        assert!(!document_is_signed(&doc_with_catalog(|_, _| {})));
    }
    #[test]
    fn unreadable_object_streams_are_refused() {
        let mut doc = doc_with_catalog(|_, _| {});
        assert!(check_object_streams(&doc).is_ok());
        // The xref says object 50 lives in object stream 40, but the loader
        // could not produce it.
        doc.reference_table.entries.insert(
            50,
            lopdf::xref::XrefEntry::Compressed {
                container: 40,
                index: 0,
            },
        );
        let err = check_object_streams(&doc).unwrap_err().to_string();
        assert!(err.contains("object stream 40"), "{err}");
    }

    #[test]
    fn a_corrupt_object_stream_fails_compression() {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_with_options(&mut bytes, compact_save_options())
            .unwrap();
        assert!(compress_bytes(&bytes, &CompressOptions::default()).is_ok());
        // Rename the object stream's /First key (same length, so the xref
        // offsets stay valid): the loader can no longer parse that stream.
        let at = bytes
            .windows(6)
            .position(|w| w == b"/First")
            .expect("an object stream");
        bytes[at..at + 6].copy_from_slice(b"/Fxrst");
        let err = compress_bytes(&bytes, &CompressOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not be read"), "{err}");
    }

    #[test]
    fn an_image_used_as_another_images_palette_is_kept() {
        // Image 1 is also the /Indexed lookup table of image 2: its bytes
        // are image 2's colors, so it is left as it is (recompressing it
        // would repaint image 2), and image 2 reads it unchanged.
        let n = 64usize;
        let mut doc = Document::with_version("1.7");
        let rgb: Vec<u8> = (0..n * n * 3).map(|i| (i % 251) as u8).collect();
        let rgb_copy = rgb.clone();
        let a = doc.add_object(lopdf::Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => n as i64,
                "Height" => n as i64, "BitsPerComponent" => 8, "ColorSpace" => "DeviceRGB",
            },
            rgb,
        ));
        let indices: Vec<u8> = (0..n * n).map(|i| (i % 2) as u8).collect();
        let b = doc.add_object(lopdf::Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => n as i64,
                "Height" => n as i64, "BitsPerComponent" => 8,
                "ColorSpace" => vec!["Indexed".into(), "DeviceRGB".into(), 1.into(), a.into()],
            },
            indices,
        ));
        let content = doc.add_object(lopdf::Stream::new(
            dictionary! {},
            b"q 64 0 0 64 0 0 cm /A Do Q q 64 0 0 64 0 0 cm /B Do Q".to_vec(),
        ));
        let pages = doc.new_object_id();
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages, "Contents" => content,
            "MediaBox" => vec![0.into(), 0.into(), 64.into(), 64.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "A" => a, "B" => b } },
        });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            }),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let (out, stats) = compress_bytes(&bytes, &CompressOptions::default()).unwrap();
        assert_eq!(
            stats.log[0], "skip obj 1 0: also an Indexed lookup table",
            "{:?}",
            stats.log
        );
        assert!(
            !stats.log.iter().any(|l| l.starts_with("obj 1 0")),
            "{:?}",
            stats.log
        );
        let out = Document::load_mem(&out).unwrap();
        let table = out
            .get_object(a)
            .unwrap()
            .as_stream()
            .unwrap()
            .get_plain_content()
            .unwrap();
        assert_eq!(table, rgb_copy);
    }

    #[test]
    fn lookup_tables_are_found_through_references() {
        // A color space array kept as its own object, in a resource
        // dictionary, whose lookup is reached through a chain of references.
        let mut doc = Document::with_version("1.7");
        let table = doc.add_object(lopdf::Stream::new(dictionary! {}, vec![0; 6]));
        let hop = doc.add_object(Object::Reference(table));
        let cs = doc.add_object(Object::Array(vec![
            "I".into(),
            "DeviceRGB".into(),
            1.into(),
            hop.into(),
        ]));
        doc.add_object(dictionary! { "ColorSpace" => dictionary! { "CS0" => cs } });
        assert_eq!(
            indexed_lookups(&doc),
            std::collections::HashSet::from([table])
        );
    }

    #[test]
    fn work_pixels_counts_the_soft_mask() {
        // A 1x1 image with a 10000x10000 soft mask weighs as much as the
        // mask, which bleeding or the /Matte pass decodes.
        let mut doc = Document::with_version("1.7");
        let mask = doc.add_object(lopdf::Stream::new(
            dictionary! { "Subtype" => "Image", "Width" => 10_000, "Height" => 10_000 },
            vec![],
        ));
        let image = doc.add_object(lopdf::Stream::new(
            dictionary! { "Subtype" => "Image", "Width" => 1, "Height" => 1, "SMask" => mask },
            vec![],
        ));
        let info = collect_images(&doc)
            .into_iter()
            .find(|i| i.id == image)
            .unwrap();
        assert_eq!(work_pixels(&doc, &info), 100_000_001);
        // Also when the mask is reached through another reference.
        let hop = doc.add_object(Object::Reference(mask));
        doc.get_object_mut(image)
            .unwrap()
            .as_stream_mut()
            .unwrap()
            .dict
            .set("SMask", hop);
        assert_eq!(work_pixels(&doc, &info), 100_000_001);
    }
}
