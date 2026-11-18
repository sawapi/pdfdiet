//! Opt-in outline subsetting for CID-keyed CFF fonts (FontFile3 /
//! CIDFontType0C), the CFF counterpart of `subset_outlines` for TrueType.
//!
//! Only Type0 fonts with an Identity-H/V encoding are handled: there a
//! shown string's two-byte codes are CIDs, and the CFF charset maps CIDs to
//! glyphs, so the glyphs the document can show are known exactly from the
//! strict usage walk (`subset::collect_font_usage`). Every other glyph's
//! charstring becomes a bare `endchar`; glyph order, the charset, FDSelect,
//! the global and local subroutines and every dictionary value stay as they
//! are. The file is re-laid out with every offset written as a 5-byte
//! integer, so dictionary sizes do not depend on where things land.
//!
//! Anything outside that model (a Type0 font reaching the program with
//! another encoding, a simple font using it, a tainted user, an odd-length
//! string, a CFF structure not read here) leaves the program untouched.
//! Implemented from Adobe Technical Notes #5176 (CFF) and #5177 (Type 2
//! charstrings) and ISO 32000.

use std::collections::{BTreeSet, HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::subset::collect_font_usage;
use crate::{MAX_INFLATE, inflate, resolve, resolve_dict};

/// Empty the unused outlines of every eligible CID-keyed CFF program.
/// Returns (programs rewritten, uncompressed bytes dropped); the rewritten
/// programs are compressed by the document-wide Flate pass.
pub(crate) fn subset_cff_cid_fonts(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    let Some(usage) = collect_font_usage(doc) else {
        log.push("CFF subsetting: skipped (document outside the walker's model)".into());
        return (0, 0);
    };

    let mut users: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut blocked: HashSet<ObjectId> = HashSet::new();
    for (&fid, obj) in &doc.objects {
        let Object::Dictionary(font) = obj else {
            continue;
        };
        if !matches!(font.get(b"Type"), Ok(Object::Name(n)) if n == b"Font") {
            continue;
        }
        let Some((program, eligible)) = reach_program(doc, font) else {
            continue;
        };
        if eligible && !usage.tainted.contains(&fid) {
            users.entry(program).or_default().push(fid);
        } else {
            blocked.insert(program);
        }
    }

    let mut ids: Vec<ObjectId> = users
        .keys()
        .copied()
        .filter(|p| !blocked.contains(p))
        .collect();
    ids.sort();
    let mut rewritten = 0usize;
    let mut dropped = 0i64;
    for program in ids {
        // CIDs shown through any user (Identity: each two-byte code is a CID).
        let mut cids: BTreeSet<u16> = BTreeSet::new();
        let mut ok = true;
        for fid in &users[&program] {
            for s in usage.strings.get(fid).into_iter().flatten() {
                if s.len() % 2 != 0 {
                    ok = false;
                }
                cids.extend(s.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)));
            }
        }
        if !ok {
            continue;
        }
        let Ok(Object::Stream(stream)) = doc.get_object(program) else {
            continue;
        };
        if !matches!(
            crate::filter_names(&stream.dict)
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
            [] | ["FlateDecode"]
        ) {
            continue;
        }
        let Ok(data) = inflate(stream, MAX_INFLATE) else {
            continue;
        };
        let Some((out, glyphs, emptied)) = subset_cid_cff(&data, &cids) else {
            continue;
        };
        if out.len() >= data.len() {
            continue;
        }
        let Some(Object::Stream(target)) = doc.objects.get_mut(&program) else {
            continue;
        };
        target.dict.remove(b"Filter");
        target.dict.remove(b"DecodeParms");
        target.dict.set("Length", out.len() as i64);
        log.push(format!(
            "font program {} {}: CFF {} glyphs, {} unused outlines emptied ({} -> {} bytes uncompressed)",
            program.0,
            program.1,
            glyphs,
            emptied,
            data.len(),
            out.len()
        ));
        dropped += (data.len() - out.len()) as i64;
        target.set_content(out);
        target.allows_compression = true;
        rewritten += 1;
    }
    (rewritten, dropped)
}

