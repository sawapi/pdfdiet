//! Finding the images in a document and the largest size each one is drawn
//! at on any page.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::decode::{colorspace_components, colorspace_name};
use crate::limits::{Walk, page_content_bounded};
use crate::{CompressOptions, as_f64, dict_int, filter_names, resolve, resolve_dict};

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

/// Output size for an image: shrink only the axes whose measured DPI exceeds
/// the target (5% hysteresis).
pub(crate) fn target_size(info: &ImageInfo, opts: &CompressOptions) -> (u32, u32) {
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

pub fn collect_images(doc: &Document) -> Vec<ImageInfo> {
    let usage = image_usage(doc);

    let mut images = Vec::new();
    for (&id, obj) in &doc.objects {
        let Object::Stream(stream) = obj else {
            continue;
        };
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
                        if !walk_ops(
                            doc,
                            &c.operations,
                            &inner_res,
                            inner_ctm,
                            usage,
                            depth + 1,
                            walk,
                        ) {
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
        let Object::Stream(stream) = obj else {
            continue;
        };
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
