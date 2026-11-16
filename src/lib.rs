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
//! let opts = CompressOptions { target_dpi: 150.0, jpeg_quality: 80, ..Default::default() };
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
#![allow(clippy::needless_range_loop, clippy::type_complexity, clippy::too_many_arguments)]

use std::collections::HashMap;

use image::DynamicImage;
use lopdf::content::Content;
use lopdf::{Dictionary, Document, Object, ObjectId};

#[doc(hidden)] // public for the g4_force verification example only
pub mod ccitt;
mod cffsubset;
mod fontunion;
mod subset;
mod type1;
#[cfg(test)]
mod fontfuzz;

pub struct CompressOptions {
    /// Target effective DPI; images rendered above this are downsampled.
    pub target_dpi: f64,
    /// JPEG quality for re-encoded images (1-100).
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

/// Why a document was refused rather than compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
                 permission restrictions — pass --strip-protection to do that explicitly"
            }
            Refusal::Signed => {
                "document is digitally signed; compressing would invalidate the signatures — \
                 pass --break-signatures to do that explicitly"
            }
            Refusal::Empty => "document has no readable pages",
        })
    }
}

impl std::error::Error for Refusal {}

/// Errors from [`compress_bytes`].
#[derive(Debug)]
pub enum Error {
    Pdf(lopdf::Error),
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

impl From<lopdf::Error> for Error {
    fn from(e: lopdf::Error) -> Self {
        Error::Pdf(e)
    }
}

impl From<Refusal> for Error {
    fn from(r: Refusal) -> Self {
        Error::Refused(r)
    }
}

pub struct CompressStats {
    pub images_total: usize,
    pub images_recompressed: usize,
    pub streams_deduped: usize,
    pub fonts_merged: usize,
    pub fonts_subset: usize,
    pub fonts_converted: usize,
    pub stream_bytes_saved: i64,
    pub log: Vec<String>,
}

/// Compress a PDF byte buffer and return the new PDF bytes.
pub fn compress_bytes(
    input: &[u8],
    opts: &CompressOptions,
) -> Result<(Vec<u8>, CompressStats), Error> {
    let mut doc = Document::load_mem_with_options(input, load_options()).map_err(Error::Pdf)?;
    let stats = compress_document(&mut doc, opts)?;
    let mut out = Vec::new();
    doc.save_with_options(&mut out, compact_save_options())
        .map_err(|e| Error::Pdf(e.into()))?;
    Ok((out, stats))
}

/// Serialization settings for the final write: pack non-stream objects into
/// object streams and use a cross-reference stream (PDF 1.5). On dictionary-
/// heavy documents the plain-text structure can be nearly half the file.
pub fn compact_save_options() -> lopdf::SaveOptions {
    lopdf::SaveOptions {
        use_object_streams: true,
        use_xref_streams: true,
        linearize: false,
        object_stream_config: Default::default(),
    }
}

/// List the images embedded in a PDF byte buffer.
pub fn inspect_bytes(input: &[u8]) -> Result<Vec<ImageInfo>, lopdf::Error> {
    let doc = Document::load_mem_with_options(input, load_options())?;
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

/// PDF 2D affine transformation matrix [a b c d e f].
#[derive(Clone, Copy)]
struct Mat {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Mat {
    const ID: Mat = Mat {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// Apply `self` first, then `rhs` (CTM' = M × CTM).
    fn then(&self, rhs: &Mat) -> Mat {
        Mat {
            a: self.a * rhs.a + self.b * rhs.c,
            b: self.a * rhs.b + self.b * rhs.d,
            c: self.c * rhs.a + self.d * rhs.c,
            d: self.c * rhs.b + self.d * rhs.d,
            e: self.e * rhs.a + self.f * rhs.c + rhs.e,
            f: self.e * rhs.b + self.f * rhs.d + rhs.f,
        }
    }
}

/// Largest physical size (in inches) at which an image is rendered. When an
/// image is used multiple times we keep the maximum: preserving quality at the
/// largest rendering automatically satisfies every smaller one.
#[derive(Clone, Copy, Default)]
pub struct Extent {
    pub w_inch: f64,
    pub h_inch: f64,
}

impl Extent {
    fn merge(&mut self, other: Extent) {
        self.w_inch = self.w_inch.max(other.w_inch);
        self.h_inch = self.h_inch.max(other.h_inch);
    }
}

/// Static information about an image XObject.
pub struct ImageInfo {
    pub id: ObjectId,
    pub width: i64,
    pub height: i64,
    pub bpc: i64,
    pub color: String,
    pub components: Option<u8>,
    pub filters: Vec<String>,
    pub stream_len: usize,
    pub extent: Option<Extent>,
}

impl ImageInfo {
    pub fn dpi(&self) -> Option<(f64, f64)> {
        let e = self.extent?;
        if e.w_inch < 0.01 || e.h_inch < 0.01 {
            return None;
        }
        Some((self.width as f64 / e.w_inch, self.height as f64 / e.h_inch))
    }

    pub fn dpi_label(&self) -> String {
        match self.dpi() {
            Some((x, y)) if (x - y).abs() < x * 0.05 => format!("{x:.0}"),
            Some((x, y)) => format!("{x:.0}/{y:.0}"),
            None => "?".into(),
        }
    }
}

/// Recompress the images inside a document (in place).
pub fn compress_document(
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
        stats
            .log
            .push("document is signed; the signatures will not survive (explicitly allowed)".into());
    }

    // Premultiplied-alpha pairs must be resampled together; see
    // recompress_matte_pairs. Whatever it handled is done.
    let matte_done = recompress_matte_pairs(doc, &images, opts, &mut stats);
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

    for info in &images {
        match matte_done.get(&info.id) {
            Some(MatteHalf::Written) => continue,
            Some(MatteHalf::Kept) => {
                // Pixels stay as they are; a DCT half can still be
                // entropy-optimized, which changes nothing the pair relies on.
                if let Some((from, to)) = try_lossless_jpeg_optimize(doc, info) {
                    stats.log.push(format!(
                        "obj {} {}: {} -> {} bytes [lossless entropy]",
                        info.id.0, info.id.1, from, to
                    ));
                    stats.stream_bytes_saved += from as i64 - to as i64;
                    stats.images_recompressed += 1;
                }
                continue;
            }
            None => {}
        }
        // Bilevel images take the lossless CCITT G4 path; the JPEG pipeline
        // below is 8-bit only and would be lossy on scanned text anyway.
        if info.bpc == 1 {
            match recompress_bilevel(doc, info) {
                BilevelOutcome::Recompressed { to } => {
                    stats.log.push(format!(
                        "obj {} {}: {} -> {} bytes [G4 lossless]",
                        info.id.0, info.id.1, info.stream_len, to
                    ));
                    stats.stream_bytes_saved += info.stream_len as i64 - to as i64;
                    stats.images_recompressed += 1;
                }
                BilevelOutcome::Larger { to } => {
                    stats.log.push(format!(
                        "keep obj {} {}: G4 {} >= original {}",
                        info.id.0, info.id.1, to, info.stream_len
                    ));
                }
                BilevelOutcome::Unsupported => {
                    stats.log.push(format!(
                        "skip obj {} {} ({}, {}): unsupported bilevel variant",
                        info.id.0,
                        info.id.1,
                        info.color,
                        info.filters.join("+")
                    ));
                }
            }
            continue;
        }

        let decoded = match decode_image(doc, info.id) {
            Ok(img) => img,
            Err(reason) => {
                // CMYK JPEGs never go through RGB: their four stored channels
                // are resampled and re-encoded as they are (see
                // recompress_cmyk_jpeg).
                let mut cmyk_larger = None;
                match recompress_cmyk_jpeg(doc, info, opts) {
                    CmykOutcome::Recompressed { to, resized, flate } => {
                        stats.log.push(format!(
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
                        stats.stream_bytes_saved += info.stream_len as i64 - to as i64;
                        stats.images_recompressed += 1;
                        continue;
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
                if let Some((from, to)) = try_lossless_jpeg_optimize(doc, info) {
                    stats.log.push(format!(
                        "obj {} {}: {} -> {} bytes [lossless entropy]",
                        info.id.0, info.id.1, from, to
                    ));
                    stats.stream_bytes_saved += from as i64 - to as i64;
                    stats.images_recompressed += 1;
                } else if let Some(to) = cmyk_larger {
                    stats.log.push(format!(
                        "keep obj {} {}: cmyk re-encode {} >= original {}",
                        info.id.0, info.id.1, to, info.stream_len
                    ));
                } else {
                    stats.log.push(format!(
                        "skip obj {} {} ({}, {}): {reason}",
                        info.id.0,
                        info.id.1,
                        info.color,
                        info.filters.join("+")
                    ));
                }
                continue;
            }
        };

        // Images made of at most two colors (QR codes, binarized line art
        // stored as 8-bit) become 1-bit palette images at full resolution:
        // exact, and nothing to blur.
        match recompress_two_color(doc, info, &decoded, soft_masks.contains(&info.id)) {
            TwoColor::Recompressed { to } => {
                stats.log.push(format!(
                    "obj {} {}: {} -> {} bytes [2-color, lossless]",
                    info.id.0, info.id.1, info.stream_len, to
                ));
                stats.stream_bytes_saved += info.stream_len as i64 - to as i64;
                stats.images_recompressed += 1;
                continue;
            }
            TwoColor::Larger { to } => {
                stats.log.push(format!(
                    "keep obj {} {}: 2-color lossless {} >= original {}",
                    info.id.0, info.id.1, to, info.stream_len
                ));
                continue;
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
        let mode = if bled { format!("{mode} [bled]") } else { mode.to_string() };
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
            encode_jpeg(&img, opts.jpeg_quality, sharp_chroma).map(|j| (j, Some(b"DCTDecode".as_slice())))
        };
        let Some((jpeg, filter)) = encoded else {
            stats
                .log
                .push(format!(
                    "keep obj {} {}: jpeg encode failed or not baseline-conformant",
                    info.id.0, info.id.1
                ));
            continue;
        };

        if jpeg.len() >= info.stream_len {
            // Recompressing lost; if the original is DCT, still try the
            // lossless entropy re-optimization before keeping it as-is.
            if let Some((from, to)) = try_lossless_jpeg_optimize(doc, info) {
                stats.log.push(format!(
                    "obj {} {}: {} -> {} bytes [lossless entropy]",
                    info.id.0, info.id.1, from, to
                ));
                stats.stream_bytes_saved += from as i64 - to as i64;
                stats.images_recompressed += 1;
            } else {
                stats.log.push(format!(
                    "keep obj {} {}: recompressed {} >= original {}",
                    info.id.0,
                    info.id.1,
                    jpeg.len(),
                    info.stream_len
                ));
            }
            continue;
        }

        let Some(Object::Stream(stream)) = doc.objects.get_mut(&info.id) else {
            continue;
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

        stats.log.push(format!(
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
            if filter == Some(b"FlateDecode".as_slice()) { " [flate]" } else { "" }
        ));
        stats.stream_bytes_saved += info.stream_len as i64 - jpeg.len() as i64;
        stats.images_recompressed += 1;
    }

    if opts.strip_jpeg_metadata {
        let (stripped, saved) = strip_jpeg_metadata(doc);
        if stripped > 0 {
            stats.log.push(format!("stripped metadata from {stripped} JPEG images ({saved} bytes)"));
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

    let (fonts_subset, subset_saved) = subset::subset_truetype_fonts(doc, &mut stats.log, opts.subset_outlines);
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
        stats
            .log
            .push(format!("stripped {stripped} PieceInfo private-data entries"));
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

/// Whether the document carries digital signatures (which any rewrite would
/// invalidate — their ByteRange refers to the original bytes).
fn document_is_signed(doc: &Document) -> bool {
    if let Ok(catalog) = doc.catalog()
        && let Ok(acro) = catalog.get(b"AcroForm").and_then(|o| resolve_dict(doc, o))
        && dict_int(acro, b"SigFlags").unwrap_or(0) != 0
    {
        return true;
    }
    doc.objects.values().any(|obj| {
        let dict = match obj {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            _ => return false,
        };
        matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Sig")
            || matches!(dict.get(b"FT"), Ok(Object::Name(n)) if n == b"Sig")
    })
}

/// Is this JPEG progressive (SOF2)? `None` for malformed or exotic
/// (hierarchical/lossless/arithmetic/12-bit) files, which we then refuse to
/// touch.
fn jpeg_is_progressive(data: &[u8]) -> Option<bool> {
    smallpeg::jpeg_info(data).map(|i| i.progressive)
}

/// Output size for an image: shrink only the axes whose measured DPI exceeds
/// the target (5% hysteresis).
fn target_size(info: &ImageInfo, opts: &CompressOptions) -> (u32, u32) {
    let (mut new_w, mut new_h) = (info.width as f64, info.height as f64);
    if let Some((dpi_x, dpi_y)) = info.dpi() {
        if dpi_x > opts.target_dpi * 1.05 {
            new_w = info.width as f64 * opts.target_dpi / dpi_x;
        }
        if dpi_y > opts.target_dpi * 1.05 {
            new_h = info.height as f64 * opts.target_dpi / dpi_y;
        }
    }
    ((new_w.round() as u32).max(1), (new_h.round() as u32).max(1))
}

enum CmykSource {
    /// A DCT stream (possibly unwrapped from Flate).
    Jpeg(Vec<u8>),
    /// Uncompressed 8-bit CMYK samples.
    Raw(Vec<u8>),
}

enum CmykOutcome {
    Recompressed { to: usize, resized: Option<(u32, u32)>, flate: bool },
    /// Re-encoding did not beat the stored bytes; nothing was changed.
    Larger { to: usize },
    /// Not a CMYK JPEG this path can handle; nothing was changed.
    NotApplicable,
}

/// Is this image tied to a soft mask by premultiplied alpha (/Matte)?
///
/// A soft mask carrying /Matte declares that the parent image's colors were
/// premultiplied by it. The parent must then have exactly the mask's
/// dimensions, and /Matte one entry per parent color component, so
/// resampling either one, or turning an RGB parent into gray, makes the pair
/// invalid; some viewers drop such an image entirely (a QR code vanished from a
/// real government PDF this way). Both the parent (whose /SMask has /Matte)
/// and the mask itself (which has /Matte) are covered: the per-image
/// pipeline refuses both, and recompress_matte_pairs handles them as a pair.
fn matte_bound(doc: &Document, dict: &Dictionary) -> bool {
    if dict.get(b"Matte").is_ok() {
        return true;
    }
    matches!(
        dict.get(b"SMask").and_then(|o| resolve(doc, o)),
        Ok(Object::Stream(s)) if s.dict.get(b"Matte").is_ok()
    )
}

/// Downsample and recompress premultiplied-alpha (/Matte) pairs as pairs.
///
/// matte_bound explains the constraint: the parent keeps the mask's exact
/// dimensions and its own color components. Both halves are therefore
/// resampled to the same size (taken from the parent's measured DPI) with
/// the same filter, and the parent is never turned into gray. Resampling is
/// linear and premultiplied samples are linear in alpha (c' = m + a(c - m)),
/// so the resampled pair is premultiplied consistently.
///
/// A pair is only taken when nothing else can disturb it: the mask belongs
/// to this one parent, both decode as 8-bit gray/RGB samples of equal size,
/// and /Matte has one number per parent component. When resampled, both
/// halves are rewritten and the pair as a whole must shrink; otherwise each
/// half is replaced only when it shrinks on its own. Returns what happened
/// to each half it decided on, so the main loop neither touches nor
/// re-reports them; images of pairs it could not take fall through to the
/// main loop unchanged (and are refused there by decode_image).
fn recompress_matte_pairs(
    doc: &mut Document,
    images: &[ImageInfo],
    opts: &CompressOptions,
    stats: &mut CompressStats,
) -> HashMap<ObjectId, MatteHalf> {
    let mut done = HashMap::new();
    // How many images point at each soft mask: a shared mask cannot change
    // size without breaking its other parents.
    let mut mask_refs: HashMap<ObjectId, usize> = HashMap::new();
    for obj in doc.objects.values() {
        if let Object::Stream(s) = obj
            && let Ok(Object::Reference(sid)) = s.dict.get(b"SMask")
        {
            *mask_refs.entry(*sid).or_default() += 1;
        }
    }
    for info in images {
        let Some(mask_id) = matte_pair_mask(doc, info.id, &mask_refs) else {
            continue;
        };
        let Some(mask_len) = (match doc.get_object(mask_id) {
            Ok(Object::Stream(s)) => Some(s.content.len()),
            _ => None,
        }) else {
            continue;
        };
        let (Ok(parent), Ok(mask)) = (decode_samples(doc, info.id), decode_samples(doc, mask_id))
        else {
            continue;
        };
        let parent_components = match parent {
            DynamicImage::ImageRgb8(_) => 3,
            DynamicImage::ImageLuma8(_) => 1,
            _ => continue,
        };
        if !matches!(mask, DynamicImage::ImageLuma8(_))
            || matte_len(doc, mask_id) != Some(parent_components)
            || (parent.width(), parent.height()) != (mask.width(), mask.height())
            || parent.width() as i64 != info.width
            || parent.height() as i64 != info.height
        {
            continue;
        }
        let (w, h) = target_size(info, opts);
        let resized = (w, h) != (parent.width(), parent.height());
        let (parent, mask) = if resized {
            (resize_image(&parent, w, h), resize_image(&mask, w, h))
        } else {
            (parent, mask)
        };
        let sharp = matches!(&parent, DynamicImage::ImageRgb8(rgb)
            if matches!(classify_chroma(rgb), ChromaClass::SharpColor));
        let (Some(new_parent), Some(new_mask)) = (
            smallest_encoding(&parent, opts.jpeg_quality, sharp),
            smallest_encoding(&mask, opts.jpeg_quality, false),
        ) else {
            continue;
        };
        let halves = [(info.id, info.stream_len, new_parent), (mask_id, mask_len, new_mask)];
        let keep: Vec<bool> = if resized {
            let before = info.stream_len + mask_len;
            let after = halves[0].2.0.len() + halves[1].2.0.len();
            if after >= before {
                stats.log.push(format!(
                    "keep obj {} {} and its /Matte mask {} {}: downsampled pair {} >= original {}",
                    info.id.0, info.id.1, mask_id.0, mask_id.1, after, before
                ));
                done.insert(info.id, MatteHalf::Kept);
                done.insert(mask_id, MatteHalf::Kept);
                continue;
            }
            vec![true, true]
        } else {
            halves.iter().map(|(_, from, (data, _))| data.len() < *from).collect()
        };
        for ((id, from, (data, filter)), write) in halves.into_iter().zip(keep) {
            if !write {
                stats.log.push(format!(
                    "keep obj {} {}: /Matte pair half, re-encode {} >= original {}",
                    id.0,
                    id.1,
                    data.len(),
                    from
                ));
                done.insert(id, MatteHalf::Kept);
                continue;
            }
            let Some(Object::Stream(stream)) = doc.objects.get_mut(&id) else {
                continue;
            };
            // ColorSpace stays (components are unchanged), and so do /Matte
            // and /SMask: the pair still refers to each other the same way.
            let dict = &mut stream.dict;
            dict.remove(b"DecodeParms");
            dict.remove(b"Decode");
            dict.set("Width", w as i64);
            dict.set("Height", h as i64);
            dict.set("BitsPerComponent", 8i64);
            match filter {
                Some(f) => dict.set("Filter", Object::Name(f.to_vec())),
                None => {
                    dict.remove(b"Filter");
                }
            }
            dict.set("Length", data.len() as i64);
            let to = data.len();
            stream.set_content(data);
            stream.allows_compression = false;
            stats.log.push(format!(
                "obj {} {}: {} -> {} bytes{} [matte pair{}]",
                id.0,
                id.1,
                from,
                to,
                if resized { format!(" (downsampled to {w}x{h})") } else { String::new() },
                if filter == Some(b"FlateDecode".as_slice()) { ", flate" } else { "" }
            ));
            stats.stream_bytes_saved += from as i64 - to as i64;
            stats.images_recompressed += 1;
            done.insert(id, MatteHalf::Written);
        }
    }
    done
}

/// What recompress_matte_pairs did with one half of a pair.
enum MatteHalf {
    /// Resampled and/or re-encoded.
    Written,
    /// Considered and deliberately left as it is (re-encoding did not win).
    Kept,
}

/// The soft mask of `parent` if the two form a /Matte pair that
/// recompress_matte_pairs may resample: the mask carries /Matte, is used by
/// this parent alone, and neither side has further masking of its own.
fn matte_pair_mask(
    doc: &Document,
    parent: ObjectId,
    mask_refs: &HashMap<ObjectId, usize>,
) -> Option<ObjectId> {
    let Ok(Object::Stream(p)) = doc.get_object(parent) else { return None };
    if p.dict.get(b"Matte").is_ok() {
        return None;
    }
    let Ok(Object::Reference(mask_id)) = p.dict.get(b"SMask") else { return None };
    let Ok(Object::Stream(m)) = doc.get_object(*mask_id) else { return None };
    let ok = m.dict.get(b"Matte").is_ok()
        && mask_refs.get(mask_id) == Some(&1)
        && m.dict.get(b"SMask").is_err()
        && m.dict.get(b"Mask").is_err();
    ok.then_some(*mask_id)
}

/// Number of entries in a mask's /Matte array, if every one is a number.
fn matte_len(doc: &Document, mask: ObjectId) -> Option<u8> {
    let Ok(Object::Stream(m)) = doc.get_object(mask) else { return None };
    let Ok(Object::Array(a)) = m.dict.get(b"Matte").and_then(|o| resolve(doc, o)) else {
        return None;
    };
    a.iter()
        .all(|o| matches!(o, Object::Integer(_) | Object::Real(_)))
        .then(|| u8::try_from(a.len()).ok())
        .flatten()
}

/// The smaller of a JPEG and a lossless encoding (see lossless_encoding)
/// of 8-bit samples, with the /Filter it needs.
fn smallest_encoding(
    img: &DynamicImage,
    quality: u8,
    sharp_chroma: bool,
) -> Option<(Vec<u8>, Option<&'static [u8]>)> {
    let lossless = lossless_encoding(img)?;
    match encode_jpeg(img, quality, sharp_chroma) {
        Some(jpeg) if jpeg.len() < lossless.0.len() => Some((jpeg, Some(b"DCTDecode"))),
        _ => Some(lossless),
    }
}

/// Flate-compressed 8-bit samples (or unfiltered, when Flate does not
/// help), with the /Filter they need.
fn lossless_encoding(img: &DynamicImage) -> Option<(Vec<u8>, Option<&'static [u8]>)> {
    let raw = match img {
        DynamicImage::ImageLuma8(g) => g.as_raw().clone(),
        DynamicImage::ImageRgb8(rgb) => rgb.as_raw().clone(),
        _ => return None,
    };
    flate_or_raw(raw)
}

/// The Adobe transform to write for re-encoded CMYK. A source that already
/// carries an Adobe APP14 marker is written as YCCK (transform 2) even when
/// it stored plain CMYK (transform 0): the decoder undoes the transform, so
/// the CMYK samples it hands the reader are the same, the marker's presence
/// (which is what reader inversion conventions key on) does not change, and
/// decorrelating C/M/Y makes the stream much smaller. A source without
/// APP14 stays without it.
fn ycck_if_adobe(adobe: Option<smallpeg::AdobeTransform>) -> Option<smallpeg::AdobeTransform> {
    adobe.map(|_| smallpeg::AdobeTransform::Ycck)
}

/// Is the image's color space four-component CMYK (DeviceCMYK, or an ICC
/// profile with N = 4)?
fn is_cmyk_colorspace(doc: &Document, dict: &Dictionary) -> bool {
    dict.get(b"ColorSpace").is_ok_and(|cs| is_cmyk_object(doc, cs))
}

/// is_cmyk_colorspace for a color space object (name, array or reference).
fn is_cmyk_object(doc: &Document, cs: &Object) -> bool {
    match resolve(doc, cs) {
        Ok(Object::Name(n)) => n == b"DeviceCMYK",
        // [/DeviceCMYK]: a family with no parameters, written as an array.
        Ok(Object::Array(a)) if a.len() == 1 => is_cmyk_object(doc, &a[0]),
        Ok(Object::Array(a)) => {
            matches!(a.first(), Some(Object::Name(n)) if n == b"ICCBased")
                && matches!(
                    a.get(1).and_then(|o| resolve(doc, o).ok()),
                    Some(Object::Stream(s)) if dict_int(&s.dict, b"N") == Some(4)
                )
        }
        _ => false,
    }
}

/// Recompress a CMYK JPEG without ever converting it to RGB.
///
/// The four channels are taken exactly as stored (YCCK un-transformed, no
/// inversion applied), resampled per channel, and re-encoded with the same
/// Adobe APP14 presence as the original. Whether a given file's values are
/// inverted is a convention this code never interprets: standalone
/// image-editor CMYK JPEGs usually store inverted values, PDF-producing
/// tools plain ones, and readers differ in what they assume. Keeping the stored values
/// and the marker as they were makes the output read exactly like the input
/// under any of those conventions; resampling is linear, so it does not
/// care which one applies.
///
/// Raw (Flate or unfiltered) CMYK samples are encoded as stored, with an
/// Adobe YCCK marker.
///
/// Anything that would change how the samples are interpreted at the PDF
/// level (a /Decode array, /DecodeParms, a color-key mask) is left alone.
fn recompress_cmyk_jpeg(doc: &mut Document, info: &ImageInfo, opts: &CompressOptions) -> CmykOutcome {
    let filters: Vec<&str> = info.filters.iter().map(|s| s.as_str()).collect();
    let data = {
        let Ok(Object::Stream(stream)) = doc.get_object(info.id) else {
            return CmykOutcome::NotApplicable;
        };
        let dict = &stream.dict;
        let indexed = match indexed_palette(doc, dict) {
            Ok(Some((4, palette, base))) => Some((palette, base)),
            Ok(None) => None,
            _ => return CmykOutcome::NotApplicable,
        };
        let bpc = dict_int(dict, b"BitsPerComponent");
        if !(is_cmyk_colorspace(doc, dict) || indexed.is_some())
            || !decode_is_default(doc, dict)
            || !decode_parms_understood(doc, dict)
            || matches!(dict.get(b"ImageMask"), Ok(Object::Boolean(true)))
            || matches!(dict.get(b"Mask").and_then(|o| resolve(doc, o)), Ok(Object::Array(_)))
            || !(bpc == Some(8) || (indexed.is_some() && matches!(bpc, Some(1 | 2 | 4))))
            || matte_bound(doc, dict)
        {
            return CmykOutcome::NotApplicable;
        }
        // The same pixel bound as every other image path, before anything
        // is inflated, expanded or decoded.
        let (w, h) = (info.width, info.height);
        if w <= 0 || h <= 0 || w > 65_535 || h > 65_535 || (w * h) as u64 > MAX_PIXELS {
            return CmykOutcome::NotApplicable;
        }
        let (w, h) = (w as usize, h as usize);
        if let Some((palette, base)) = indexed {
            let expected = (w * bpc.unwrap_or(8) as usize).div_ceil(8) * h + h;
            let samples = match filters.as_slice() {
                [] => stream.content.clone(),
                ["FlateDecode"] => match inflate(stream, expected) {
                    Ok(d) => d,
                    Err(_) => return CmykOutcome::NotApplicable,
                },
                _ => return CmykOutcome::NotApplicable,
            };
            let (w, h) = (info.width as u32, info.height as u32);
            match expand_indices(&samples, w, h, bpc.unwrap_or(0), 4, &palette) {
                Ok(raw) => (CmykSource::Raw(raw), Some(base)),
                Err(_) => return CmykOutcome::NotApplicable,
            }
        } else {
            (match filters.as_slice() {
            ["DCTDecode"] => CmykSource::Jpeg(stream.content.clone()),
            ["FlateDecode", "DCTDecode"] => match inflate_dct_wrapper(stream) {
                Ok(d) => CmykSource::Jpeg(d),
                Err(_) => return CmykOutcome::NotApplicable,
            },
            [] => CmykSource::Raw(stream.content.clone()),
            ["FlateDecode"] => match inflate(stream, w * h * 4 + h) {
                Ok(d) => CmykSource::Raw(d),
                Err(_) => return CmykOutcome::NotApplicable,
            },
            _ => return CmykOutcome::NotApplicable,
        }, None)
        }
    };
    let (data, indexed_base) = data;
    let src = match data {
        CmykSource::Jpeg(jpeg) if !jpeg_plausible(&jpeg, Some((info.width as u32, info.height as u32))) => {
            return CmykOutcome::NotApplicable;
        }
        CmykSource::Jpeg(jpeg) => match smallpeg::decode_cmyk_with(&jpeg, &jpeg_limits()) {
            Some(img) => img,
            None => return CmykOutcome::NotApplicable,
        },
        CmykSource::Raw(samples) => {
            // Exact length only, as for RGB: anything else means we
            // misunderstand the sample layout.
            if samples.len() as i64 != info.width * info.height * 4 {
                return CmykOutcome::NotApplicable;
            }
            // Inside a PDF, DCT CMYK samples mean what they store (any
            // inversion is the job of /Decode), so the samples go in as they
            // are, NOT inverted the way standalone image-editor JPEGs are. The
            // Adobe YCCK marker matches what PDF-producing tools carry
            // (checked on a real government PDF, which renders
            // correctly) and lets the encoder decorrelate C/M/Y.
            smallpeg::CmykImage {
                width: info.width as u32,
                height: info.height as u32,
                pixels: samples,
                adobe: Some(smallpeg::AdobeTransform::Ycck),
            }
        }
    };
    // The JPEG header must agree with the PDF dictionary; a mismatch means
    // we misunderstand the stream, so leave it alone.
    if src.width as i64 != info.width || src.height as i64 != info.height {
        return CmykOutcome::NotApplicable;
    }
    let (w, h) = target_size(info, opts);
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        return CmykOutcome::NotApplicable;
    }
    let resized = (w, h) != (src.width, src.height);
    let pixels = if resized {
        smallpeg::resize_cmyk(&src.pixels, src.width, src.height, w, h)
    } else {
        src.pixels
    };
    // The Photo tables, unlike the gray/RGB path's Annex-K: on the CMYK
    // images of two MHLW PDFs they came out 4% smaller with slightly lower
    // render differences (pdfcmp average page mean 0.450 -> 0.443).
    let mut jpeg = smallpeg::Encoder::new(opts.jpeg_quality)
        .progressive(false)
        .quant_preset(smallpeg::QuantPreset::Photo)
        .encode_cmyk(&pixels, w, h, ycck_if_adobe(src.adobe));
    if !huffman_symbols_baseline(&jpeg) {
        return CmykOutcome::NotApplicable;
    }
    let mut filter: Option<&[u8]> = Some(b"DCTDecode");
    // Palette images are mostly flat-color art: once resampled, lossless
    // Flate can beat JPEG, and then it wins (as for gray/RGB palettes).
    if indexed_base.is_some() && resized {
        let mut flate = lopdf::Stream::new(Dictionary::new(), pixels);
        if flate.compress().is_ok() && flate.content.len() < jpeg.len() {
            filter = flate.dict.get(b"Filter").is_ok().then_some(b"FlateDecode".as_slice());
            jpeg = flate.content;
        }
    }
    if jpeg.len() >= info.stream_len {
        return CmykOutcome::Larger { to: jpeg.len() };
    }
    let Some(Object::Stream(stream)) = doc.objects.get_mut(&info.id) else {
        return CmykOutcome::NotApplicable;
    };
    // ColorSpace (ICC profile included) stays: the samples are still CMYK.
    // An expanded palette takes its base color space instead.
    let dict = &mut stream.dict;
    if let Some(base) = indexed_base {
        dict.set("ColorSpace", base);
    }
    dict.set("Width", w as i64);
    dict.set("Height", h as i64);
    dict.set("BitsPerComponent", 8i64);
    match filter {
        Some(f) => dict.set("Filter", Object::Name(f.to_vec())),
        None => {
            dict.remove(b"Filter");
        }
    }
    // Only no-op values can be present here; they would no longer apply.
    dict.remove(b"Decode");
    dict.remove(b"DecodeParms");
    dict.set("Length", jpeg.len() as i64);
    let to = jpeg.len();
    stream.set_content(jpeg);
    stream.allows_compression = false;
    CmykOutcome::Recompressed {
        to,
        resized: resized.then_some((w, h)),
        flate: filter != Some(b"DCTDecode".as_slice()),
    }
}

/// Try smallpeg's lossless entropy re-optimization on a DCT-encoded image
/// stream. The quantized coefficients are byte-for-byte preserved, so pixels
/// and every PDF-level property (ColorSpace, Decode, SMask) stay valid.
/// Enforces the baseline-only policy for PDF embedding: a progressive result
/// is only accepted when the input was already progressive.
fn try_lossless_jpeg_optimize(doc: &mut Document, info: &ImageInfo) -> Option<(usize, usize)> {
    let filters: Vec<&str> = info.filters.iter().map(|s| s.as_str()).collect();
    let (data, stored_len, unwrapped_flate) = {
        let Ok(Object::Stream(stream)) = doc.get_object(info.id) else {
            return None;
        };
        match filters.as_slice() {
            ["DCTDecode"] => {
                // DecodeParms (/ColorTransform) pins down color
                // interpretation relative to the stream's markers, which the
                // optimizer rewrites; skip.
                if stream.dict.get(b"DecodeParms").is_ok() {
                    return None;
                }
                (stream.content.clone(), stream.content.len(), false)
            }
            ["FlateDecode", "DCTDecode"] => {
                // A DecodeParms array here could carry DCT parameters
                // (/ColorTransform) whose position we'd disturb; skip.
                if stream.dict.get(b"DecodeParms").is_ok() {
                    return None;
                }
                (inflate_dct_wrapper(stream).ok()?, stream.content.len(), true)
            }
            _ => return None,
        }
    };
    // The optimizer decodes every coefficient, allocating from the frame
    // header: the same plausibility gate as decoding.
    if !jpeg_plausible(&data, None) {
        return None;
    }
    let was_progressive = jpeg_is_progressive(&data)?;
    let optimized = smallpeg::optimize_jpeg_baseline_with(&data, &jpeg_limits())?;
    if !huffman_symbols_baseline(&optimized) {
        return None;
    }
    if jpeg_is_progressive(&optimized)? && !was_progressive {
        return None;
    }
    if optimized.len() >= stored_len {
        return None;
    }
    let Some(Object::Stream(stream)) = doc.objects.get_mut(&info.id) else {
        return None;
    };
    if unwrapped_flate {
        stream.dict.set("Filter", Object::Name(b"DCTDecode".to_vec()));
    }
    stream.dict.set("Length", optimized.len() as i64);
    let new_len = optimized.len();
    stream.set_content(optimized);
    stream.allows_compression = false;
    Some((stored_len, new_len))
}

/// Remove metadata segments from every image stream that is a JPEG
/// (DCTDecode, possibly inside FlateDecode), losslessly. Returns (images
/// changed, stored bytes saved). Streams with DecodeParms are left alone,
/// as in try_lossless_jpeg_optimize.
fn strip_jpeg_metadata(doc: &mut Document) -> (usize, i64) {
    let (mut count, mut saved) = (0, 0i64);
    for obj in doc.objects.values_mut() {
        let Object::Stream(stream) = obj else { continue };
        if !matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
            || stream.dict.get(b"DecodeParms").is_ok()
        {
            continue;
        }
        let filters: Vec<Vec<u8>> = match stream.dict.get(b"Filter") {
            Ok(Object::Name(n)) => vec![n.clone()],
            Ok(Object::Array(a)) => a.iter().filter_map(|o| o.as_name().ok().map(<[u8]>::to_vec)).collect(),
            _ => continue,
        };
        let wrapped = match filters.iter().map(Vec::as_slice).collect::<Vec<_>>().as_slice() {
            [b"DCTDecode"] => false,
            [b"FlateDecode", b"DCTDecode"] => true,
            _ => continue,
        };
        let jpeg = if wrapped {
            let Ok(d) = inflate_dct_wrapper(stream) else { continue };
            d
        } else {
            stream.content.clone()
        };
        let Some(clean) = smallpeg::strip_metadata(&jpeg) else { continue };
        if clean.len() >= jpeg.len() {
            continue; // no metadata
        }
        // A Flate wrapper stays if it still pays (small JPEGs' tables
        // deflate well); otherwise the bare JPEG is stored.
        let rewrapped = if wrapped {
            let mut s = lopdf::Stream::new(Dictionary::new(), clean.clone());
            s.compress().ok().map(|_| s.content).filter(|c| c.len() < clean.len())
        } else {
            None
        };
        let (content, filter) = match rewrapped {
            Some(c) => (c, Object::Array(vec!["FlateDecode".into(), "DCTDecode".into()])),
            None => (clean, Object::Name(b"DCTDecode".to_vec())),
        };
        if content.len() >= stream.content.len() {
            continue;
        }
        saved += stream.content.len() as i64 - content.len() as i64;
        count += 1;
        stream.dict.set("Filter", filter);
        stream.dict.set("Length", content.len() as i64);
        stream.set_content(content);
        stream.allows_compression = false;
    }
    (count, saved)
}

/// The JPEG inside a /Filter [/FlateDecode /DCTDecode] stream: lopdf
/// refuses to decode a chain that ends in a filter it cannot decode, so only
/// the Flate layer is undone here (bounded by MAX_INFLATE). Streams with
/// DecodeParms are refused: their parameters' positions would have to be
/// matched to the layers.
pub(crate) fn inflate_dct_wrapper(stream: &lopdf::Stream) -> lopdf::Result<Vec<u8>> {
    if stream.dict.get(b"DecodeParms").is_ok() {
        return Err(lopdf::Error::Unimplemented("DecodeParms on a Flate-wrapped JPEG"));
    }
    let mut dict = Dictionary::new();
    dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
    inflate(&lopdf::Stream::new(dict, stream.content.clone()), MAX_INFLATE)
}

/// Lanczos3 downsampling via smallpeg's zero-dependency resampler (kept in
/// the same crate family as the encoder; quality verified equivalent to the
/// image crate's Lanczos3).
fn resize_image(img: &DynamicImage, new_w: u32, new_h: u32) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(g) => {
            let out = smallpeg::resize_gray(g.as_raw(), g.width(), g.height(), new_w, new_h);
            image::GrayImage::from_raw(new_w, new_h, out)
                .map(DynamicImage::ImageLuma8)
                .unwrap_or_else(|| img.clone())
        }
        DynamicImage::ImageRgb8(rgb) => {
            // Straight from the decoded buffer: a large scan is ~80 MB, and
            // a to_rgb8() copy here used to double it.
            let out = smallpeg::resize_rgb(rgb.as_raw(), rgb.width(), rgb.height(), new_w, new_h);
            image::RgbImage::from_raw(new_w, new_h, out)
                .map(DynamicImage::ImageRgb8)
                .unwrap_or_else(|| img.clone())
        }
        _ => {
            let rgb = img.to_rgb8();
            let out = smallpeg::resize_rgb(rgb.as_raw(), rgb.width(), rgb.height(), new_w, new_h);
            image::RgbImage::from_raw(new_w, new_h, out)
                .map(DynamicImage::ImageRgb8)
                .unwrap_or_else(|| img.clone())
        }
    }
}

enum TwoColor {
    Recompressed { to: usize },
    /// Two colors, but the lossless encoding did not beat the stored bytes;
    /// left exactly as it is (lossy recompression would only blur it).
    Larger { to: usize },
    MoreColors,
}

/// Re-encode an image whose decoded pixels take at most two colors
/// losslessly, at its own resolution, with Flate; nothing is resampled or
/// JPEG-encoded, so a QR code or binarized line art keeps every edge.
///
/// Gray images whose samples are only 0 and 255 keep their samples as they
/// are (they may be soft masks, which must stay DeviceGray). Any other pair
/// becomes one 8-bit index per pixel under [/Indexed base 1 <palette>], the
/// palette holding both colors exactly in the image's own color space (the
/// Indexed base when it already was a palette image); a soft mask never
/// gets a palette.
///
/// Samples stay 8-bit on purpose. Packing to 1 bit (and CCITT G4) would be
/// smaller still, but some viewers render 1-bit images through a different,
/// smoothing path: a 1-bit QR code drawn small came out visibly softer than
/// the 8-bit original, and failed to decode at one zoom level where the
/// original did. Flate on runs of identical bytes is small anyway.
fn recompress_two_color(
    doc: &mut Document,
    info: &ImageInfo,
    img: &DynamicImage,
    is_soft_mask: bool,
) -> TwoColor {
    let (raw, c): (&[u8], usize) = match img {
        DynamicImage::ImageRgb8(rgb) => (rgb.as_raw(), 3),
        DynamicImage::ImageLuma8(g) => (g.as_raw(), 1),
        _ => return TwoColor::MoreColors,
    };
    let mut colors: Vec<&[u8]> = Vec::with_capacity(2);
    for px in raw.chunks_exact(c) {
        if !colors.contains(&px) {
            if colors.len() == 2 {
                return TwoColor::MoreColors;
            }
            colors.push(px);
        }
    }
    let plain_gray = c == 1 && colors.iter().all(|p| p[0] == 0 || p[0] == 255);
    if is_soft_mask && !plain_gray {
        return TwoColor::MoreColors;
    }
    let base = {
        let Ok(Object::Stream(s)) = doc.get_object(info.id) else { return TwoColor::MoreColors };
        match indexed_palette(doc, &s.dict) {
            Ok(Some((comps, _, base))) if comps as usize == c => base,
            Ok(None) if colorspace_components(doc, &s.dict) == Some(c as u8) => {
                match s.dict.get(b"ColorSpace") {
                    Ok(cs) => cs.clone(),
                    Err(_) => return TwoColor::MoreColors,
                }
            }
            _ => return TwoColor::MoreColors,
        }
    };
    let (samples, colorspace) = if plain_gray {
        (raw.to_vec(), base)
    } else {
        let palette: Vec<u8> =
            (0..2).flat_map(|i| colors[i.min(colors.len() - 1)].to_vec()).collect();
        let indices = raw.chunks_exact(c).map(|px| u8::from(px != colors[0])).collect();
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            base,
            1.into(),
            Object::String(palette, lopdf::StringFormat::Hexadecimal),
        ]);
        (indices, cs)
    };
    let Some((data, filter)) = flate_or_raw(samples) else { return TwoColor::MoreColors };
    if data.len() >= info.stream_len {
        return TwoColor::Larger { to: data.len() };
    }
    let Some(Object::Stream(stream)) = doc.objects.get_mut(&info.id) else {
        return TwoColor::MoreColors;
    };
    let dict = &mut stream.dict;
    dict.set("ColorSpace", colorspace);
    dict.set("BitsPerComponent", 8i64);
    dict.remove(b"Decode");
    dict.remove(b"DecodeParms");
    match filter {
        Some(f) => dict.set("Filter", Object::Name(f.to_vec())),
        None => {
            dict.remove(b"Filter");
        }
    }
    dict.set("Length", data.len() as i64);
    let to = data.len();
    stream.set_content(data);
    stream.allows_compression = false;
    TwoColor::Recompressed { to }
}

/// Flate-compress samples (or leave them unfiltered when Flate does not
/// help), with the /Filter they need.
fn flate_or_raw(samples: Vec<u8>) -> Option<(Vec<u8>, Option<&'static [u8]>)> {
    let mut flate = lopdf::Stream::new(Dictionary::new(), samples);
    flate.compress().ok()?;
    let filter: Option<&'static [u8]> = flate.dict.get(b"Filter").is_ok().then_some(b"FlateDecode");
    Some((flate.content, filter))
}

/// Replace the colors of an image's fully transparent pixels (soft-mask
/// value 0) with those of the nearest visible ones. Returns whether
/// anything changed.
///
/// Those colors are invisible in the original, so this changes nothing a
/// reader shows. It matters once the image is resampled or JPEG-encoded, and
/// once its soft mask is: both blur across the mask edge, so hidden colors
/// (often black) would bleed into a visible halo, and a soft mask whose
/// zeros ring slightly above zero would reveal them outright (a dark frame
/// around a white QR-code card on a real leaflet). Filling them with the
/// neighboring visible colors makes any such leak invisible.
///
/// The mask is read as it is in the file (any size; sampled nearest), and
/// only an 8-bit gray mask that decodes cleanly is used. /Matte masks never
/// get here (the pair code keeps their samples consistent instead).
fn bleed_under_soft_mask(doc: &Document, id: ObjectId, img: &mut DynamicImage) -> bool {
    let Ok(Object::Stream(s)) = doc.get_object(id) else { return false };
    let Ok(Object::Reference(mask_id)) = s.dict.get(b"SMask") else { return false };
    let Ok(DynamicImage::ImageLuma8(mask)) = decode_samples(doc, *mask_id) else {
        return false;
    };
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (mw, mh) = (mask.width() as usize, mask.height() as usize);
    let hidden: Vec<bool> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            mask.as_raw()[(y * mh / h) * mw + x * mw / w] == 0
        })
        .collect();
    if !hidden.contains(&true) || !hidden.contains(&false) {
        return false;
    }
    let (pixels, c): (&mut [u8], usize) = match img {
        DynamicImage::ImageRgb8(rgb) => (&mut **rgb, 3),
        DynamicImage::ImageLuma8(g) => (&mut **g, 1),
        _ => return false,
    };
    bleed(pixels, w, h, c, hidden);
    true
}

/// Fill every `unknown` pixel, layer by layer outward from the known ones,
/// with the mean of its already-known 8-neighbors. Exact for pixels next to
/// the visible area, smooth further out; O(pixels).
fn bleed(pixels: &mut [u8], w: usize, h: usize, c: usize, mut unknown: Vec<bool>) {
    let neighbors = |i: usize| {
        let (x, y) = ((i % w) as isize, (i / w) as isize);
        (-1isize..=1)
            .flat_map(move |dy| (-1isize..=1).map(move |dx| (x + dx, y + dy)))
            .filter(move |&(nx, ny)| {
                (nx, ny) != (x, y) && nx >= 0 && ny >= 0 && nx < w as isize && ny < h as isize
            })
            .map(move |(nx, ny)| ny as usize * w + nx as usize)
    };
    let mut layer: Vec<usize> = (0..w * h)
        .filter(|&i| unknown[i] && neighbors(i).any(|n| !unknown[n]))
        .collect();
    let mut queued = vec![false; w * h];
    while !layer.is_empty() {
        let mut fills = Vec::with_capacity(layer.len());
        for &i in &layer {
            let mut sum = [0u32; 3];
            let mut n = 0;
            for j in neighbors(i).filter(|&j| !unknown[j]) {
                for k in 0..c {
                    sum[k] += pixels[j * c + k] as u32;
                }
                n += 1;
            }
            fills.push((i, sum, n));
        }
        for &(i, sum, n) in &fills {
            for k in 0..c {
                pixels[i * c + k] = ((sum[k] + n / 2) / n) as u8;
            }
            unknown[i] = false;
        }
        let mut next = Vec::new();
        for &(i, _, _) in &fills {
            for j in neighbors(i) {
                if unknown[j] && !queued[j] {
                    queued[j] = true;
                    next.push(j);
                }
            }
        }
        layer = next;
    }
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

/// How much chroma information an RGB image really carries.
enum ChromaClass {
    /// Visually grayscale — encode as a single luma channel.
    Gray,
    /// Smooth chroma (photographs) — 4:2:0 subsampling is invisible.
    SmoothColor,
    /// Sharp chroma edges (stamps, colored line art) — subsampling bleeds.
    SharpColor,
}

fn classify_chroma(rgb: &image::RgbImage) -> ChromaClass {
    let (w, h) = rgb.dimensions();
    let total = (w as u64) * (h as u64);
    let mut colored = 0u64;
    let mut edge_acc = 0u64;
    let mut edge_n = 0u64;
    for y in 0..h {
        let mut prev: Option<(i32, i32)> = None;
        for x in 0..w {
            let p = rgb.get_pixel(x, y).0;
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            let spread = r.max(g).max(b) - r.min(g).min(b);
            if spread > 12 {
                colored += 1;
            }
            let (cb, cr) = (b - g, r - g);
            if let Some((pcb, pcr)) = prev {
                edge_acc += ((cb - pcb).unsigned_abs() + (cr - pcr).unsigned_abs()) as u64;
                edge_n += 1;
            }
            prev = Some((cb, cr));
        }
    }
    // Calibrated on scan (0.00% / 0.00), smooth photo (0.63), sharp graphics (5.50).
    if colored * 2000 < total {
        ChromaClass::Gray
    } else if edge_acc as f64 / edge_n.max(1) as f64 > 2.0 {
        ChromaClass::SharpColor
    } else {
        ChromaClass::SmoothColor
    }
}

/// Encode as JPEG with smallpeg. Pinned to baseline output (progressive
/// JPEG inside PDF has known viewer/printer compatibility concerns) and the
/// Annex-K tables (measured better for document/scan content, and the
/// configuration the encoder-swap acceptance tests were run under).
/// Smooth color images get 4:2:0 chroma subsampling (like every camera);
/// images with sharp chroma edges keep full chroma resolution.
fn encode_jpeg(img: &DynamicImage, quality: u8, sharp_chroma: bool) -> Option<Vec<u8>> {
    if img.width() > u16::MAX as u32 || img.height() > u16::MAX as u32 {
        return None;
    }
    let enc = smallpeg::Encoder::new(quality)
        .progressive(false)
        .quant_preset(smallpeg::QuantPreset::AnnexK);
    let jpeg = match img {
        DynamicImage::ImageLuma8(g) => enc.encode_gray(g.as_raw(), img.width(), img.height()),
        _ => {
            let sampling = if sharp_chroma {
                smallpeg::Sampling::S444
            } else {
                smallpeg::Sampling::S420
            };
            // Borrow RGB samples directly; only other layouts are converted.
            let converted;
            let raw = match img {
                DynamicImage::ImageRgb8(rgb) => rgb.as_raw(),
                _ => {
                    converted = img.to_rgb8();
                    converted.as_raw()
                }
            };
            enc.sampling(sampling).encode_rgb(raw, img.width(), img.height())
        }
    };
    huffman_symbols_baseline(&jpeg).then_some(jpeg)
}

/// Do the JPEG's Huffman tables stay within what baseline decoders accept?
/// DC symbols are magnitude categories 0..=11; AC symbols are run/size bytes
/// with size 1..=10, plus EOB (0x00) and ZRL (0xF0). A table that declares
/// anything else means the scan codes a value baseline cannot carry, and
/// strict decoders (including at least one OS-level image stack) then drop
/// the whole image.
/// An encoder output that fails this is discarded, never embedded.
fn huffman_symbols_baseline(jpeg: &[u8]) -> bool {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return false;
    }
    let mut i = 2;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            return false;
        }
        let marker = jpeg[i + 1];
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            return false;
        }
        if marker == 0xDA {
            return true;
        }
        if marker == 0xC4 {
            let mut t = &jpeg[i + 4..i + 2 + len];
            while !t.is_empty() {
                if t.len() < 17 {
                    return false;
                }
                let ac = t[0] >> 4 == 1;
                let n: usize = t[1..17].iter().map(|&c| c as usize).sum();
                let Some(symbols) = t.get(17..17 + n) else { return false };
                let ok = symbols.iter().all(|&s| {
                    if ac {
                        matches!(s, 0x00 | 0xF0) || (1..=10).contains(&(s & 0x0F))
                    } else {
                        s <= 11
                    }
                });
                if !ok {
                    return false;
                }
                t = &t[17 + n..];
            }
        }
        i += 2 + len;
    }
    false
}

/// Dictionary types that are pure values: they are only ever reached by
/// following a key or a resource name, never addressed by identity, so two
/// byte-identical ones are interchangeable.
///
/// This has to stay an allowlist. For most dictionary types object identity
/// is itself semantic: two byte-identical `/Type /OCG` dicts are two
/// independently toggleable layers, two identical `/Type /Page` dicts are two
/// pages (merging them makes `/Kids` hold the same reference twice and breaks
/// every `/Dest` aimed at the second one), and annotations, form fields and
/// named destinations are all addressed by reference. Merging any of those
/// silently changes what the document means, however identical their bytes.
const MERGEABLE_DICT_TYPES: [&[u8]; 5] = [
    b"Group",          // transparency group attributes, a `/Group` value
    b"ExtGState",      // graphics state parameters, named from /Resources
    b"Font",           // selected by resource name
    b"FontDescriptor", // a `/FontDescriptor` value
    b"Encoding",       // an `/Encoding` value
];

/// May two byte-identical copies of this dictionary be collapsed into one?
fn dict_is_mergeable(dict: &Dictionary) -> bool {
    matches!(dict.get(b"Type"),
        Ok(Object::Name(n)) if MERGEABLE_DICT_TYPES.contains(&n.as_slice()))
}

/// Streams are content, and content is safe to share — a page's `/Contents`,
/// an image, a font program are all addressed by name or by a single key.
/// The exceptions are streams that *are* file structure: merging two object
/// streams would drop every object the loser carried.
fn stream_is_mergeable(dict: &Dictionary) -> bool {
    !matches!(dict.get(b"Type"),
        Ok(Object::Name(n)) if n == b"ObjStm" || n == b"XRef" || n == b"Linearized")
}

/// Append a canonical, injective byte encoding of `obj` to `out`.
///
/// `format!("{obj:?}")` cannot be used for this. lopdf renders Name and String
/// payloads through `String::from_utf8_lossy`, so `Name(b"\x80")` and
/// `Name(b"\x81")` both print as `/\u{FFFD}`, and `Integer(100000000)` prints
/// exactly like `Real(1e8)`. Colliding keys make dedup merge unrelated objects
/// and redirect references to the wrong survivor. Every variant here gets a
/// distinct tag byte and every variable-length payload a length prefix, so
/// distinct objects always produce distinct keys.
fn write_object_key(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Null => out.push(0),
        Object::Boolean(b) => out.extend_from_slice(&[1, *b as u8]),
        Object::Integer(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_be_bytes());
        }
        // Bit pattern, not the decimal rendering: 1e8f32 must not key like
        // Integer(100000000), and -0.0 must not key like 0.0.
        Object::Real(r) => {
            out.push(3);
            out.extend_from_slice(&r.to_bits().to_be_bytes());
        }
        Object::Name(n) => {
            out.push(4);
            write_bytes_key(n, out);
        }
        Object::String(s, format) => {
            out.push(5);
            out.push(matches!(format, lopdf::StringFormat::Hexadecimal) as u8);
            write_bytes_key(s, out);
        }
        Object::Array(a) => {
            out.push(6);
            out.extend_from_slice(&(a.len() as u64).to_be_bytes());
            for item in a {
                write_object_key(item, out);
            }
        }
        Object::Dictionary(d) => {
            out.push(7);
            write_dict_key(d, out);
        }
        Object::Stream(s) => {
            out.push(8);
            write_dict_key(&s.dict, out);
            write_bytes_key(&s.content, out);
        }
        Object::Reference((number, generation)) => {
            out.push(9);
            out.extend_from_slice(&number.to_be_bytes());
            out.extend_from_slice(&generation.to_be_bytes());
        }
    }
}