/// The CFF program a font reaches through FontFile3, and whether the font
/// is a user this module can account for: a Type0 font with an Identity-H/V
/// encoding over a single CIDFontType0 whose program is CIDFontType0C.
/// Any other font reaching a FontFile3 program blocks it.
fn reach_program(doc: &Document, font: &Dictionary) -> Option<(ObjectId, bool)> {
    let descriptor_program = |d: &Dictionary| -> Option<ObjectId> {
        let desc = match d.get(b"FontDescriptor").ok()? {
            Object::Reference(r) => doc.get_dictionary(*r).ok()?,
            Object::Dictionary(dd) => dd,
            _ => return None,
        };
        match desc.get(b"FontFile3").ok()? {
            Object::Reference(p) => Some(*p),
            _ => None,
        }
    };
    match font.get(b"Subtype") {
        Ok(Object::Name(n)) if n == b"Type0" => {
            let Ok(Object::Array(desc)) =
                font.get(b"DescendantFonts").and_then(|o| resolve(doc, o))
            else {
                return None;
            };
            let cid = resolve_dict(doc, desc.first()?).ok()?;
            let program = descriptor_program(cid)?;
            let identity = matches!(font.get(b"Encoding"), Ok(Object::Name(e)) if e == b"Identity-H" || e == b"Identity-V");
            let cid_type0 =
                matches!(cid.get(b"Subtype"), Ok(Object::Name(s)) if s == b"CIDFontType0");
            let cff_stream = matches!(
                doc.get_object(program),
                Ok(Object::Stream(s)) if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(t)) if t == b"CIDFontType0C")
            );
            Some((
                program,
                identity && cid_type0 && cff_stream && desc.len() == 1,
            ))
        }
        // A CIDFont reached directly is judged through its Type0 parent; a
        // simple font (Type1C) using a program blocks it.
        Ok(Object::Name(n)) if n == b"CIDFontType0" => None,
        _ => descriptor_program(font).map(|p| (p, false)),
    }
}

// ---------------------------------------------------------------------------
// CFF reading and writing.

/// An INDEX: its absolute byte range and the absolute ranges of its items.
struct Index {
    start: usize,
    end: usize,
    items: Vec<(usize, usize)>,
}

fn read_index(d: &[u8], at: usize) -> Option<Index> {
    let count = u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]) as usize;
    if count == 0 {
        return Some(Index {
            start: at,
            end: at + 2,
            items: Vec::new(),
        });
    }
    let os = *d.get(at + 2)? as usize;
    if !(1..=4).contains(&os) {
        return None;
    }
    let off = |i: usize| -> Option<usize> {
        let p = at + 3 + i * os;
        Some(
            d.get(p..p + os)?
                .iter()
                .fold(0usize, |v, &b| (v << 8) | b as usize),
        )
    };
    let base = at + 3 + (count + 1) * os - 1;
    let mut items = Vec::with_capacity(count);
    let mut prev = off(0)?;
    if prev != 1 {
        return None;
    }
    // Every item end is checked, with checked addition: offsets are up to
    // 32 bits, so on 32-bit targets (wasm) base + offset can wrap.
    let mut start = base.checked_add(prev)?;
    for i in 1..=count {
        let next = off(i)?;
        let end = base.checked_add(next)?;
        if next < prev || end > d.len() {
            return None;
        }
        items.push((start, end));
        prev = next;
        start = end;
    }
    let end = start;
    (end <= d.len()).then_some(Index {
        start: at,
        end,
        items,
    })
}

fn write_index(items: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(items.len() as u16).to_be_bytes());
    if items.is_empty() {
        return out;
    }
    let total: usize = items.iter().map(|i| i.len()).sum::<usize>() + 1;
    let os = match total {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    out.push(os as u8);
    let mut off = 1usize;
    let push_off =
        |out: &mut Vec<u8>, v: usize| out.extend_from_slice(&(v as u32).to_be_bytes()[4 - os..]);
    push_off(&mut out, off);
    for item in items {
        off += item.len();
        push_off(&mut out, off);
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// One DICT entry: raw operand encodings and the operator (escaped
/// operators as 1200 + second byte).
struct Entry {
    operands: Vec<Vec<u8>>,
    op: u16,
}

fn parse_dict(d: &[u8]) -> Option<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut operands: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let b = d[i];
        let len = match b {
            0..=21 => {
                let op = if b == 12 {
                    let op = 1200 + *d.get(i + 1)? as u16;
                    i += 2;
                    op
                } else {
                    i += 1;
                    b as u16
                };
                entries.push(Entry {
                    operands: std::mem::take(&mut operands),
                    op,
                });
                continue;
            }
            28 => 3,
            29 => 5,
            30 => {
                let mut j = i + 1;
                loop {
                    let x = *d.get(j)?;
                    j += 1;
                    if x >> 4 == 0xF || x & 0xF == 0xF {
                        break;
                    }
                }
                j - i
            }
            32..=246 => 1,
            247..=254 => 2,
            _ => return None,
        };
        operands.push(d.get(i..i + len)?.to_vec());
        i += len;
    }
    operands.is_empty().then_some(entries)
}

