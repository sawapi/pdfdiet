//! In-place TrueType font subsetting.
//!
//! Embedded subsets produced by figure exporters often keep full-length
//! `loca`/`hmtx`/`cmap` tables for thousands of glyph slots while only a few
//! dozen have outlines: the tail is pure padding. This pass truncates a font
//! program's glyph count down to what the document can actually reach,
//! without renumbering glyph ids — so the `cmap`, the PDF text strings and
//! the PDF-level metrics (/Widths) all remain valid as they are.
//!
//! Safety comes from the shape of the kept set, not from trusting the text
//! interpreter: we keep every glyph that has an outline, every glyph any
//! shown character resolves to (under every plausible viewer lookup path),
//! the closure over composite-glyph components, and glyph 0. Dropping is
//! therefore restricted to empty glyphs past the cutoff that no shown text
//! reaches — and whenever any piece of the document cannot be interpreted
//! (an unknown encoding, an unresolvable character, a construct the walker
//! does not model), the affected font — or the whole document — is left
//! untouched. Implemented from ISO 32000 and the OpenType specification.

use std::collections::{BTreeSet, HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::{
    MAX_INFLATE, Walk, dict_int, filter_names, inflate, page_content_bounded, page_resource_dicts,
    resolve, resolve_dict,
};

/// Character bytes shown per font dictionary, as raw PDF string bytes
/// (interpretation into codes depends on the font's class).
pub(crate) struct FontUsage {
    pub(crate) strings: HashMap<ObjectId, HashSet<Vec<u8>>>,
    /// Fonts whose usage could not be fully interpreted — never subset these.
    pub(crate) tainted: HashSet<ObjectId>,
}

/// Content streams are parsed strictly (to the last byte): lopdf's lenient
/// decode returns the operators before the first unparsable token as a
/// success, while viewers skip that token and keep drawing, so a lenient
/// parse could miss glyphs that are shown. Anything not parsed to the end
/// disables subsetting.
///
/// Walk every content stream that can mark glyphs as used: page contents,
/// Form XObjects (which inherit the text state of their invoker), tiling
/// patterns, soft-mask groups, annotation appearance streams, and Type 3
/// glyph procedures. Returns None when anything cannot be interpreted,
/// which disables subsetting for the whole document.
pub(crate) fn collect_font_usage(doc: &Document) -> Option<FontUsage> {
    let mut usage = FontUsage {
        strings: HashMap::new(),
        tainted: HashSet::new(),
    };
    // Interactive forms can synthesize new appearance streams from /DA
    // default-appearance strings at fill time; way outside our model. A
    // form with no fields (and no XFA) has nothing to fill, though: editors
    // often leave such an empty /AcroForm behind. Its /DR fonts are still
    // what a viewer would draw new annotation text with, so they are kept
    // whole (tainted) rather than trusted.
    let mut form_fonts: Option<&Dictionary> = None;
    if let Ok(catalog) = doc.catalog()
        && let Ok(form) = catalog.get(b"AcroForm")
    {
        let form = resolve_dict(doc, form).ok()?;
        let has_fields = match form.get(b"Fields").and_then(|o| resolve(doc, o)) {
            Ok(Object::Array(a)) => !a.is_empty(),
            Ok(_) => true,
            Err(_) => false,
        };
        if has_fields || form.get(b"XFA").is_ok() {
            return None;
        }
        form_fonts = form.get(b"DR").and_then(|o| resolve_dict(doc, o)).ok();
    }
    if let Some(dr) = form_fonts {
        taint_resource_fonts(doc, dr, &mut usage)?;
    }
    // Document-wide stream-walk budget: nested forms multiply exponentially,
    // so a runaway document aborts the walk (and with it, subsetting), as
    // does one past the walk's parse bounds.
    let mut budget = Walk::new(200_000);
    for (_num, page_id) in doc.get_pages() {
        let res_stack = page_resource_dicts(doc, page_id);
        // Strict content fetch: any unreadable stream disables subsetting
        // (Document::get_page_content silently skips broken streams).
        // Strict, bounded fetch: any unreadable or oversized stream disables
        // subsetting (a page without /Contents shows no text).
        let data = page_content_bounded(doc, page_id)?;
        budget.page();
        let (content, _) = budget.parse(&data)?;
        walk_ops(
            doc,
            &content.operations,
            &res_stack,
            None,
            &mut usage,
            0,
            &mut budget,
        )?;

        // Annotation appearance streams.
        let page = doc.get_dictionary(page_id).ok()?;
        let Ok(annots) = page.get(b"Annots").and_then(|o| resolve(doc, o)) else {
            continue;
        };
        let Object::Array(annots) = annots else {
            return None;
        };
        for annot in annots {
            let annot = resolve_dict(doc, annot).ok()?;
            // An annotation with /DA (a FreeText note, say) can have its
            // appearance regenerated by a viewer, with new text, from the
            // fonts its appearance already uses or the form's /DR (tainted
            // above). Those fonts are kept whole; the rest of the document
            // is still subset.
            let regenerable = annot.get(b"DA").is_ok();
            let Ok(ap) = annot.get(b"AP").and_then(|o| resolve_dict(doc, o)) else {
                continue;
            };
            for (_key, val) in ap.iter() {
                // Each entry is a form stream or a dict of state -> stream.
                let streams: Vec<&Object> = match resolve(doc, val).ok()? {
                    Object::Dictionary(states) => states.iter().map(|(_k, v)| v).collect(),
                    other => vec![other],
                };
                for s in streams {
                    walk_form(doc, s, &[], None, &mut usage, 0, &mut budget)?;
                    if regenerable {
                        let Object::Stream(st) = resolve(doc, s).ok()? else {
                            return None;
                        };
                        if let Ok(res) = st.dict.get(b"Resources") {
                            taint_resource_fonts(doc, resolve_dict(doc, res).ok()?, &mut usage)?;
                        }
                    }
                }
            }
        }
    }
    Some(usage)
}

/// Mark every font in a resource dictionary's /Font as tainted (never
/// subset). Fonts must be indirect, as everywhere in this walker.
fn taint_resource_fonts(doc: &Document, res: &Dictionary, usage: &mut FontUsage) -> Option<()> {
    let Ok(fonts) = res.get(b"Font") else {
        return Some(());
    };
    for (_name, font) in resolve_dict(doc, fonts).ok()?.iter() {
        let Object::Reference(fid) = font else {
            return None;
        };
        usage.tainted.insert(*fid);
    }
    Some(())
}

/// Walk one referenced form/pattern/glyph stream object.
fn walk_form<'a>(
    doc: &'a Document,
    obj: &'a Object,
    outer_res: &[&'a Dictionary],
    cur_font: Option<ObjectId>,
    usage: &mut FontUsage,
    depth: u8,
    budget: &mut Walk,
) -> Option<()> {
    if depth > 12 || budget.forms == 0 {
        return None;
    }
    budget.forms -= 1;
    let Object::Stream(stream) = resolve(doc, obj).ok()? else {
        return None;
    };
    let mut res_stack: Vec<&Dictionary> = Vec::new();
    if let Ok(rd) = stream
        .dict
        .get(b"Resources")
        .and_then(|o| resolve_dict(doc, o))
    {
        res_stack.push(rd);
    }
    res_stack.extend_from_slice(outer_res);
    let (content, opened) = budget.parse_stream(stream)?;
    walk_ops(
        doc,
        &content.operations,
        &res_stack,
        cur_font,
        usage,
        depth + 1,
        budget,
    )?;
    budget.close(opened);
    Some(())
}

fn lookup_resource<'a>(
    doc: &'a Document,
    res_stack: &[&'a Dictionary],
    category: &[u8],
    name: &[u8],
) -> Option<&'a Object> {
    for res in res_stack {
        if let Ok(dict) = res.get(category).and_then(|o| resolve_dict(doc, o))
            && let Ok(obj) = dict.get(name)
        {
            return Some(obj);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn walk_ops<'a>(
    doc: &'a Document,
    ops: &[lopdf::content::Operation],
    res_stack: &[&'a Dictionary],
    start_font: Option<ObjectId>,
    usage: &mut FontUsage,
    depth: u8,
    budget: &mut Walk,
) -> Option<()> {
    if depth > 12 {
        return None;
    }
    let mut cur_font = start_font;
    let mut gs_stack: Vec<Option<ObjectId>> = Vec::new();
    for op in ops {
        match op.operator.as_str() {
            "q" => gs_stack.push(cur_font),
            "Q" => {
                if let Some(f) = gs_stack.pop() {
                    cur_font = f;
                }
            }
            "Tf" => {
                let Some(Object::Name(name)) = op.operands.first() else {
                    return None;
                };
                let font = lookup_resource(doc, res_stack, b"Font", name)?;
                let Object::Reference(fid) = font else {
                    return None; // inline font dicts: unmodelled
                };
                cur_font = Some(*fid);
                usage.strings.entry(*fid).or_default();
                // Type 3 glyph procedures can draw with other fonts.
                walk_type3(doc, *fid, res_stack, usage, depth, budget)?;
            }
            "gs" => {
                let Some(Object::Name(name)) = op.operands.first() else {
                    return None;
                };
                let Some(egs) = lookup_resource(doc, res_stack, b"ExtGState", name) else {
                    continue;
                };
                let egs = resolve_dict(doc, egs).ok()?;
                if let Ok(font_entry) = egs.get(b"Font").and_then(|o| resolve(doc, o)) {
                    let Object::Array(a) = font_entry else {
                        return None;
                    };
                    let Some(Object::Reference(fid)) = a.first() else {
                        return None;
                    };
                    cur_font = Some(*fid);
                    usage.strings.entry(*fid).or_default();
                    walk_type3(doc, *fid, res_stack, usage, depth, budget)?;
                }
                if let Ok(smask) = egs.get(b"SMask").and_then(|o| resolve(doc, o))
                    && let Object::Dictionary(sm) = smask
                    && let Ok(g) = sm.get(b"G")
                {
                    walk_form(doc, g, res_stack, cur_font, usage, depth, budget)?;
                }
            }
            "Tj" | "'" => {
                let Some(Object::String(bytes, _)) = op.operands.last() else {
                    return None;
                };
                record_string(usage, cur_font?, bytes);
            }
            "\"" => {
                let Some(Object::String(bytes, _)) = op.operands.last() else {
                    return None;
                };
                record_string(usage, cur_font?, bytes);
            }
            "TJ" => {
                let Some(Object::Array(items)) = op.operands.first() else {
                    return None;
                };
                for item in items {
                    if let Object::String(bytes, _) = item {
                        record_string(usage, cur_font?, bytes);
                    }
                }
            }
            "Do" => {
                let Some(Object::Name(name)) = op.operands.first() else {
                    return None;
                };
                let xobj = lookup_resource(doc, res_stack, b"XObject", name)?;
                let Object::Stream(s) = resolve(doc, xobj).ok()? else {
                    return None;
                };
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(st)) if st == b"Form") {
                    walk_form(doc, xobj, res_stack, cur_font, usage, depth, budget)?;
                }
            }
            "scn" | "SCN" => {
                // Tiling pattern content streams can draw text of their own.
                if let Some(Object::Name(name)) = op.operands.last()
                    && let Some(pat) = lookup_resource(doc, res_stack, b"Pattern", name)
                {
                    let pat_obj = resolve(doc, pat).ok()?;
                    if let Object::Stream(ps) = pat_obj
                        && dict_int(&ps.dict, b"PatternType") == Some(1)
                    {
                        walk_form(doc, pat, res_stack, None, usage, depth, budget)?;
                    }
                }
            }
            "BI" => return None, // inline image: parser reliability unproven
            _ => {}
        }
    }
    Some(())
}

/// If `fid` is a Type 3 font, walk all its glyph procedures (they are content
/// streams and may use other fonts) and taint the font itself.
fn walk_type3<'a>(
    doc: &'a Document,
    fid: ObjectId,
    outer_res: &[&'a Dictionary],
    usage: &mut FontUsage,
    depth: u8,
    budget: &mut Walk,
) -> Option<()> {
    let font = doc.get_dictionary(fid).ok()?;
    if !matches!(font.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Type3") {
        return Some(());
    }
    usage.tainted.insert(fid);
    let mut res_stack: Vec<&Dictionary> = Vec::new();
    if let Ok(rd) = font.get(b"Resources").and_then(|o| resolve_dict(doc, o)) {
        res_stack.push(rd);
    }
    res_stack.extend_from_slice(outer_res);
    let procs = font
        .get(b"CharProcs")
        .and_then(|o| resolve_dict(doc, o))
        .ok()?;
    for (_name, proc) in procs.iter() {
        let Object::Stream(s) = resolve(doc, proc).ok()? else {
            return None;
        };
        if budget.forms == 0 {
            return None;
        }
        budget.forms -= 1;
        let (content, opened) = budget.parse_stream(s)?;
        walk_ops(
            doc,
            &content.operations,
            &res_stack,
            None,
            usage,
            depth + 1,
            budget,
        )?;
        budget.close(opened);
    }
    Some(())
}

fn record_string(usage: &mut FontUsage, font: ObjectId, bytes: &[u8]) {
    usage
        .strings
        .entry(font)
        .or_default()
        .insert(bytes.to_vec());
}

// ---------------------------------------------------------------------------
// Character-code to glyph-id resolution.

/// WinAnsiEncoding (CP1252) code -> Unicode. Identity with Latin-1 except in
/// 0x80-0x9F; codes with no assignment return None.
fn winansi_to_unicode(code: u8) -> Option<u16> {
    Some(match code {
        0x80 => 0x20AC, // Euro
        0x82 => 0x201A,
        0x83 => 0x0192,
        0x84 => 0x201E,
        0x85 => 0x2026,
        0x86 => 0x2020,
        0x87 => 0x2021,
        0x88 => 0x02C6,
        0x89 => 0x2030,
        0x8A => 0x0160,
        0x8B => 0x2039,
        0x8C => 0x0152,
        0x8E => 0x017D,
        0x91 => 0x2018,
        0x92 => 0x2019,
        0x93 => 0x201C,
        0x94 => 0x201D,
        0x95 => 0x2022,
        0x96 => 0x2013,
        0x97 => 0x2014,
        0x98 => 0x02DC,
        0x99 => 0x2122,
        0x9A => 0x0161,
        0x9B => 0x203A,
        0x9C => 0x0153,
        0x9E => 0x017E,
        0x9F => 0x0178,
        0x81 | 0x8D | 0x8F | 0x90 | 0x9D => return None,
        0x20..=0x7E | 0xA0..=0xFF => code as u16,
        _ => return None,
    })
}

/// Glyph name -> Unicode for /Differences entries. Only names whose meaning
/// is beyond doubt; anything else makes the font ineligible for subsetting.
fn glyph_name_to_unicode(name: &[u8]) -> Option<u16> {
    // uniXXXX / uXXXX with hex digits.
    let hex4 = |h: &[u8]| -> Option<u16> {
        (h.len() == 4 && h.iter().all(u8::is_ascii_hexdigit))
            .then(|| u16::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())
            .flatten()
    };
    if let Some(h) = name.strip_prefix(b"uni") {
        return hex4(h);
    }
    if let Some(h) = name.strip_prefix(b"u")
        && h.len() == 4
    {
        return hex4(h);
    }
    const NAMES: &[(&[u8], u16)] = &[
        (b"space", 0x20),
        (b"exclam", 0x21),
        (b"quotedbl", 0x22),
        (b"numbersign", 0x23),
        (b"dollar", 0x24),
        (b"percent", 0x25),
        (b"ampersand", 0x26),
        (b"quotesingle", 0x27),
        (b"parenleft", 0x28),
        (b"parenright", 0x29),
        (b"asterisk", 0x2A),
        (b"plus", 0x2B),
        (b"comma", 0x2C),
        (b"hyphen", 0x2D),
        (b"period", 0x2E),
        (b"slash", 0x2F),
        (b"zero", 0x30),
        (b"one", 0x31),
        (b"two", 0x32),
        (b"three", 0x33),
        (b"four", 0x34),
        (b"five", 0x35),
        (b"six", 0x36),
        (b"seven", 0x37),
        (b"eight", 0x38),
        (b"nine", 0x39),
        (b"colon", 0x3A),
        (b"semicolon", 0x3B),
        (b"less", 0x3C),
        (b"equal", 0x3D),
        (b"greater", 0x3E),
        (b"question", 0x3F),
        (b"at", 0x40),
        (b"bracketleft", 0x5B),
        (b"backslash", 0x5C),
        (b"bracketright", 0x5D),
        (b"asciicircum", 0x5E),
        (b"underscore", 0x5F),
        (b"grave", 0x60),
        (b"braceleft", 0x7B),
        (b"bar", 0x7C),
        (b"braceright", 0x7D),
        (b"asciitilde", 0x7E),
        (b"bullet", 0x2022),
        (b"emdash", 0x2014),
        (b"endash", 0x2013),
        (b"quoteleft", 0x2018),
        (b"quoteright", 0x2019),
        (b"quotedblleft", 0x201C),
        (b"quotedblright", 0x201D),
        (b"fi", 0xFB01),
        (b"fl", 0xFB02),
    ];
    if name.len() == 1 && (name[0].is_ascii_alphanumeric()) {
        return Some(name[0] as u16); // single-letter names: A -> U+0041
    }
    NAMES.iter().find(|(n, _)| *n == name).map(|&(_, u)| u)
}

/// The character-code interpretation of one font dictionary.
enum FontClass {
    /// Simple TrueType: 1-byte codes -> Unicode via WinAnsi (+ Differences).
    SimpleWinAnsi { differences: HashMap<u8, u16> },
    /// Simple TrueType flagged symbolic, no /Encoding: codes go directly to
    /// the (3,0) / (1,0) cmap subtables.
    SimpleSymbolic,
    /// CIDFontType2 behind Identity-H/V: 2-byte codes are CIDs; glyph ids
    /// via CIDToGIDMap (None = identity, Some = the map stream's bytes).
    CidIdentity { cid_to_gid: Option<Vec<u8>> },
}

/// Classify `font` and find its TrueType program. None = not eligible.
fn classify(doc: &Document, font: &Dictionary) -> Option<(ObjectId, FontClass)> {
    let subtype = match font.get(b"Subtype") {
        Ok(Object::Name(n)) => n.as_slice(),
        _ => return None,
    };
    match subtype {
        b"TrueType" => {
            let desc = font_descriptor(doc, font)?;
            let program = descriptor_program(doc, desc)?;
            let flags = dict_int(desc, b"Flags").unwrap_or(0);
            let symbolic = flags & 4 != 0 && flags & 32 == 0;
            match font.get(b"Encoding") {
                Ok(enc) => {
                    let enc = resolve(doc, enc).ok()?;
                    let (base, diff_obj) = match enc {
                        Object::Name(n) => (n.clone(), None),
                        Object::Dictionary(d) => {
                            // A dict without BaseEncoding falls back to the
                            // font's builtin encoding, which we do not model.
                            let Ok(Object::Name(base)) = d.get(b"BaseEncoding") else {
                                return None;
                            };
                            (base.clone(), d.get(b"Differences").ok())
                        }
                        _ => return None,
                    };
                    if base != b"WinAnsiEncoding" {
                        return None; // MacRoman etc.: unimplemented, skip
                    }
                    let mut differences = HashMap::new();
                    if let Some(dobj) = diff_obj {
                        let Object::Array(items) = resolve(doc, dobj).ok()? else {
                            return None;
                        };
                        let mut code: i64 = -1;
                        for item in items {
                            match item {
                                Object::Integer(i) => code = *i,
                                Object::Name(n) => {
                                    if !(0..=255).contains(&code) {
                                        return None;
                                    }
                                    differences.insert(code as u8, glyph_name_to_unicode(n)?);
                                    code += 1;
                                }
                                _ => return None,
                            }
                        }
                    }
                    Some((program, FontClass::SimpleWinAnsi { differences }))
                }
                Err(_) if symbolic => Some((program, FontClass::SimpleSymbolic)),
                Err(_) => None, // nonsymbolic w/o encoding: builtin, unmodelled
            }
        }
        b"Type0" => {
            match font.get(b"Encoding") {
                Ok(Object::Name(n)) if n == b"Identity-H" || n == b"Identity-V" => {}
                _ => return None,
            }
            let descendants = resolve(doc, font.get(b"DescendantFonts").ok()?).ok()?;
            let Object::Array(a) = descendants else {
                return None;
            };
            if a.len() != 1 {
                return None;
            }
            let cid = resolve_dict(doc, a.first()?).ok()?;
            if !matches!(cid.get(b"Subtype"), Ok(Object::Name(n)) if n == b"CIDFontType2") {
                return None;
            }
            let desc = font_descriptor(doc, cid)?;
            let program = descriptor_program(doc, desc)?;
            let cid_to_gid = match cid.get(b"CIDToGIDMap") {
                Err(_) => None,
                Ok(Object::Name(n)) if n == b"Identity" => None,
                Ok(obj) => {
                    let Object::Stream(s) = resolve(doc, obj).ok()? else {
                        return None;
                    };
                    // Two bytes per CID: 128 KiB covers every CID.
                    Some(inflate(s, 1 << 20).ok()?)
                }
            };
            Some((program, FontClass::CidIdentity { cid_to_gid }))
        }
        _ => None,
    }
}

/// A font's /FontDescriptor, whether referenced or written inline (some
/// producers put the whole CIDFont and its descriptor inside the Type0
/// font's DescendantFonts array).
fn font_descriptor<'a>(doc: &'a Document, font: &'a Dictionary) -> Option<&'a Dictionary> {
    match font.get(b"FontDescriptor").ok()? {
        Object::Reference(did) => doc.get_dictionary(*did).ok(),
        Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

fn descriptor_program(doc: &Document, desc: &Dictionary) -> Option<ObjectId> {
    let Ok(Object::Reference(fid)) = desc.get(b"FontFile2") else {
        return None;
    };
    matches!(doc.get_object(*fid), Ok(Object::Stream(_))).then_some(*fid)
}

/// Resolve the glyph ids a font's shown strings can reach inside `program`.
/// Every plausible viewer lookup path contributes (union), so a glyph
/// survives no matter which path a given viewer takes. None = some code
/// could not be resolved -> do not subset this program.
fn used_gids(
    class: &FontClass,
    strings: &HashSet<Vec<u8>>,
    program: &[u8],
) -> Option<BTreeSet<u16>> {
    let cmap = CmapIndex::parse(program);
    let mut gids = BTreeSet::new();
    for s in strings {
        match class {
            FontClass::SimpleWinAnsi { differences } => {
                for &code in s {
                    let uni = match differences.get(&code) {
                        Some(&u) => Some(u),
                        None => winansi_to_unicode(code),
                    };
                    let mut found = false;
                    if let Some(u) = uni
                        && let Some(g) = cmap.lookup(3, 1, u as u32)
                    {
                        gids.insert(g);
                        found = true;
                    }
                    // Symbolic-style fallbacks some viewers try:
                    for (p, e, c) in [
                        (3u16, 0u16, 0xF000 + code as u32),
                        (3, 0, code as u32),
                        (1, 0, code as u32),
                    ] {
                        if let Some(g) = cmap.lookup(p, e, c) {
                            gids.insert(g);
                            found = true;
                        }
                    }
                    if !found {
                        return None;
                    }
                }
            }
            FontClass::SimpleSymbolic => {
                for &code in s {
                    let mut found = false;
                    for (p, e, c) in [
                        (3u16, 0u16, 0xF000 + code as u32),
                        (3, 0, code as u32),
                        (1, 0, code as u32),
                    ] {
                        if let Some(g) = cmap.lookup(p, e, c) {
                            gids.insert(g);
                            found = true;
                        }
                    }
                    if !found {
                        return None;
                    }
                }
            }
            FontClass::CidIdentity { cid_to_gid } => {
                if s.len() % 2 != 0 {
                    return None;
                }
                for pair in s.chunks(2) {
                    let cid = u16::from_be_bytes([pair[0], pair[1]]);
                    let gid = match cid_to_gid {
                        None => cid,
                        Some(map) => {
                            let i = cid as usize * 2;
                            if i + 2 > map.len() {
                                0
                            } else {
                                u16::from_be_bytes([map[i], map[i + 1]])
                            }
                        }
                    };
                    gids.insert(gid);
                }
            }
        }
    }
    Some(gids)
}

// ---------------------------------------------------------------------------
// TrueType parsing.

pub(crate) fn be16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_be_bytes(d.get(o..o + 2)?.try_into().ok()?))
}
pub(crate) fn be32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

pub(crate) struct CmapIndex {
    /// (platform, encoding, absolute subtable offset)
    pub(crate) subtables: Vec<(u16, u16, usize)>,
    pub(crate) data: Vec<u8>,
}

impl CmapIndex {
    pub(crate) fn parse(font: &[u8]) -> CmapIndex {
        let mut subtables = Vec::new();
        let mut data = Vec::new();
        if let Some((off, len)) = find_table(font, b"cmap")
            && let Some(table) = font.get(off..off + len)
        {
            data = table.to_vec();
            if let Some(n) = be16(table, 2) {
                for i in 0..n as usize {
                    let rec = 4 + i * 8;
                    if let (Some(p), Some(e), Some(o)) =
                        (be16(table, rec), be16(table, rec + 2), be32(table, rec + 4))
                    {
                        subtables.push((p, e, o as usize));
                    }
                }
            }
        }
        CmapIndex { subtables, data }
    }

    /// Look `code` up in the (platform, encoding) subtable; None when the
    /// subtable is absent, the format is unsupported, or the code unmapped.
    /// Glyph 0 is treated as unmapped (it is kept unconditionally anyway).
    pub(crate) fn lookup(&self, platform: u16, encoding: u16, code: u32) -> Option<u16> {
        let &(_, _, off) = self
            .subtables
            .iter()
            .find(|&&(p, e, _)| p == platform && e == encoding)?;
        let d = &self.data;
        let gid = match be16(d, off)? {
            0
                // Byte encoding table.
                if code < 256 => {
                    *d.get(off + 6 + code as usize)? as u16
                }
            4 => {
                // Segment mapping to delta values (BMP).
                if code > 0xFFFF {
                    return None;
                }
                let code = code as u16;
                let segx2 = be16(d, off + 6)? as usize;
                let ends = off + 14;
                let starts = ends + segx2 + 2;
                let deltas = starts + segx2;
                let ranges = deltas + segx2;
                let mut seg = None;
                for i in (0..segx2).step_by(2) {
                    if be16(d, ends + i)? >= code {
                        seg = Some(i);
                        break;
                    }
                }
                let i = seg?;
                let start = be16(d, starts + i)?;
                if start > code {
                    return None;
                }
                let delta = be16(d, deltas + i)?;
                let range_off = be16(d, ranges + i)? as usize;
                if range_off == 0 {
                    code.wrapping_add(delta)
                } else {
                    let idx = ranges + i + range_off + 2 * (code - start) as usize;
                    let g = be16(d, idx)?;
                    if g == 0 {
                        return None;
                    }
                    g.wrapping_add(delta)
                }
            }
            6 => {
                // Trimmed table mapping.
                let first = be16(d, off + 6)? as u32;
                let count = be16(d, off + 8)? as u32;
                if code < first || code >= first + count {
                    return None;
                }
                be16(d, off + 10 + 2 * (code - first) as usize)?
            }
            _ => return None,
        };
        (gid != 0).then_some(gid)
    }
}

/// Most tables a font may list. Real fonts have a few dozen; the directory is
/// scanned once per table on rebuild.
const MAX_TABLES: usize = 1024;

/// The directory's tags, in order. None when a tag repeats, the directory is
/// implausibly long, or the tables claim more bytes than the font holds: a
/// valid sfnt does none of these, and the rebuilds copy each listed table
/// once, so their output stays within the input size.
pub(crate) fn table_tags(font: &[u8]) -> Option<Vec<[u8; 4]>> {
    let n = be16(font, 4)? as usize;
    if n > MAX_TABLES {
        return None;
    }
    let mut tags = Vec::with_capacity(n);
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0usize;
    for i in 0..n {
        let rec = 12 + i * 16;
        let tag: [u8; 4] = font.get(rec..rec + 4)?.try_into().ok()?;
        if !seen.insert(tag) {
            return None;
        }
        total = total.checked_add(be32(font, rec + 12)? as usize)?;
        tags.push(tag);
    }
    (total <= font.len()).then_some(tags)
}

pub(crate) fn find_table(font: &[u8], tag: &[u8; 4]) -> Option<(usize, usize)> {
    let n = be16(font, 4)? as usize;
    for i in 0..n {
        let rec = 12 + i * 16;
        if font.get(rec..rec + 4)? == tag {
            let off = be32(font, rec + 8)? as usize;
            let len = be32(font, rec + 12)? as usize;
            if off.checked_add(len)? <= font.len() {
                return Some((off, len));
            }
        }
    }
    None
}

/// Tables that never index by glyph id: safe to carry over unchanged.
const CARRY_TABLES: &[&[u8; 4]] = &[
    b"cmap", b"cvt ", b"fpgm", b"prep", b"name", b"post", b"OS/2", b"gasp", b"PCLT", b"STAT",
];
/// Optional tables no PDF consumer reads: per-glyph acceleration caches,
/// ClearType merge classes and baseline data. Safe to drop outright.
const DROP_TABLES: &[&[u8; 4]] = &[b"hdmx", b"LTSH", b"VDMX", b"MERG", b"BASE"];

/// Truncate a TrueType font program to `count` glyph slots, rebuilding
/// glyf/loca/hmtx/maxp/hhea/head. Glyphs below the cutoff are byte-identical
/// to the original, except that with `keep`, glyphs outside it get an empty
/// outline (their slot, metrics and id stay). None = the font has a
/// structure we do not handle.
fn truncate_truetype(font: &[u8], count: u16, keep: Option<&BTreeSet<u16>>) -> Option<Vec<u8>> {
    if be32(font, 0)? != 0x0001_0000 {
        return None;
    }
    let numtables = table_tags(font)?.len();
    // Reject fonts with tables we have no policy for (e.g. vertical metrics,
    // which would also need truncating).
    for i in 0..numtables {
        let tag: &[u8; 4] = font.get(12 + i * 16..16 + i * 16)?.try_into().ok()?;
        let known = matches!(
            tag.as_slice(),
            b"glyf" | b"loca" | b"head" | b"hhea" | b"hmtx" | b"maxp" | b"vhea" | b"vmtx"
        ) || CARRY_TABLES.contains(&tag)
            || DROP_TABLES.contains(&tag);
        if !known {
            return None;
        }
    }

    let (head_o, head_l) = find_table(font, b"head")?;
    let (maxp_o, maxp_l) = find_table(font, b"maxp")?;
    let (hhea_o, hhea_l) = find_table(font, b"hhea")?;
    let (hmtx_o, hmtx_l) = find_table(font, b"hmtx")?;
    let (loca_o, loca_l) = find_table(font, b"loca")?;
    let (glyf_o, glyf_l) = find_table(font, b"glyf")?;
    if head_l < 54 || maxp_l < 6 || hhea_l < 36 {
        return None;
    }
    let old_count = be16(font, maxp_o + 4)? as usize;
    let count = count as usize;
    if count == 0 || count > old_count {
        return None;
    }
    let long_loca = match be16(font, head_o + 50)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let loca_at = |i: usize| -> Option<usize> {
        if long_loca {
            Some(be32(font, loca_o + i * 4)? as usize)
        } else {
            Some(be16(font, loca_o + i * 2)? as usize * 2)
        }
    };
    if long_loca && loca_l < (old_count + 1) * 4 {
        return None;
    }
    if !long_loca && loca_l < (old_count + 1) * 2 {
        return None;
    }

    // glyf/loca: glyph data below the cutoff is carried over verbatim.
    let glyf_end = loca_at(count)?;
    if glyf_end > glyf_l {
        return None;
    }
    let mut new_glyf;
    let mut new_loca = Vec::with_capacity((count + 1) * 4);
    let mut out_long = long_loca;
    match keep {
        None => {
            new_glyf = font.get(glyf_o..glyf_o + glyf_end)?.to_vec();
            for i in 0..=count {
                let v = loca_at(i)?;
                if long_loca {
                    new_loca.extend_from_slice(&(v as u32).to_be_bytes());
                } else {
                    new_loca.extend_from_slice(&((v / 2) as u16).to_be_bytes());
                }
            }
        }
        Some(keep) => {
            // Rebuilt glyph by glyph, long offsets, each glyph 4-aligned.
            // Offsets must not go backwards: overlapping glyphs would each
            // be copied in full, growing the table far beyond its size.
            let mut prev = 0;
            for i in 0..=count {
                let v = loca_at(i)?;
                if v < prev {
                    return None;
                }
                prev = v;
            }
            out_long = true;
            new_glyf = Vec::new();
            for i in 0..count {
                new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
                if keep.contains(&(i as u16)) {
                    let (s, e) = (loca_at(i)?, loca_at(i + 1)?);
                    if e < s || e > glyf_l {
                        return None;
                    }
                    new_glyf.extend_from_slice(font.get(glyf_o + s..glyf_o + e)?);
                    new_glyf.resize(new_glyf.len().div_ceil(4) * 4, 0);
                }
            }
            new_loca.extend_from_slice(&(new_glyf.len() as u32).to_be_bytes());
        }
    }

    // hmtx: full longHorMetric per kept glyph (numberOfHMetrics = count).
    let old_nhm = be16(font, hhea_o + 34)? as usize;
    if old_nhm == 0 {
        return None;
    }
    let metric = |gid: usize| -> Option<(u16, u16)> {
        if gid < old_nhm {
            Some((
                be16(font, hmtx_o + gid * 4)?,
                be16(font, hmtx_o + gid * 4 + 2)?,
            ))
        } else {
            let advance = be16(font, hmtx_o + (old_nhm - 1) * 4)?;
            let lsb_off = hmtx_o + old_nhm * 4 + (gid - old_nhm) * 2;
            if lsb_off + 2 <= hmtx_o + hmtx_l {
                Some((advance, be16(font, lsb_off)?))
            } else {
                Some((advance, 0))
            }
        }
    };
    let mut new_hmtx = Vec::with_capacity(count * 4);
    for gid in 0..count {
        let (adv, lsb) = metric(gid)?;
        new_hmtx.extend_from_slice(&adv.to_be_bytes());
        new_hmtx.extend_from_slice(&lsb.to_be_bytes());
    }

    // vmtx: the same layout as hmtx (numOfLongVerMetrics in vhea), so the
    // same rebuild: one full vertical metric per kept slot.
    let vertical = match (find_table(font, b"vhea"), find_table(font, b"vmtx")) {
        (Some((vhea_o, vhea_l)), Some((vmtx_o, vmtx_l))) if vhea_l >= 36 => {
            let nvm = be16(font, vhea_o + 34)? as usize;
            if nvm == 0 {
                return None;
            }
            let mut new_vmtx = Vec::with_capacity(count * 4);
            for gid in 0..count {
                let (adv, tsb) = if gid < nvm {
                    (
                        be16(font, vmtx_o + gid * 4)?,
                        be16(font, vmtx_o + gid * 4 + 2)?,
                    )
                } else {
                    let advance = be16(font, vmtx_o + (nvm - 1) * 4)?;
                    let off = vmtx_o + nvm * 4 + (gid - nvm) * 2;
                    (
                        advance,
                        if off + 2 <= vmtx_o + vmtx_l {
                            be16(font, off)?
                        } else {
                            0
                        },
                    )
                };
                new_vmtx.extend_from_slice(&adv.to_be_bytes());
                new_vmtx.extend_from_slice(&tsb.to_be_bytes());
            }
            let mut new_vhea = font.get(vhea_o..vhea_o + vhea_l)?.to_vec();
            new_vhea[34..36].copy_from_slice(&(count as u16).to_be_bytes());
            Some((new_vhea, new_vmtx))
        }
        (None, None) => None,
        _ => return None, // one without the other: malformed
    };

    let mut new_maxp = font.get(maxp_o..maxp_o + maxp_l)?.to_vec();
    new_maxp[4..6].copy_from_slice(&(count as u16).to_be_bytes());
    let mut new_hhea = font.get(hhea_o..hhea_o + hhea_l)?.to_vec();
    new_hhea[34..36].copy_from_slice(&(count as u16).to_be_bytes());
    let mut new_head = font.get(head_o..head_o + head_l)?.to_vec();
    new_head[8..12].fill(0); // checkSumAdjustment, recomputed below
    if out_long {
        new_head[50..52].copy_from_slice(&1u16.to_be_bytes());
    }

    // Reassemble, keeping the original table order for the survivors.
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    for i in 0..numtables {
        let tag: [u8; 4] = font.get(12 + i * 16..16 + i * 16)?.try_into().ok()?;
        if DROP_TABLES.contains(&&tag) {
            continue;
        }
        let content = match &tag {
            b"glyf" => new_glyf.clone(),
            b"loca" => new_loca.clone(),
            b"hmtx" => new_hmtx.clone(),
            b"maxp" => new_maxp.clone(),
            b"hhea" => new_hhea.clone(),
            b"head" => new_head.clone(),
            b"vhea" => vertical.as_ref()?.0.clone(),
            b"vmtx" => vertical.as_ref()?.1.clone(),
            _ => {
                let (o, l) = find_table(font, &tag)?;
                font.get(o..o + l)?.to_vec()
            }
        };
        tables.push((tag, content));
    }
    Some(assemble_font(&tables))
}

fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// Build an sfnt from (tag, content) pairs: directory (sorted by tag, as the
/// spec requires), checksums, and head.checkSumAdjustment.
pub(crate) fn assemble_font(tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| tables[i].0);
    let entry_selector = (n as f32).log2() as u16;
    let search_range = (1u16 << entry_selector) * 16;
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&(n as u16 * 16 - search_range).to_be_bytes());

    let mut offset = 12 + n * 16;
    let mut head_pos = None;
    let mut records = Vec::new();
    for &i in &order {
        let (tag, content) = &tables[i];
        let padded = content.len().div_ceil(4) * 4;
        records.push((*tag, table_checksum(content), offset, content.len()));
        if tag == b"head" {
            head_pos = Some(offset);
        }
        offset += padded;
    }
    for (tag, cksum, off, len) in &records {
        out.extend_from_slice(tag);
        out.extend_from_slice(&cksum.to_be_bytes());
        out.extend_from_slice(&(*off as u32).to_be_bytes());
        out.extend_from_slice(&(*len as u32).to_be_bytes());
    }
    for &i in &order {
        let content = &tables[i].1;
        out.extend_from_slice(content);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    if let Some(pos) = head_pos {
        let adjustment = 0xB1B0_AFBAu32.wrapping_sub(table_checksum(&out));
        out[pos + 8..pos + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    out
}

/// Glyph ids with a non-empty outline (their loca range has length > 0).
pub(crate) fn outlined_gids(font: &[u8]) -> Option<BTreeSet<u16>> {
    let (head_o, _) = find_table(font, b"head")?;
    let (maxp_o, _) = find_table(font, b"maxp")?;
    let (loca_o, loca_l) = find_table(font, b"loca")?;
    let count = be16(font, maxp_o + 4)? as usize;
    let long = be16(font, head_o + 50)? == 1;
    if (long && loca_l < (count + 1) * 4) || (!long && loca_l < (count + 1) * 2) {
        return None;
    }
    let at = |i: usize| -> Option<usize> {
        if long {
            Some(be32(font, loca_o + i * 4)? as usize)
        } else {
            Some(be16(font, loca_o + i * 2)? as usize * 2)
        }
    };
    let mut set = BTreeSet::new();
    for i in 0..count {
        if at(i + 1)? > at(i)? {
            set.insert(i as u16);
        }
    }
    Some(set)
}

/// Add the components of composite glyphs (transitively) to `gids`.
pub(crate) fn add_composite_closure(font: &[u8], gids: &mut BTreeSet<u16>) -> Option<()> {
    let (head_o, _) = find_table(font, b"head")?;
    let (loca_o, _) = find_table(font, b"loca")?;
    let (glyf_o, glyf_l) = find_table(font, b"glyf")?;
    let long = be16(font, head_o + 50)? == 1;
    let at = |i: usize| -> Option<usize> {
        if long {
            Some(be32(font, loca_o + i * 4)? as usize)
        } else {
            Some(be16(font, loca_o + i * 2)? as usize * 2)
        }
    };
    let mut queue: Vec<u16> = gids.iter().copied().collect();
    while let Some(gid) = queue.pop() {
        let (start, end) = (at(gid as usize)?, at(gid as usize + 1)?);
        if start == end {
            continue;
        }
        // Subtraction, not start + 10: offsets come from the font, and on
        // 32-bit targets (wasm) that sum can wrap past the check.
        if end > glyf_l || start > end || end - start < 10 {
            return None;
        }
        let g = font.get(glyf_o + start..glyf_o + end)?;
        let contours = i16::from_be_bytes([g[0], g[1]]);
        if contours >= 0 {
            continue;
        }
        // Composite glyph: walk the component records.
        let mut p = 10;
        loop {
            let flags = be16(g, p)?;
            let component = be16(g, p + 2)?;
            if gids.insert(component) {
                queue.push(component);
            }
            p += 4;
            p += if flags & 0x0001 != 0 { 4 } else { 2 }; // args
            if flags & 0x0008 != 0 {
                p += 2;
            } else if flags & 0x0040 != 0 {
                p += 4;
            } else if flags & 0x0080 != 0 {
                p += 8;
            }
            if flags & 0x0020 == 0 {
                break;
            }
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------
// Top level.

/// Shrink embedded TrueType programs to the glyph slots the document can
/// reach. With `drop_outlines`, also empty the outlines of glyphs no shown
/// text reaches (opt-in; see CompressOptions::subset_outlines). Returns
/// (programs shrunk, stream bytes saved).
pub(crate) fn subset_truetype_fonts(
    doc: &mut Document,
    log: &mut Vec<String>,
    drop_outlines: bool,
) -> (usize, i64) {
    let Some(usage) = collect_font_usage(doc) else {
        log.push("font subsetting: skipped (document outside the walker's model)".into());
        return (0, 0);
    };

    // Group font dicts by TrueType program; every user of a program must be
    // fully interpretable or the program is left alone.
    let mut programs: HashMap<ObjectId, Vec<(ObjectId, FontClass)>> = HashMap::new();
    let mut blocked: HashSet<ObjectId> = HashSet::new();
    for (&fid, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else {
            continue;
        };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Font") {
            continue;
        }
        // Skip descendant CIDFonts; they are handled through their Type0 parent.
        if matches!(dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"CIDFontType2") {
            continue;
        }
        match classify(doc, dict) {
            Some((program, class)) => {
                if usage.tainted.contains(&fid) {
                    blocked.insert(program);
                } else {
                    programs.entry(program).or_default().push((fid, class));
                }
            }
            None => {
                // Not eligible itself; block its program (if any, reached
                // directly or through a Type0 descendant) so a shared program
                // is not shrunk based on partial knowledge.
                let desc = font_descriptor(doc, dict).or_else(|| {
                    let descendants = resolve(doc, dict.get(b"DescendantFonts").ok()?).ok()?;
                    let Object::Array(a) = descendants else {
                        return None;
                    };
                    font_descriptor(doc, resolve_dict(doc, a.first()?).ok()?)
                });
                if let Some(desc) = desc
                    && let Some(p) = descriptor_program(doc, desc)
                {
                    blocked.insert(p);
                }
            }
        }
    }

    let mut shrunk = 0usize;
    let mut saved = 0i64;
    let mut ids: Vec<ObjectId> = programs.keys().copied().collect();
    ids.sort();
    for program_id in ids {
        if blocked.contains(&program_id) {
            continue;
        }
        let Ok(Object::Stream(stream)) = doc.get_object(program_id) else {
            continue;
        };
        if !matches!(
            filter_names(&stream.dict)
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
            [] | ["FlateDecode"]
        ) {
            continue;
        }
        let old_stored = stream.content.len();
        let Ok(font) = inflate(stream, MAX_INFLATE) else {
            continue;
        };

        // Keep set: every outlined glyph, every glyph shown text resolves to,
        // composite closure, and glyph 0. By default outlines are never
        // dropped, so a wrong usage resolution can at worst keep too much.
        // With drop_outlines the outlined glyphs no text reaches go too; the
        // usage resolution (every viewer lookup path, unioned) is then what
        // keeps every shown glyph.
        let Some(outlined) = outlined_gids(&font) else {
            continue;
        };
        let mut keep: BTreeSet<u16> = if drop_outlines {
            BTreeSet::new()
        } else {
            outlined.clone()
        };
        keep.insert(0);
        let mut resolvable = true;
        for (fid, class) in &programs[&program_id] {
            let empty = HashSet::new();
            let strings = usage.strings.get(fid).unwrap_or(&empty);
            match used_gids(class, strings, &font) {
                Some(gids) => keep.extend(gids),
                None => {
                    resolvable = false;
                    break;
                }
            }
        }
        if !resolvable {
            continue;
        }
        if add_composite_closure(&font, &mut keep).is_none() {
            continue;
        }
        let count = keep.iter().max().map(|&m| m as u32 + 1).unwrap_or(1);
        if count > u16::MAX as u32 {
            continue;
        }
        let old_slots = find_table(&font, b"maxp")
            .and_then(|(o, _)| be16(&font, o + 4))
            .unwrap_or(0);
        let emptied = if drop_outlines {
            outlined.iter().filter(|g| !keep.contains(g)).count()
        } else {
            0
        };
        let Some(new_font) = truncate_truetype(&font, count as u16, (emptied > 0).then_some(&keep))
        else {
            continue;
        };
        if new_font.len() >= font.len() {
            continue;
        }

        // Build the replacement aside so the size guard can still reject it
        // after compression.
        let mut new_stream = stream.clone();
        new_stream.dict.set("Length1", new_font.len() as i64);
        new_stream.dict.remove(b"Filter");
        new_stream.dict.set("Length", new_font.len() as i64);
        new_stream.set_content(new_font);
        new_stream.allows_compression = true;
        let _ = new_stream.compress();
        let new_stored = new_stream.content.len();
        if new_stored >= old_stored {
            continue;
        }
        doc.objects.insert(program_id, Object::Stream(new_stream));
        log.push(format!(
            "font program {} {}: {} -> {} glyph slots{}, {} -> {} bytes stored",
            program_id.0,
            program_id.1,
            old_slots,
            count,
            if emptied > 0 {
                format!(", {emptied} unused outlines emptied")
            } else {
                String::new()
            },
            old_stored,
            new_stored
        ));
        saved += old_stored as i64 - new_stored as i64;
        shrunk += 1;
    }
    (shrunk, saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An sfnt directory listing `tags`, each pointing at `len` bytes at
    /// the same offset after the directory.
    fn directory(tags: &[&[u8; 4]], len: u32) -> Vec<u8> {
        let mut f = vec![0, 1, 0, 0];
        f.extend_from_slice(&(tags.len() as u16).to_be_bytes());
        f.extend_from_slice(&[0; 6]);
        let off = 12 + 16 * tags.len() as u32;
        for t in tags {
            f.extend_from_slice(*t);
            f.extend_from_slice(&[0; 4]);
            f.extend_from_slice(&off.to_be_bytes());
            f.extend_from_slice(&len.to_be_bytes());
        }
        f.resize(off as usize + len as usize, 0);
        f
    }

    #[test]
    fn table_directory_must_not_repeat_or_overclaim() {
        assert_eq!(
            table_tags(&directory(&[b"cvt ", b"kern"], 8)).map(|t| t.len()),
            Some(2)
        );
        // A repeated tag would be copied once per entry on rebuild.
        assert!(table_tags(&directory(&[b"cvt ", b"cvt "], 8)).is_none());
        // Distinct tags sharing one region claim more bytes than exist.
        let shared = directory(&[b"cvt ", b"fpgm", b"prep"], 1000);
        assert!(table_tags(&shared).is_none());
    }

    #[test]
    fn composite_closure_guard_does_not_rely_on_wrapping_addition() {
        // loca[0] = 0xFFFFFFFF, loca[1] = 9 with glyf at offset 0: on 32-bit
        // targets start + 10 used to wrap below end and slice backwards.
        let mut f = directory(&[b"glyf", b"head", b"loca"], 0);
        let n = 3usize;
        let set = |f: &mut Vec<u8>, i: usize, off: u32, len: u32| {
            f[12 + 16 * i + 8..12 + 16 * i + 12].copy_from_slice(&off.to_be_bytes());
            f[12 + 16 * i + 12..12 + 16 * i + 16].copy_from_slice(&len.to_be_bytes());
        };
        let head_at = 12 + 16 * n;
        let loca_at = head_at + 54;
        f.resize(loca_at + 8, 0);
        set(&mut f, 0, 0, 64);
        set(&mut f, 1, head_at as u32, 54);
        set(&mut f, 2, loca_at as u32, 8);
        f[head_at + 50..head_at + 52].copy_from_slice(&1u16.to_be_bytes());
        f[loca_at..loca_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        f[loca_at + 4..loca_at + 8].copy_from_slice(&9u32.to_be_bytes());
        assert!(add_composite_closure(&f, &mut BTreeSet::from([0u16])).is_none());
    }

    /// One page drawing text with font F1; returns (doc, catalog, page,
    /// F1 id, spare font id F2).
    fn doc_with_text_page() -> (Document, ObjectId, ObjectId, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.5");
        let font = |name: &str| {
            let mut f = Dictionary::new();
            f.set("Type", Object::Name(b"Font".to_vec()));
            f.set("Subtype", Object::Name(b"TrueType".to_vec()));
            f.set("BaseFont", Object::Name(name.as_bytes().to_vec()));
            f
        };
        let f1 = doc.add_object(font("F1Font"));
        let f2 = doc.add_object(font("F2Font"));
        let content = doc.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf (A) Tj ET".to_vec(),
        ));
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference(f1));
        let mut res = Dictionary::new();
        res.set("Font", Object::Dictionary(fonts));
        let pages_id = doc.new_object_id();
        let mut page = Dictionary::new();
        page.set("Type", Object::Name(b"Page".to_vec()));
        page.set("Parent", Object::Reference(pages_id));
        page.set("Contents", Object::Reference(content));
        page.set("Resources", Object::Dictionary(res));
        let page_id = doc.add_object(page);
        let mut pages = Dictionary::new();
        pages.set("Type", Object::Name(b"Pages".to_vec()));
        pages.set("Kids", Object::Array(vec![Object::Reference(page_id)]));
        pages.set("Count", 1i64);
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(catalog);
        doc.trailer.set("Root", Object::Reference(catalog_id));
        (doc, catalog_id, page_id, f1, f2)
    }

    fn font_resources(id: ObjectId) -> Dictionary {
        let mut fonts = Dictionary::new();
        fonts.set("Helv", Object::Reference(id));
        let mut res = Dictionary::new();
        res.set("Font", Object::Dictionary(fonts));
        res
    }

    #[test]
    fn empty_form_is_walked_but_its_fonts_are_kept_whole() {
        let (mut doc, catalog, _, f1, f2) = doc_with_text_page();
        let mut form = Dictionary::new();
        form.set("Fields", Object::Array(vec![]));
        form.set("DR", Object::Dictionary(font_resources(f2)));
        if let Ok(Object::Dictionary(c)) = doc.get_object_mut(catalog) {
            c.set("AcroForm", Object::Dictionary(form));
        }
        let usage = collect_font_usage(&doc).expect("an empty form is walkable");
        assert!(usage.strings.contains_key(&f1));
        assert!(usage.tainted.contains(&f2) && !usage.tainted.contains(&f1));

        // A form with a field can synthesize appearances: no subsetting.
        let field = doc.add_object(Dictionary::new());
        if let Ok(Object::Dictionary(c)) = doc.get_object_mut(catalog)
            && let Ok(Object::Dictionary(form)) = c.get_mut(b"AcroForm")
        {
            form.set("Fields", Object::Array(vec![Object::Reference(field)]));
        }
        assert!(collect_font_usage(&doc).is_none());
    }

    #[test]
    fn content_that_does_not_parse_to_the_end_stops_the_walk() {
        let (mut doc, _, page, _, _) = doc_with_text_page();
        // Garbage mid-stream: lopdf's lenient decode would return only the
        // first text object, missing (B), which a viewer still draws.
        let bad = doc.add_object(lopdf::Stream::new(
            Dictionary::new(),
            b"BT /F1 12 Tf (A) Tj ET <zz> BT /F1 12 Tf (B) Tj ET".to_vec(),
        ));
        if let Ok(Object::Dictionary(p)) = doc.get_object_mut(page) {
            p.set("Contents", Object::Reference(bad));
        }
        assert!(collect_font_usage(&doc).is_none());
    }

    #[test]
    fn regenerable_annotation_fonts_are_kept_whole() {
        let (mut doc, _, page, f1, f2) = doc_with_text_page();
        let mut ap_dict = Dictionary::new();
        ap_dict.set("Subtype", Object::Name(b"Form".to_vec()));
        ap_dict.set("Resources", Object::Dictionary(font_resources(f2)));
        let ap = doc.add_object(lopdf::Stream::new(
            ap_dict,
            b"BT /Helv 9 Tf (A) Tj ET".to_vec(),
        ));
        let mut n = Dictionary::new();
        n.set("N", Object::Reference(ap));
        let mut annot = Dictionary::new();
        annot.set("Subtype", Object::Name(b"FreeText".to_vec()));
        annot.set("DA", Object::string_literal("/Helv 9 Tf 0 g"));
        annot.set("AP", Object::Dictionary(n));
        let annot = doc.add_object(annot);
        if let Ok(Object::Dictionary(p)) = doc.get_object_mut(page) {
            p.set("Annots", Object::Array(vec![Object::Reference(annot)]));
        }
        let usage = collect_font_usage(&doc).expect("a /DA annotation no longer stops the walk");
        assert!(usage.tainted.contains(&f2) && !usage.tainted.contains(&f1));
    }

    /// Synthetic 5-slot font: glyph 1 = simple outline, glyph 2 = composite
    /// referencing glyph 4 (also outlined), glyphs 0/3 empty. Long loca,
    /// format 4 cmap mapping 'A' (U+0041) -> gid 1 under (3,1).
    fn mini_font() -> Vec<u8> {
        // glyf: gid1 simple (12 bytes), gid2 composite -> 4 (18 bytes), gid4 simple (12 bytes)
        let simple: Vec<u8> = {
            let mut g = vec![0u8; 12];
            g[0..2].copy_from_slice(&1i16.to_be_bytes()); // one contour
            g
        };
        let composite: Vec<u8> = {
            let mut g = vec![0u8; 18];
            g[0..2].copy_from_slice(&(-1i16).to_be_bytes());
            // flags: ARG_1_AND_2_ARE_WORDS, no MORE_COMPONENTS
            g[10..12].copy_from_slice(&0x0001u16.to_be_bytes());
            g[12..14].copy_from_slice(&4u16.to_be_bytes()); // component gid
            g
        };
        let mut glyf = Vec::new();
        let mut loca_vals = vec![0u32];
        for content in [&[][..], &simple, &composite, &[], &simple] {
            glyf.extend_from_slice(content);
            loca_vals.push(glyf.len() as u32);
        }
        let mut loca = Vec::new();
        for v in &loca_vals {
            loca.extend_from_slice(&v.to_be_bytes());
        }
        let mut head = vec![0u8; 54];
        head[50..52].copy_from_slice(&1i16.to_be_bytes()); // long loca
        let mut maxp = vec![0u8; 6];
        maxp[4..6].copy_from_slice(&5u16.to_be_bytes());
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&5u16.to_be_bytes());
        let mut hmtx = Vec::new();
        for gid in 0u16..5 {
            hmtx.extend_from_slice(&(500 + gid * 10).to_be_bytes());
            hmtx.extend_from_slice(&(gid * 3).to_be_bytes());
        }
        // cmap: one (3,1) format-4 subtable, segments ['A'..'A'], [0xFFFF]
        let mut sub = Vec::new();
        sub.extend_from_slice(&4u16.to_be_bytes()); // format
        sub.extend_from_slice(&32u16.to_be_bytes()); // length
        sub.extend_from_slice(&0u16.to_be_bytes()); // language
        sub.extend_from_slice(&4u16.to_be_bytes()); // segCountX2
        sub.extend_from_slice(&[0, 4, 0, 1, 0, 0]); // search params (unused)
        for seg in [[0x41u16, 0xFFFF], [0x41, 0xFFFF], [0xFFC0, 1], [0, 0]] {
            // endCode[], pad, startCode[] handled below; build per-array:
            let _ = seg;
        }
        // arrays: endCode, reservedPad, startCode, idDelta, idRangeOffset
        for v in [0x41u16, 0xFFFF] {
            sub.extend_from_slice(&v.to_be_bytes());
        }
        sub.extend_from_slice(&0u16.to_be_bytes());
        for v in [0x41u16, 0xFFFF] {
            sub.extend_from_slice(&v.to_be_bytes());
        }
        for v in [0xFFC0u16, 1] {
            sub.extend_from_slice(&v.to_be_bytes()); // idDelta: 0x41+0xFFC0=1; 0xFFFF+1=0
        }
        for v in [0u16, 0] {
            sub.extend_from_slice(&v.to_be_bytes());
        }
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&0u16.to_be_bytes());
        cmap.extend_from_slice(&1u16.to_be_bytes());
        cmap.extend_from_slice(&3u16.to_be_bytes());
        cmap.extend_from_slice(&1u16.to_be_bytes());
        cmap.extend_from_slice(&12u32.to_be_bytes());
        cmap.extend_from_slice(&sub);

        let tables: Vec<([u8; 4], Vec<u8>)> = vec![
            (*b"cmap", cmap),
            (*b"glyf", glyf),
            (*b"head", head),
            (*b"hhea", hhea),
            (*b"hmtx", hmtx),
            (*b"loca", loca),
            (*b"maxp", maxp),
        ];
        assemble_font(&tables)
    }

    #[test]
    fn outlines_and_cmap() {
        let font = mini_font();
        assert_eq!(
            outlined_gids(&font)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        let cmap = CmapIndex::parse(&font);
        assert_eq!(cmap.lookup(3, 1, 0x41), Some(1));
        assert_eq!(cmap.lookup(3, 1, 0x42), None);
        assert_eq!(cmap.lookup(3, 0, 0x41), None); // subtable absent
    }

    #[test]
    fn composite_closure_raises_cutoff() {
        let font = mini_font();
        let mut keep: BTreeSet<u16> = [0u16, 2].into_iter().collect();
        add_composite_closure(&font, &mut keep).unwrap();
        assert!(keep.contains(&4), "composite component must be kept");
    }

    #[test]
    fn truncation_preserves_prefix() {
        let font = mini_font();
        // Keep {0,1}: cutoff at 2 slots.
        let out = truncate_truetype(&font, 2, None).unwrap();
        let (maxp_o, _) = find_table(&out, b"maxp").unwrap();
        assert_eq!(be16(&out, maxp_o + 4), Some(2));
        let (hhea_o, _) = find_table(&out, b"hhea").unwrap();
        assert_eq!(be16(&out, hhea_o + 34), Some(2));
        let (hmtx_o, hmtx_l) = find_table(&out, b"hmtx").unwrap();
        assert_eq!(hmtx_l, 8);
        assert_eq!(be16(&out, hmtx_o), Some(500)); // gid 0 advance
        assert_eq!(be16(&out, hmtx_o + 4), Some(510)); // gid 1 advance
        assert_eq!(
            outlined_gids(&out).unwrap().into_iter().collect::<Vec<_>>(),
            vec![1]
        );
        // Glyph 1's bytes survive verbatim.
        let (g_new, _) = find_table(&out, b"glyf").unwrap();
        let (g_old, _) = find_table(&font, b"glyf").unwrap();
        assert_eq!(out[g_new..g_new + 12], font[g_old..g_old + 12]);
        // The whole-font checksum invariant holds.
        assert_eq!(table_checksum(&out), 0xB1B0_AFBA);
    }

    #[test]
    fn dropping_outlines_refuses_overlapping_glyph_ranges() {
        // loca[4] = 0: glyph 3 runs backwards (never kept, never checked
        // before) and glyph 4 spans the whole table, so every kept glyph
        // laid out like it would copy the table again.
        let mut font = mini_font();
        let (lo, _) = find_table(&font, b"loca").unwrap();
        font[lo + 16..lo + 20].copy_from_slice(&0u32.to_be_bytes());
        let keep: BTreeSet<u16> = [0, 2, 4].into_iter().collect();
        assert!(truncate_truetype(&font, 5, Some(&keep)).is_none());
    }

    #[test]
    fn dropping_outlines_keeps_ids_and_kept_bytes() {
        // mini_font: gid 1 simple, gid 2 composite -> 4, gid 4 simple.
        let font = mini_font();
        let keep: BTreeSet<u16> = [0, 2, 4].into_iter().collect();
        let out = truncate_truetype(&font, 5, Some(&keep)).unwrap();
        // gid 1 is emptied; 2 and 4 keep their outlines at the same ids.
        assert_eq!(outlined_gids(&out).unwrap(), [2, 4].into_iter().collect());
        let (maxp_o, _) = find_table(&out, b"maxp").unwrap();
        assert_eq!(be16(&out, maxp_o + 4), Some(5));
        let (head_o, _) = find_table(&out, b"head").unwrap();
        assert_eq!(be16(&out, head_o + 50), Some(1), "rebuilt loca is long");
        let glyph = |f: &[u8], gid: usize| {
            let (lo, _) = find_table(f, b"loca").unwrap();
            let (go, _) = find_table(f, b"glyf").unwrap();
            let s = be32(f, lo + gid * 4).unwrap() as usize;
            let e = be32(f, lo + gid * 4 + 4).unwrap() as usize;
            f[go + s..go + e].to_vec()
        };
        assert_eq!(glyph(&out, 4)[..12], glyph(&font, 4)[..12]);
        assert_eq!(glyph(&out, 2)[..18], glyph(&font, 2)[..18]);
        assert_eq!(table_checksum(&out), 0xB1B0_AFBA);
    }

    #[test]
    fn vertical_metrics_are_truncated_like_horizontal_ones() {
        let base = mini_font();
        // Rebuild mini_font's tables plus vhea (1 long metric) and vmtx.
        let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        let n = be16(&base, 4).unwrap() as usize;
        for i in 0..n {
            let tag: [u8; 4] = base[12 + i * 16..16 + i * 16].try_into().unwrap();
            let (o, l) = find_table(&base, &tag).unwrap();
            tables.push((tag, base[o..o + l].to_vec()));
        }
        let mut vhea = vec![0u8; 36];
        vhea[34..36].copy_from_slice(&1u16.to_be_bytes());
        let mut vmtx = Vec::new();
        vmtx.extend_from_slice(&1000u16.to_be_bytes());
        vmtx.extend_from_slice(&7u16.to_be_bytes());
        for tsb in [11u16, 12, 13, 14] {
            vmtx.extend_from_slice(&tsb.to_be_bytes());
        }
        tables.push((*b"vhea", vhea));
        tables.push((*b"vmtx", vmtx));
        let font = assemble_font(&tables);
        let out = truncate_truetype(&font, 3, None).unwrap();
        let (vhea_o, _) = find_table(&out, b"vhea").unwrap();
        assert_eq!(be16(&out, vhea_o + 34), Some(3));
        let (vmtx_o, vmtx_l) = find_table(&out, b"vmtx").unwrap();
        assert_eq!(vmtx_l, 12);
        // gid 2: advance from the last long metric, its own top bearing.
        assert_eq!(be16(&out, vmtx_o + 8), Some(1000));
        assert_eq!(be16(&out, vmtx_o + 10), Some(12));
    }

    #[test]
    fn encodings() {
        assert_eq!(winansi_to_unicode(b'A'), Some(0x41));
        assert_eq!(winansi_to_unicode(0x92), Some(0x2019)); // right single quote
        assert_eq!(winansi_to_unicode(0x81), None);
        assert_eq!(glyph_name_to_unicode(b"space"), Some(0x20));
        assert_eq!(glyph_name_to_unicode(b"uni20AC"), Some(0x20AC));
        assert_eq!(glyph_name_to_unicode(b"A"), Some(0x41));
        assert_eq!(glyph_name_to_unicode(b"gobbledygook"), None);
    }
}