fn write_dict_key(dict: &Dictionary, out: &mut Vec<u8>) {
    out.extend_from_slice(&(dict.len() as u64).to_be_bytes());
    for (key, value) in dict {
        write_bytes_key(key, out);
        write_object_key(value, out);
    }
}

fn write_bytes_key(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Merge byte-identical stream objects (duplicated fonts, images, content
/// streams — common when figures are included as PDFs that each embed their
/// own copy of the same font) and rewrite every reference to the survivor.
/// Runs to a fixpoint so chains of duplicates (a stream whose dict references
/// another duplicated stream) also collapse.
///
/// Plain dictionaries are merged too, but only for the handful of types where
/// object identity provably carries no meaning — see `MERGEABLE_DICT_TYPES`.
fn dedup_streams(doc: &mut Document) -> (usize, i64) {
    let mut merged_total = 0usize;
    let mut saved_total = 0i64;
    for _pass in 0..4 {
        let mut canonical: HashMap<Vec<u8>, ObjectId> = HashMap::new();
        let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
        let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        ids.sort();
        for id in ids {
            // Objects referencing different sub-objects produce different keys,
            // so this can never merge objects with diverging dependencies.
            // Iterating to a fixpoint lets merges cascade: once two Group dicts
            // collapse into one, the form streams referencing them can become
            // identical too.
            let mut key = Vec::new();
            match doc.objects.get(&id) {
                Some(obj @ Object::Stream(stream)) if stream_is_mergeable(&stream.dict) => {
                    write_object_key(obj, &mut key)
                }
                Some(obj @ Object::Dictionary(dict)) if dict_is_mergeable(dict) => {
                    write_object_key(obj, &mut key)
                }
                _ => continue,
            }
            if let Some(&keep) = canonical.get(&key) {
                remap.insert(id, keep);
                if let Some(Object::Stream(s)) = doc.objects.get(&id) {
                    saved_total += s.content.len() as i64;
                }
            } else {
                canonical.insert(key, id);
            }
        }
        if remap.is_empty() {
            break;
        }
        let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        for id in all_ids {
            if remap.contains_key(&id) {
                continue;
            }
            if let Some(obj) = doc.objects.get_mut(&id) {
                remap_refs(obj, &remap);
            }
        }
        remap_refs_in_dict(&mut doc.trailer, &remap);
        for dup in remap.keys() {
            doc.objects.remove(dup);
        }
        merged_total += remap.len();
    }
    (merged_total, saved_total)
}

fn remap_refs(obj: &mut Object, remap: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(r) => {
            if let Some(n) = remap.get(r) {
                *r = *n;
            }
        }
        Object::Array(a) => {
            for o in a.iter_mut() {
                remap_refs(o, remap);
            }
        }
        Object::Dictionary(d) => remap_refs_in_dict(d, remap),
        Object::Stream(s) => remap_refs_in_dict(&mut s.dict, remap),
        _ => {}
    }
}

fn remap_refs_in_dict(dict: &mut Dictionary, remap: &HashMap<ObjectId, ObjectId>) {
    for (_key, value) in dict.iter_mut() {
        remap_refs(value, remap);
    }
}

/// Merge semantically duplicated TrueType font subsets.
///
/// Figures saved as PDF each embed their own subset of the same font, and
/// subsetters bake the random six-letter subset tag (`SKOJEB+ArialMT`) into
/// the font program's `name` table, so the copies are near- but not
/// byte-identical and slip past `dedup_streams`. Two font programs are merged
/// only when they are byte-identical after neutralizing exactly that tag (and
/// the checksums derived from it) — anything else about them must match, so a
/// merge provably cannot change rendering.
///
/// When programs merge, the losing subsets' FontDescriptor `FontName` and
/// Font `BaseFont` tags are rewritten to the surviving subset's tag; the
/// dictionaries frequently become fully identical then and are merged too.
fn merge_font_subsets(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    // FontFile2 stream -> descriptors referencing it.
    let mut descs_of_file: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else { continue };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"FontDescriptor") {
            continue;
        }
        if let Ok(Object::Reference(fid)) = dict.get(b"FontFile2") {
            descs_of_file.entry(*fid).or_default().push(id);
        }
    }
    for descs in descs_of_file.values_mut() {
        descs.sort();
    }

    // Group the font programs by their tag-neutralized bytes.
    let mut file_ids: Vec<ObjectId> = descs_of_file.keys().copied().collect();
    file_ids.sort();
    let mut groups: HashMap<Vec<u8>, Vec<ObjectId>> = HashMap::new();
    // Every key is a whole program: bound what the pass holds at once.
    let mut held = 0usize;
    for fid in file_ids {
        let Ok(Object::Stream(stream)) = doc.get_object(fid) else {
            continue;
        };
        let Ok(data) = inflate(stream, MAX_INFLATE) else { continue };
        held += data.len();
        if held > MAX_PASS_BYTES {
            break;
        }
        if let Some(key) = truetype_tag_neutralized(&data) {
            groups.entry(key).or_default().push(fid);
        }
    }

    let mut file_remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    // Descriptor -> surviving subset tag, for the name rewrite pass.
    let mut retagged: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    let mut saved = 0i64;
    let mut group_keys: Vec<&Vec<u8>> = groups.keys().filter(|k| groups[*k].len() > 1).collect();
    group_keys.sort();
    for key in group_keys {
        let members = &groups[key];
        let canon = members[0];
        let Some(canon_name) = descriptor_font_name(doc, descs_of_file[&canon][0]) else {
            continue;
        };
        let Some((canon_tag, canon_base)) = split_subset_name(&canon_name) else {
            continue;
        };
        let (canon_tag, canon_base) = (canon_tag.to_vec(), canon_base.to_vec());
        let mut merged_here = 0usize;
        for &dup in &members[1..] {
            // Every descriptor of the duplicate must carry a subset name with
            // the same base font; otherwise leave that copy alone.
            let dup_descs: Vec<ObjectId> = descs_of_file[&dup]
                .iter()
                .filter(|&&d| {
                    descriptor_font_name(doc, d)
                        .as_deref()
                        .and_then(split_subset_name)
                        .is_some_and(|(_tag, base)| base == canon_base)
                })
                .copied()
                .collect();
            if dup_descs.len() != descs_of_file[&dup].len() {
                continue;
            }
            if let Ok(Object::Stream(s)) = doc.get_object(dup) {
                saved += s.content.len() as i64;
            }
            file_remap.insert(dup, canon);
            for d in dup_descs {
                retagged.insert(d, canon_tag.clone());
            }
            merged_here += 1;
        }
        if merged_here > 0 {
            log.push(format!(
                "font {}: merged {} tag-equivalent subset programs",
                String::from_utf8_lossy(&canon_base),
                merged_here
            ));
        }
    }
    if file_remap.is_empty() {
        return (0, 0);
    }

    // Point every reference at the surviving programs and drop the duplicates.
    let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    for id in &all_ids {
        if file_remap.contains_key(id) {
            continue;
        }
        if let Some(obj) = doc.objects.get_mut(id) {
            remap_refs(obj, &file_remap);
        }
    }
    remap_refs_in_dict(&mut doc.trailer, &file_remap);
    for dup in file_remap.keys() {
        doc.objects.remove(dup);
    }
    let merged_programs = file_remap.len();

    // Rewrite the subset tags so each descriptor matches the program it now
    // embeds: FontName on the descriptor itself, BaseFont on fonts that
    // reference it (directly, or through DescendantFonts for Type0 parents).
    let mut renames: Vec<(ObjectId, &[u8], Vec<u8>)> = Vec::new();
    for (&desc, new_tag) in &retagged {
        renames.push((desc, b"FontName", new_tag.clone()));
    }
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else { continue };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Font") {
            continue;
        }
        let desc = match dict.get(b"FontDescriptor") {
            Ok(Object::Reference(d)) => Some(*d),
            _ => type0_descendant_descriptor(doc, dict),
        };
        if let Some(new_tag) = desc.and_then(|d| retagged.get(&d)) {
            renames.push((id, b"BaseFont", new_tag.clone()));
        }
    }
    for (id, key, new_tag) in renames {
        let Some(Object::Dictionary(dict)) = doc.objects.get_mut(&id) else {
            continue;
        };
        let name = match dict.get(key) {
            Ok(Object::Name(n)) => n.clone(),
            _ => continue,
        };
        if let Some((_tag, base)) = split_subset_name(&name) {
            let mut renamed = new_tag;
            renamed.push(b'+');
            renamed.extend_from_slice(base);
            dict.set(key, Object::Name(renamed));
        }
    }

    // The retagged dictionaries are often now byte-identical to the survivors'
    // — collapse those as well.
    let dicts_merged = dedup_font_dicts(doc);
    if dicts_merged > 0 {
        log.push(format!("merged {dicts_merged} identical font dictionaries"));
    }

    (merged_programs, saved)
}

