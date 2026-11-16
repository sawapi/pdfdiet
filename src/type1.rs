//! Type 1 -> CFF (Type1C) font conversion.
//!
//! A Type 1 program spends bytes on eexec/charstring encryption, PostScript
//! boilerplate, per-glyph procedures in an inefficient charstring dialect,
//! and 512 bytes of trailing zeros; CFF stores the same outlines, hints and
//! glyph names in packed binary. The conversion is written from the Adobe
//! Type 1 Font Format and CFF/Type 2 Charstring specifications only.
//!
//! The glyph names, encoding, metrics and hint parameters are all carried
//! over, so nothing at the PDF level needs rewriting beyond swapping
//! /FontFile for /FontFile3 (Subtype /Type1C). Charstrings are interpreted
//! (subroutines inlined, the flex and hint-replacement OtherSubrs protocols
//! resolved) and re-emitted with the same operator forms and values; the one
//! deliberate approximation is that values produced by `div` are emitted as
//! 16.16 fixed-point (error <= 2^-17 em). Anything outside the modelled
//! feature set — seac, sbw, unknown OtherSubrs, malformed structure — makes
//! the conversion return None and the original font is kept.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::{MAX_INFLATE, dict_int, filter_names, inflate};

// ---------------------------------------------------------------------------
// Type 1 parsing.

/// Type 1 eexec / charstring decryption.
fn decrypt(data: &[u8], key: u16, skip: usize) -> Vec<u8> {
    let mut r = key;
    let mut out = Vec::with_capacity(data.len().saturating_sub(skip));
    for (i, &c) in data.iter().enumerate() {
        let p = c ^ (r >> 8) as u8;
        r = (c as u16).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
        if i >= skip {
            out.push(p);
        }
    }
    out
}

struct Type1Font {
    font_name: Vec<u8>,
    /// None when it is the default [0.001 0 0 0.001 0 0].
    font_matrix: Option<[f64; 6]>,
    font_bbox: Option<[f64; 4]>,
    italic_angle: Option<f64>,
    is_fixed_pitch: Option<bool>,
    underline_position: Option<f64>,
    underline_thickness: Option<f64>,
    /// None = StandardEncoding; Some = the `dup code /name put` entries.
    encoding: Option<Vec<(u8, Vec<u8>)>>,
    private: PrivateInfo,
    subrs: Vec<Vec<u8>>,
    /// (name, decrypted charstring) in source order.
    charstrings: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Default)]
