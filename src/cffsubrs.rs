//! Empty the subroutines no charstring calls, in every bare CFF font
//! program (FontFile3 /Type1C or /CIDFontType0C).
//!
//! Subset fonts often keep the whole font's subroutines although their few
//! glyphs call a handful. Every charstring is run as a Type 2 interpreter
//! would (no arithmetic or storage operators, so the path is fixed), which
//! finds exactly the local and global subroutines that can ever be called.
//! The others become a bare `return`: their numbers, and so every call and
//! every charstring, stay as they are, and the glyphs draw the same.
//!
//! Anything outside that model (another charstring type, an operator this
//! does not run, a call computed or out of range, a CFF structure not read
//! here, too much work) leaves the program untouched.
//! Implemented from Adobe Technical Notes #5176 (CFF) and #5177 (Type 2
//! charstrings).

use lopdf::{Document, Object, ObjectId};

use crate::cffsubset::{
    CHARSET, CHARSTRING_TYPE, CHARSTRINGS, Entry, FD_ARRAY, FD_SELECT, PRIVATE, ROS, SUBRS,
    int_value, int32, parse_dict, read_charset, read_index, set_operands, single_int, write_dict,
    write_index,
};
use crate::{MAX_INFLATE, inflate};

const ENCODING: u16 = 16;
const SYNTHETIC_BASE: u16 = 1220;
/// Charstring operators run per program, at most.
const MAX_STEPS: usize = 4_000_000;
/// Subroutine nesting, as Type 2 allows.
const MAX_DEPTH: usize = 10;
const MAX_STACK: usize = 48;
const RETURN: &[u8] = &[11];