/// For a Type0 font dictionary, the FontDescriptor of its descendant CIDFont.
fn type0_descendant_descriptor(doc: &Document, dict: &Dictionary) -> Option<ObjectId> {
    let a = match resolve(doc, dict.get(b"DescendantFonts").ok()?).ok()? {
        Object::Array(a) => a,
        _ => return None,
    };
    let cid = match resolve(doc, a.first()?).ok()? {
        Object::Dictionary(d) => d,
        _ => return None,
    };
    match cid.get(b"FontDescriptor") {
        Ok(Object::Reference(d)) => Some(*d),
        _ => None,
    }
}

/// Merge byte-identical Font / FontDescriptor dictionaries. Restricted to
/// those two types: fonts are selected by reference from resource dicts, so
/// object identity carries no meaning for them (unlike e.g. page dicts).
fn dedup_font_dicts(doc: &mut Document) -> usize {
    let mut merged_total = 0usize;
    // FontDescriptors merge on the first pass, which can make their Font
    // dictionaries identical on the next; a few passes reach the fixpoint.
    for _pass in 0..4 {
        let mut canonical: HashMap<Vec<u8>, ObjectId> = HashMap::new();
        let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
        let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        ids.sort();
        for id in ids {
            let Some(Object::Dictionary(dict)) = doc.objects.get(&id) else {
                continue;
            };
            if !matches!(dict.get(b"Type"),
                Ok(Object::Name(n)) if n == b"Font" || n == b"FontDescriptor")
            {
                continue;
            }
            // Canonical bytes, not the Debug string, which collides on
            // non-UTF-8 names (a subset tag is ASCII, but /BaseFont and
            // /Differences entries need not be).
            let mut key = Vec::new();
            write_dict_key(dict, &mut key);
            if let Some(&keep) = canonical.get(&key) {
                remap.insert(id, keep);
            } else {
                canonical.insert(key, id);
            }
        }
        if remap.is_empty() {
            break;
        }
        let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        for id in all_ids {
            if remap.contains_key(&id) {
                continue;
            }
            if let Some(obj) = doc.objects.get_mut(&id) {
                remap_refs(obj, &remap);
            }
        }
        remap_refs_in_dict(&mut doc.trailer, &remap);
        for dup in remap.keys() {
            doc.objects.remove(dup);
        }
        merged_total += remap.len();
    }
    merged_total
}

