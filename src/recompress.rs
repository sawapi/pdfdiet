//! Resampling and re-encoding images: premultiplied soft-mask pairs,
//! two-color and bilevel images, lossless candidates, and filling pixels
//! hidden under a soft mask.

use std::collections::HashMap;

use image::DynamicImage;
use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::ccitt;
use crate::decode::{colorspace_components, decode_samples, indexed_palette};
use crate::images::{ImageInfo, target_size};
use crate::jpeg::{ChromaClass, classify_chroma, encode_jpeg};
use crate::limits::{MAX_INFLATE, inflate};
use crate::{CompressOptions, CompressStats, filter_names, resolve};

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
pub(crate) fn recompress_matte_pairs(
    doc: &mut Document,
    images: &[ImageInfo],
    opts: &CompressOptions,
    stats: &mut CompressStats,
) -> HashMap<ObjectId, MatteHalf> {
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
    // A pair's halves belong to no other pair (the parent has no /Matte,
    // the mask has one and a single parent), so pairs are independent and
    // can be processed on several threads, then written back in order.
    let outcomes = crate::parallel::map_bounded(
        &(0..images.len()).collect::<Vec<_>>(),
        |i| 2 * crate::decoded_pixels(&images[i]),
        |i| matte_pair(doc, &images[i], opts, &mask_refs),
    );
    let mut done = HashMap::new();
    for outcome in outcomes {
        outcome.staged.apply(doc);
        stats.log.extend(outcome.log);
        stats.stream_bytes_saved += outcome.saved;
        stats.images_recompressed += outcome.recompressed;
        done.extend(outcome.done);
    }
    done
}

/// What recompress_matte_pairs did with one parent image and its mask.
#[derive(Default)]
struct PairOutcome {
    log: Vec<String>,
    saved: i64,
    recompressed: usize,
    done: Vec<(ObjectId, MatteHalf)>,
    staged: Staged,
}