/// Empty the uncalled subroutines of every bare CFF program. Returns
/// (programs rewritten, uncompressed bytes dropped).
pub(crate) fn prune_cff_subrs(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    let mut ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter_map(|(&id, o)| match o {
            Object::Stream(s)
                if matches!(s.dict.get(b"Subtype"), Ok(Object::Name(n))
                    if n == b"Type1C" || n == b"CIDFontType0C") =>
            {
                Some(id)
            }
            _ => None,
        })
        .collect();
    ids.sort();
    let mut rewritten = 0;
    let mut dropped = 0i64;
    for id in ids {
        let Some(Object::Stream(stream)) = doc.objects.get(&id) else {
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
        let Some((out, emptied)) = prune(&data) else {
            continue;
        };
        if out.len() >= data.len() {
            continue;
        }
        let Some(Object::Stream(target)) = doc.objects.get_mut(&id) else {
            continue;
        };
        target.dict.remove(b"Filter");
        target.dict.remove(b"DecodeParms");
        target.dict.set("Length", out.len() as i64);
        log.push(format!(
            "font program {} {}: CFF {} uncalled subroutines emptied ({} -> {} bytes uncompressed)",
            id.0,
            id.1,
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

/// A Private DICT and its local subroutines, as read.
struct Private {
    dict: Vec<Entry>,
    /// Absolute byte ranges of the local subroutines, if any.
    subrs: Option<Vec<(usize, usize)>>,
}

fn read_private(d: &[u8], entry: &Entry) -> Option<Private> {
    let [size, off] = entry.operands.as_slice() else {
        return None;
    };
    let size = usize::try_from(int_value(size)?).ok()?;
    let off = usize::try_from(int_value(off)?).ok()?;
    let dict = parse_dict(d.get(off..off.checked_add(size)?)?)?;
    let subrs = match single_int(&dict, SUBRS) {
        Some(rel) => Some(read_index(d, off.checked_add(rel)?)?.items),
        None => None,
    };
    Some(Private { dict, subrs })
}

/// The byte length of a custom Encoding at `at`.
fn encoding_len(d: &[u8], at: usize) -> Option<usize> {
    let format = *d.get(at)?;
    let n = *d.get(at + 1)? as usize;
    let mut len = match format & 0x7F {
        0 => 2 + n,
        1 => 2 + 2 * n,
        _ => return None,
    };
    if format & 0x80 != 0 {
        len += 1 + 3 * *d.get(at + len)? as usize;
    }
    (at + len <= d.len()).then_some(len)
}

/// The Font DICT index of every glyph.
fn read_fdselect(d: &[u8], at: usize, glyphs: usize) -> Option<Vec<usize>> {
    match *d.get(at)? {
        0 => Some(
            d.get(at + 1..at + 1 + glyphs)?
                .iter()
                .map(|&f| f as usize)
                .collect(),
        ),
        3 => {
            let be = |p: usize| Some(u16::from_be_bytes([*d.get(p)?, *d.get(p + 1)?]) as usize);
            let ranges = be(at + 1)?;
            let mut out = vec![usize::MAX; glyphs];
            for r in 0..ranges {
                let p = at + 3 + r * 3;
                let (first, fd, next) = (be(p)?, *d.get(p + 2)? as usize, be(p + 3)?);
                for slot in out.get_mut(first..next.min(glyphs))? {
                    *slot = fd;
                }
            }
            (!out.contains(&usize::MAX)).then_some(out)
        }
        _ => None,
    }
}

fn bias(count: usize) -> i64 {
    match count {
        0..1240 => 107,
        1240..33900 => 1131,
        _ => 32768,
    }
}

enum Flow {
    Return,
    End,
}

/// Runs charstrings, marking the subroutines they call.
struct Runner<'a> {
    d: &'a [u8],
    global: &'a [(usize, usize)],
    global_used: Vec<bool>,
    steps: usize,
}

/// One glyph's state: the argument stack (None for a non-integer) and the
/// stem hints declared so far.
struct Glyph {
    stack: Vec<Option<i64>>,
    stems: usize,
}

impl Runner<'_> {
    /// Run `code` (a charstring or a subroutine) at nesting `depth`.
    fn run(
        &mut self,
        code: (usize, usize),
        local: Option<&[(usize, usize)]>,
        local_used: &mut [bool],
        glyph: &mut Glyph,
        depth: usize,
    ) -> Option<Flow> {
        let d = self.d;
        let (mut i, end) = code;
        while i < end {
            self.steps += 1;
            if self.steps > MAX_STEPS || glyph.stack.len() > MAX_STACK {
                return None;
            }
            let b = d[i];
            i += 1;
            match b {
                32..=246 => glyph.stack.push(Some(b as i64 - 139)),
                247..=250 => {
                    let w = *d.get(i)? as i64;
                    i += 1;
                    glyph.stack.push(Some((b as i64 - 247) * 256 + w + 108));
                }
                251..=254 => {
                    let w = *d.get(i)? as i64;
                    i += 1;
                    glyph.stack.push(Some(-(b as i64 - 251) * 256 - w - 108));
                }
                28 => {
                    let v = i16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]);
                    i += 2;
                    glyph.stack.push(Some(v as i64));
                }
                255 => {
                    let v = i32::from_be_bytes([
                        *d.get(i)?,
                        *d.get(i + 1)?,
                        *d.get(i + 2)?,
                        *d.get(i + 3)?,
                    ]);
                    i += 4;
                    glyph
                        .stack
                        .push((v & 0xFFFF == 0).then_some((v >> 16) as i64));
                }
                // Stem hints: pairs of arguments (an odd one is the width).
                1 | 3 | 18 | 23 => {
                    glyph.stems += glyph.stack.len() / 2;
                    glyph.stack.clear();
                }
                // hintmask / cntrmask: arguments left are implicit vstems;
                // one mask bit per stem follows.
                19 | 20 => {
                    glyph.stems += glyph.stack.len() / 2;
                    glyph.stack.clear();
                    i += glyph.stems.div_ceil(8);
                    if i > end {
                        return None;
                    }
                }
                10 | 29 => {
                    let n = glyph.stack.pop()??;
                    let (subrs, used): (&[(usize, usize)], &mut [bool]) = if b == 10 {
                        (local?, &mut *local_used)
                    } else {
                        (self.global, &mut self.global_used[..])
                    };
                    let k = usize::try_from(n + bias(subrs.len())).ok()?;
                    let sub = *subrs.get(k)?;
                    used[k] = true;
                    if depth >= MAX_DEPTH {
                        return None;
                    }
                    if let Flow::End = self.run(sub, local, local_used, glyph, depth + 1)? {
                        return Some(Flow::End);
                    }
                }
                11 => return Some(Flow::Return),
                14 => return Some(Flow::End),
                12 => {
                    let e = *d.get(i)?;
                    i += 1;
                    // flex, hflex, hflex1, flex1 (and the old dotsection)
                    // draw; arithmetic and storage would make the path
                    // depend on values, so they are not run.
                    if !matches!(e, 0 | 34..=37) {
                        return None;
                    }
                    glyph.stack.clear();
                }
                4..=8 | 21 | 22 | 24..=27 | 30 | 31 => glyph.stack.clear(),
                _ => return None,
            }
        }
        // Fell off the end without return or endchar.
        None
    }
}

/// The program with its uncalled subroutines emptied, and how many were,
/// or None if it is outside what this reads or nothing would change.
fn prune(d: &[u8]) -> Option<(Vec<u8>, usize)> {
    if d.len() < 4 || d[0] != 1 {
        return None;
    }
    let name = read_index(d, d[2] as usize)?;
    let top = read_index(d, name.end)?;
    let strings = read_index(d, top.end)?;
    let gsubrs = read_index(d, strings.end)?;
    let [(ts, te)] = top.items.as_slice() else {
        return None;
    };
    let mut top_dict = parse_dict(d.get(*ts..*te)?)?;
    if single_int(&top_dict, CHARSTRING_TYPE).is_some_and(|t| t != 2)
        || top_dict.iter().any(|e| e.op == SYNTHETIC_BASE)
    {
        return None;
    }
    let cs_at = single_int(&top_dict, CHARSTRINGS)?;
    let cs = read_index(d, cs_at)?;
    let glyphs = cs.items.len();
    if glyphs == 0 {
        return None;
    }
    let cid = top_dict.iter().any(|e| e.op == ROS);

    // The Private DICTs (one per Font DICT for a CID font) and which one
    // each glyph uses.
    let mut fd_dicts: Vec<Vec<Entry>> = Vec::new();
    let mut privates: Vec<Private> = Vec::new();
    let glyph_fd: Vec<usize>;
    let mut fdselect = None;
    if cid {
        let fdarray = read_index(d, single_int(&top_dict, FD_ARRAY)?)?;
        for &(s, e) in &fdarray.items {
            let dict = parse_dict(d.get(s..e)?)?;
            privates.push(read_private(d, dict.iter().find(|x| x.op == PRIVATE)?)?);
            fd_dicts.push(dict);
        }
        let at = single_int(&top_dict, FD_SELECT)?;
        glyph_fd = read_fdselect(d, at, glyphs)?;
        if glyph_fd.iter().any(|&f| f >= privates.len()) {
            return None;
        }
        fdselect = Some((at, crate::cffsubset::fdselect_len(d, at, glyphs)?));
    } else {
        privates.push(read_private(d, top_dict.iter().find(|x| x.op == PRIVATE)?)?);
        glyph_fd = vec![0; glyphs];
    }

    // Run every glyph.
    let mut runner = Runner {
        d,
        global: &gsubrs.items,
        global_used: vec![false; gsubrs.items.len()],
        steps: 0,
    };
    let mut local_used: Vec<Vec<bool>> = privates
        .iter()
        .map(|p| vec![false; p.subrs.as_ref().map_or(0, |s| s.len())])
        .collect();
    for (gid, &code) in cs.items.iter().enumerate() {
        let fd = glyph_fd[gid];
        let mut glyph = Glyph {
            stack: Vec::new(),
            stems: 0,
        };
        let local = privates[fd].subrs.as_deref();
        runner.run(code, local, &mut local_used[fd], &mut glyph, 0)?;
    }

    let mut emptied = 0;
    let mut keep = |items: &[(usize, usize)], used: &[bool]| -> Vec<u8> {
        let list: Vec<&[u8]> = items
            .iter()
            .zip(used)
            .map(|(&(s, e), &u)| {
                if u || e - s <= 1 {
                    &d[s..e]
                } else {
                    emptied += 1;
                    RETURN
                }
            })
            .collect();
        write_index(&list)
    };
    let new_gsubrs = keep(&gsubrs.items, &runner.global_used);
    let new_locals: Vec<Option<Vec<u8>>> = privates
        .iter()
        .zip(&local_used)
        .map(|(p, used)| p.subrs.as_ref().map(|s| keep(s, used)))
        .collect();
    if emptied == 0 {
        return None;
    }

    // Lay the file out again: header and names, Top DICT (offsets as fixed
    // 5-byte integers), strings, global subroutines, then charset,
    // encoding, FDSelect, CharStrings, FDArray, and each Private DICT
    // followed by its local subroutines.
    let charset = match single_int(&top_dict, CHARSET).filter(|&v| v > 2) {
        Some(at) => Some((at, read_charset(d, at, glyphs)?.1)),
        None => None,
    };
    let encoding = match single_int(&top_dict, ENCODING).filter(|&v| v > 1 && !cid) {
        Some(at) => Some((at, encoding_len(d, at)?)),
        None => None,
    };
    for (op, present) in [
        (CHARSET, charset.is_some()),
        (ENCODING, encoding.is_some()),
        (FD_SELECT, cid),
        (FD_ARRAY, cid),
        (CHARSTRINGS, true),
    ] {
        if present {
            set_operands(&mut top_dict, op, vec![int32(0)])?;
        }
    }
    if !cid {
        set_operands(&mut top_dict, PRIVATE, vec![int32(0), int32(0)])?;
    }
    for fd in &mut fd_dicts {
        set_operands(fd, PRIVATE, vec![int32(0), int32(0)])?;
    }
    for p in &mut privates {
        if p.subrs.is_some() {
            set_operands(&mut p.dict, SUBRS, vec![int32(0)])?;
        }
    }
    let top_len = write_index(&[&write_dict(&top_dict)]).len();
    let fdarray_len = |dicts: &[Vec<Entry>]| {
        let bytes: Vec<Vec<u8>> = dicts.iter().map(|f| write_dict(f)).collect();
        write_index(&bytes.iter().map(|v| v.as_slice()).collect::<Vec<_>>()).len()
    };

    let head = &d[..name.end];
    let mut pos = head.len() + top_len + (strings.end - strings.start) + new_gsubrs.len();
    let mut place = |len: usize| {
        let at = pos;
        pos += len;
        at
    };
    let charset_pos = charset.map(|(_, len)| place(len));
    let encoding_pos = encoding.map(|(_, len)| place(len));
    let fdselect_pos = fdselect.map(|(_, len)| place(len));
    let cs_pos = place(cs.end - cs.start);
    let fdarray_pos = cid.then(|| place(fdarray_len(&fd_dicts)));
    let mut private_bytes = Vec::new();
    for (k, p) in privates.iter_mut().enumerate() {
        let len = write_dict(&p.dict).len();
        if p.subrs.is_some() {
            set_operands(&mut p.dict, SUBRS, vec![int32(len)])?;
        }
        let bytes = write_dict(&p.dict);
        let at = place(bytes.len() + new_locals[k].as_ref().map_or(0, |s| s.len()));
        let operands = vec![int32(bytes.len()), int32(at)];
        if cid {
            set_operands(&mut fd_dicts[k], PRIVATE, operands)?;
        } else {
            set_operands(&mut top_dict, PRIVATE, operands)?;
        }
        private_bytes.push(bytes);
    }
    for (op, at) in [
        (CHARSET, charset_pos),
        (ENCODING, encoding_pos),
        (FD_SELECT, fdselect_pos),
        (FD_ARRAY, fdarray_pos),
        (CHARSTRINGS, Some(cs_pos)),
    ] {
        if let Some(at) = at {
            set_operands(&mut top_dict, op, vec![int32(at)])?;
        }
    }

    let mut out = Vec::with_capacity(pos);
    out.extend_from_slice(head);
    out.extend_from_slice(&write_index(&[&write_dict(&top_dict)]));
    out.extend_from_slice(&d[strings.start..strings.end]);
    out.extend_from_slice(&new_gsubrs);
    for (at, len) in [charset, encoding, fdselect].into_iter().flatten() {
        out.extend_from_slice(d.get(at..at + len)?);
    }
    out.extend_from_slice(&d[cs.start..cs.end]);
    if cid {
        let bytes: Vec<Vec<u8>> = fd_dicts.iter().map(|f| write_dict(f)).collect();
        out.extend_from_slice(&write_index(
            &bytes.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
        ));
    }
    for (bytes, local) in private_bytes.iter().zip(&new_locals) {
        out.extend_from_slice(bytes);
        if let Some(local) = local {
            out.extend_from_slice(local);
        }
    }
    (out.len() == pos).then_some((out, emptied))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal name-keyed CFF: the given charstrings, local and global
    /// subroutines, and a Private DICT with Subrs.
    fn mini_cff(charstrings: &[&[u8]], local: &[&[u8]], global: &[&[u8]]) -> Vec<u8> {
        let header = vec![1, 0, 4, 4];
        let name = write_index(&[b"X"]);
        let strings = write_index(&[]);
        let gsubrs = write_index(global);
        let cs = write_index(charstrings);
        // Private DICT: just Subrs, which start right after it (6 bytes).
        let mut private = int32(6);
        private.push(SUBRS as u8);
        let top_with = |cs_at: usize, priv_at: usize| {
            let mut t = int32(cs_at);
            t.push(CHARSTRINGS as u8);
            t.extend_from_slice(&int32(private.len()));
            t.extend_from_slice(&int32(priv_at));
            t.push(PRIVATE as u8);
            t
        };
        let top_len = write_index(&[&top_with(0, 0)]).len();
        let cs_at = header.len() + name.len() + top_len + strings.len() + gsubrs.len();
        let priv_at = cs_at + cs.len();
        let mut out = header;
        out.extend(name);
        out.extend(write_index(&[&top_with(cs_at, priv_at)]));
        out.extend(strings);
        out.extend(gsubrs);
        out.extend(cs);
        out.extend(&private);
        out.extend(write_index(local));
        out
    }

    fn charstrings_and_subrs(d: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let name = read_index(d, d[2] as usize).unwrap();
        let top = read_index(d, name.end).unwrap();
        let strings = read_index(d, top.end).unwrap();
        let g = read_index(d, strings.end).unwrap();
        let (ts, te) = top.items[0];
        let t = parse_dict(&d[ts..te]).unwrap();
        let cs = read_index(d, single_int(&t, CHARSTRINGS).unwrap()).unwrap();
        let p = read_private(d, t.iter().find(|e| e.op == PRIVATE).unwrap()).unwrap();
        let items = |v: &[(usize, usize)]| v.iter().map(|&(s, e)| d[s..e].to_vec()).collect();
        (items(&cs.items), items(&p.subrs.unwrap()), items(&g.items))
    }

    // Type 2 numbers: n in -107..=107 is the byte n + 139.
    const fn n(v: i8) -> u8 {
        (v as i16 + 139) as u8
    }

    #[test]
    fn uncalled_subroutines_are_emptied() {
        // Glyph 0 calls local 1 (bias 107: operand -106), which calls
        // global 0 (operand -107). Local 0 and 2 and global 1 are unused.
        let glyph: &[u8] = &[n(-106), 10, 14];
        let local: [&[u8]; 3] = [
            &[n(1), n(2), 5, 11],
            &[n(-107), 29, 11],
            &[n(3), n(4), 5, 11],
        ];
        let global: [&[u8]; 2] = [&[n(5), n(6), 5, 11], &[n(7), n(8), 5, 11]];
        let d = mini_cff(&[glyph], &local, &global);
        let (out, emptied) = prune(&d).unwrap();
        assert_eq!(emptied, 3);
        let (cs, l, g) = charstrings_and_subrs(&out);
        assert_eq!(cs, vec![glyph.to_vec()]);
        assert_eq!(l, vec![vec![11], local[1].to_vec(), vec![11]]);
        assert_eq!(g, vec![global[0].to_vec(), vec![11]]);
        // Nothing left to empty.
        assert!(prune(&out).is_none());
    }

    #[test]
    fn hint_mask_bytes_are_skipped_inside_subroutines() {
        // Two stems, then a subroutine whose hintmask is followed by one
        // mask byte that reads as callsubr (10) if not skipped.
        let glyph: &[u8] = &[n(0), n(10), n(20), n(10), 1, n(-107), 10, 14];
        let local: [&[u8]; 2] = [&[19, 10, n(1), n(2), 5, 11], &[n(1), n(2), 5, 11]];
        let d = mini_cff(&[glyph], &local, &[]);
        let (out, emptied) = prune(&d).unwrap();
        assert_eq!(emptied, 1);
        let (_, l, _) = charstrings_and_subrs(&out);
        assert_eq!(l, vec![local[0].to_vec(), vec![11]]);
    }

    #[test]
    fn computed_paths_are_left_alone() {
        // `add` (12 10) before a call: the subroutine number is computed.
        let glyph: &[u8] = &[n(-107), n(0), 12, 10, 10, 14];
        let local: [&[u8]; 2] = [&[11], &[n(1), n(2), 5, 11]];
        assert!(prune(&mini_cff(&[glyph], &local, &[])).is_none());
        // A call out of range, and a charstring with no endchar.
        assert!(prune(&mini_cff(&[&[n(50), 10, 14]], &local, &[])).is_none());
        assert!(prune(&mini_cff(&[&[n(1), n(2), 5]], &local, &[])).is_none());
    }

    /// Prune every `*.cff` in CFF_PRUNE_IN into CFF_PRUNE_OUT (an outside
    /// oracle then compares the outlines):
    ///
    ///   CFF_PRUNE_IN=dir CFF_PRUNE_OUT=dir \
    ///     cargo test --release cff_prune_files -- --ignored --nocapture
    #[test]
    #[ignore]
    fn cff_prune_files() {
        let (Ok(input), Ok(output)) = (
            std::env::var("CFF_PRUNE_IN"),
            std::env::var("CFF_PRUNE_OUT"),
        ) else {
            return;
        };
        let mut names: Vec<_> = std::fs::read_dir(&input)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "cff"))
            .collect();
        names.sort();
        for path in names {
            let data = std::fs::read(&path).unwrap();
            let name = path.file_name().unwrap();
            match prune(&data) {
                Some((out, emptied)) => {
                    println!(
                        "{}: {} -> {} ({} emptied)",
                        name.display(),
                        data.len(),
                        out.len(),
                        emptied
                    );
                    std::fs::write(std::path::Path::new(&output).join(name), out).unwrap();
                }
                None => println!("{}: untouched", name.display()),
            }
        }
    }
}