fn descriptor_font_name(doc: &Document, desc: ObjectId) -> Option<Vec<u8>> {
    match doc.get_dictionary(desc).ok()?.get(b"FontName") {
        Ok(Object::Name(n)) => Some(n.clone()),
        _ => None,
    }
}

/// Split a subset font name like `SKOJEB+ArialMT` into (tag, base name).
fn split_subset_name(name: &[u8]) -> Option<(&[u8], &[u8])> {
    if name.len() > 7 && name[..6].iter().all(u8::is_ascii_uppercase) && name[6] == b'+' {
        Some((&name[..6], &name[7..]))
    } else {
        None
    }
}

/// Comparison key for a TrueType font program with the subset tag neutralized.
///
/// Every `name`-table string starting with a six-uppercase-letter tag and '+'
/// (ASCII or UTF-16BE) gets the tag overwritten with `AAAAAA`, and the fields
/// derived from those bytes — the `name` and `head` directory checksums and
/// `head.checkSumAdjustment` — are zeroed. Every other byte is compared
/// strictly, so equal keys prove the fonts differ at most in their random
/// subset tag. Returns None when the data does not parse as TrueType, in
/// which case no merging is attempted.
fn truetype_tag_neutralized(data: &[u8]) -> Option<Vec<u8>> {
    let version = be32(data, 0)?;
    if version != 0x0001_0000 && version != u32::from_be_bytes(*b"true") {
        return None;
    }
    let num_tables = be16(data, 4)? as usize;
    let mut out = data.to_vec();
    let mut name = None;
    let mut head = None;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = data.get(rec..rec + 4)?;
        let off = be32(data, rec + 8)? as usize;
        let len = be32(data, rec + 12)? as usize;
        if off.checked_add(len)? > data.len() {
            return None;
        }
        match tag {
            b"name" => {
                out[rec + 4..rec + 8].fill(0);
                name = Some((off, len));
            }
            b"head" => {
                if len < 12 {
                    return None;
                }
                out[rec + 4..rec + 8].fill(0);
                head = Some(off);
            }
            _ => {}
        }
    }
    if let Some(off) = head {
        out[off + 8..off + 12].fill(0); // checkSumAdjustment
    }
    let (noff, nlen) = name?;
    if nlen < 6 {
        return None;
    }
    let count = be16(data, noff + 2)? as usize;
    let str_base = noff + be16(data, noff + 4)? as usize;
    for i in 0..count {
        let rec = noff + 6 + i * 12;
        let len = be16(data, rec + 8)? as usize;
        let off = be16(data, rec + 10)? as usize;
        let start = str_base.checked_add(off)?;
        let end = start.checked_add(len)?;
        // Strings pointing outside the name table are left as-is; they then
        // participate in the strict byte comparison instead.
        if end > (noff + nlen).min(data.len()) {
            continue;
        }
        neutralize_subset_tag(&mut out[start..end]);
    }
    Some(out)
}

