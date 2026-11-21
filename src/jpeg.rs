//! JPEG paths: CMYK re-encoding, lossless optimization, metadata stripping,
//! and the encoder settings for re-encoded images.

use image::DynamicImage;
use lopdf::{Dictionary, Document, Object};

use crate::decode::{
    MAX_PIXELS, decode_is_default, decode_parms_understood, expand_indices, indexed_palette,
    inflate_dct_wrapper, is_cmyk_colorspace, jpeg_limits, jpeg_plausible, matte_bound,
};
use crate::images::{ImageInfo, target_size};
use crate::limits::inflate;
use crate::recompress::Staged;
use crate::{CompressOptions, dict_int, resolve};

/// Is this JPEG progressive (SOF2)? `None` for malformed or exotic
/// (hierarchical/lossless/arithmetic/12-bit) files, which we then refuse to
/// touch.
fn jpeg_is_progressive(data: &[u8]) -> Option<bool> {
    smallpeg::jpeg_info(data).map(|i| i.progressive)
}

enum CmykSource {
    /// A DCT stream (possibly unwrapped from Flate).
    Jpeg(Vec<u8>),
    /// Uncompressed 8-bit CMYK samples.
    Raw(Vec<u8>),
}

pub(crate) enum CmykOutcome {
    Recompressed {
        to: usize,
        resized: Option<(u32, u32)>,
        flate: bool,
    },
    /// Re-encoding did not beat the stored bytes; nothing was changed.
    Larger { to: usize },
    /// Not a CMYK JPEG this path can handle; nothing was changed.
    NotApplicable,
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
pub(crate) fn recompress_cmyk_jpeg(
    doc: &Document,
    staged: &mut Staged,
    info: &ImageInfo,
    opts: &CompressOptions,
) -> CmykOutcome {
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
            || matches!(
                dict.get(b"Mask").and_then(|o| resolve(doc, o)),
                Ok(Object::Array(_))
            )
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
            (
                match filters.as_slice() {
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
                },
                None,
            )
        }
    };
    let (data, indexed_base) = data;
    let src = match data {
        CmykSource::Jpeg(jpeg)
            if !jpeg_plausible(&jpeg, Some((info.width as u32, info.height as u32))) =>
        {
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
            filter = flate
                .dict
                .get(b"Filter")
                .is_ok()
                .then_some(b"FlateDecode".as_slice());
            jpeg = flate.content;
        }
    }
    if jpeg.len() >= info.stream_len {
        return CmykOutcome::Larger { to: jpeg.len() };
    }
    let Some(stream) = staged.stream(doc, info.id) else {
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
pub(crate) fn try_lossless_jpeg_optimize(
    doc: &Document,
    staged: &mut Staged,
    info: &ImageInfo,
) -> Option<(usize, usize)> {
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
                (
                    inflate_dct_wrapper(stream).ok()?,
                    stream.content.len(),
                    true,
                )
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
    let stream = staged.stream(doc, info.id)?;
    if unwrapped_flate {
        stream
            .dict
            .set("Filter", Object::Name(b"DCTDecode".to_vec()));
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
pub(crate) fn strip_jpeg_metadata(doc: &mut Document) -> (usize, i64) {
    let (mut count, mut saved) = (0, 0i64);
    for obj in doc.objects.values_mut() {
        let Object::Stream(stream) = obj else {
            continue;
        };
        if !matches!(stream.dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Image")
            || stream.dict.get(b"DecodeParms").is_ok()
        {
            continue;
        }
        let filters: Vec<Vec<u8>> = match stream.dict.get(b"Filter") {
            Ok(Object::Name(n)) => vec![n.clone()],
            Ok(Object::Array(a)) => a
                .iter()
                .filter_map(|o| o.as_name().ok().map(<[u8]>::to_vec))
                .collect(),
            _ => continue,
        };
        let wrapped = match filters
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
            .as_slice()
        {
            [b"DCTDecode"] => false,
            [b"FlateDecode", b"DCTDecode"] => true,
            _ => continue,
        };
        let jpeg = if wrapped {
            let Ok(d) = inflate_dct_wrapper(stream) else {
                continue;
            };
            d
        } else {
            stream.content.clone()
        };
        let Some(clean) = smallpeg::strip_metadata(&jpeg) else {
            continue;
        };
        if clean.len() >= jpeg.len() {
            continue; // no metadata
        }
        // A Flate wrapper stays if it still pays (small JPEGs' tables
        // deflate well); otherwise the bare JPEG is stored.
        let rewrapped = if wrapped {
            let mut s = lopdf::Stream::new(Dictionary::new(), clean.clone());
            s.compress()
                .ok()
                .map(|_| s.content)
                .filter(|c| c.len() < clean.len())
        } else {
            None
        };
        let (content, filter) = match rewrapped {
            Some(c) => (
                c,
                Object::Array(vec!["FlateDecode".into(), "DCTDecode".into()]),
            ),
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

/// How much chroma information an RGB image really carries.
pub(crate) enum ChromaClass {
    /// Visually grayscale — encode as a single luma channel.
    Gray,
    /// Smooth chroma (photographs) — 4:2:0 subsampling is invisible.
    SmoothColor,
    /// Sharp chroma edges (stamps, colored line art) — subsampling bleeds.
    SharpColor,
}

pub(crate) fn classify_chroma(rgb: &image::RgbImage) -> ChromaClass {
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
pub(crate) fn encode_jpeg(img: &DynamicImage, quality: u8, sharp_chroma: bool) -> Option<Vec<u8>> {
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
            enc.sampling(sampling)
                .encode_rgb(raw, img.width(), img.height())
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
                let Some(symbols) = t.get(17..17 + n) else {
                    return false;
                };
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::decode::tests::doc_with_image;

    #[test]
    fn non_baseline_huffman_symbols_are_rejected() {
        // A sharp black/white pattern at quality 100 (quant value 1).
        let px: Vec<u8> = (0..64 * 64)
            .map(|i| if (i / 4 + i / 256) % 2 == 0 { 0 } else { 255 })
            .collect();
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
        let mut wd = doc
            .get_object(plain)
            .unwrap()
            .as_stream()
            .unwrap()
            .dict
            .clone();
        wd.remove(b"Filter");
        let mut ws = lopdf::Stream::new(wd.clone(), tagged.clone());
        ws.compress().unwrap();
        assert!(ws.content.len() < tagged.len(), "deflated");
        ws.dict.set(
            "Filter",
            Object::Array(vec!["FlateDecode".into(), "DCTDecode".into()]),
        );
        let wrapped = doc.add_object(ws);
        let untouched = doc.add_object(lopdf::Stream::new(
            {
                let mut d = wd.clone();
                d.set("Filter", "DCTDecode");
                d
            },
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
}