/// One /Matte pair, if `info` is the parent of one; reads the document only.
fn matte_pair(
    doc: &Document,
    info: &ImageInfo,
    opts: &CompressOptions,
    mask_refs: &HashMap<ObjectId, usize>,
) -> PairOutcome {
    let mut out = PairOutcome::default();
    let Some(mask_id) = matte_pair_mask(doc, info.id, mask_refs) else {
        return out;
    };
    let Some(mask_len) = (match doc.get_object(mask_id) {
        Ok(Object::Stream(s)) => Some(s.content.len()),
        _ => None,
    }) else {
        return out;
    };
    let (Ok(parent), Ok(mask)) = (decode_samples(doc, info.id), decode_samples(doc, mask_id))
    else {
        return out;
    };
    let parent_components = match parent {
        DynamicImage::ImageRgb8(_) => 3,
        DynamicImage::ImageLuma8(_) => 1,
        _ => return out,
    };
    if !matches!(mask, DynamicImage::ImageLuma8(_))
        || matte_len(doc, mask_id) != Some(parent_components)
        || (parent.width(), parent.height()) != (mask.width(), mask.height())
        || parent.width() as i64 != info.width
        || parent.height() as i64 != info.height
    {
        return out;
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
        return out;
    };
    let halves = [
        (info.id, info.stream_len, new_parent),
        (mask_id, mask_len, new_mask),
    ];
    let keep: Vec<bool> = if resized {
        let before = info.stream_len + mask_len;
        let after = halves[0].2.0.len() + halves[1].2.0.len();
        if after >= before {
            out.log.push(format!(
                "keep obj {} {} and its /Matte mask {} {}: downsampled pair {} >= original {}",
                info.id.0, info.id.1, mask_id.0, mask_id.1, after, before
            ));
            out.done.push((info.id, MatteHalf::Kept));
            out.done.push((mask_id, MatteHalf::Kept));
            return out;
        }
        vec![true, true]
    } else {
        halves
            .iter()
            .map(|(_, from, (data, _))| data.len() < *from)
            .collect()
    };
    for ((id, from, (data, filter)), write) in halves.into_iter().zip(keep) {
        if !write {
            out.log.push(format!(
                "keep obj {} {}: /Matte pair half, re-encode {} >= original {}",
                id.0,
                id.1,
                data.len(),
                from
            ));
            out.done.push((id, MatteHalf::Kept));
            continue;
        }
        let Some(stream) = out.staged.stream(doc, id) else {
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
        out.log.push(format!(
            "obj {} {}: {} -> {} bytes{} [matte pair{}]",
            id.0,
            id.1,
            from,
            to,
            if resized {
                format!(" (downsampled to {w}x{h})")
            } else {
                String::new()
            },
            if filter == Some(b"FlateDecode".as_slice()) {
                ", flate"
            } else {
                ""
            }
        ));
        out.saved += from as i64 - to as i64;
        out.recompressed += 1;
        out.done.push((id, MatteHalf::Written));
    }
    out
}

/// Stream replacements computed against an unchanged document and written
/// back afterwards, so that images can be processed on several threads.
#[derive(Default)]
pub(crate) struct Staged(Vec<(ObjectId, lopdf::Stream)>);

impl Staged {
    /// A copy of stream `id` to modify in place of the original.
    pub(crate) fn stream(&mut self, doc: &Document, id: ObjectId) -> Option<&mut lopdf::Stream> {
        let Some(Object::Stream(s)) = doc.objects.get(&id) else {
            return None;
        };
        self.0.push((id, s.clone()));
        self.0.last_mut().map(|(_, s)| s)
    }

    /// Write the replacements into the document.
    pub(crate) fn apply(self, doc: &mut Document) {
        for (id, s) in self.0 {
            doc.objects.insert(id, Object::Stream(s));
        }
    }
}

/// What recompress_matte_pairs did with one half of a pair.
pub(crate) enum MatteHalf {
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
    let Ok(Object::Stream(p)) = doc.get_object(parent) else {
        return None;
    };
    if p.dict.get(b"Matte").is_ok() {
        return None;
    }
    let Ok(Object::Reference(mask_id)) = p.dict.get(b"SMask") else {
        return None;
    };
    let Ok(Object::Stream(m)) = doc.get_object(*mask_id) else {
        return None;
    };
    let ok = m.dict.get(b"Matte").is_ok()
        && mask_refs.get(mask_id) == Some(&1)
        && m.dict.get(b"SMask").is_err()
        && m.dict.get(b"Mask").is_err();
    ok.then_some(*mask_id)
}

/// Number of entries in a mask's /Matte array, if every one is a number.
fn matte_len(doc: &Document, mask: ObjectId) -> Option<u8> {
    let Ok(Object::Stream(m)) = doc.get_object(mask) else {
        return None;
    };
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
pub(crate) fn smallest_encoding(
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

/// Lanczos3 downsampling via smallpeg's zero-dependency resampler (kept in
/// the same crate family as the encoder; quality verified equivalent to the
/// image crate's Lanczos3).
pub(crate) fn resize_image(img: &DynamicImage, new_w: u32, new_h: u32) -> DynamicImage {
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

pub(crate) enum TwoColor {
    Recompressed {
        to: usize,
    },
    /// Two colors, but the lossless encoding did not beat the stored bytes;
    /// left exactly as it is (lossy recompression would only blur it).
    Larger {
        to: usize,
    },
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
pub(crate) fn recompress_two_color(
    doc: &Document,
    staged: &mut Staged,
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
        let Ok(Object::Stream(s)) = doc.get_object(info.id) else {
            return TwoColor::MoreColors;
        };
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
        let palette: Vec<u8> = (0..2)
            .flat_map(|i| colors[i.min(colors.len() - 1)].to_vec())
            .collect();
        let indices = raw
            .chunks_exact(c)
            .map(|px| u8::from(px != colors[0]))
            .collect();
        let cs = Object::Array(vec![
            Object::Name(b"Indexed".to_vec()),
            base,
            1.into(),
            Object::String(palette, lopdf::StringFormat::Hexadecimal),
        ]);
        (indices, cs)
    };
    let Some((data, filter)) = flate_or_raw(samples) else {
        return TwoColor::MoreColors;
    };
    if data.len() >= info.stream_len {
        return TwoColor::Larger { to: data.len() };
    }
    let Some(stream) = staged.stream(doc, info.id) else {
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
pub(crate) fn bleed_under_soft_mask(doc: &Document, id: ObjectId, img: &mut DynamicImage) -> bool {
    let Ok(Object::Stream(s)) = doc.get_object(id) else {
        return false;
    };
    let Ok(Object::Reference(mask_id)) = s.dict.get(b"SMask") else {
        return false;
    };
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

pub(crate) enum BilevelOutcome {
    /// Replaced with a smaller CCITT G4 stream (new stream size).
    Recompressed { to: usize },
    /// G4 did not shrink it; the original stream is kept.
    Larger,
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
pub(crate) fn recompress_bilevel(
    doc: &Document,
    staged: &mut Staged,
    info: &ImageInfo,
) -> BilevelOutcome {
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
    // The same per-axis bound as every other image path. The changing
    // elements of one row cost a word per pixel, not a bit, so the packed
    // size below does not bound them on its own.
    if !(1..=65_535).contains(&info.width) || !(1..=65_535).contains(&info.height) {
        return BilevelOutcome::Unsupported;
    }
    let filters = filter_names(dict);
    // Inflate no further than the packed rows the image declares.
    let expected = (info.width.max(0) as usize)
        .div_ceil(8)
        .saturating_mul(info.height.max(0) as usize);
    if expected == 0 || expected > MAX_INFLATE {
        return BilevelOutcome::Unsupported;
    }
    let data = match filters
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .as_slice()
    {
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

    // Stop as soon as the encoding is no smaller than the original.
    let Some(g4) = ccitt::encode_g4_within(&data, w, h, info.stream_len) else {
        return BilevelOutcome::Larger;
    };
    if g4.len() >= info.stream_len {
        return BilevelOutcome::Larger;
    }

    let Some(stream) = staged.stream(doc, info.id) else {
        return BilevelOutcome::Unsupported;
    };
    let to = g4.len();
    let mut parms = Dictionary::new();
    parms.set("K", Object::Integer(-1));
    parms.set("Columns", w as i64);
    parms.set("Rows", h as i64);
    stream
        .dict
        .set("Filter", Object::Name(b"CCITTFaxDecode".to_vec()));
    stream.dict.set("DecodeParms", Object::Dictionary(parms));
    stream.dict.set("Length", to as i64);
    stream.set_content(g4);
    stream.allows_compression = false;
    BilevelOutcome::Recompressed { to }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilevel_rejects_dimensions_beyond_the_axis_bound() {
        // Widths up to 2^31 pack into MAX_INFLATE, but the changing
        // elements of a row take a word per pixel: anything wider than the
        // 65535 every other image path allows is refused before inflating.
        let w = 70_000usize;
        let data = vec![0x55u8; w / 8];
        let (doc, id) = doc_with_image(
            |d| {
                d.set("Width", w as i64);
                d.set("Height", 1i64);
                d.set("BitsPerComponent", 1i64);
                d.set("ColorSpace", Object::Name(b"DeviceGray".to_vec()));
            },
            &data,
        );
        let info = ImageInfo {
            id,
            width: w as i64,
            height: 1,
            bpc: 1,
            color: "DeviceGray".into(),
            components: Some(1),
            filters: vec![],
            stream_len: data.len(),
            extent: None,
        };
        assert!(matches!(
            recompress_bilevel(&doc, &mut Staged::default(), &info),
            BilevelOutcome::Unsupported
        ));
    }

    #[test]
    fn g4_encoding_stops_at_the_limit() {
        // Alternating pixels: G4 is larger than the packed input.
        let noisy = vec![0x55u8; 64 * 64 / 8];
        let full = crate::ccitt::encode_g4(&noisy, 64, 64);
        assert!(full.len() > noisy.len());
        assert!(crate::ccitt::encode_g4_within(&noisy, 64, 64, noisy.len()).is_none());
        assert_eq!(
            crate::ccitt::encode_g4_within(&noisy, 64, 64, usize::MAX),
            Some(full)
        );
    }

    use crate::decode::tests::doc_with_image;
    use crate::decode::{decode_image, expand_indices};
    use crate::dict_int;
    use crate::images::Extent;

    /// A 64x64 RGB parent (visually gray) whose soft mask has /Matte, drawn
    /// 0.1 inch wide (640 dpi), plus its ImageInfo.
    fn matte_pair_doc() -> (Document, ObjectId, ObjectId, ImageInfo) {
        let n = 64usize;
        let rgb: Vec<u8> = (0..n * n)
            .flat_map(|i| [((i * 7) % 256) as u8; 3])
            .collect();
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
            extent: Some(Extent {
                w_inch: 0.1,
                h_inch: 0.1,
            }),
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
        let done =
            recompress_matte_pairs(&mut doc, &[info], &CompressOptions::default(), &mut stats);
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
        assert_eq!(
            p.get(b"ColorSpace").unwrap(),
            &Object::Name(b"DeviceRGB".to_vec())
        );
        assert!(m.get(b"Matte").is_ok());
        assert_eq!(p.get(b"SMask").unwrap(), &Object::Reference(mask_id));
        // The rewritten pair decodes at the new size.
        assert_eq!(decode_samples(&doc, id).unwrap().width(), 15);
        assert_eq!(decode_samples(&doc, mask_id).unwrap().width(), 15);
    }

    #[test]
    fn matte_pair_half_that_does_not_shrink_keeps_the_other() {
        // Drawn at its own size, so each half is replaced only if it shrinks:
        // the parent (claimed to be stored in one byte) stays, the mask is
        // still re-encoded.
        let (mut doc, id, mask_id, mut info) = matte_pair_doc();
        info.extent = None;
        info.stream_len = 1;
        let mask_before = match doc.get_object(mask_id) {
            Ok(Object::Stream(s)) => s.content.len(),
            _ => panic!(),
        };
        let mut stats = empty_stats();
        let done =
            recompress_matte_pairs(&mut doc, &[info], &CompressOptions::default(), &mut stats);
        assert!(matches!(done.get(&id), Some(MatteHalf::Kept)));
        assert!(matches!(done.get(&mask_id), Some(MatteHalf::Written)));
        let Ok(Object::Stream(m)) = doc.get_object(mask_id) else {
            panic!()
        };
        assert!(m.content.len() < mask_before);
        assert_eq!(stats.images_recompressed, 1);
    }

    #[test]
    fn shared_matte_mask_is_left_alone() {
        let (mut doc, id, mask_id, info) = matte_pair_doc();
        let mut other = Dictionary::new();
        other.set("Subtype", Object::Name(b"Image".to_vec()));
        other.set("SMask", Object::Reference(mask_id));
        doc.add_object(lopdf::Stream::new(other, vec![]));
        let mut stats = empty_stats();
        let done =
            recompress_matte_pairs(&mut doc, &[info], &CompressOptions::default(), &mut stats);
        assert!(done.is_empty());
        let Ok(Object::Stream(s)) = doc.get_object(id) else {
            panic!()
        };
        assert_eq!(dict_int(&s.dict, b"Width"), Some(64));
    }

    #[test]
    fn two_color_images_become_exact_one_bit_palettes() {
        // 16x16 red/white checker as raw RGB.
        let n = 16usize;
        let rgb: Vec<u8> = (0..n * n)
            .flat_map(|i| {
                if (i % n / 4 + i / n / 4).is_multiple_of(2) {
                    [255, 0, 0]
                } else {
                    [255, 255, 255]
                }
            })
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
        let mut staged = Staged::default();
        assert!(matches!(
            recompress_two_color(&doc, &mut staged, &info, &img, false),
            TwoColor::Recompressed { .. }
        ));
        staged.apply(&mut doc);
        // Decoding the rewritten stream through its palette gives back
        // exactly the original pixels.
        let Ok(Object::Stream(s)) = doc.get_object(id) else {
            panic!()
        };
        assert_eq!(dict_int(&s.dict, b"BitsPerComponent"), Some(8));
        let (comps, palette, _) = indexed_palette(&doc, &s.dict).unwrap().unwrap();
        assert_eq!(
            (comps, palette.clone()),
            (3, vec![255, 0, 0, 255, 255, 255])
        );
        let indices = s.decompressed_content().unwrap();
        let back = expand_indices(&indices, n as u32, n as u32, 8, 3, &palette).unwrap();
        assert_eq!(back, rgb);
        // A third color anywhere disqualifies.
        let mut three = rgb.clone();
        three[0] = 7;
        let (doc, id) = doc_with_image(
            |d| {
                d.set("Width", n as i64);
                d.set("Height", n as i64);
            },
            &three,
        );
        let img = decode_image(&doc, id).unwrap();
        assert!(matches!(
            recompress_two_color(
                &doc,
                &mut Staged::default(),
                &info_for(id, n, three.len()),
                &img,
                false
            ),
            TwoColor::MoreColors
        ));
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
}