/// If `s` starts with a six-uppercase-letter subset tag followed by '+'
/// (ASCII or UTF-16BE), overwrite the tag letters with 'A'.
fn neutralize_subset_tag(s: &mut [u8]) {
    if s.len() >= 7 && s[..6].iter().all(u8::is_ascii_uppercase) && s[6] == b'+' {
        s[..6].fill(b'A');
    } else if s.len() >= 14
        && s[..14].chunks(2).all(|c| c[0] == 0)
        && s[1..12].iter().step_by(2).all(u8::is_ascii_uppercase)
        && s[13] == b'+'
    {
        for i in 0..6 {
            s[i * 2 + 1] = b'A';
        }
    }
}

fn be16(data: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(off..off + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

pub fn collect_images(doc: &Document) -> Vec<ImageInfo> {
    let usage = image_usage(doc);

    let mut images = Vec::new();
    for (&id, obj) in &doc.objects {
        let Object::Stream(stream) = obj else { continue };
        let dict = &stream.dict;
        if !matches!(dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
            continue;
        }
        images.push(ImageInfo {
            id,
            width: dict_int(dict, b"Width").unwrap_or(0),
            height: dict_int(dict, b"Height").unwrap_or(0),
            bpc: dict_int(dict, b"BitsPerComponent").unwrap_or(0),
            color: colorspace_name(doc, dict),
            components: colorspace_components(doc, dict),
            filters: filter_names(dict),
            stream_len: stream.content.len(),
            extent: usage.get(&id).copied(),
        });
    }
    images
}

/// Interpret content streams to measure the physical size at which each image
/// is actually rendered.
fn image_usage(doc: &Document) -> HashMap<ObjectId, Extent> {
    let mut usage: HashMap<ObjectId, Extent> = HashMap::new();
    // One parse budget for the document: a form reused on every page
    // counts each time.
    let mut walk = Walk::new(0);
    for (_num, page_id) in doc.get_pages() {
        let res_dicts = page_resource_dicts(doc, page_id);
        if walk_page(doc, page_id, &res_dicts, &mut usage, &mut walk).is_err() {
            // For unparseable pages, assume images cover the full page (errs toward shrinking less).
            fallback_page_usage(doc, page_id, &res_dicts, &mut usage);
        }
    }
    propagate_smask_usage(doc, &mut usage);
    usage
}

fn walk_page(
    doc: &Document,
    page_id: ObjectId,
    res_dicts: &[&Dictionary],
    usage: &mut HashMap<ObjectId, Extent>,
    walk: &mut Walk,
) -> Result<(), lopdf::Error> {
    const OVER: lopdf::Error = lopdf::Error::Unimplemented("unreadable or oversized page content");
    let content = page_content_bounded(doc, page_id).ok_or(OVER)?;
    // Strict: Content::decode stops at the first token it cannot parse and
    // returns what came before as a success, while viewers skip the bad
    // token and keep drawing. A partial parse would miss later draws, so a
    // page that does not parse to the end takes the full-page fallback
    // (as does one past the walk's parse budget).
    walk.page();
    let (content, _) = walk.parse(&content).ok_or(OVER)?;
    // Form-invocation budget per page: nested forms re-invoking each other
    // multiply exponentially; a page that exhausts this is treated like an
    // unparseable one (fall back to full-page usage, which shrinks less).
    walk.forms = 100_000;
    if !walk_ops(doc, &content.operations, res_dicts, Mat::ID, usage, 0, walk) {
        return Err(lopdf::Error::PageNumberNotFound(0));
    }
    Ok(())
}

/// Returns false when the form budget ran out (results are incomplete).
#[allow(clippy::too_many_arguments)]
fn walk_ops(
    doc: &Document,
    ops: &[lopdf::content::Operation],
    res_dicts: &[&Dictionary],
    base_ctm: Mat,
    usage: &mut HashMap<ObjectId, Extent>,
    depth: u8,
    walk: &mut Walk,
) -> bool {
    let mut ctm = base_ctm;
    let mut stack: Vec<Mat> = Vec::new();
    for op in ops {
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(m) = stack.pop() {
                    ctm = m;
                }
            }
            "cm" => {
                if let Some(m) = mat_from_operands(&op.operands) {
                    ctm = m.then(&ctm);
                }
            }
            "Do" => {
                let Some(Object::Name(name)) = op.operands.first() else {
                    continue;
                };
                let Some(xid) = lookup_xobject(doc, res_dicts, name) else {
                    continue;
                };
                let Ok(Object::Stream(stream)) = doc.get_object(xid) else {
                    continue;
                };
                match stream.dict.get(b"Subtype") {
                    Ok(Object::Name(st)) if st == b"Image" => {
                        // The CTM maps the unit square onto the target rectangle;
                        // the basis vector lengths are the rendered size in points.
                        let w_pt = (ctm.a * ctm.a + ctm.b * ctm.b).sqrt();
                        let h_pt = (ctm.c * ctm.c + ctm.d * ctm.d).sqrt();
                        usage.entry(xid).or_default().merge(Extent {
                            w_inch: w_pt / 72.0,
                            h_inch: h_pt / 72.0,
                        });
                    }
                    Ok(Object::Name(st)) if st == b"Form" && depth < 8 => {
                        if walk.forms == 0 {
                            return false;
                        }
                        walk.forms -= 1;
                        let form_mat = stream
                            .dict
                            .get(b"Matrix")
                            .ok()
                            .and_then(|o| resolve(doc, o).ok())
                            .and_then(|o| match o {
                                Object::Array(a) => mat_from_operands(a),
                                _ => None,
                            })
                            .unwrap_or(Mat::ID);
                        let inner_ctm = form_mat.then(&ctm);
                        let mut inner_res: Vec<&Dictionary> = Vec::new();
                        if let Ok(rd) = stream
                            .dict
                            .get(b"Resources")
                            .and_then(|o| resolve_dict(doc, o))
                        {
                            inner_res.push(rd);
                        }
                        inner_res.extend_from_slice(res_dicts);
                        // Unreadable, unparseable or over budget: the whole
                        // page takes the fallback.
                        let Some((c, opened)) = walk.parse_stream(stream) else {
                            return false;
                        };
                        if !walk_ops(doc, &c.operations, &inner_res, inner_ctm, usage, depth + 1, walk) {
                            return false;
                        }
                        walk.close(opened);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    true
}

/// Fallback for pages whose content could not be interpreted: pretend every
/// image is used at full-page size.
fn fallback_page_usage(
    doc: &Document,
    page_id: ObjectId,
    res_dicts: &[&Dictionary],
    usage: &mut HashMap<ObjectId, Extent>,
) {
    let (w_inch, h_inch) = page_media_size_inch(doc, page_id).unwrap_or((8.27, 11.69)); // A4
    for res in res_dicts {
        let Ok(xobjs) = res.get(b"XObject").and_then(|o| resolve_dict(doc, o)) else {
            continue;
        };
        for (_name, val) in xobjs.iter() {
            if let Object::Reference(rid) = val {
                usage
                    .entry(*rid)
                    .or_default()
                    .merge(Extent { w_inch, h_inch });
            }
        }
    }
}

/// An SMask is stretched onto the same rectangle as its base image, so it
/// inherits the base image's rendered extent.
fn propagate_smask_usage(doc: &Document, usage: &mut HashMap<ObjectId, Extent>) {
    let mut links: Vec<(ObjectId, ObjectId)> = Vec::new();
    for (&id, obj) in &doc.objects {
        let Object::Stream(stream) = obj else { continue };
        if !matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image") {
            continue;
        }
        if let Ok(Object::Reference(sid)) = stream.dict.get(b"SMask") {
            links.push((id, *sid));
        }
    }
    for (base, smask) in links {
        if let Some(ext) = usage.get(&base).copied() {
            usage.entry(smask).or_default().merge(ext);
        }
    }
}

pub(crate) fn page_resource_dicts(doc: &Document, page_id: ObjectId) -> Vec<&Dictionary> {
    let mut dicts = Vec::new();
    if let Ok((res_opt, res_ids)) = doc.get_page_resources(page_id) {
        if let Some(d) = res_opt {
            dicts.push(d);
        }
        for rid in res_ids {
            if let Ok(d) = doc.get_dictionary(rid) {
                dicts.push(d);
            }
        }
    }
    dicts
}

fn lookup_xobject(doc: &Document, res_dicts: &[&Dictionary], name: &[u8]) -> Option<ObjectId> {
    for res in res_dicts {
        let Ok(xobjs) = res.get(b"XObject").and_then(|o| resolve_dict(doc, o)) else {
            continue;
        };
        if let Ok(Object::Reference(rid)) = xobjs.get(name) {
            return Some(*rid);
        }
    }
    None
}

fn mat_from_operands(operands: &[Object]) -> Option<Mat> {
    if operands.len() != 6 {
        return None;
    }
    let v: Vec<f64> = operands.iter().filter_map(as_f64).collect();
    if v.len() != 6 {
        return None;
    }
    Some(Mat {
        a: v[0],
        b: v[1],
        c: v[2],
        d: v[3],
        e: v[4],
        f: v[5],
    })
}

fn page_media_size_inch(doc: &Document, page_id: ObjectId) -> Option<(f64, f64)> {
    // MediaBox is inherited through the Pages tree, so walk up the parents.
    let mut dict = doc.get_dictionary(page_id).ok()?;
    for _ in 0..16 {
        if let Ok(Object::Array(a)) = dict.get(b"MediaBox").and_then(|o| resolve(doc, o)) {
            let v: Vec<f64> = a.iter().filter_map(as_f64).collect();
            if v.len() == 4 {
                return Some(((v[2] - v[0]).abs() / 72.0, (v[3] - v[1]).abs() / 72.0));
            }
        }
        match dict.get(b"Parent") {
            Ok(Object::Reference(pid)) => dict = doc.get_dictionary(*pid).ok()?,
            _ => break,
        }
    }
    None
}

enum BilevelOutcome {
    /// Replaced with a smaller CCITT G4 stream (new stream size).
    Recompressed { to: usize },
    /// G4 did not shrink it; the original stream is kept.
    Larger { to: usize },
    /// A variant this pass does not handle; left untouched.
    Unsupported,
}

/// Losslessly recompress a 1-bit grayscale image as CCITT Group 4.
///
/// G4 typically beats Flate several times over on scanned text, and because
/// the recompression is exactly reversible it needs no quality judgement —
/// only the size guard. Handled conservatively: plain (or Flate-compressed)
/// gray samples with no Decode array, no DecodeParms (a Flate Predictor
/// would change what the raw bytes mean), and not an ImageMask. The
/// ColorSpace is kept, and CCITTFaxDecode's default BlackIs1 = false matches
/// the 1-bit gray convention (sample 0 = black), so no Decode array is
/// needed on the result either.
fn recompress_bilevel(doc: &mut Document, info: &ImageInfo) -> BilevelOutcome {
    let Ok(Object::Stream(stream)) = doc.get_object(info.id) else {
        return BilevelOutcome::Unsupported;
    };
    let dict = &stream.dict;
    if matches!(dict.get(b"ImageMask"), Ok(Object::Boolean(true)))
        || dict.get(b"Decode").is_ok()
        || dict.get(b"DecodeParms").is_ok()
        || info.components != Some(1)
    {
        return BilevelOutcome::Unsupported;
    }
    let filters = filter_names(dict);
    // Inflate no further than the packed rows the image declares.
    let expected = (info.width.max(0) as usize).div_ceil(8).saturating_mul(info.height.max(0) as usize);
    if expected == 0 || expected > MAX_INFLATE {
        return BilevelOutcome::Unsupported;
    }
    let data = match filters.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
        [] => stream.content.clone(),
        ["FlateDecode"] => match inflate(stream, expected) {
            Ok(d) => d,
            Err(_) => return BilevelOutcome::Unsupported,
        },
        _ => return BilevelOutcome::Unsupported,
    };
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || data.len() < w.div_ceil(8) * h {
        return BilevelOutcome::Unsupported;
    }

    let g4 = ccitt::encode_g4(&data, w, h);
    if g4.len() >= info.stream_len {
        return BilevelOutcome::Larger { to: g4.len() };
    }

    let Some(Object::Stream(stream)) = doc.objects.get_mut(&info.id) else {
        return BilevelOutcome::Unsupported;
    };
    let to = g4.len();
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", w as i64);
    parms.set("Rows", h as i64);
    stream.dict.set("Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
    stream.dict.set("DecodeParms", Object::Dictionary(parms));
    stream.dict.set("Length", to as i64);
    stream.set_content(g4);
    stream.allows_compression = false;
    BilevelOutcome::Recompressed { to }
}

/// Upper bound for inflating any one stream whose expected size is not
/// known in advance (font programs, content streams, JPEG data inside a
/// Flate wrapper), and for loading object streams. Deflate alone expands up
/// to ~1000x and filter chains multiply that, so every inflation is bounded;
/// real streams stay far below this.
pub(crate) const MAX_INFLATE: usize = 256 << 20;

/// Upper bound on the bytes a whole pass keeps inflated at once (the font
/// passes hold every candidate program side by side).
pub(crate) const MAX_PASS_BYTES: usize = 1 << 30;

/// Upper bound on one content stream's inflated size in the walkers (the
/// largest real page seen is 4.7 MB).
pub(crate) const MAX_CONTENT: usize = 32 << 20;
/// Parse cost (see `parse_cost`) a walk may hold at once: a page plus the
/// forms open inside it. lopdf takes ~650 bytes per parsed operation and
/// ~250-370 per array element, far more than the source (780k "q"
/// operations, 1.6 MB, peak at ~500 MB), so this, not the byte size, bounds
/// the walkers' memory. The costliest real page seen (two nested forms)
/// holds 0.41M.
pub(crate) const MAX_LIVE_COST: usize = 800_000;
/// Parse cost one walk of a document may spend in total, counting every
/// invocation of a reused form. Repeated large parses also grow the heap
/// (fragmentation, ~40-65 bytes per operation parsed), so this bounds
/// memory as well as time. The costliest real document seen spends 1.5M;
/// past this, pages take the walkers' conservative fallbacks.
pub(crate) const MAX_WALK_COST: usize = 6_000_000;

/// Upper estimate of what lopdf's parse of content bytes costs, in
/// operations: one per operator-like token (a run starting with a letter,
/// ' or "), half one per token or delimiter inside an array or dictionary,
/// an eighth one per other operand token (measured: "0 0 m" costs what "q"
/// does, while array elements take ~300 bytes each). String contents and inline image
/// data count too, which only overestimates.
fn parse_cost(data: &[u8]) -> usize {
    let (mut ops, mut in_array, mut other) = (0usize, 0usize, 0usize);
    let mut depth = 0usize;
    let mut in_run = false;
    let operand = |depth: usize, in_array: &mut usize, other: &mut usize| {
        if depth > 0 { *in_array += 1 } else { *other += 1 }
    };
    for &b in data {
        match b {
            b' ' | b'\n' | b'\r' | b'\t' | b'\x0c' | 0 => in_run = false,
            b'/' => {
                operand(depth, &mut in_array, &mut other);
                in_run = true; // the name's characters follow
            }
            // Arrays and dictionaries (and, overestimating, hex strings).
            b'[' | b']' | b'<' | b'>' => {
                operand(depth, &mut in_array, &mut other);
                depth = if matches!(b, b'[' | b'<') { depth + 1 } else { depth.saturating_sub(1) };
                in_run = false;
            }
            b'(' | b')' | b'{' | b'}' | b'%' => {
                operand(depth, &mut in_array, &mut other);
                in_run = false;
            }
            _ => {
                if !in_run {
                    if depth == 0 && (b.is_ascii_alphabetic() || b == b'\'' || b == b'"') {
                        ops += 1;
                    } else {
                        operand(depth, &mut in_array, &mut other);
                    }
                }
                in_run = true;
            }
        }
    }
    ops + in_array.div_ceil(2) + other.div_ceil(8)
}

/// Budgets for one content-stream walk of a document.
pub(crate) struct Walk {
    /// Form/glyph-procedure invocations left.
    pub(crate) forms: u32,
    live: usize,
    parsed: usize,
}

/// What one parse holds open in a Walk, to give back with `close`.
pub(crate) struct Opened(usize);

impl Walk {
    pub(crate) fn new(forms: u32) -> Self {
        Walk { forms, live: 0, parsed: 0 }
    }

    /// Start a new page: nothing is open.
    pub(crate) fn page(&mut self) {
        self.live = 0;
    }

    /// Parse content bytes within both bounds. On success the cost counts
    /// as open until `close` (callers that abandon the walk need not).
    pub(crate) fn parse(&mut self, data: &[u8]) -> Option<(Content, Opened)> {
        let cost = parse_cost(data);
        let live = self.live.checked_add(cost).filter(|&v| v <= MAX_LIVE_COST)?;
        let parsed = self.parsed.checked_add(cost).filter(|&v| v <= MAX_WALK_COST)?;
        let content = Content::decode_strict(data).ok()?;
        (self.live, self.parsed) = (live, parsed);
        Some((content, Opened(cost)))
    }

    /// Inflate and parse a form or glyph-procedure stream.
    pub(crate) fn parse_stream(&mut self, stream: &lopdf::Stream) -> Option<(Content, Opened)> {
        self.parse(&inflate(stream, MAX_CONTENT).ok()?)
    }

    pub(crate) fn close(&mut self, opened: Opened) {
        self.live -= opened.0;
    }
}

/// Load options for untrusted input: object and cross-reference streams
/// are inflated with the shared bound.
pub fn load_options() -> lopdf::LoadOptions {
    lopdf::LoadOptions::with_max_decompressed_size(MAX_INFLATE)
}

/// Inflate a stream, refusing output beyond `limit` bytes.
pub(crate) fn inflate(stream: &lopdf::Stream, limit: usize) -> lopdf::Result<Vec<u8>> {
    stream.decompressed_content_with_limit(limit)
}

/// A page's content streams, concatenated, inflated with MAX_CONTENT
/// applied to each stream and to the total (a Contents array may name the
/// same stream any number of times). None if anything cannot be read.
pub(crate) fn page_content_bounded(doc: &Document, page_id: ObjectId) -> Option<Vec<u8>> {
    let page = doc.get_dictionary(page_id).ok()?;
    let mut data = Vec::new();
    let Ok(contents) = page.get(b"Contents") else { return Some(data) };
    let streams: Vec<&Object> = match resolve(doc, contents).ok()? {
        Object::Array(a) => a.iter().collect(),
        one => vec![one],
    };
    for s in streams {
        let Object::Stream(s) = resolve(doc, s).ok()? else { return None };
        data.extend(inflate(s, MAX_CONTENT).ok()?);
        data.push(b'\n');
        if data.len() > MAX_CONTENT {
            return None;
        }
    }
    Some(data)
}

/// Is a JPEG safe to hand to a decoder or optimizer? Its frame header must
/// declare exactly the PDF image's dimensions (when given), stay within
/// MAX_PIXELS, and not claim implausibly many pixels for its size: decoders
/// allocate from the header alone, so a few dozen bytes declaring a huge
/// frame would otherwise cost a gigabyte. Real JPEGs, even of blank pages,
/// stay well above one byte per thousand pixels.
fn jpeg_plausible(jpeg: &[u8], expected: Option<(u32, u32)>) -> bool {
    let Some(info) = smallpeg::jpeg_info(jpeg) else { return false };
    let (w, h) = (info.width, info.height);
    let pixels = w as u64 * h as u64;
    pixels <= MAX_PIXELS
        && expected.is_none_or(|e| e == (w, h))
        && (jpeg.len() as u64) * 1024 >= pixels
}

/// Most pixels any image may have before pdfdiet decodes it.
const MAX_PIXELS: u64 = 100_000_000;

/// The same bound handed to smallpeg, which enforces it again (and a scan
/// count bound) while parsing, should a file get past jpeg_plausible.
fn jpeg_limits() -> smallpeg::Limits {
    smallpeg::Limits::default().max_pixels(MAX_PIXELS)
}

/// Decode into a DynamicImage; `Err` carries the reason it was left alone.
fn decode_image(doc: &Document, id: ObjectId) -> Result<DynamicImage, &'static str> {
    // Premultiplied alpha (/Matte) ties the image's samples, component count
    // and dimensions to its soft mask's, so such images are only ever
    // processed as a pair (recompress_matte_pairs); see matte_bound.
    if let Ok(Object::Stream(stream)) = doc.get_object(id)
        && matte_bound(doc, &stream.dict)
    {
        return Err("premultiplied soft mask (/Matte) not resamplable as a pair");
    }
    decode_samples(doc, id)
}

/// decode_image without the /Matte refusal, for callers that keep a
/// premultiplied pair consistent themselves.
fn decode_samples(doc: &Document, id: ObjectId) -> Result<DynamicImage, &'static str> {
    let Ok(Object::Stream(stream)) = doc.get_object(id) else {
        return Err("not a stream");
    };
    let dict = &stream.dict;
    if matches!(dict.get(b"ImageMask"), Ok(Object::Boolean(true))) {
        return Err("stencil mask");
    }
    if !decode_is_default(doc, dict) {
        return Err("has a Decode array");
    }
    // DecodeParms change what the filtered bytes mean (a Flate Predictor
    // produces filtered rows, not pixels; /ColorTransform changes DCT color
    // interpretation). Treating such data as raw pixels silently corrupts.
    // Only parameters that provably change nothing are accepted.
    if !decode_parms_understood(doc, dict) {
        return Err("has DecodeParms (predictor / color transform)");
    }
    // A color-key /Mask array makes specific sample values transparent; the
    // lossy pipeline would shift values across the key range and break the
    // mask (a stencil /Mask *stream* is unaffected by recompressing the base).
    if matches!(dict.get(b"Mask").and_then(|o| resolve(doc, o)), Ok(Object::Array(_))) {
        return Err("has a color-key mask");
    }
    // Resource cap: refuse to expand pathological dimensions (a kilobyte of
    // stream can declare a gigapixel image).
    let w = dict_int(dict, b"Width").ok_or("no width")? as u64;
    let h = dict_int(dict, b"Height").ok_or("no height")? as u64;
    if w == 0 || h == 0 || w > 65_535 || h > 65_535 || w * h > MAX_PIXELS {
        return Err("dimensions out of bounds");
    }
    let filters = filter_names(dict);
    let filters: Vec<&str> = filters.iter().map(|s| s.as_str()).collect();
    match filters.as_slice() {
        ["DCTDecode"] | ["FlateDecode", "DCTDecode"] => {
            // Only gray and RGB are re-encodable. A CMYK JPEG must never be
            // converted to RGB here: Adobe-style CMYK is stored inverted by
            // convention, which a generic decoder does not apply, so the
            // "conversion" silently turns white into black.
            let components =
                colorspace_components(doc, dict).ok_or("unsupported color space")?;
            let data = if filters.len() == 2 {
                inflate_dct_wrapper(stream).map_err(|_| "undecodable or oversized Flate wrapper")?
            } else {
                stream.content.clone()
            };
            if !jpeg_plausible(&data, Some((w as u32, h as u32))) {
                return Err("JPEG header disagrees with the image or is implausible");
            }
            // The decoder trusts the JPEG's own header, so bound it too.
            let mut reader = image::ImageReader::with_format(
                std::io::Cursor::new(&data),
                image::ImageFormat::Jpeg,
            );
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(65_535);
            limits.max_image_height = Some(65_535);
            limits.max_alloc = Some(512 << 20);
            reader.limits(limits);
            let img = reader.decode().map_err(|_| "undecodable or oversized JPEG")?;
            // The JPEG's own component layout must agree with the PDF color
            // space; anything else (CMYK data under an RGB ICC profile, 16-bit,
            // alpha) would need a conversion we cannot do faithfully.
            match (&img, components) {
                (DynamicImage::ImageRgb8(_), 3) | (DynamicImage::ImageLuma8(_), 1) => Ok(img),
                _ => Err("JPEG components do not match color space"),
            }
        }
        ["FlateDecode"] | [] if indexed_palette(doc, dict)?.is_some() => {
            let (components, palette, _) = indexed_palette(doc, dict)?.unwrap();
            if components == 4 {
                // Expanded as four channels by recompress_cmyk_jpeg instead.
                return Err("Indexed with a CMYK base");
            }
            let bpc = dict_int(dict, b"BitsPerComponent").ok_or("no BitsPerComponent")?;
            if !matches!(bpc, 1 | 2 | 4 | 8) {
                return Err("unsupported bit depth");
            }
            // Inflate no further than the index data the image needs (plus
            // one PNG-predictor filter byte per row, which lopdf strips).
            let expected = (w as usize * bpc as usize).div_ceil(8) * h as usize + h as usize;
            let data = if filters.is_empty() {
                stream.content.clone()
            } else {
                inflate(stream, expected).map_err(|_| "undecodable or oversized Flate data")?
            };
            expand_indexed(&data, w as u32, h as u32, bpc, components, &palette)
        }
        ["FlateDecode"] | [] => {
            let bpc = dict_int(dict, b"BitsPerComponent").ok_or("no BitsPerComponent")?;
            if bpc != 8 {
                return Err("unsupported bit depth");
            }
            let (w, h) = (w as u32, h as u32);
            let components =
                colorspace_components(doc, dict).ok_or("unsupported color space")?;
            // Inflate no further than the sample data the image needs (plus
            // one PNG-predictor filter byte per row, which lopdf strips).
            let expected = w as usize * h as usize * components as usize + h as usize;
            let data = if filters.is_empty() {
                stream.content.clone()
            } else {
                inflate(stream, expected).map_err(|_| "undecodable or oversized Flate data")?
            };
            // Exact length only: excess bytes mean we misunderstand the
            // sample layout, and truncated data would render garbage.
            if data.len() as u64 != w as u64 * h as u64 * components as u64 {
                return Err("sample data length mismatch");
            }
            match components {
                3 => image::RgbImage::from_raw(w, h, data)
                    .map(DynamicImage::ImageRgb8)
                    .ok_or("sample data length mismatch"),
                1 => image::GrayImage::from_raw(w, h, data)
                    .map(DynamicImage::ImageLuma8)
                    .ok_or("sample data length mismatch"),
                _ => Err("unsupported component count"),
            }
        }
        _ => Err("unsupported filter chain"),
    }
}

/// An Indexed color space's base components, its palette (exactly
/// `(hival + 1) * components` bytes) and its base color space object as
/// written (so an ICC reference can be carried over). Ok(None) when the
/// image is not Indexed; Err when it is, but in a form we cannot expand
/// faithfully (a CMYK, Lab or nested base, or a short lookup table).
fn indexed_palette(
    doc: &Document,
    dict: &Dictionary,
) -> Result<Option<(u8, Vec<u8>, Object)>, &'static str> {
    let Ok(Object::Array(a)) = dict.get(b"ColorSpace").and_then(|o| resolve(doc, o)) else {
        return Ok(None);
    };
    if !matches!(a.first(), Some(Object::Name(n)) if n == b"Indexed") {
        return Ok(None);
    }
    let [_, base, hival, lookup] = a.as_slice() else {
        return Err("malformed Indexed color space");
    };
    let components = cs_object_components(doc, base)
        .or_else(|| is_cmyk_object(doc, base).then_some(4))
        .ok_or("Indexed base not gray, RGB or CMYK")?;
    let hival = match resolve(doc, hival) {
        Ok(Object::Integer(n)) if (0..=255).contains(n) => *n as usize,
        _ => return Err("malformed Indexed color space"),
    };
    let table = match resolve(doc, lookup) {
        Ok(Object::String(bytes, _)) => bytes.clone(),
        // At most 256 entries of 4 bytes are used; a longer table is legal,
        // but nothing past this bound is ever needed.
        Ok(Object::Stream(s)) => inflate(s, 1 << 20)
            .map_err(|_| "undecodable or oversized Indexed lookup table")?,
        _ => return Err("malformed Indexed color space"),
    };
    let need = (hival + 1) * components as usize;
    // A longer table is allowed (the excess is ignored); a shorter one leaves
    // colors undefined.
    if table.len() < need {
        return Err("Indexed lookup table too short");
    }
    Ok(Some((components, table[..need].to_vec(), base.clone())))
}

/// Expand packed palette indices into gray or RGB samples (see
/// expand_indices).
fn expand_indexed(
    data: &[u8],
    w: u32,
    h: u32,
    bpc: i64,
    components: u8,
    palette: &[u8],
) -> Result<DynamicImage, &'static str> {
    let out = expand_indices(data, w, h, bpc, components, palette)?;
    match components {
        3 => image::RgbImage::from_raw(w, h, out).map(DynamicImage::ImageRgb8),
        1 => image::GrayImage::from_raw(w, h, out).map(DynamicImage::ImageLuma8),
        _ => None,
    }
    .ok_or("sample data length mismatch")
}

