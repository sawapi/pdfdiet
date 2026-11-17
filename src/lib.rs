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
#![allow(
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::too_many_arguments
)]

use image::DynamicImage;
use lopdf::{Dictionary, Document, Object, ObjectId};

#[doc(hidden)] // public for the g4_force verification example only
pub mod ccitt;
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
mod recompress;
mod subset;
mod type1;

pub use images::{Extent, ImageInfo, collect_images};
pub use limits::load_options;

use decode::{decode_image, indexed_palette};
use dedup::dedup_streams;
use fontmerge::merge_font_subsets;
use images::target_size;
use jpeg::{
    ChromaClass, CmykOutcome, classify_chroma, encode_jpeg, recompress_cmyk_jpeg,
    strip_jpeg_metadata, try_lossless_jpeg_optimize,
};
use recompress::{
    BilevelOutcome, MatteHalf, TwoColor, bleed_under_soft_mask, recompress_bilevel,
    recompress_matte_pairs, recompress_two_color, resize_image, smallest_encoding,
};

// Shared with the font modules, which reach them through the crate root.
pub(crate) use images::page_resource_dicts;
pub(crate) use limits::{MAX_INFLATE, MAX_PASS_BYTES, Walk, inflate, page_content_bounded};

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
        stats.log.push(
            "document is signed; the signatures will not survive (explicitly allowed)".into(),
        );
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
            stats.log.push(format!(
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
            if filter == Some(b"FlateDecode".as_slice()) {
                " [flate]"
            } else {
                ""
            }
        ));
        stats.stream_bytes_saved += info.stream_len as i64 - jpeg.len() as i64;
        stats.images_recompressed += 1;
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