fn int_value(raw: &[u8]) -> Option<i64> {
    let b0 = *raw.first()? as i64;
    Some(match raw[0] {
        28 => i16::from_be_bytes([*raw.get(1)?, *raw.get(2)?]) as i64,
        29 => i32::from_be_bytes([*raw.get(1)?, *raw.get(2)?, *raw.get(3)?, *raw.get(4)?]) as i64,
        32..=246 => b0 - 139,
        247..=250 => (b0 - 247) * 256 + *raw.get(1)? as i64 + 108,
        251..=254 => -(b0 - 251) * 256 - *raw.get(1)? as i64 - 108,
        _ => return None,
    })
}

fn int32(v: usize) -> Vec<u8> {
    let mut out = vec![29];
    out.extend_from_slice(&(v as i32).to_be_bytes());
    out
}

fn write_dict(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        for o in &e.operands {
            out.extend_from_slice(o);
        }
        if e.op >= 1200 {
            out.extend_from_slice(&[12, (e.op - 1200) as u8]);
        } else {
            out.push(e.op as u8);
        }
    }
    out
}

fn single_int(entries: &[Entry], op: u16) -> Option<usize> {
    let e = entries.iter().find(|e| e.op == op)?;
    match e.operands.as_slice() {
        [v] => usize::try_from(int_value(v)?).ok(),
        _ => None,
    }
}

/// Replace an entry's operands (the entry must exist).
fn set_operands(entries: &mut [Entry], op: u16, operands: Vec<Vec<u8>>) -> Option<()> {
    entries.iter_mut().find(|e| e.op == op)?.operands = operands;
    Some(())
}

const CHARSET: u16 = 15;
const CHARSTRINGS: u16 = 17;
const PRIVATE: u16 = 18;
const SUBRS: u16 = 19;
const CHARSTRING_TYPE: u16 = 1206;
const ROS: u16 = 1230;
const FD_ARRAY: u16 = 1236;
const FD_SELECT: u16 = 1237;

/// CID of every glyph (gid 0 is CID 0) and the charset's byte length.
fn read_charset(d: &[u8], at: usize, glyphs: usize) -> Option<(Vec<u16>, usize)> {
    let mut cids = vec![0u16];
    let be = |p: usize| Some(u16::from_be_bytes([*d.get(p)?, *d.get(p + 1)?]));
    let format = *d.get(at)?;
    let mut p = at + 1;
    while cids.len() < glyphs {
        match format {
            0 => {
                cids.push(be(p)?);
                p += 2;
            }
            1 | 2 => {
                let first = be(p)?;
                let left = if format == 1 {
                    *d.get(p + 2)? as u16
                } else {
                    be(p + 2)?
                };
                p += if format == 1 { 3 } else { 4 };
                for k in 0..=left {
                    cids.push(first.checked_add(k)?);
                }
            }
            _ => return None,
        }
    }
    (cids.len() == glyphs).then_some((cids, p - at))
}

fn fdselect_len(d: &[u8], at: usize, glyphs: usize) -> Option<usize> {
    match *d.get(at)? {
        0 => (at + 1 + glyphs <= d.len()).then_some(1 + glyphs),
        3 => {
            let ranges = u16::from_be_bytes([*d.get(at + 1)?, *d.get(at + 2)?]) as usize;
            let len = 3 + ranges * 3 + 2;
            (at + len <= d.len()).then_some(len)
        }
        _ => None,
    }
}