/// Expand packed palette indices (1, 2, 4 or 8 bits, rows padded to a byte)
/// into `components` samples per pixel. Out-of-range indices are clamped to
/// the last entry, as ISO 32000 specifies.
fn expand_indices(
    data: &[u8],
    w: u32,
    h: u32,
    bpc: i64,
    components: u8,
    palette: &[u8],
) -> Result<Vec<u8>, &'static str> {
    if !matches!(bpc, 1 | 2 | 4 | 8) {
        return Err("unsupported bit depth");
    }
    let bpc = bpc as usize;
    let row = (w as usize * bpc).div_ceil(8);
    // Exact length only, as for direct samples.
    if data.len() as u64 != row as u64 * h as u64 {
        return Err("sample data length mismatch");
    }
    let c = components as usize;
    let last = palette.len() / c - 1;
    let mask = (1u16 << bpc) as usize - 1;
    let mut out = Vec::with_capacity(w as usize * h as usize * c);
    for line in data.chunks_exact(row) {
        for x in 0..w as usize {
            let bit = x * bpc;
            let index = (line[bit / 8] as usize >> (8 - bpc - bit % 8)) & mask;
            let at = index.min(last) * c;
            out.extend_from_slice(&palette[at..at + c]);
        }
    }
    Ok(out)
}

/// Is /Decode absent, or exactly the default mapping (and so a no-op)?
/// The default is [0 1] per component, or [0 2^bpc-1] for Indexed.
fn decode_is_default(doc: &Document, dict: &Dictionary) -> bool {
    let Ok(obj) = dict.get(b"Decode") else { return true };
    let Ok(Object::Array(a)) = resolve(doc, obj) else { return false };
    let Some(values) = a
        .iter()
        .map(|o| match o {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(*r as f64),
            _ => None,
        })
        .collect::<Option<Vec<f64>>>()
    else {
        return false;
    };
    let expected = match indexed_palette(doc, dict) {
        Ok(Some(_)) => match dict_int(dict, b"BitsPerComponent") {
            Some(b @ 1..=8) => vec![0.0, ((1u32 << b) - 1) as f64],
            _ => return false,
        },
        Ok(None) => {
            let Some(cs) = dict.get(b"ColorSpace").ok() else { return false };
            let Some(n) =
                cs_object_components(doc, cs).or_else(|| is_cmyk_object(doc, cs).then_some(4))
            else {
                return false;
            };
            [0.0, 1.0].repeat(n as usize)
        }
        Err(_) => return false,
    };
    values == expected
}