struct PrivateInfo {
    blue_values: Option<Vec<f64>>,
    other_blues: Option<Vec<f64>>,
    family_blues: Option<Vec<f64>>,
    family_other_blues: Option<Vec<f64>>,
    blue_scale: Option<f64>,
    blue_shift: Option<f64>,
    blue_fuzz: Option<f64>,
    std_hw: Option<f64>,
    std_vw: Option<f64>,
    stem_snap_h: Option<Vec<f64>>,
    stem_snap_v: Option<Vec<f64>>,
    force_bold: Option<bool>,
    language_group: Option<f64>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn skip_ws(d: &[u8], mut i: usize) -> usize {
    while i < d.len() && (d[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Parse a number token at `i`; returns (value, next index).
fn parse_number(d: &[u8], i: usize) -> Option<(f64, usize)> {
    let i = skip_ws(d, i);
    let mut j = i;
    while j < d.len() && matches!(d[j], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
        j += 1;
    }
    if j == i {
        return None;
    }
    let v: f64 = std::str::from_utf8(&d[i..j]).ok()?.parse().ok()?;
    Some((v, j))
}

/// Parse `[..]` or `{..}` (or a bare number as a 1-element array) after `key`.
fn parse_number_array(d: &[u8], key: &[u8]) -> Option<Vec<f64>> {
    let mut i = find(d, key)? + key.len();
    i = skip_ws(d, i);
    let close = match d.get(i) {
        Some(b'[') => b']',
        Some(b'{') => b'}',
        _ => {
            let (v, _) = parse_number(d, i)?;
            return Some(vec![v]);
        }
    };
    i += 1;
    let mut out = Vec::new();
    loop {
        i = skip_ws(d, i);
        if d.get(i) == Some(&close) {
            return Some(out);
        }
        let (v, j) = parse_number(d, i)?;
        out.push(v);
        i = j;
    }
}

fn parse_bool(d: &[u8], key: &[u8]) -> Option<bool> {
    let i = find(d, key)? + key.len();
    let i = skip_ws(d, i);
    if d[i..].starts_with(b"true") {
        Some(true)
    } else if d[i..].starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

fn parse_scalar(d: &[u8], key: &[u8]) -> Option<f64> {
    parse_number_array(d, key).filter(|a| a.len() == 1).map(|a| a[0])
}

/// Parse a PostScript name token starting at the '/'.
fn parse_name(d: &[u8], i: usize) -> Option<(Vec<u8>, usize)> {
    let i = skip_ws(d, i);
    if d.get(i) != Some(&b'/') {
        return None;
    }
    let mut j = i + 1;
    while j < d.len()
        && !(d[j] as char).is_ascii_whitespace()
        && !matches!(d[j], b'/' | b'{' | b'}' | b'(' | b')' | b'[' | b']' | b'<' | b'>' | b'%')
    {
        j += 1;
    }
    (j > i + 1).then(|| (d[i + 1..j].to_vec(), j))
}

/// Binary entries of the form `<int> <int> RD <bytes> ..`, used by both
/// /Subrs (`dup idx len RD`) and /CharStrings (`/name len RD`). The reader
/// procedure name is font-defined (`RD`, `-|`, ...); the binary starts after
/// exactly one space following it.
fn parse_binary_entry(d: &[u8], mut i: usize, len: usize) -> Option<(&[u8], usize)> {
    // Skip the reader token.
    i = skip_ws(d, i);
    while i < d.len() && !(d[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    i += 1; // the single delimiter space
    let bytes = d.get(i..i + len)?;
    Some((bytes, i + len))
}

fn parse_type1(data: &[u8]) -> Option<Type1Font> {
    // Strip PFB segment headers if present.
    let data = if data.first() == Some(&0x80) {
        return None; // PFB container: not produced inside PDFs
    } else {
        data
    };
    let eexec_at = find(data, b"eexec")?;
    let clear = &data[..eexec_at];
    let mut enc_start = eexec_at + 5;
    // Exactly the whitespace after `eexec` is skipped; the ciphertext may
    // itself begin with whitespace-looking bytes.
    while matches!(data.get(enc_start), Some(b' ' | b'\r' | b'\n' | b'\t')) {
        enc_start += 1;
    }
    let enc = &data[enc_start..];
    // Hex (PFA-style) if the first 4 bytes are hex digits.
    let is_hex = enc.len() >= 4 && enc[..4].iter().all(u8::is_ascii_hexdigit);
    let plain = if is_hex {
        let hex: Vec<u8> = enc.iter().copied().filter(u8::is_ascii_hexdigit).collect();
        let bytes: Option<Vec<u8>> = hex
            .chunks(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
            .collect();
        decrypt(&bytes?, 55665, 4)
    } else {
        decrypt(enc, 55665, 4)
    };

    let font_name = {
        let i = find(clear, b"/FontName")? + 9;
        parse_name(clear, skip_ws(clear, i))?.0
    };
    let font_matrix = parse_number_array(clear, b"/FontMatrix").and_then(|m| {
        let m: [f64; 6] = m.try_into().ok()?;
        (m != [0.001, 0.0, 0.0, 0.001, 0.0, 0.0]).then_some(m)
    });
    let font_bbox = parse_number_array(clear, b"/FontBBox")
        .and_then(|b| <[f64; 4]>::try_from(b).ok());

    // Encoding: StandardEncoding, or `dup <code> /<name> put` entries.
    let encoding = {
        let i = find(clear, b"/Encoding")? + 9;
        let i = skip_ws(clear, i);
        if clear[i..].starts_with(b"StandardEncoding") {
            None
        } else {
            let mut entries = Vec::new();
            let mut pos = i;
            while let Some(off) = find(&clear[pos..], b"dup ") {
                let mut j = pos + off + 4;
                let Some((code, j2)) = parse_number(clear, j) else {
                    pos = pos + off + 4;
                    continue;
                };
                j = j2;
                let Some((name, j3)) = parse_name(clear, j) else {
                    pos = j;
                    continue;
                };
                let j4 = skip_ws(clear, j3);
                if !clear[j4..].starts_with(b"put") {
                    return None;
                }
                if !(0.0..=255.0).contains(&code) || code.fract() != 0.0 {
                    return None;
                }
                entries.push((code as u8, name));
                pos = j4 + 3;
            }
            Some(entries)
        }
    };

    let italic_angle = parse_scalar(clear, b"/ItalicAngle");
    let is_fixed_pitch = parse_bool(clear, b"/isFixedPitch");
    let underline_position = parse_scalar(clear, b"/UnderlinePosition");
    let underline_thickness = parse_scalar(clear, b"/UnderlineThickness");

    if find(clear, b"/PaintType").is_some() && parse_scalar(clear, b"/PaintType") != Some(0.0) {
        return None;
    }

    let len_iv = parse_scalar(&plain, b"/lenIV").unwrap_or(4.0);
    if len_iv.fract() != 0.0 || !(0.0..=16.0).contains(&len_iv) {
        return None;
    }
    let len_iv = len_iv as usize;

    let private = PrivateInfo {
        blue_values: parse_number_array(&plain, b"/BlueValues"),
        other_blues: parse_number_array(&plain, b"/OtherBlues"),
        family_blues: parse_number_array(&plain, b"/FamilyBlues"),
        family_other_blues: parse_number_array(&plain, b"/FamilyOtherBlues"),
        blue_scale: parse_scalar(&plain, b"/BlueScale"),
        blue_shift: parse_scalar(&plain, b"/BlueShift"),
        blue_fuzz: parse_scalar(&plain, b"/BlueFuzz"),
        std_hw: parse_number_array(&plain, b"/StdHW").filter(|a| a.len() == 1).map(|a| a[0]),
        std_vw: parse_number_array(&plain, b"/StdVW").filter(|a| a.len() == 1).map(|a| a[0]),
        stem_snap_h: parse_number_array(&plain, b"/StemSnapH"),
        stem_snap_v: parse_number_array(&plain, b"/StemSnapV"),
        force_bold: parse_bool(&plain, b"/ForceBold"),
        language_group: parse_scalar(&plain, b"/LanguageGroup"),
    };

    // Subrs.
    let mut subrs: Vec<Vec<u8>> = Vec::new();
    if let Some(s) = find(&plain, b"/Subrs") {
        let (count, mut pos) = parse_number(&plain, s + 6)?;
        // A subroutine entry costs at least ~10 bytes ("dup N L RD x NP");
        // a count that cannot fit in the remaining data is hostile input,
        // and must not size an allocation.
        if count.fract() != 0.0 || count < 0.0 || count as usize > plain.len() / 8 {
            return None;
        }
        let count = count as usize;
        subrs = vec![Vec::new(); count];
        for _ in 0..count {
            let off = find(&plain[pos..], b"dup ")?;
            let (idx, j) = parse_number(&plain, pos + off + 4)?;
            let (len, j) = parse_number(&plain, j)?;
            let (bytes, next) = parse_binary_entry(&plain, j, len as usize)?;
            *subrs.get_mut(idx as usize)? = decrypt(bytes, 4330, len_iv);
            pos = next;
        }
    }

    // CharStrings.
    let cs_at = find(&plain, b"/CharStrings")?;
    let (declared, mut pos) = parse_number(&plain, cs_at + 12)?;
    if declared.fract() != 0.0 || declared < 1.0 || declared as usize > plain.len() / 8 {
        return None;
    }
    let mut charstrings = Vec::new();
    // Skip past the `dict dup begin` boilerplate to the first glyph.
    while charstrings.len() < declared as usize {
        pos = skip_ws(&plain, pos);
        if plain.get(pos) != Some(&b'/') {
            pos += 1;
            if pos >= plain.len() {
                return None;
            }
            continue;
        }
        let (name, j) = parse_name(&plain, pos)?;
        let (len, j) = parse_number(&plain, j)?;
        let (bytes, next) = parse_binary_entry(&plain, j, len as usize)?;
        charstrings.push((name, decrypt(bytes, 4330, len_iv)));
        pos = next;
    }

    Some(Type1Font {
        font_name,
        font_matrix,
        font_bbox,
        italic_angle,
        is_fixed_pitch,
        underline_position,
        underline_thickness,
        encoding,
        private,
        subrs,
        charstrings,
    })
}

// ---------------------------------------------------------------------------
// Type 1 charstring interpretation.

/// One resolved glyph: advance width plus an operator sequence where flex and
/// hint replacement have been normalized into explicit ops.
struct Glyph {
    width: f64,
    ops: Vec<GOp>,
}

enum GOp {
    /// (horizontal?, position, extent) — positions already absolute (the
    /// Type 1 relative-to-sidebearing adjustment applied), extent may be
    /// negative for edge hints.
    Stem(bool, f64, f64),
    /// Marks a hint-replacement boundary; following Stems form a new group.
    NewGroup,
    /// hstem3/vstem3 counter constraint over the previous three Stems.
    Counter3,
    Move([f64; 2]),
    HMove(f64),
    VMove(f64),
    Line([f64; 2]),
    HLine(f64),
    VLine(f64),
    Curve([f64; 6]),
    VHCurve([f64; 4]),
    HVCurve([f64; 4]),
    Flex([f64; 12], f64),
}

struct Interp<'a> {
    subrs: &'a [Vec<u8>],
    stack: Vec<f64>,
    ps_stack: Vec<f64>,
    sbx: f64,
    width: Option<f64>,
    /// Deltas collected while inside the flex protocol.
    flex: Option<Vec<f64>>,
    moved: bool,
    path_started: bool,
    pending_group: bool,
    ops: Vec<GOp>,
    done: bool,
    /// Charstring bytes this font may still interpret. Subrs calling subrs
    /// fan out exponentially within the depth cap, so the total is bounded.
    steps: u64,
}

impl<'a> Interp<'a> {
    fn take(&mut self, n: usize) -> Option<Vec<f64>> {
        if self.stack.len() != n {
            return None;
        }
        Some(std::mem::take(&mut self.stack))
    }

    fn first_move_adjust(&mut self, dx: f64, dy: f64) -> GOp {
        // hsbw leaves the current point at (sbx, 0); Type 2 starts at (0, 0),
        // so the sidebearing folds into the first moveto.
        let sbx = if self.moved { 0.0 } else { self.sbx };
        self.moved = true;
        self.path_started = true;
        if sbx == 0.0 {
            if dx == 0.0 && dy != 0.0 {
                return GOp::VMove(dy);
            }
            if dy == 0.0 {
                return GOp::HMove(dx);
            }
        }
        GOp::Move([dx + sbx, dy])
    }

    fn stem(&mut self, horiz: bool, a: f64, d: f64) -> Option<()> {
        if self.path_started && self.pending_group {
            self.ops.push(GOp::NewGroup);
            self.pending_group = false;
        }
        let pos = if horiz { a } else { a + self.sbx };
        self.ops.push(GOp::Stem(horiz, pos, d));
        Some(())
    }

    fn exec(&mut self, cs: &[u8], depth: u8) -> Option<()> {
        if depth > 10 {
            return None;
        }
        let mut i = 0;
        while i < cs.len() {
            if self.done {
                return Some(());
            }
            self.steps = self.steps.checked_sub(1)?;
            let b = cs[i];
            match b {
                32..=246 => {
                    self.stack.push((b as i32 - 139) as f64);
                    i += 1;
                }
                247..=250 => {
                    let w = *cs.get(i + 1)? as i32;
                    self.stack.push(((b as i32 - 247) * 256 + w + 108) as f64);
                    i += 2;
                }
                251..=254 => {
                    let w = *cs.get(i + 1)? as i32;
                    self.stack.push((-(b as i32 - 251) * 256 - w - 108) as f64);
                    i += 2;
                }
                255 => {
                    let v = i32::from_be_bytes(cs.get(i + 1..i + 5)?.try_into().ok()?);
                    self.stack.push(v as f64);
                    i += 5;
                }
                13 => {
                    // hsbw: sbx wx
                    let a = self.take(2)?;
                    self.sbx = a[0];
                    self.width = Some(a[1]);
                    i += 1;
                }
                1 | 3 => {
                    let a = self.take(2)?;
                    self.stem(b == 1, a[0], a[1])?;
                    i += 1;
                }
                4 | 21 | 22 => {
                    // vmoveto / rmoveto / hmoveto
                    let (dx, dy) = match b {
                        4 => (0.0, self.take(1)?[0]),
                        22 => (self.take(1)?[0], 0.0),
                        _ => {
                            let a = self.take(2)?;
                            (a[0], a[1])
                        }
                    };
                    if let Some(pts) = &mut self.flex {
                        if b != 21 {
                            return None; // flex points must be rmoveto
                        }
                        pts.push(dx);
                        pts.push(dy);
                    } else {
                        let op = self.first_move_adjust(dx, dy);
                        self.ops.push(op);
                    }
                    i += 1;
                }
                5 => {
                    let a = self.take(2)?;
                    self.ops.push(GOp::Line([a[0], a[1]]));
                    i += 1;
                }
                6 | 7 => {
                    let a = self.take(1)?;
                    self.ops.push(if b == 6 { GOp::HLine(a[0]) } else { GOp::VLine(a[0]) });
                    i += 1;
                }
                8 => {
                    let a = self.take(6)?;
                    self.ops.push(GOp::Curve(a.try_into().ok()?));
                    i += 1;
                }
                30 | 31 => {
                    let a = self.take(4)?;
                    let a: [f64; 4] = a.try_into().ok()?;
                    self.ops.push(if b == 30 { GOp::VHCurve(a) } else { GOp::HVCurve(a) });
                    i += 1;
                }
                9 => {
                    // closepath: Type 2 closes subpaths implicitly; the
                    // current point is unchanged, so it simply disappears.
                    self.take(0)?;
                    i += 1;
                }
                10 => {
                    // callsubr
                    let idx = self.stack.pop()?;
                    if idx.fract() != 0.0 || idx < 0.0 {
                        return None;
                    }
                    let subrs: &'a [Vec<u8>] = self.subrs;
                    self.exec(subrs.get(idx as usize)?, depth + 1)?;
                    i += 1;
                }
                11 => return Some(()),
                14 => {
                    self.take(0)?;
                    self.done = true;
                    return Some(());
                }
                12 => {
                    let b2 = *cs.get(i + 1)?;
                    match b2 {
                        12 => {
                            // div
                            let den = self.stack.pop()?;
                            let num = self.stack.pop()?;
                            if den == 0.0 {
                                return None;
                            }
                            self.stack.push(num / den);
                        }
                        1 | 2 => {
                            // vstem3 / hstem3: three stems plus a counter
                            // constraint (Type 2: stems + cntrmask).
                            let a = self.take(6)?;
                            let horiz = b2 == 2;
                            for k in 0..3 {
                                self.stem(horiz, a[k * 2], a[k * 2 + 1])?;
                            }
                            self.ops.push(GOp::Counter3);
                        }
                        16 => {
                            // callothersubr: arg1..argn n othersubr#
                            let idx = self.stack.pop()?;
                            let n = self.stack.pop()?;
                            if n.fract() != 0.0 || n < 0.0 || self.stack.len() < n as usize {
                                return None;
                            }
                            let args: Vec<f64> =
                                self.stack.split_off(self.stack.len() - n as usize);
                            match idx as i64 {
                                1 => {
                                    // begin flex
                                    if !args.is_empty() || self.flex.is_some() {
                                        return None;
                                    }
                                    self.flex = Some(Vec::new());
                                }
                                2 => {} // flex point (already collected by rmoveto)
                                0 => {
                                    // end flex: args = (fd, endx, endy)
                                    if args.len() != 3 {
                                        return None;
                                    }
                                    let d = self.flex.take()?;
                                    if d.len() != 14 {
                                        return None; // 7 collected points
                                    }
                                    // The first collected point is the flex
                                    // reference point; fold it into the first
                                    // control point's delta.
                                    let f = [
                                        d[0] + d[2],
                                        d[1] + d[3],
                                        d[4],
                                        d[5],
                                        d[6],
                                        d[7],
                                        d[8],
                                        d[9],
                                        d[10],
                                        d[11],
                                        d[12],
                                        d[13],
                                    ];
                                    self.ops.push(GOp::Flex(f, args[0]));
                                    // The two following `pop`s retrieve the
                                    // endpoint for setcurrentpoint.
                                    self.ps_stack.push(args[2]);
                                    self.ps_stack.push(args[1]);
                                }
                                3 => {
                                    // hint replacement: the following pop +
                                    // callsubr executes the replacement subr.
                                    if args.len() != 1 {
                                        return None;
                                    }
                                    self.pending_group = true;
                                    self.ps_stack.push(args[0]);
                                }
                                _ => return None,
                            }
                        }
                        17 => {
                            // pop: transfer from the PostScript stack
                            let v = self.ps_stack.pop()?;
                            self.stack.push(v);
                        }
                        33 => {
                            // setcurrentpoint: only meaningful after flex;
                            // our current point is already correct.
                            self.take(2)?;
                        }
                        _ => return None, // seac / sbw / dotsection / unknown
                    }
                    i += 2;
                }
                _ => return None,
            }
        }
        Some(())
    }
}

/// Charstring bytes one font may interpret in total (see Interp::steps).
const MAX_FONT_STEPS: u64 = 4_000_000;

fn interpret(cs: &[u8], subrs: &[Vec<u8>], steps: &mut u64) -> Option<Glyph> {
    let mut it = Interp {
        subrs,
        stack: Vec::new(),
        ps_stack: Vec::new(),
        sbx: 0.0,
        width: None,
        flex: None,
        moved: false,
        path_started: false,
        pending_group: false,
        ops: Vec::new(),
        done: false,
        steps: *steps,
    };
    let r = it.exec(cs, 0);
    *steps = it.steps;
    r?;
    if !it.done || it.flex.is_some() {
        return None;
    }
    Some(Glyph {
        width: it.width?,
        ops: it.ops,
    })
}

// ---------------------------------------------------------------------------
// Type 2 charstring emission.

fn push_num(out: &mut Vec<u8>, v: f64) -> Option<()> {
    if v.fract() == 0.0 && (-32768.0..=32767.0).contains(&v) {
        let n = v as i32;
        match n {
            -107..=107 => out.push((n + 139) as u8),
            108..=1131 => {
                let n = n - 108;
                out.push((n / 256 + 247) as u8);
                out.push((n % 256) as u8);
            }
            -1131..=-108 => {
                let n = -n - 108;
                out.push((n / 256 + 251) as u8);
                out.push((n % 256) as u8);
            }
            _ => {
                out.push(28);
                out.extend_from_slice(&(n as i16).to_be_bytes());
            }
        }
        Some(())
    } else if v.abs() < 32768.0 {
        // 16.16 fixed — the only place the conversion rounds. Reached only
        // by `div` results (integers take the exact branch above); the error
        // bound is 2^-17 em ≈ 0.0000076 em, orders of magnitude below any
        // rasterization quantum.
        out.push(255);
        out.extend_from_slice(&(((v * 65536.0).round()) as i32).to_be_bytes());
        Some(())
    } else {
        None
    }
}

/// A stem as (position, extent), compared bit-exactly for dedup.
type StemKey = (u64, u64);
fn stem_key(pos: f64, ext: f64) -> StemKey {
    (pos.to_bits(), ext.to_bits())
}

fn emit_type2(glyph: &Glyph, default_width: f64, nominal_width: f64) -> Option<Vec<u8>> {
    // Collect stems: unique sorted lists per direction, group memberships,
    // counter bits.
    let mut h: Vec<(f64, f64)> = Vec::new();
    let mut v: Vec<(f64, f64)> = Vec::new();
    let mut groups: Vec<Vec<StemKey>> = vec![Vec::new()];
    let mut counters: Vec<StemKey> = Vec::new();
    for op in &glyph.ops {
        match op {
            GOp::Stem(horiz, pos, ext) => {
                let list = if *horiz { &mut h } else { &mut v };
                if !list.iter().any(|&(p, e)| stem_key(p, e) == stem_key(*pos, *ext)) {
                    list.push((*pos, *ext));
                }
                groups.last_mut().unwrap().push(stem_key(*pos, *ext));
            }
            GOp::NewGroup => groups.push(Vec::new()),
            GOp::Counter3 => {
                let last = groups.last().unwrap();
                if last.len() < 3 {
                    return None;
                }
                counters.extend_from_slice(&last[last.len() - 3..]);
            }
            _ => {}
        }
    }
    if !v.iter().chain(&h).all(|s| s.0.is_finite() && s.1.is_finite()) {
        return None;
    }
    h.sort_by(|a, b| a.partial_cmp(b).unwrap()); // finite by the check above
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let nstems = h.len() + v.len();
    if nstems > 96 {
        return None; // Type 2 hint limit
    }
    // Stem index for mask construction: h first, then v.
    let index_of = |horiz: bool, key: StemKey| -> Option<usize> {
        let (list, base) = if horiz { (&h, 0) } else { (&v, h.len()) };
        list.iter()
            .position(|&(p, e)| stem_key(p, e) == key)
            .map(|i| i + base)
    };
    let masked = groups.len() > 1;

    let mut out = Vec::new();
    let mut width_pending = if glyph.width.to_bits() != default_width.to_bits() {
        Some(glyph.width - nominal_width)
    } else {
        None
    };
    let first_arg = |out: &mut Vec<u8>, w: &mut Option<f64>| -> Option<()> {
        if let Some(width) = w.take() {
            push_num(out, width)?;
        }
        Some(())
    };

    // Stem declarations (delta-encoded, sorted).
    let emit_stems = |out: &mut Vec<u8>,
                          list: &[(f64, f64)],
                          op: u8,
                          w: &mut Option<f64>|
     -> Option<()> {
        if list.is_empty() {
            return Some(());
        }
        first_arg(out, w)?;
        let mut prev = 0.0;
        for &(pos, ext) in list {
            push_num(out, pos - prev)?;
            push_num(out, ext)?;
            prev = pos + ext;
        }
        out.push(op);
        Some(())
    };
    // hstem(hm) then vstem(hm); *hm forms whenever masks follow.
    let use_masks = masked || !counters.is_empty();
    emit_stems(&mut out, &h, if use_masks { 18 } else { 1 }, &mut width_pending)?;
    emit_stems(&mut out, &v, if use_masks { 23 } else { 3 }, &mut width_pending)?;

    let mask_bytes = nstems.div_ceil(8).max(1);
    let push_mask = |out: &mut Vec<u8>, op: u8, bits: &[usize]| {
        out.push(op);
        let mut mask = vec![0u8; mask_bytes];
        for &b in bits {
            mask[b / 8] |= 0x80 >> (b % 8);
        }
        out.extend_from_slice(&mask);
    };
    if !counters.is_empty() {
        let bits: Option<Vec<usize>> = counters
            .iter()
            .map(|&k| index_of(true, k).or_else(|| index_of(false, k)))
            .collect();
        push_mask(&mut out, 20, &bits?); // cntrmask
    }
    let group_bits = |g: &[StemKey]| -> Option<Vec<usize>> {
        g.iter()
            .map(|&k| index_of(true, k).or_else(|| index_of(false, k)))
            .collect()
    };
    let mut group_iter = groups.iter();
    if masked {
        push_mask(&mut out, 19, &group_bits(group_iter.next()?)?); // initial hintmask
    } else {
        group_iter.next();
    }

    for op in &glyph.ops {
        match op {
            GOp::Stem(..) | GOp::Counter3 => {}
            GOp::NewGroup => {
                if masked {
                    push_mask(&mut out, 19, &group_bits(group_iter.next()?)?);
                }
            }
            GOp::Move(a) => {
                first_arg(&mut out, &mut width_pending)?;
                push_num(&mut out, a[0])?;
                push_num(&mut out, a[1])?;
                out.push(21);
            }
            GOp::HMove(dx) => {
                first_arg(&mut out, &mut width_pending)?;
                push_num(&mut out, *dx)?;
                out.push(22);
            }
            GOp::VMove(dy) => {
                first_arg(&mut out, &mut width_pending)?;
                push_num(&mut out, *dy)?;
                out.push(4);
            }
            GOp::Line(a) => {
                for x in a {
                    push_num(&mut out, *x)?;
                }
                out.push(5);
            }
            GOp::HLine(d) => {
                push_num(&mut out, *d)?;
                out.push(6);
            }
            GOp::VLine(d) => {
                push_num(&mut out, *d)?;
                out.push(7);
            }
            GOp::Curve(a) => {
                for x in a {
                    push_num(&mut out, *x)?;
                }
                out.push(8);
            }
            GOp::VHCurve(a) => {
                for x in a {
                    push_num(&mut out, *x)?;
                }
                out.push(30);
            }
            GOp::HVCurve(a) => {
                for x in a {
                    push_num(&mut out, *x)?;
                }
                out.push(31);
            }
            GOp::Flex(a, fd) => {
                for x in a {
                    push_num(&mut out, *x)?;
                }
                push_num(&mut out, *fd)?;
                out.push(12);
                out.push(35);
            }
        }
    }
    first_arg(&mut out, &mut width_pending)?; // width even for empty glyphs
    out.push(14); // endchar
    Some(out)
}

// ---------------------------------------------------------------------------
// CFF assembly.

fn cff_index(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(items.len() as u16).to_be_bytes());
    if items.is_empty() {
        return out;
    }
    let total: usize = items.iter().map(Vec::len).sum();
    let off_size: u8 = match total + 1 {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFF_FFFF => 3,
        _ => 4,
    };
    out.push(off_size);
    let mut off = 1usize;
    for i in 0..=items.len() {
        out.extend_from_slice(&(off as u32).to_be_bytes()[4 - off_size as usize..]);
        if i < items.len() {
            off += items[i].len();
        }
    }
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

fn dict_number(out: &mut Vec<u8>, v: f64) {
    if v.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&v) {
        let n = v as i64;
        match n {
            -107..=107 => out.push((n + 139) as u8),
            108..=1131 => {
                let n = n - 108;
                out.push((n / 256 + 247) as u8);
                out.push((n % 256) as u8);
            }
            -1131..=-108 => {
                let n = -n - 108;
                out.push((n / 256 + 251) as u8);
                out.push((n % 256) as u8);
            }
            -32768..=32767 => {
                out.push(28);
                out.extend_from_slice(&(n as i16).to_be_bytes());
            }
            _ => {
                out.push(29);
                out.extend_from_slice(&(n as i32).to_be_bytes());
            }
        }
    } else {
        // Real: nibble-encoded shortest decimal representation, which parses
        // back to the identical f64.
        let s = format!("{v}");
        let mut nibbles: Vec<u8> = Vec::new();
        let mut chars = s.bytes().peekable();
        while let Some(c) = chars.next() {
            match c {
                b'0'..=b'9' => nibbles.push(c - b'0'),
                b'.' => nibbles.push(0xa),
                b'-' => nibbles.push(0xe),
                b'e' => {
                    if chars.peek() == Some(&b'-') {
                        chars.next();
                        nibbles.push(0xc);
                    } else {
                        nibbles.push(0xb);
                    }
                }
                _ => {}
            }
        }
        nibbles.push(0xf);
        if nibbles.len() % 2 == 1 {
            nibbles.push(0xf);
        }
        out.push(30);
        for pair in nibbles.chunks(2) {
            out.push(pair[0] << 4 | pair[1]);
        }
    }
}

/// A 32-bit-encoded integer whose byte length never changes: used for the
/// offsets patched in a second pass.
fn dict_int32(out: &mut Vec<u8>, v: i32) {
    out.push(29);
    out.extend_from_slice(&v.to_be_bytes());
}

fn dict_op(out: &mut Vec<u8>, op: u16) {
    if op > 0xFF {
        out.push(12);
        out.push((op & 0xFF) as u8);
    } else {
        out.push(op as u8);
    }
}

fn dict_entry(out: &mut Vec<u8>, op: u16, vals: &[f64]) {
    for &v in vals {
        dict_number(out, v);
    }
    dict_op(out, op);
}

fn dict_delta(out: &mut Vec<u8>, op: u16, vals: &[f64]) {
    let mut prev = 0.0;
    for &v in vals {
        dict_number(out, v - prev);
        prev = v;
    }
    dict_op(out, op);
}

/// Convert a Type 1 font program to a bare CFF (Type1C) font.
pub(crate) fn type1_to_cff(data: &[u8]) -> Option<Vec<u8>> {
    let font = parse_type1(data)?;
    if font.charstrings.is_empty() {
        return None;
    }

    // Interpret every glyph.
    let mut glyphs: Vec<(Vec<u8>, Glyph)> = Vec::new();
    let mut steps = MAX_FONT_STEPS;
    for (name, cs) in &font.charstrings {
        glyphs.push((name.clone(), interpret(cs, &font.subrs, &mut steps)?));
    }

    // Glyph order: .notdef first; when a custom encoding is present, encoded
    // glyphs next in code order (CFF Encoding format 0 lists one code per
    // glyph), unencoded glyphs last.
    let notdef = glyphs.iter().position(|(n, _)| n == b".notdef")?;
    let mut order: Vec<usize> = vec![notdef];
    let mut codes_of: HashMap<&[u8], Vec<u8>> = HashMap::new();
    if let Some(entries) = &font.encoding {
        for (code, name) in entries {
            codes_of.entry(name).or_default().push(*code);
        }
        let mut encoded: Vec<(u8, usize)> = Vec::new();
        for (i, (name, _)) in glyphs.iter().enumerate() {
            if i == notdef {
                continue;
            }
            if let Some(codes) = codes_of.get(name.as_slice()) {
                encoded.push((codes[0], i));
            }
        }
        encoded.sort();
        order.extend(encoded.iter().map(|&(_c, i)| i));
    }
    for i in 0..glyphs.len() {
        if !order.contains(&i) {
            order.push(i);
        }
    }

    // Width defaults: most frequent width becomes defaultWidthX. Ties break
    // on the bit pattern so the choice never depends on hash-map order
    // (native and wasm builds must produce identical bytes).
    let mut freq: HashMap<u64, usize> = HashMap::new();
    for (_n, g) in &glyphs {
        *freq.entry(g.width.to_bits()).or_default() += 1;
    }
    let default_width = f64::from_bits(
        freq.iter().max_by_key(|&(&w, &c)| (c, std::cmp::Reverse(w))).map(|(&w, _)| w)?,
    );
    let nominal_width = default_width;

    // Charstrings and charset in final GID order.
    let mut charstrings: Vec<Vec<u8>> = Vec::new();
    let mut strings: Vec<Vec<u8>> = Vec::new(); // custom SIDs from 391
    let mut charset = vec![0u8]; // format 0
    for (gid, &i) in order.iter().enumerate() {
        let (name, glyph) = &glyphs[i];
        charstrings.push(emit_type2(glyph, default_width, nominal_width)?);
        if gid > 0 {
            // All names get custom SIDs; matching against the standard
            // strings would only save bytes, at the cost of a 391-entry
            // table to get exactly right.
            let sid = 391 + strings.len() as u16;
            strings.push(name.clone());
            charset.extend_from_slice(&sid.to_be_bytes());
        }
    }

    // Encoding (custom only; StandardEncoding is the CFF default).
    let mut encoding = Vec::new();
    if let Some(_entries) = &font.encoding {
        let mut codes: Vec<u8> = Vec::new();
        let mut supplements: Vec<(u8, u16)> = Vec::new();
        let mut ncodes = 0usize;
        for (gid, &i) in order.iter().enumerate().skip(1) {
            let name = &glyphs[i].0;
            match codes_of.get(name.as_slice()) {
                Some(cs) if ncodes == gid - 1 => {
                    codes.push(cs[0]);
                    ncodes += 1;
                    // A glyph encoded at several codes needs supplements.
                    let sid = 391 + (gid as u16 - 1);
                    for &extra in &cs[1..] {
                        supplements.push((extra, sid));
                    }
                }
                _ => break, // unencoded tail
            }
        }
        encoding.push(if supplements.is_empty() { 0u8 } else { 0x80 });
        encoding.push(ncodes as u8);
        encoding.extend_from_slice(&codes);
        if !supplements.is_empty() {
            encoding.push(supplements.len() as u8);
            for (code, sid) in supplements {
                encoding.push(code);
                encoding.extend_from_slice(&sid.to_be_bytes());
            }
        }
    }

    // Private DICT.
    let mut private = Vec::new();
    let p = &font.private;
    if let Some(v) = &p.blue_values {
        dict_delta(&mut private, 6, v);
    }
    if let Some(v) = &p.other_blues {
        dict_delta(&mut private, 7, v);
    }
    if let Some(v) = &p.family_blues {
        dict_delta(&mut private, 8, v);
    }
    if let Some(v) = &p.family_other_blues {
        dict_delta(&mut private, 9, v);
    }
    if let Some(v) = p.blue_scale {
        dict_entry(&mut private, 0xC09, &[v]);
    }
    if let Some(v) = p.blue_shift {
        dict_entry(&mut private, 0xC0A, &[v]);
    }
    if let Some(v) = p.blue_fuzz {
        dict_entry(&mut private, 0xC0B, &[v]);
    }
    if let Some(v) = p.std_hw {
        dict_entry(&mut private, 10, &[v]);
    }
    if let Some(v) = p.std_vw {
        dict_entry(&mut private, 11, &[v]);
    }
    if let Some(v) = &p.stem_snap_h {
        dict_delta(&mut private, 0xC0C, v);
    }
    if let Some(v) = &p.stem_snap_v {
        dict_delta(&mut private, 0xC0D, v);
    }
    if p.force_bold == Some(true) {
        dict_entry(&mut private, 0xC0E, &[1.0]);
    }
    if let Some(v) = p.language_group {
        dict_entry(&mut private, 0xC11, &[v]);
    }
    dict_entry(&mut private, 20, &[default_width]);
    dict_entry(&mut private, 21, &[nominal_width]);

    // Top DICT with fixed-width offset slots, patched after layout.
    let build_top = |charset_off: i32, encoding_off: i32, cs_off: i32, priv_off: i32| {
        let mut d = Vec::new();
        if let Some(bb) = &font.font_bbox {
            dict_entry(&mut d, 5, &bb[..]);
        }
        if let Some(v) = font.is_fixed_pitch {
            dict_entry(&mut d, 0xC01, &[if v { 1.0 } else { 0.0 }]);
        }
        if let Some(v) = font.italic_angle
            && v != 0.0 {
                dict_entry(&mut d, 0xC02, &[v]);
            }
        if let Some(v) = font.underline_position {
            dict_entry(&mut d, 0xC03, &[v]);
        }
        if let Some(v) = font.underline_thickness {
            dict_entry(&mut d, 0xC04, &[v]);
        }
        if let Some(m) = &font.font_matrix {
            dict_entry(&mut d, 0xC07, &m[..]);
        }
        dict_int32(&mut d, charset_off);
        dict_op(&mut d, 15);
        if encoding_off != 0 {
            dict_int32(&mut d, encoding_off);
            dict_op(&mut d, 16);
        }
        dict_int32(&mut d, cs_off);
        dict_op(&mut d, 17);
        dict_int32(&mut d, private.len() as i32);
        dict_int32(&mut d, priv_off);
        dict_op(&mut d, 18);
        d
    };

    let header = [1u8, 0, 4, 4]; // major, minor, hdrSize, offSize
    let name_index = cff_index(std::slice::from_ref(&font.font_name));
    let string_index = cff_index(&strings);
    let gsubr_index = cff_index(&[]);
    let cs_index = cff_index(&charstrings);
    let has_encoding = !encoding.is_empty();

    // Two passes: sizes are stable because the offsets use fixed-width ints.
    let mut offsets = (0i32, 0i32, 0i32, 0i32);
    for _ in 0..2 {
        let top = build_top(offsets.0, offsets.1, offsets.2, offsets.3);
        let top_index = cff_index(&[top]);
        let fixed =
            header.len() + name_index.len() + top_index.len() + string_index.len() + gsubr_index.len();
        let charset_off = fixed;
        let encoding_off = charset_off + charset.len();
        let cs_off = encoding_off + encoding.len();
        let priv_off = cs_off + cs_index.len();
        offsets = (
            charset_off as i32,
            if has_encoding { encoding_off as i32 } else { 0 },
            cs_off as i32,
            priv_off as i32,
        );
    }
    let top = build_top(offsets.0, offsets.1, offsets.2, offsets.3);
    let top_index = cff_index(&[top]);

    let mut out = Vec::new();
    out.extend_from_slice(&header);
    out.extend_from_slice(&name_index);
    out.extend_from_slice(&top_index);
    out.extend_from_slice(&string_index);
    out.extend_from_slice(&gsubr_index);
    out.extend_from_slice(&charset);
    out.extend_from_slice(&encoding);
    out.extend_from_slice(&cs_index);
    out.extend_from_slice(&private);
    Some(out)
}

// ---------------------------------------------------------------------------
// PDF integration.

/// Convert every eligible Type 1 program (FontDescriptor /FontFile) to CFF
/// (/FontFile3, Subtype /Type1C). Returns (fonts converted, bytes saved).
/// Do the stream's /Length1, /Length2 and /Length3 describe the program
/// exactly as the converter splits it? Length1 must end where the converter
/// starts decrypting (right after `eexec` and its whitespace), and the three
/// lengths must add up to the data. A viewer may trust these lengths rather
/// than search for `eexec`, so when they disagree the font is ambiguous:
/// converting it could make it render or extract differently from what that
/// viewer showed, and it is left alone.
fn type1_lengths_consistent(dict: &Dictionary, data: &[u8]) -> bool {
    let (Some(l1), Some(l2)) = (dict_int(dict, b"Length1"), dict_int(dict, b"Length2")) else {
        return false;
    };
    let l3 = dict_int(dict, b"Length3").unwrap_or(0);
    if l1 < 0 || l2 < 0 || l3 < 0 {
        return false;
    }
    let Some(eexec_at) = find(data, b"eexec") else { return false };
    let mut enc_start = eexec_at + 5;
    while matches!(data.get(enc_start), Some(b' ' | b'\r' | b'\n' | b'\t')) {
        enc_start += 1;
    }
    l1 as usize == enc_start && (l1 + l2 + l3) as usize == data.len()
}

pub(crate) fn convert_type1_fonts(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    // program -> descriptors referencing it (shared programs update all).
    let mut programs: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else { continue };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"FontDescriptor") {
            continue;
        }
        if let Ok(Object::Reference(fid)) = dict.get(b"FontFile") {
            programs.entry(*fid).or_default().push(id);
        }
    }
    let mut ids: Vec<ObjectId> = programs.keys().copied().collect();
    ids.sort();

    let mut converted = 0usize;
    let mut saved = 0i64;
    for program_id in ids {
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
        let Ok(data) = inflate(stream, MAX_INFLATE) else {
            continue;
        };
        if !type1_lengths_consistent(&stream.dict, &data) {
            log.push(format!(
                "font program {} {}: Type1 left as-is (Length1/2/3 disagree with the program)",
                program_id.0, program_id.1
            ));
            continue;
        }
        let Some(cff) = type1_to_cff(&data) else {
            log.push(format!(
                "font program {} {}: Type1 left as-is (outside converter model)",
                program_id.0, program_id.1
            ));
            continue;
        };

        let mut new_stream = stream.clone();
        new_stream.dict = Dictionary::new();
        new_stream.dict.set("Subtype", Object::Name(b"Type1C".to_vec()));
        new_stream.dict.set("Length", cff.len() as i64);
        new_stream.set_content(cff);
        new_stream.allows_compression = true;
        let _ = new_stream.compress();
        let new_stored = new_stream.content.len();
        if new_stored >= old_stored {
            log.push(format!(
                "keep font program {} {}: Type1C {} >= original {}",
                program_id.0, program_id.1, new_stored, old_stored
            ));
            continue;
        }
        doc.objects.insert(program_id, Object::Stream(new_stream));
        for &desc in &programs[&program_id] {
            if let Some(Object::Dictionary(d)) = doc.objects.get_mut(&desc) {
                d.remove(b"FontFile");
                d.set("FontFile3", Object::Reference(program_id));
            }
        }
        log.push(format!(
            "font program {} {}: Type1 -> CFF, {} -> {} bytes stored",
            program_id.0, program_id.1, old_stored, new_stored
        ));
        saved += old_stored as i64 - new_stored as i64;
        converted += 1;
    }
    (converted, saved)
}

#[cfg(test)]
mod tests {
    #[test]
    fn subrs_fanning_out_exhaust_the_step_budget() {
        // Subr k calls subr k-1 fifty times: 50^9 calls from depth 10 if
        // only the depth were capped. (Real fonts use under 25k steps.)
        let mut subrs = vec![vec![11u8]];
        for k in 1..10u8 {
            let mut s = Vec::new();
            for _ in 0..50 {
                s.extend_from_slice(&[139 + k - 1, 10]);
            }
            s.push(11);
            subrs.push(s);
        }
        let cs = [139, 139 + 100, 13, 139 + 9, 10, 14]; // 0 100 hsbw 9 callsubr endchar
        let t = std::time::Instant::now();
        let mut steps = super::MAX_FONT_STEPS;
        assert!(super::interpret(&cs, &subrs, &mut steps).is_none());
        assert_eq!(steps, 0);
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn type1_lengths_must_match_the_program() {
        let data = b"%!FontType1 clear part currentfile eexec\nCIPHERTEXT0000".to_vec();
        let l1 = data.iter().position(|&b| b == b'C').unwrap() as i64; // after "eexec\n"
        let mut d = Dictionary::new();
        d.set("Length1", l1);
        d.set("Length2", data.len() as i64 - l1);
        d.set("Length3", 0i64);
        assert!(type1_lengths_consistent(&d, &data));
        d.set("Length1", 0i64);
        assert!(!type1_lengths_consistent(&d, &data));
        d.set("Length1", l1);
        let mut longer = data.clone();
        longer.extend_from_slice(b"junk");
        assert!(!type1_lengths_consistent(&d, &longer));
    }

    use super::*;

    /// Inverse of `decrypt`, for building test fonts.
    fn encrypt(plain: &[u8], key: u16, lead: usize) -> Vec<u8> {
        let mut r = key;
        let mut out = Vec::new();
        for &p in std::iter::repeat_n(&0u8, lead).chain(plain.iter()) {
            let c = p ^ (r >> 8) as u8;
            r = (c as u16).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
            out.push(c);
        }
        out
    }

    #[test]
    fn hostile_subrs_count_is_rejected() {
        // A tiny program declaring a gigantic /Subrs count must not allocate:
        // parse_type1 returns None well before sizing anything.
        let mut plain = Vec::new();
        plain.extend_from_slice(b"/Subrs 2000000000 array\n");
        let mut font = Vec::new();
        font.extend_from_slice(b"/FontName /X def /Encoding StandardEncoding def eexec\n");
        font.extend_from_slice(&encrypt(&plain, 55665, 4));
        assert!(parse_type1(&font).is_none());
    }

    #[test]
    fn decrypt_roundtrip() {
        let plain = b"charstring bytes with \x00 and \xff inside";
        let enc = encrypt(plain, 4330, 4);
        assert_eq!(decrypt(&enc, 4330, 4), plain);
    }

    #[test]
    fn type2_number_encodings() {
        let mut out = Vec::new();
        push_num(&mut out, 0.0).unwrap();
        assert_eq!(out, [139]);
        out.clear();
        push_num(&mut out, -107.0).unwrap();
        assert_eq!(out, [32]);
        out.clear();
        push_num(&mut out, 108.0).unwrap();
        assert_eq!(out, [247, 0]);
        out.clear();
        push_num(&mut out, -1131.0).unwrap();
        assert_eq!(out, [254, 255]);
        out.clear();
        push_num(&mut out, 2000.0).unwrap();
        assert_eq!(out, [28, 0x07, 0xD0]);
        out.clear();
        push_num(&mut out, 545.13).unwrap();
        assert_eq!(out[0], 255);
        let v = i32::from_be_bytes(out[1..5].try_into().unwrap());
        assert!((v as f64 / 65536.0 - 545.13).abs() < 1.0 / 65536.0);
        assert!(push_num(&mut out, 40000.0).is_none());
    }

    #[test]
    fn dict_real_nibbles() {
        let mut out = Vec::new();
        dict_number(&mut out, 0.04796);
        // 30, then nibbles 0 . 0 4 7 9 6 f
        assert_eq!(out, [30, 0x0a, 0x04, 0x79, 0x6f]);
    }

    /// Build a miniature Type 1 font with two glyphs and run the converter.
    fn mini_type1() -> Vec<u8> {
        let mut plain = Vec::new();
        plain.extend_from_slice(b"dup /Private 8 dict dup begin\n");
        plain.extend_from_slice(b"/BlueValues[-10 0 700 712]|-\n/StdHW[30]|-\n");
        // Subr 0 draws a line, called from glyph "a".
        let sub = encrypt(&[25 + 139, 5, 11], 4330, 4); // 25 rlineto? needs 2 args
        let _ = sub;
        plain.extend_from_slice(b"/Subrs 1 array\n");
        let sub = encrypt(&[139 + 10, 139 + 20, 5, 11], 4330, 4); // 10 20 rlineto return
        plain.extend_from_slice(format!("dup 0 {} RD ", sub.len()).as_bytes());
        plain.extend_from_slice(&sub);
        plain.extend_from_slice(b" NP\n");
        plain.extend_from_slice(b"2 index /CharStrings 2 dict dup begin\n");
        // .notdef: 0 250 hsbw endchar
        let notdef = encrypt(&[139, 247, 250 - 108, 13, 14], 4330, 4); // 0 250 hsbw endchar
        plain.extend_from_slice(format!("/.notdef {} RD ", notdef.len()).as_bytes());
        plain.extend_from_slice(&notdef);
        plain.extend_from_slice(b" ND\n");
        // a: 20 500 hsbw 100 hstem 10 10 rmoveto <subr0> closepath endchar
        let a: Vec<u8> = vec![
            139 + 20,
            248,
            136, // 500 = (248-247)*256 + 136 + 108
            13, // hsbw
            139,
            139 + 100,
            1, // 0 100 hstem
            139 + 10,
            139 + 10,
            21, // 10 10 rmoveto
            139,
            10, // 0 callsubr
            9,  // closepath
            14, // endchar
        ];
        let a = encrypt(&a, 4330, 4);
        plain.extend_from_slice(format!("/a {} RD ", a.len()).as_bytes());
        plain.extend_from_slice(&a);
        plain.extend_from_slice(b" ND\nend\n");

        let mut font = Vec::new();
        font.extend_from_slice(b"%!PS-AdobeFont-1.0: Mini 001\n");
        font.extend_from_slice(b"/FontName /TESTAA+Mini def\n");
        font.extend_from_slice(b"/FontBBox {0 -10 400 712 }readonly def\n");
        font.extend_from_slice(b"/FontMatrix [0.001 0 0 0.001 0 0 ]readonly def\n");
        font.extend_from_slice(b"/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\n");
        font.extend_from_slice(b"dup 97 /a put\nreadonly def\ncurrentdict end\ncurrentfile eexec\n");
        font.extend_from_slice(&encrypt(&plain, 55665, 4));
        font.extend_from_slice(b"\n0000000000000000000000000000000000000000\n");
        font
    }

    /// Walk a CFF INDEX, returning its items and the offset just past it.
    fn read_index(d: &[u8], at: usize) -> (Vec<Vec<u8>>, usize) {
        let count = u16::from_be_bytes([d[at], d[at + 1]]) as usize;
        if count == 0 {
            return (Vec::new(), at + 2);
        }
        let osz = d[at + 2] as usize;
        let off = |i: usize| -> usize {
            let p = at + 3 + i * osz;
            d[p..p + osz].iter().fold(0usize, |acc, &b| acc << 8 | b as usize)
        };
        let data0 = at + 3 + (count + 1) * osz - 1;
        let items = (0..count)
            .map(|i| d[data0 + off(i)..data0 + off(i + 1)].to_vec())
            .collect();
        (items, data0 + off(count))
    }

    #[test]
    fn mini_font_converts() {
        let t1 = mini_type1();

        // Parser + interpreter: subr inlined, sidebearing folded, width kept.
        let f = parse_type1(&t1).unwrap();
        assert_eq!(f.font_name, b"TESTAA+Mini");
        assert_eq!(f.charstrings.len(), 2);
        let (_n, cs) = f.charstrings.iter().find(|(n, _)| n == b"a").unwrap();
        let glyph = interpret(cs, &f.subrs, &mut MAX_FONT_STEPS.clone()).unwrap();
        assert_eq!(glyph.width, 500.0);
        let moves: Vec<[f64; 2]> = glyph
            .ops
            .iter()
            .filter_map(|op| match op {
                GOp::Move(a) => Some(*a),
                _ => None,
            })
            .collect();
        assert_eq!(moves, vec![[30.0, 10.0]]); // 10 + sbx 20
        assert!(glyph.ops.iter().any(|op| matches!(op, GOp::Line(a) if *a == [10.0, 20.0])));
        assert!(glyph
            .ops
            .iter()
            .any(|op| matches!(op, GOp::Stem(true, p, e) if *p == 0.0 && *e == 100.0)));

        // CFF assembly: header, name, one custom string, 2 charstrings.
        let cff = type1_to_cff(&t1).expect("conversion should succeed");
        assert_eq!(&cff[..4], &[1, 0, 4, 4]);
        let (names, at) = read_index(&cff, 4);
        assert_eq!(names, vec![b"TESTAA+Mini".to_vec()]);
        let (tops, at) = read_index(&cff, at);
        assert_eq!(tops.len(), 1);
        let (strings, _) = read_index(&cff, at);
        assert_eq!(strings, vec![b"a".to_vec()]);
    }
}