/// Rewrite a CID-keyed CFF so that only glyph 0 and the glyphs of `cids`
/// keep their charstrings. Returns (new program, glyph count, outlines
/// emptied), or None if the font is outside what this reads or nothing
/// would be emptied.
fn subset_cid_cff(d: &[u8], cids: &BTreeSet<u16>) -> Option<(Vec<u8>, usize, usize)> {
    if d.len() < 4 || d[0] != 1 {
        return None;
    }
    let hdr = d[2] as usize;
    let name = read_index(d, hdr)?;
    let top = read_index(d, name.end)?;
    let strings = read_index(d, top.end)?;
    let gsubrs = read_index(d, strings.end)?;
    let [(ts, te)] = top.items.as_slice() else {
        return None;
    };
    let mut top_dict = parse_dict(d.get(*ts..*te)?)?;
    if !top_dict.iter().any(|e| e.op == ROS)
        || top_dict.iter().any(|e| e.op == PRIVATE || e.op == 16)
        || single_int(&top_dict, CHARSTRING_TYPE).is_some_and(|t| t != 2)
    {
        return None;
    }
    let charset_at = single_int(&top_dict, CHARSET).filter(|&v| v > 2)?;
    let cs = read_index(d, single_int(&top_dict, CHARSTRINGS)?)?;
    let fdselect_at = single_int(&top_dict, FD_SELECT)?;
    let fdarray = read_index(d, single_int(&top_dict, FD_ARRAY)?)?;
    let glyphs = cs.items.len();
    if glyphs == 0 {
        return None;
    }
    let (gid_cid, charset_len) = read_charset(d, charset_at, glyphs)?;
    let fds_len = fdselect_len(d, fdselect_at, glyphs)?;

    // Font DICTs and their Private DICTs (with local Subrs, if any).
    struct Fd {
        dict: Vec<Entry>,
        private: Vec<Entry>,
        subrs: Option<(usize, usize)>,
    }
    let mut fds = Vec::new();
    for &(s, e) in &fdarray.items {
        let dict = parse_dict(d.get(s..e)?)?;
        let p = dict.iter().find(|x| x.op == PRIVATE)?;
        let [size, off] = p.operands.as_slice() else {
            return None;
        };
        let (size, off) = (
            usize::try_from(int_value(size)?).ok()?,
            usize::try_from(int_value(off)?).ok()?,
        );
        let private = parse_dict(d.get(off..off.checked_add(size)?)?)?;
        let subrs = match single_int(&private, SUBRS) {
            Some(rel) => {
                let idx = read_index(d, off.checked_add(rel)?)?;
                Some((idx.start, idx.end))
            }
            None => None,
        };
        fds.push(Fd {
            dict,
            private,
            subrs,
        });
    }

    // New CharStrings: unused glyphs become a bare endchar.
    const ENDCHAR: &[u8] = &[14];
    let mut emptied = 0;
    let charstrings: Vec<&[u8]> = cs
        .items
        .iter()
        .enumerate()
        .map(|(gid, &(s, e))| {
            if gid == 0 || cids.contains(&gid_cid[gid]) {
                &d[s..e]
            } else {
                if e - s > 1 {
                    emptied += 1;
                }
                ENDCHAR
            }
        })
        .collect();
    if emptied == 0 {
        return None;
    }
    let charstrings = write_index(&charstrings);

    // Every offset operand becomes a 5-byte integer, so sizes are fixed
    // before positions are known.
    for op in [CHARSET, CHARSTRINGS, FD_SELECT, FD_ARRAY] {
        set_operands(&mut top_dict, op, vec![int32(0)])?;
    }
    for fd in &mut fds {
        set_operands(&mut fd.dict, PRIVATE, vec![int32(0), int32(0)])?;
        if fd.subrs.is_some() {
            set_operands(&mut fd.private, SUBRS, vec![int32(0)])?;
        }
    }
    let top_len = write_index(&[&write_dict(&top_dict)]).len();
    let fd_dicts: Vec<Vec<u8>> = fds.iter().map(|f| write_dict(&f.dict)).collect();
    let fdarray_len = write_index(&fd_dicts.iter().map(|v| v.as_slice()).collect::<Vec<_>>()).len();

    let head = &d[..name.end]; // header and Name INDEX, verbatim
    let mut pos =
        head.len() + top_len + (strings.end - strings.start) + (gsubrs.end - gsubrs.start);
    let charset_pos = pos;
    pos += charset_len;
    let fdselect_pos = pos;
    pos += fds_len;
    let charstrings_pos = pos;
    pos += charstrings.len();
    let fdarray_pos = pos;
    pos += fdarray_len;
    let mut privates = Vec::new();
    for fd in &mut fds {
        let private_len = write_dict(&fd.private).len();
        if fd.subrs.is_some() {
            set_operands(&mut fd.private, SUBRS, vec![int32(private_len)])?;
        }
        let private = write_dict(&fd.private);
        set_operands(
            &mut fd.dict,
            PRIVATE,
            vec![int32(private.len()), int32(pos)],
        )?;
        pos += private.len() + fd.subrs.map_or(0, |(s, e)| e - s);
        privates.push(private);
    }
    set_operands(&mut top_dict, CHARSET, vec![int32(charset_pos)])?;
    set_operands(&mut top_dict, CHARSTRINGS, vec![int32(charstrings_pos)])?;
    set_operands(&mut top_dict, FD_SELECT, vec![int32(fdselect_pos)])?;
    set_operands(&mut top_dict, FD_ARRAY, vec![int32(fdarray_pos)])?;

    let mut out = Vec::with_capacity(pos);
    out.extend_from_slice(head);
    out.extend_from_slice(&write_index(&[&write_dict(&top_dict)]));
    out.extend_from_slice(&d[strings.start..strings.end]);
    out.extend_from_slice(&d[gsubrs.start..gsubrs.end]);
    out.extend_from_slice(&d[charset_at..charset_at + charset_len]);
    out.extend_from_slice(&d[fdselect_at..fdselect_at + fds_len]);
    out.extend_from_slice(&charstrings);
    let fd_dicts: Vec<Vec<u8>> = fds.iter().map(|f| write_dict(&f.dict)).collect();
    out.extend_from_slice(&write_index(
        &fd_dicts.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
    ));
    for (fd, private) in fds.iter().zip(&privates) {
        out.extend_from_slice(private);
        if let Some((s, e)) = fd.subrs {
            out.extend_from_slice(&d[s..e]);
        }
    }
    (out.len() == pos).then_some((out, glyphs, emptied))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_offsets_past_the_data_are_refused() {
        // One item whose end offset is near 2^32: base + offset wraps on
        // 32-bit targets; every end must be checked, with checked addition.
        let d = [0, 1, 4, 0, 0, 0, 1, 0xFF, 0xFF, 0xFF, 0xEC, 0, 0, 0, 0];
        assert!(read_index(&d, 0).is_none());
        // A well-formed one-item INDEX still reads.
        let ok = [0, 1, 1, 1, 3, b'a', b'b'];
        let idx = read_index(&ok, 0).unwrap();
        assert_eq!(idx.items, vec![(5, 7)]);
        assert_eq!(idx.end, 7);
    }

    /// A minimal CID-keyed CFF: 4 glyphs (CIDs 0, 5, 6, 9), one FD with a
    /// Private DICT and two local subrs.
    fn mini_cid_cff() -> Vec<u8> {
        let header = vec![1, 0, 4, 4];
        let name = write_index(&[b"X"]);
        let strings = write_index(&[b"Adobe", b"Identity"]);
        let gsubrs = write_index(&[]);
        let charset = vec![0, 0, 5, 0, 6, 0, 9];
        let fdselect = vec![0, 0, 0, 0, 0];
        let outline = |tag: u8| vec![139 + tag, 139, 21, 14]; // tag 0 rmoveto endchar
        let cs_items = [vec![14], outline(1), outline(2), outline(3)];
        let charstrings = write_index(&cs_items.iter().map(|v| v.as_slice()).collect::<Vec<_>>());
        let subrs = write_index(&[&[11], &[11]]);
        // Lay out with int32 offsets so the test font itself is simple.
        let build = |charset_pos: usize, fdselect_pos: usize, cs_pos: usize, fda_pos: usize| {
            // ROS: Registry/Ordering SIDs 391/392 (the two custom strings), 0.
            let mut top = Vec::new();
            top.extend_from_slice(&int32(391));
            top.extend_from_slice(&int32(392));
            top.push(139);
            top.extend_from_slice(&[12, 30]);
            top.extend_from_slice(&int32(charset_pos));
            top.push(15);
            top.extend_from_slice(&int32(cs_pos));
            top.push(17);
            top.extend_from_slice(&int32(fdselect_pos));
            top.extend_from_slice(&[12, 37]);
            top.extend_from_slice(&int32(fda_pos));
            top.extend_from_slice(&[12, 36]);
            top
        };
        let top_len = write_index(&[&build(0, 0, 0, 0)]).len();
        let mut pos = header.len() + name.len() + top_len + strings.len() + gsubrs.len();
        let charset_pos = pos;
        pos += charset.len();
        let fdselect_pos = pos;
        pos += fdselect.len();
        let cs_pos = pos;
        pos += charstrings.len();
        let fda_pos = pos;
        let mut private = Vec::new();
        private.extend_from_slice(&int32(0)); // Subrs placeholder
        private.push(19);
        let private_len = private.len();
        private.clear();
        private.extend_from_slice(&int32(private_len));
        private.push(19);
        let mk_fd = |private_pos: usize| {
            let mut fd = Vec::new();
            fd.extend_from_slice(&int32(private_len));
            fd.extend_from_slice(&int32(private_pos));
            fd.push(18);
            fd
        };
        let fda_len = write_index(&[&mk_fd(0)]).len();
        let private_pos = fda_pos + fda_len;
        let mut out = header;
        out.extend(name);
        out.extend(write_index(&[&build(
            charset_pos,
            fdselect_pos,
            cs_pos,
            fda_pos,
        )]));
        out.extend(strings);
        out.extend(gsubrs);
        out.extend(charset);
        out.extend(fdselect);
        out.extend(charstrings);
        out.extend(write_index(&[&mk_fd(private_pos)]));
        out.extend(private);
        out.extend(subrs);
        out
    }

    fn charstrings_of(cff: &[u8]) -> Vec<Vec<u8>> {
        let name = read_index(cff, cff[2] as usize).unwrap();
        let top = read_index(cff, name.end).unwrap();
        let dict = parse_dict(&cff[top.items[0].0..top.items[0].1]).unwrap();
        let cs = read_index(cff, single_int(&dict, CHARSTRINGS).unwrap()).unwrap();
        cs.items.iter().map(|&(s, e)| cff[s..e].to_vec()).collect()
    }

    #[test]
    fn unused_cids_lose_their_outlines_and_the_rest_is_kept() {
        let cff = mini_cid_cff();
        let before = charstrings_of(&cff);
        let used: BTreeSet<u16> = [6].into_iter().collect(); // gid 2
        let (out, glyphs, emptied) = subset_cid_cff(&cff, &used).unwrap();
        assert_eq!((glyphs, emptied), (4, 2));
        let after = charstrings_of(&out);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], vec![14]);
        assert_eq!(after[2], before[2]);
        assert_eq!(after[3], vec![14]);
        // The rewritten font is still fully readable: charset, FDSelect,
        // Private and local Subrs land where the new offsets say.
        let name = read_index(&out, out[2] as usize).unwrap();
        let top = read_index(&out, name.end).unwrap();
        let dict = parse_dict(&out[top.items[0].0..top.items[0].1]).unwrap();
        let (cids, _) = read_charset(&out, single_int(&dict, CHARSET).unwrap(), 4).unwrap();
        assert_eq!(cids, vec![0, 5, 6, 9]);
        assert!(fdselect_len(&out, single_int(&dict, FD_SELECT).unwrap(), 4).is_some());
        let fda = read_index(&out, single_int(&dict, FD_ARRAY).unwrap()).unwrap();
        let fd = parse_dict(&out[fda.items[0].0..fda.items[0].1]).unwrap();
        let p = fd.iter().find(|e| e.op == PRIVATE).unwrap();
        let (size, off) = (
            int_value(&p.operands[0]).unwrap() as usize,
            int_value(&p.operands[1]).unwrap() as usize,
        );
        let private = parse_dict(&out[off..off + size]).unwrap();
        let subrs = read_index(&out, off + single_int(&private, SUBRS).unwrap()).unwrap();
        assert_eq!(subrs.items.len(), 2);
        assert_eq!(subrs.end, out.len());
    }

    #[test]
    fn nothing_to_empty_or_a_non_cid_font_is_left_alone() {
        let cff = mini_cid_cff();
        let all: BTreeSet<u16> = [5, 6, 9].into_iter().collect();
        assert!(subset_cid_cff(&cff, &all).is_none());
        assert!(subset_cid_cff(&cff[..20], &BTreeSet::new()).is_none());
    }
}