/// Are /DecodeParms absent, or fully accounted for by the Flate decoding
/// lopdf does (`decompressed_content`)? Accepted only on a Flate-only (or
/// unfiltered) stream:
///
/// - null, or dictionaries with no Predictor or Predictor 1: the other
///   Flate parameters (Colors, BitsPerComponent, Columns) only matter to a
///   predictor, so these change nothing;
/// - a PNG predictor (10..=15), which lopdf undoes, but only for 8-bit
///   samples (lopdf assumes at least 8 bits per component) whose Colors and
///   Columns match the image itself, so the undone rows are exactly the
///   image's rows. Callers still require the exact sample length.
///
/// TIFF predictor 2 (which lopdf leaves in place) and DCT parameters
/// (/ColorTransform) always count as not understood. Writers that replace
/// the samples must remove /DecodeParms.
fn decode_parms_understood(doc: &Document, dict: &Dictionary) -> bool {
    let Ok(obj) = dict.get(b"DecodeParms") else { return true };
    if filter_names(dict).iter().any(|f| f != "FlateDecode") {
        return false;
    }
    let components = match indexed_palette(doc, dict) {
        Ok(Some(_)) => Some(1),
        Ok(None) => dict.get(b"ColorSpace").ok().and_then(|cs| {
            cs_object_components(doc, cs).or_else(|| is_cmyk_object(doc, cs).then_some(4))
        }),
        Err(_) => None,
    };
    let noop = |o: &Object| match resolve(doc, o) {
        Ok(Object::Null) => true,
        Ok(Object::Dictionary(d)) => match d.get(b"Predictor") {
            Err(_) => true,
            Ok(_) => match dict_int(d, b"Predictor") {
                Some(1) => true,
                Some(10..=15) => {
                    dict_int(d, b"BitsPerComponent").unwrap_or(8) == 8
                        && dict_int(dict, b"BitsPerComponent") == Some(8)
                        && components.is_some()
                        && dict_int(d, b"Colors").unwrap_or(1) == components.unwrap_or(0) as i64
                        && dict_int(d, b"Columns").unwrap_or(1) == dict_int(dict, b"Width").unwrap_or(-1)
                }
                _ => false,
            },
        },
        _ => false,
    };
    match resolve(doc, obj) {
        Ok(Object::Array(a)) => a.iter().all(noop),
        Ok(_) => noop(obj),
        Err(_) => false,
    }
}

fn colorspace_components(doc: &Document, dict: &Dictionary) -> Option<u8> {
    cs_object_components(doc, dict.get(b"ColorSpace").ok()?)
}

/// Components of a gray or RGB color space object (name, ICCBased array, or
/// a reference to either); None for anything else.
fn cs_object_components(doc: &Document, cs: &Object) -> Option<u8> {
    let cs = resolve(doc, cs).ok()?;
    match cs {
        Object::Name(n) => match n.as_slice() {
            b"DeviceRGB" | b"CalRGB" => Some(3),
            b"DeviceGray" | b"CalGray" => Some(1),
            _ => None,
        },
        // [/DeviceRGB]: a family with no parameters, written as an array
        // (seen in real files; readers accept it).
        Object::Array(a) if a.len() == 1 && matches!(a[0], Object::Name(_)) => {
            cs_object_components(doc, &a[0])
        }
        Object::Array(a) => {
            if matches!(a.first(), Some(Object::Name(n)) if n == b"ICCBased")
                && let Some(Object::Reference(rid)) = a.get(1)
                    && let Ok(Object::Stream(s)) = doc.get_object(*rid) {
                        return dict_int(&s.dict, b"N").and_then(|n| match n {
                            3 => Some(3),
                            1 => Some(1),
                            _ => None,
                        });
                    }
            None
        }
        _ => None,
    }
}

