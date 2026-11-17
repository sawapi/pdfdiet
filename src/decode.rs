//! Decoding image streams to pixels. Anything not understood exactly (filters,
//! decode arrays, color spaces, premultiplied masks) is refused, and the image
//! is then left alone.

use image::DynamicImage;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::limits::{MAX_INFLATE, inflate};
use crate::{dict_int, filter_names, resolve};

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
pub(crate) fn matte_bound(doc: &Document, dict: &Dictionary) -> bool {
    if dict.get(b"Matte").is_ok() {
        return true;
    }
    matches!(
        dict.get(b"SMask").and_then(|o| resolve(doc, o)),
        Ok(Object::Stream(s)) if s.dict.get(b"Matte").is_ok()
    )
}

/// Is the image's color space four-component CMYK (DeviceCMYK, or an ICC
/// profile with N = 4)?
pub(crate) fn is_cmyk_colorspace(doc: &Document, dict: &Dictionary) -> bool {
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

/// Is a JPEG safe to hand to a decoder or optimizer? Its frame header must
/// declare exactly the PDF image's dimensions (when given), stay within
/// MAX_PIXELS, and not claim implausibly many pixels for its size: decoders
/// allocate from the header alone, so a few dozen bytes declaring a huge
/// frame would otherwise cost a gigabyte. Real JPEGs, even of blank pages,
/// stay well above one byte per thousand pixels.
pub(crate) fn jpeg_plausible(jpeg: &[u8], expected: Option<(u32, u32)>) -> bool {
    let Some(info) = smallpeg::jpeg_info(jpeg) else { return false };
    let (w, h) = (info.width, info.height);
    let pixels = w as u64 * h as u64;
    pixels <= MAX_PIXELS
        && expected.is_none_or(|e| e == (w, h))
        && (jpeg.len() as u64) * 1024 >= pixels
}

/// Most pixels any image may have before pdfdiet decodes it.
pub(crate) const MAX_PIXELS: u64 = 100_000_000;

/// The same bound handed to smallpeg, which enforces it again (and a scan
/// count bound) while parsing, should a file get past jpeg_plausible.
pub(crate) fn jpeg_limits() -> smallpeg::Limits {
    smallpeg::Limits::default().max_pixels(MAX_PIXELS)
}

/// Decode into a DynamicImage; `Err` carries the reason it was left alone.
pub(crate) fn decode_image(doc: &Document, id: ObjectId) -> Result<DynamicImage, &'static str> {
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
pub(crate) fn decode_samples(doc: &Document, id: ObjectId) -> Result<DynamicImage, &'static str> {
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
pub(crate) fn indexed_palette(
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
pub(crate) fn expand_indices(
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
pub(crate) fn decode_is_default(doc: &Document, dict: &Dictionary) -> bool {
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
pub(crate) fn decode_parms_understood(doc: &Document, dict: &Dictionary) -> bool {
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

pub(crate) fn colorspace_components(doc: &Document, dict: &Dictionary) -> Option<u8> {
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

pub(crate) fn colorspace_name(doc: &Document, dict: &Dictionary) -> String {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a one-image document in memory (no PDF parsing needed).
    pub(crate) fn doc_with_image(build: impl FnOnce(&mut Dictionary), data: &[u8]) -> (Document, ObjectId) {
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
}