fn colorspace_name(doc: &Document, dict: &Dictionary) -> String {
    match dict.get(b"ColorSpace").and_then(|o| resolve(doc, o)) {
        Ok(Object::Name(n)) => String::from_utf8_lossy(n).into_owned(),
        Ok(Object::Array(a)) => match a.first() {
            Some(Object::Name(n)) => {
                let base = String::from_utf8_lossy(n).into_owned();
                if n == b"ICCBased"
                    && let Some(Object::Reference(rid)) = a.get(1)
                        && let Ok(Object::Stream(s)) = doc.get_object(*rid)
                            && let Some(comps) = dict_int(&s.dict, b"N") {
                                return format!("{base}(N={comps})");
                            }
                base
            }
            _ => "array".into(),
        },
        _ => "-".into(),
    }
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

pub(crate) fn resolve_dict<'a>(doc: &'a Document, obj: &'a Object) -> lopdf::Result<&'a Dictionary> {
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
mod tests {
    use super::*;

    /// Build a one-image document in memory (no PDF parsing needed).
    fn doc_with_image(build: impl FnOnce(&mut Dictionary), data: &[u8]) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(b"XObject".to_vec()));
        dict.set("Subtype", Object::Name(b"Image".to_vec()));
        dict.set("Width", 4i64);
        dict.set("Height", 4i64);
        dict.set("BitsPerComponent", 8i64);
        dict.set("ColorSpace", Object::Name(b"DeviceRGB".to_vec()));
        build(&mut dict);
        let id = doc.add_object(lopdf::Stream::new(dict, data.to_vec()));
        (doc, id)
    }

    #[test]
    fn decode_image_accepts_exact_rgb() {
        let (doc, id) = doc_with_image(|_| {}, &[0u8; 48]); // 4*4*3
        assert!(decode_image(&doc, id).is_ok());
    }

    #[test]
    fn decode_image_rejects_premultiplied_soft_mask() {
        const MATTE: &str = "premultiplied soft mask (/Matte) not resamplable as a pair";
        // Parent whose /SMask carries /Matte.
        let (mut doc, id) = doc_with_image(|_| {}, &[0u8; 48]);
        let mut mask = Dictionary::new();
        mask.set("Subtype", Object::Name(b"Image".to_vec()));
        mask.set("Matte", Object::Array(vec![0.into(), 0.into(), 0.into()]));
        let mask_id = doc.add_object(lopdf::Stream::new(mask, vec![0u8; 16]));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
            s.dict.set("SMask", Object::Reference(mask_id));
        }
        assert_eq!(decode_image(&doc, id), Err(MATTE));

        // The mask itself.
        let (doc, id) = doc_with_image(
            |d| {
                d.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
                d.set("Matte", Object::Array(vec![0.into(), 0.into(), 0.into()]));
            },
            &[0u8; 16],
        );
        assert_eq!(decode_image(&doc, id), Err(MATTE));
    }

    /// A 64x64 RGB parent (visually gray) whose soft mask has /Matte, drawn
    /// 0.1 inch wide (640 dpi), plus its ImageInfo.
    fn matte_pair_doc() -> (Document, ObjectId, ObjectId, ImageInfo) {
        let n = 64usize;
        let rgb: Vec<u8> = (0..n * n).flat_map(|i| [((i * 7) % 256) as u8; 3]).collect();
        let (mut doc, id) = doc_with_image(
            |d| {
                d.set("Width", n as i64);
                d.set("Height", n as i64);
            },
            &rgb,
        );
        let mut mask = Dictionary::new();
        mask.set("Subtype", Object::Name(b"Image".to_vec()));
        mask.set("Width", n as i64);
        mask.set("Height", n as i64);
        mask.set("BitsPerComponent", 8i64);
        mask.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
        mask.set("Matte", Object::Array(vec![0.into(), 0.into(), 0.into()]));
        let alpha: Vec<u8> = (0..n * n).map(|i| ((i % n) * 4) as u8).collect();
        let mask_id = doc.add_object(lopdf::Stream::new(mask, alpha));
        if let Ok(Object::Stream(s)) = doc.get_object_mut(id) {
            s.dict.set("SMask", Object::Reference(mask_id));
        }
        let info = ImageInfo {
            id,
            width: n as i64,
            height: n as i64,
            bpc: 8,
            color: "DeviceRGB".into(),
            components: Some(3),
            filters: vec![],
            stream_len: rgb.len(),
            extent: Some(Extent { w_inch: 0.1, h_inch: 0.1 }),
        };
        (doc, id, mask_id, info)
    }

    fn empty_stats() -> CompressStats {
        CompressStats {
            images_total: 0,
            images_recompressed: 0,
            streams_deduped: 0,
            fonts_merged: 0,
            fonts_subset: 0,
            fonts_converted: 0,
            stream_bytes_saved: 0,
            log: Vec::new(),
        }
    }

    #[test]
    fn matte_pair_is_resampled_together_and_stays_rgb() {
        let (mut doc, id, mask_id, info) = matte_pair_doc();
        let mut stats = empty_stats();
        let done = recompress_matte_pairs(&mut doc, &[info], &CompressOptions::default(), &mut stats);
        assert!(matches!(done.get(&id), Some(MatteHalf::Written)));
        assert!(matches!(done.get(&mask_id), Some(MatteHalf::Written)));
        let dict = |o| match doc.get_object(o) {
            Ok(Object::Stream(s)) => s.dict.clone(),
            _ => panic!(),
        };
        let (p, m) = (dict(id), dict(mask_id));
        // 64 px over 0.1 inch is 640 dpi; 150 dpi gives 15 px, for both.
        for d in [&p, &m] {
            assert_eq!(dict_int(d, b"Width"), Some(15));
            assert_eq!(dict_int(d, b"Height"), Some(15));
        }
        // Visually gray, but never converted: /Matte has three entries.
        assert_eq!(p.get(b"ColorSpace").unwrap(), &Object::Name(b"DeviceRGB".to_vec()));
        assert!(m.get(b"Matte").is_ok());
        assert_eq!(p.get(b"SMask").unwrap(), &Object::Reference(mask_id));
        // The rewritten pair decodes at the new size.
        assert_eq!(decode_samples(&doc, id).unwrap().width(), 15);
        assert_eq!(decode_samples(&doc, mask_id).unwrap().width(), 15);
    }

    #[test]
    fn shared_matte_mask_is_left_alone() {
        let (mut doc, id, mask_id, info) = matte_pair_doc();
        let mut other = Dictionary::new();
        other.set("Subtype", Object::Name(b"Image".to_vec()));
        other.set("SMask", Object::Reference(mask_id));
        doc.add_object(lopdf::Stream::new(other, vec![]));
        let mut stats = empty_stats();
        let done = recompress_matte_pairs(&mut doc, &[info], &CompressOptions::default(), &mut stats);
        assert!(done.is_empty());
        let Ok(Object::Stream(s)) = doc.get_object(id) else { panic!() };
        assert_eq!(dict_int(&s.dict, b"Width"), Some(64));
    }

    #[test]
    fn decode_image_rejects_cmyk_jpeg() {
        // Rejected on the color space alone, before the JPEG is even parsed.
        let (doc, id) = doc_with_image(
            |d| {
                d.set("ColorSpace", Object::Name(b"DeviceCMYK".to_vec()));
                d.set("Filter", Object::Name(b"DCTDecode".to_vec()));
            },
            b"not a jpeg",
        );
        assert_eq!(decode_image(&doc, id), Err("unsupported color space"));
    }

    #[test]
    fn decode_image_rejects_decodeparms() {
        let (doc, id) = doc_with_image(
            |d| {
                let mut parms = Dictionary::new();
                parms.set("Predictor", 12i64);
                d.set("DecodeParms", Object::Dictionary(parms));
            },
            &[0u8; 48],
        );
        assert_eq!(
            decode_image(&doc, id),
            Err("has DecodeParms (predictor / color transform)")
        );
        // Empty parameters, or Predictor 1, change nothing.
        for predictor in [None, Some(1i64)] {
            let (doc, id) = doc_with_image(
                |d| {
                    let mut parms = Dictionary::new();
                    if let Some(p) = predictor {
                        parms.set("Predictor", p);
                    }
                    d.set("DecodeParms", Object::Array(vec![Object::Dictionary(parms)]));
                },
                &[0u8; 48],
            );
            assert!(decode_image(&doc, id).is_ok());
        }
    }

    #[test]
    fn png_predictor_is_undone_only_when_it_matches_the_image() {
        // 16x4 RGB, PNG "Sub" filter (1) on every row: first pixel
        // (10, 20, 30), then +1 per channel per pixel. Then Flate.
        let (w, h) = (16usize, 4usize);
        let mut rows = Vec::new();
        for _ in 0..h {
            rows.push(1u8);
            rows.extend([10u8, 20, 30]);
            for _ in 1..w {
                rows.extend([1u8, 1, 1]);
            }
        }
        let mut enc = lopdf::Stream::new(Dictionary::new(), rows);
        enc.compress().unwrap();
        assert!(enc.dict.get(b"Filter").is_ok(), "test data must actually be Flate");
        let parms = |colors: i64, columns: i64| {
            let mut p = Dictionary::new();
            p.set("Predictor", 12i64);
            p.set("Colors", colors);
            p.set("Columns", columns);
            p.set("BitsPerComponent", 8i64);
            Object::Dictionary(p)
        };
        let make = |p: Object| {
            doc_with_image(
                |d| {
                    d.set("Width", w as i64);
                    d.set("Height", h as i64);
                    d.set("Filter", Object::Name(b"FlateDecode".to_vec()));
                    d.set("DecodeParms", p);
                },
                &enc.content,
            )
        };
        let (doc, id) = make(parms(3, w as i64));
        let DynamicImage::ImageRgb8(img) = decode_image(&doc, id).unwrap() else { panic!() };
        for x in 0..w as u32 {
            let i = x as u8;
            assert_eq!(img.get_pixel(x, 3).0, [10 + i, 20 + i, 30 + i]);
        }
        // Colors or Columns that disagree with the image: refused.
        let (doc, id) = make(parms(1, 3 * w as i64));
        assert!(decode_image(&doc, id).is_err());
    }

    #[test]
    fn jpeg_headers_are_checked_before_any_decoder_allocates() {
        let jpeg = |w: u16, h: u16, pad: usize| {
            let mut sof = vec![8];
            sof.extend_from_slice(&h.to_be_bytes());
            sof.extend_from_slice(&w.to_be_bytes());
            sof.extend_from_slice(&[1, 1, 0x11, 0]);
            let mut j = vec![0xFF, 0xD8, 0xFF, 0xFE];
            j.extend_from_slice(&((pad + 2) as u16).to_be_bytes()); // COM padding
            j.extend(std::iter::repeat_n(0u8, pad));
            j.extend_from_slice(&[0xFF, 0xC0]);
            j.extend_from_slice(&((sof.len() + 2) as u16).to_be_bytes());
            j.extend_from_slice(&sof);
            j.extend_from_slice(&[0xFF, 0xD9]);
            j
        };
        let info = smallpeg::jpeg_info(&jpeg(300, 200, 0)).unwrap();
        assert_eq!((info.width, info.height, info.components), (300, 200, 1));
        // A header-only file declaring 11584x11584 (decoders would allocate
        // ~1 GiB from it): implausibly small for its pixel count.
        assert!(!jpeg_plausible(&jpeg(11584, 11584, 0), None));
        // Same header with a realistic amount of data: accepted.
        assert!(jpeg_plausible(&jpeg(1000, 1000, 2000), None));
        // Dimensions that disagree with the PDF image are refused.
        assert!(!jpeg_plausible(&jpeg(1000, 1000, 2000), Some((1000, 999))));
    }

    #[test]
    fn content_walks_are_bounded_by_parse_cost() {
        // A 2 MB form of 1M "q" operations parses to ~650 MB: over the live
        // bound on its own, so the walk stops before lopdf parses it.
        let q = b"q\n".repeat(1_000_000);
        assert!(parse_cost(&q) > MAX_LIVE_COST);
        assert!(Walk::new(1).parse(&q).is_none());
        // Operators count one, array elements (and brackets) half, other
        // operands an eighth: 2 + 4/2 + 3/8 rounded up.
        assert_eq!(parse_cost(b"/Fm12 Do 0 0 m [1 2]"), 2 + 2 + 1);
        // A form under the live bound but invoked over and over: the total
        // bound stops the walk after a dozen parses, not 50k.
        let form = b"q\n".repeat(500_000);
        let mut walk = Walk::new(u32::MAX);
        let mut n = 0;
        while let Some((_c, opened)) = walk.parse(&form) {
            walk.close(opened);
            n += 1;
            assert!(n < 50_000);
        }
        assert_eq!(n, MAX_WALK_COST / 500_000);
    }

    #[test]
    fn inflation_is_bounded() {
        let mut st = lopdf::Stream::new(Dictionary::new(), vec![0u8; 1 << 20]);
        st.compress().unwrap();
        assert!(inflate(&st, 1 << 20).is_ok());
        assert!(inflate(&st, 1000).is_err());
    }

    #[test]
    fn default_decode_is_accepted_and_others_refused() {
        let rgb = |decode: Vec<Object>| {
            doc_with_image(|d| d.set("Decode", Object::Array(decode)), &[0u8; 48])
        };
        let (doc, id) = rgb(vec![0.into(), 1.into(), 0.into(), 1.into(), 0.into(), 1.into()]);
        assert!(decode_image(&doc, id).is_ok());
        let (doc, id) = rgb(vec![1.into(), 0.into(), 1.into(), 0.into(), 1.into(), 0.into()]);
        assert_eq!(decode_image(&doc, id), Err("has a Decode array"));
    }

    #[test]
    fn one_element_colorspace_arrays_are_understood() {
        let (doc, id) = doc_with_image(
            |d| d.set("ColorSpace", Object::Array(vec![Object::Name(b"DeviceRGB".to_vec())])),
            &[0u8; 48],
        );
        assert!(decode_image(&doc, id).is_ok());
        let cmyk = Object::Array(vec![Object::Name(b"DeviceCMYK".to_vec())]);
        assert!(is_cmyk_object(&doc, &cmyk));
    }

    #[test]
    fn non_baseline_huffman_symbols_are_rejected() {
        // A sharp black/white pattern at quality 100 (quant value 1).
        let px: Vec<u8> = (0..64 * 64).map(|i| if (i / 4 + i / 256) % 2 == 0 { 0 } else { 255 }).collect();
        let raw = smallpeg::Encoder::new(80)
            .progressive(false)
            .quant_preset(smallpeg::QuantPreset::AnnexK)
            .encode_gray(&px, 64, 64);
        assert!(huffman_symbols_baseline(&raw));
        // Hand-made DHT with AC symbol 0x0B (size 11): refused.
        let mut bad = vec![0xFF, 0xD8, 0xFF, 0xC4, 0x00, 0x14, 0x10];
        bad.extend([1u8].iter().chain([0u8; 15].iter()));
        bad.push(0x0B);
        bad.extend([0xFF, 0xDA, 0x00, 0x02]);
        assert!(!huffman_symbols_baseline(&bad));
        bad[23] = 0x0A;
        assert!(huffman_symbols_baseline(&bad));
    }

    #[test]
    fn jpeg_metadata_is_stripped_losslessly_and_only_on_request() {
        let px: Vec<u8> = (0..32 * 32 * 3).map(|i| (i * 7 % 251) as u8).collect();
        let clean = smallpeg::Encoder::new(80).encode_rgb(&px, 32, 32);
        // An EXIF segment carrying a GPS-looking payload and a blank
        // thumbnail-sized tail (so the Flate-wrapped copy really deflates).
        let mut payload = b"Exif\0\0MM\0*GPS 35.6812N 139.7671E".to_vec();
        payload.resize(4000, 0);
        let mut tagged = clean[..2].to_vec();
        tagged.extend([0xFF, 0xE1]);
        tagged.extend(((payload.len() + 2) as u16).to_be_bytes());
        tagged.extend(&payload);
        tagged.extend(&clean[2..]);
        let (mut doc, plain) = doc_with_image(|d| d.set("Filter", "DCTDecode"), &tagged);
        let mut wd = doc.get_object(plain).unwrap().as_stream().unwrap().dict.clone();
        wd.remove(b"Filter");
        let mut ws = lopdf::Stream::new(wd.clone(), tagged.clone());
        ws.compress().unwrap();
        assert!(ws.content.len() < tagged.len(), "deflated");
        ws.dict.set("Filter", Object::Array(vec!["FlateDecode".into(), "DCTDecode".into()]));
        let wrapped = doc.add_object(ws);
        let untouched = doc.add_object(lopdf::Stream::new(
            { let mut d = wd.clone(); d.set("Filter", "DCTDecode"); d },
            clean.clone(),
        ));

        let inflate_unless_bare = |s: &lopdf::Stream| match s.dict.get(b"Filter").unwrap() {
            Object::Name(_) => s.content.clone(),
            _ => inflate_dct_wrapper(s).unwrap(),
        };
        let (n, saved) = strip_jpeg_metadata(&mut doc);
        assert_eq!(n, 2);
        assert!(saved > 0);
        let get = |id| doc.get_object(id).unwrap().as_stream().unwrap().clone();
        for id in [plain, wrapped] {
            let s = get(id);
            let jpeg = inflate_unless_bare(&s);
            assert!(!jpeg.windows(4).any(|w| w == b"Exif"), "EXIF gone");
            let a = smallpeg::decode_jpeg(&jpeg).unwrap();
            let b = smallpeg::decode_jpeg(&tagged).unwrap();
            assert_eq!(a.pixels, b.pixels, "pixels unchanged");
        }
        assert_eq!(get(untouched).content, clean, "no metadata: left as is");
    }

    #[test]
    fn two_color_images_become_exact_one_bit_palettes() {
        // 16x16 red/white checker as raw RGB.
        let n = 16usize;
        let rgb: Vec<u8> = (0..n * n)
            .flat_map(|i| if (i % n / 4 + i / n / 4).is_multiple_of(2) { [255, 0, 0] } else { [255, 255, 255] })
            .collect();
        let (mut doc, id) = doc_with_image(
            |d| {
                d.set("Width", n as i64);
                d.set("Height", n as i64);
            },
            &rgb,
        );
        let info = ImageInfo {
            id,
            width: n as i64,
            height: n as i64,
            bpc: 8,
            color: "DeviceRGB".into(),
            components: Some(3),
            filters: vec![],
            stream_len: rgb.len(),
            extent: None,
        };
        let img = decode_image(&doc, id).unwrap();
        assert!(matches!(
            recompress_two_color(&mut doc, &info, &img, false),
            TwoColor::Recompressed { .. }
        ));
        // Decoding the rewritten stream through its palette gives back
        // exactly the original pixels.
        let Ok(Object::Stream(s)) = doc.get_object(id) else { panic!() };
        assert_eq!(dict_int(&s.dict, b"BitsPerComponent"), Some(8));
        let (comps, palette, _) = indexed_palette(&doc, &s.dict).unwrap().unwrap();
        assert_eq!((comps, palette.clone()), (3, vec![255, 0, 0, 255, 255, 255]));
        let indices = s.decompressed_content().unwrap();
        let back = expand_indices(&indices, n as u32, n as u32, 8, 3, &palette).unwrap();
        assert_eq!(back, rgb);
        // A third color anywhere disqualifies.
        let mut three = rgb.clone();
        three[0] = 7;
        let (mut doc, id) = doc_with_image(
            |d| {
                d.set("Width", n as i64);
                d.set("Height", n as i64);
            },
            &three,
        );
        let img = decode_image(&doc, id).unwrap();
        assert!(matches!(recompress_two_color(&mut doc, &info_for(id, n, three.len()), &img, false), TwoColor::MoreColors));
    }

    fn info_for(id: ObjectId, n: usize, len: usize) -> ImageInfo {
        ImageInfo {
            id,
            width: n as i64,
            height: n as i64,
            bpc: 8,
            color: "DeviceRGB".into(),
            components: Some(3),
            filters: vec![],
            stream_len: len,
            extent: None,
        }
    }

    #[test]
    fn bleed_fills_hidden_pixels_from_visible_neighbors() {
        // 4x1 gray: visible 100, 200, then two hidden pixels holding black.
        let mut px = vec![100, 200, 0, 0];
        bleed(&mut px, 4, 1, 1, vec![false, false, true, true]);
        assert_eq!(px, vec![100, 200, 200, 200]);
        // Visible pixels never change; a hidden pixel between two visible
        // ones takes their mean.
        let mut px = vec![10, 0, 30];
        bleed(&mut px, 3, 1, 1, vec![false, true, false]);
        assert_eq!(px, vec![10, 20, 30]);
    }

    #[test]
    fn indexed_images_expand_through_their_palette() {
        // 4-bit indices 0..3 over a 4-entry RGB palette; index 9 clamps to 3.
        let palette = vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255];
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            Object::Name(b"DeviceRGB".to_vec()),
            3.into(),
            Object::String(palette, lopdf::StringFormat::Hexadecimal),
        ]);
        let (doc, id) = doc_with_image(
            |d| {
                d.set("ColorSpace", cs);
                d.set("BitsPerComponent", 4i64);
                d.set("Decode", Object::Array(vec![0.into(), 15.into()]));
            },
            // 4 px per row = 2 bytes per row, 4 rows.
            &[0x01, 0x23, 0x01, 0x23, 0x01, 0x23, 0x01, 0x29],
        );
        let DynamicImage::ImageRgb8(img) = decode_image(&doc, id).unwrap() else { panic!() };
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [255, 0, 0]);
        assert_eq!(img.get_pixel(3, 0).0, [0, 0, 255]);
        assert_eq!(img.get_pixel(3, 3).0, [0, 0, 255]);
        // A wrong length is refused, not guessed at.
        let (doc, id) = doc_with_image(
            |d| {
                d.set("ColorSpace", Object::Array(vec![
                    Object::Name(b"Indexed".to_vec()),
                    Object::Name(b"DeviceRGB".to_vec()),
                    0.into(),
                    Object::String(vec![1, 2, 3], lopdf::StringFormat::Literal),
                ]));
                d.set("BitsPerComponent", 8i64);
            },
            &[0u8; 15],
        );
        assert_eq!(decode_image(&doc, id), Err("sample data length mismatch"));
    }

    #[test]
    fn decode_image_rejects_color_key_mask() {
        let (doc, id) = doc_with_image(
            |d| d.set("Mask", Object::Array(vec![0.into(), 10.into(), 0.into(), 10.into(), 0.into(), 10.into()])),
            &[0u8; 48],
        );
        assert_eq!(decode_image(&doc, id), Err("has a color-key mask"));
    }

    #[test]
    fn decode_image_rejects_length_mismatch() {
        // 4x4x3 = 48 expected; give 47.
        let (doc, id) = doc_with_image(|_| {}, &[0u8; 47]);
        assert_eq!(decode_image(&doc, id), Err("sample data length mismatch"));
    }

    #[test]
    fn decode_image_rejects_giant_dimensions() {
        // 50000 x 50000 = 2.5 gigapixels, far over the 100 Mpixel cap.
        let (doc, id) = doc_with_image(
            |d| {
                d.set("Width", 50_000i64);
                d.set("Height", 50_000i64);
            },
            &[0u8; 48],
        );
        assert_eq!(decode_image(&doc, id), Err("dimensions out of bounds"));
    }

    /// Minimal TrueType font: head + name tables, one MacRoman name record
    /// carrying `<tag>+Foo`, plus a fake glyf table with arbitrary bytes.
    fn tiny_font(tag: &[u8; 6], checksum_seed: u8, glyf: &[u8]) -> Vec<u8> {
        let mut name_table = Vec::new();
        let string = [tag.as_slice(), b"+Foo"].concat();
        name_table.extend_from_slice(&0u16.to_be_bytes()); // format
        name_table.extend_from_slice(&1u16.to_be_bytes()); // count
        name_table.extend_from_slice(&18u16.to_be_bytes()); // stringOffset
        for v in [1u16, 0, 0, 6, string.len() as u16, 0] {
            name_table.extend_from_slice(&v.to_be_bytes());
        }
        name_table.extend_from_slice(&string);

        let mut head_table = vec![0u8; 54];
        head_table[8..12].copy_from_slice(&[checksum_seed; 4]); // checkSumAdjustment

        let tables: [(&[u8; 4], &[u8]); 3] =
            [(b"glyf", glyf), (b"head", &head_table), (b"name", &name_table)];
        let mut out = Vec::new();
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0u8; 6]); // searchRange etc., irrelevant here
        let mut offset = 12 + tables.len() * 16;
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            // Only the name table's checksum depends on the subset tag.
            let cksum = if *tag == b"name" { checksum_seed } else { 0xAB };
            out.extend_from_slice(&[cksum; 4]);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            offset += data.len();
        }
        for (_tag, data) in &tables {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn same_font_different_tag_matches() {
        let a = tiny_font(b"SKOJEB", 0x11, b"outlines");
        let b = tiny_font(b"RZEDQD", 0x22, b"outlines");
        assert_eq!(
            truetype_tag_neutralized(&a).unwrap(),
            truetype_tag_neutralized(&b).unwrap()
        );
    }

    #[test]
    fn different_outlines_do_not_match() {
        let a = tiny_font(b"SKOJEB", 0x11, b"outlines");
        let b = tiny_font(b"SKOJEB", 0x11, b"OUTLINES");
        assert_ne!(
            truetype_tag_neutralized(&a).unwrap(),
            truetype_tag_neutralized(&b).unwrap()
        );
    }

    #[test]
    fn non_truetype_is_rejected() {
        assert!(truetype_tag_neutralized(b"%!PS-AdobeFont").is_none());
        assert!(truetype_tag_neutralized(b"").is_none());
    }

    #[test]
    fn tag_neutralization_forms() {
        let mut ascii = b"SKOJEB+ArialMT".to_vec();
        neutralize_subset_tag(&mut ascii);
        assert_eq!(ascii, b"AAAAAA+ArialMT");

        let mut utf16: Vec<u8> = "SKOJEB+A".bytes().flat_map(|c| [0, c]).collect();
        neutralize_subset_tag(&mut utf16);
        assert_eq!(utf16, "AAAAAA+A".bytes().flat_map(|c| [0, c]).collect::<Vec<u8>>());

        let mut untagged = b"ArialMT".to_vec();
        neutralize_subset_tag(&mut untagged);
        assert_eq!(untagged, b"ArialMT");

        assert_eq!(
            split_subset_name(b"SKOJEB+ArialMT"),
            Some((b"SKOJEB".as_slice(), b"ArialMT".as_slice()))
        );
        assert_eq!(split_subset_name(b"ArialMT"), None);
        assert_eq!(split_subset_name(b"skojeb+ArialMT"), None);
    }

    fn key_of(obj: &Object) -> Vec<u8> {
        let mut out = Vec::new();
        write_object_key(obj, &mut out);
        out
    }

    fn typed_dict(type_name: &[u8], key: &str, value: Object) -> Object {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(type_name.to_vec()));
        dict.set(key, value);
        Object::Dictionary(dict)
    }

    /// The dedup key must separate objects the Debug rendering cannot.
    #[test]
    fn dedup_key_is_injective_where_debug_is_not() {
        // from_utf8_lossy maps every invalid byte to the same replacement char.
        let a = Object::Name(vec![0x80]);
        let b = Object::Name(vec![0x81]);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        let a = Object::String(vec![0x80], lopdf::StringFormat::Literal);
        let b = Object::String(vec![0x81], lopdf::StringFormat::Literal);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        // Integer and Real print identically for values with no fraction.
        let a = Object::Integer(100_000_000);
        let b = Object::Real(1e8);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        // Same bytes, different framing: the length prefixes keep them apart.
        let mut ab_c = Dictionary::new();
        ab_c.set("ab", Object::Name(b"c".to_vec()));
        let mut a_bc = Dictionary::new();
        a_bc.set("a", Object::Name(b"bc".to_vec()));
        assert_ne!(
            key_of(&Object::Dictionary(ab_c)),
            key_of(&Object::Dictionary(a_bc))
        );

        // The property dedup relies on: differing sub-references differ.
        assert_ne!(
            key_of(&Object::Reference((1, 0))),
            key_of(&Object::Reference((1, 1)))
        );
    }

    /// Two byte-identical `/Type /OCG` dicts are two independently toggleable
    /// layers. Merging them silently turns two layers into one.
    #[test]
    fn identity_bearing_dicts_are_not_merged() {
        for type_name in [b"OCG".as_slice(), b"Page", b"Annot", b"StructElem"] {
            let mut doc = Document::new();
            doc.objects
                .insert((1, 0), typed_dict(type_name, "Name", Object::string_literal("x")));
            doc.objects
                .insert((2, 0), typed_dict(type_name, "Name", Object::string_literal("x")));
            let mut owner = Dictionary::new();
            owner.set(
                "Kids",
                Object::Array(vec![Object::Reference((1, 0)), Object::Reference((2, 0))]),
            );
            doc.objects.insert((3, 0), Object::Dictionary(owner));

            let (merged, _) = dedup_streams(&mut doc);
            assert_eq!(merged, 0, "{} must not be merged", String::from_utf8_lossy(type_name));
            assert!(doc.objects.contains_key(&(2, 0)));
        }
    }

    /// Non-UTF-8 names used to collapse to one replacement character, so two
    /// different graphics states merged and half the page got the wrong one.
    #[test]
    fn dicts_differing_only_in_a_non_utf8_byte_survive() {
        let mut doc = Document::new();
        doc.objects
            .insert((1, 0), typed_dict(b"ExtGState", "BM", Object::Name(vec![0x80])));
        doc.objects
            .insert((2, 0), typed_dict(b"ExtGState", "BM", Object::Name(vec![0x81])));
        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 0);
        assert!(doc.objects.contains_key(&(2, 0)));
    }

    /// The win the allowlist exists to keep: 2,555 identical transparency
    /// group dicts in one paper's worth of figures.
    #[test]
    fn value_dicts_and_streams_still_merge() {
        let mut doc = Document::new();
        let group = || {
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"Group".to_vec()));
            d.set("S", Object::Name(b"Transparency".to_vec()));
            Object::Dictionary(d)
        };
        doc.objects.insert((1, 0), group());
        doc.objects.insert((2, 0), group());
        let mut form = Dictionary::new();
        form.set("Group", Object::Reference((2, 0)));
        doc.objects
            .insert((3, 0), Object::Stream(lopdf::Stream::new(form, b"x".to_vec())));

        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 1);
        assert!(!doc.objects.contains_key(&(2, 0)));
        let Some(Object::Stream(s)) = doc.objects.get(&(3, 0)) else {
            panic!("form stream gone")
        };
        assert_eq!(s.dict.get(b"Group").unwrap(), &Object::Reference((1, 0)));
    }

    /// Object streams are file structure, not content: merging two would drop
    /// every object the loser carried.
    #[test]
    fn structural_streams_are_not_merged() {
        let mut doc = Document::new();
        for id in [(1, 0), (2, 0)] {
            let mut dict = Dictionary::new();
            dict.set("Type", Object::Name(b"ObjStm".to_vec()));
            doc.objects
                .insert(id, Object::Stream(lopdf::Stream::new(dict, b"payload".to_vec())));
        }
        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 0);
    }
}
