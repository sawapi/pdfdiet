//! Union-merge of TrueType subsets that keep the original glyph numbering.
//!
//! Office-suite exporters embed one subset of a font per page or section.
//! When every subset keeps the full original glyph-id space (all of them
//! declare the same numGlyphs, and only the outlines a subset needs are
//! present), the subsets are slices of one font: the same glyph id holds the
//! same outline wherever it is present. Such subsets can be replaced by a
//! single program holding the union of their outlines, and every font that
//! used one of them can point at the union instead. Each font still finds
//! every glyph it used, byte-identical, at the same id.
//!
//! Merging is only attempted where nothing but the glyph id selects what is
//! drawn and extracted:
//!
//! - every font using a program is either a CIDFontType2 whose CIDToGIDMap
//!   is Identity (or absent, which means Identity) under a Type0 font with a
//!   /ToUnicode CMap, which selects glyphs by id alone, or a simple TrueType
//!   font (some word processors share one program between the two). Simple fonts select
//!   glyphs through the program's cmap, so when they are present the cmap
//!   becomes the union of the subsets' cmaps, which must agree on every code
//!   they both map, and glyph names (post) must agree or be absent;
//! - no descriptor carries a /CIDSet, which would have to describe the
//!   program exactly;
//! - outlines present in several subsets are byte-identical, and every
//!   composite glyph's components are present in the subset providing it;
//! - every table other than the per-glyph ones rebuilt here, the identity
//!   tables carried over from the first subset, and acceleration tables
//!   that are dropped, is byte-identical across the group (hinting
//!   programs, layout tables, vertical metrics, embedded bitmaps, and any
//!   table we do not know).
//!
//! Anything else leaves the programs as they are.

use std::collections::{BTreeSet, HashMap};

use lopdf::{Dictionary, Document, Object, ObjectId};

use std::collections::BTreeMap;

#[cfg(test)]
use crate::subset::CmapIndex;
use crate::subset::{add_composite_closure, assemble_font, be16, be32, find_table, outlined_gids};
use crate::{MAX_INFLATE, MAX_PASS_BYTES, inflate, resolve, resolve_dict};

/// More subsets of one font than any real document carries (the corpus
/// peaks at 8): such a font is left alone rather than compared pairwise.
const MAX_SUBSETS_PER_FONT: usize = 64;
/// Glyph-metric comparisons one union pass may spend partitioning.
const MAX_METRIC_COMPARISONS: u64 = 200_000_000;
/// (code, glyph) pairs one cmap merge may enumerate: every Unicode code
/// point across a few subtables. A table that enumerates more is malformed
/// (overlapping or repeated ranges) and the union is refused.
const MAX_CMAP_ENTRIES: usize = 4_000_000;

/// Rebuilt from the union.
const REBUILT: &[&[u8; 4]] = &[b"glyf", b"loca", b"hmtx", b"hhea", b"maxp", b"head"];
/// Carried over from the group's first program: names and character maps,
/// which nothing consults for an Identity CIDFont with a ToUnicode CMap.
const CARRIED: &[&[u8; 4]] = &[b"cmap", b"name", b"post", b"OS/2", b"PCLT"];
/// Dropped: a digital signature (invalid once the program changes) and
/// per-glyph rasterizer caches, which are optional.
const DROPPED: &[&[u8; 4]] = &[b"DSIG", b"hdmx", b"LTSH", b"VDMX"];

/// Merge groups of compatible subsets. Returns (programs merged away,
/// stored bytes saved).
pub(crate) fn union_cid_truetype_subsets(
    doc: &mut Document,
    log: &mut Vec<String>,
) -> (usize, i64) {
    let Some(users) = program_users(doc) else {
        return (0, 0);
    };

    // Group candidate programs by base font name and glyph count. A name
    // with more subsets than any real document carries is skipped outright
    // (partitioning compares subsets pairwise), and what the pass holds
    // inflated at once is bounded.
    let mut per_name: HashMap<&Vec<u8>, usize> = HashMap::new();
    for (eligible, name, _) in users.values() {
        if *eligible {
            *per_name.entry(name).or_default() += 1;
        }
    }
    let mut groups: HashMap<(Vec<u8>, u16), Vec<(ObjectId, Vec<u8>, usize)>> = HashMap::new();
    let mut ids: Vec<&ObjectId> = users.keys().collect();
    ids.sort();
    let mut held = 0usize;
    for &program in ids {
        let (eligible, name, _) = &users[&program];
        if !eligible || per_name[name] > MAX_SUBSETS_PER_FONT {
            continue;
        }
        let Ok(Object::Stream(s)) = doc.get_object(program) else {
            continue;
        };
        if !matches!(
            crate::filter_names(&s.dict)
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .as_slice(),
            [] | ["FlateDecode"]
        ) {
            continue;
        }
        let Ok(font) = inflate(s, MAX_INFLATE) else {
            continue;
        };
        held += font.len();
        if held > MAX_PASS_BYTES {
            break;
        }
        let Some(count) = find_table(&font, b"maxp").and_then(|(o, _)| be16(&font, o + 4)) else {
            continue;
        };
        groups
            .entry((name.clone(), count))
            .or_default()
            .push((program, font, s.content.len()));
    }

    let mut merged = 0usize;
    let mut saved = 0i64;
    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();
    // Glyph-metric comparisons the whole pass may spend (real documents use
    // a few million; see metric_compatible_subgroups).
    let mut comparisons: u64 = MAX_METRIC_COMPARISONS;
    for key in keys {
        for group in metric_compatible_subgroups(&groups[&key], key.1 as usize, &mut comparisons) {
            let group = &group;
            if group.len() < 2 {
                continue;
            }
            let fonts: Vec<&[u8]> = group.iter().map(|(_, f, _)| f.as_slice()).collect();
            let cmap_used = group.iter().any(|(id, _, _)| users[id].2);
            let mut why = "";
            let Some(union) = build_union_why(&fonts, cmap_used, &mut why) else {
                log.push(format!(
                    "font {}: {} subsets left as they are ({why})",
                    String::from_utf8_lossy(&key.0),
                    group.len()
                ));
                continue;
            };
            let before: usize = group.iter().map(|(_, _, stored)| stored).sum();
            let mut stream = lopdf::Stream::new(Dictionary::new(), union.clone());
            if stream.compress().is_err() {
                continue;
            }
            let after = stream.content.len();
            if after >= before {
                continue;
            }
            let keep = group[0].0;
            let Some(Object::Stream(target)) = doc.objects.get_mut(&keep) else {
                continue;
            };
            target.dict.set("Length1", union.len() as i64);
            target
                .dict
                .set("Filter", Object::Name(b"FlateDecode".to_vec()));
            target.dict.remove(b"DecodeParms");
            target.dict.set("Length", after as i64);
            target.set_content(stream.content);
            target.allows_compression = false;
            let gone: Vec<ObjectId> = group[1..].iter().map(|(id, _, _)| *id).collect();
            repoint_descriptors(doc, &gone, keep);
            log.push(format!(
                "font {}: merged {} subsets into one union program, {} -> {} bytes stored",
                String::from_utf8_lossy(&key.0),
                group.len(),
                before,
                after
            ));
            merged += gone.len();
            saved += before as i64 - after as i64;
        }
    }
    (merged, saved)
}

/// Split a group into subgroups whose members agree on the metrics of
/// every glyph that none of them draws an outline for (the space, say).
/// Text extraction reads word gaps off those advances, and subsetters zero
/// the metrics of glyphs a subset does not use, so subsets that disagree
/// there must keep their own programs. Greedy, in object order; members
/// that cannot be parsed stay alone.
fn metric_compatible_subgroups(
    group: &[(ObjectId, Vec<u8>, usize)],
    count: usize,
    budget: &mut u64,
) -> Vec<Vec<(ObjectId, Vec<u8>, usize)>> {
    let parsed: Vec<Option<Parsed>> = group.iter().map(|(_, f, _)| parse(f, count)).collect();
    let compatible = |a: &Parsed, b: &Parsed| {
        (0..count).all(|g| {
            let empty = |p: &Parsed| p.loca[g] == p.loca[g + 1];
            !(empty(a) && empty(b)) || a.metrics[g] == b.metrics[g]
        })
    };
    let mut subgroups: Vec<Vec<usize>> = Vec::new();
    for (i, p) in parsed.iter().enumerate() {
        let Some(p) = p else {
            subgroups.push(vec![i]);
            continue;
        };
        // Out of comparison budget: no further merging (each subset alone).
        let mut home = None;
        for (k, sg) in subgroups.iter().enumerate() {
            let cost = (sg.len() * count) as u64;
            if cost > *budget {
                home = None;
                break;
            }
            *budget -= cost;
            if sg.iter().all(|&j| parsed[j].as_ref().is_some_and(|q| compatible(p, q))) {
                home = Some(k);
                break;
            }
        }
        match home {
            Some(k) => subgroups[k].push(i),
            None => subgroups.push(vec![i]),
        }
    }
    subgroups
        .into_iter()
        .map(|sg| sg.into_iter().map(|i| group[i].clone()).collect())
        .collect()
}

/// Tables no PDF consumer reads from an embedded TrueType program: PDF
/// content names glyphs by id (or by code through the cmap) and positions
/// them by the PDF's own widths, so glyph positioning, justification,
/// baseline and math layout are never applied; a digital signature is
/// invalid once the program changes; the rest are metadata and optional
/// rasterizer caches. ISO 32000 lists the tables a TrueType font program
/// needs (glyf, head, hhea, hmtx, loca, maxp, cvt, fpgm, prep, and cmap for
/// simple fonts); none of these is among them.
const UNUSED_ALWAYS: &[&[u8; 4]] = &[
    b"GPOS", b"kern", b"JSTF", b"BASE", b"MATH", b"DSIG", b"meta", b"hdmx", b"LTSH", b"VDMX",
];
/// Glyph substitution tables: never applied either, but a text extractor
/// facing a CIDFont without /ToUnicode may consult them to map variant
/// glyphs back to characters, so they go only when every CIDFont user has
/// a ToUnicode CMap.
const UNUSED_WITH_TOUNICODE: &[&[u8; 4]] = &[b"GSUB", b"GDEF", b"morx", b"mort", b"feat"];

/// Remove the tables above from every embedded TrueType program (FontFile2)
/// whose stored size goes down. Returns (programs rewritten, bytes saved).
pub(crate) fn strip_unused_font_tables(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    // Programs used by a Type0 font without ToUnicode keep GSUB and friends.
    let mut keep_substitution: HashMap<ObjectId, ()> = HashMap::new();
    let mut programs: Vec<ObjectId> = Vec::new();
    let descriptor_program = |d: &Dictionary| match d.get(b"FontFile2") {
        Ok(Object::Reference(p)) => Some(*p),
        _ => None,
    };
    for obj in doc.objects.values() {
        let dict = match obj {
            Object::Dictionary(d) => d,
            _ => continue,
        };
        if let Some(p) = descriptor_program(dict) {
            programs.push(p);
        }
        if matches!(dict.get(b"Subtype"), Ok(Object::Name(n)) if n == b"Type0")
            && dict.get(b"ToUnicode").is_err()
        {
            let descendants = match dict.get(b"DescendantFonts").and_then(|o| resolve(doc, o)) {
                Ok(Object::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            for d in &descendants {
                if let Ok(cid) = resolve_dict(doc, d)
                    && let Ok(fd) = cid.get(b"FontDescriptor").and_then(|o| resolve_dict(doc, o))
                    && let Some(p) = descriptor_program(fd)
                {
                    keep_substitution.insert(p, ());
                }
            }
        }
    }
    // Descriptors can also sit directly inside CIDFont dictionaries.
    for obj in doc.objects.values() {
        let Object::Dictionary(f) = obj else { continue };
        let Ok(Object::Array(descendants)) = f.get(b"DescendantFonts").and_then(|o| resolve(doc, o)) else {
            continue;
        };
        for d in descendants {
            if let Ok(cid) = resolve_dict(doc, d)
                && let Ok(Object::Dictionary(fd)) = cid.get(b"FontDescriptor")
                && let Some(p) = descriptor_program(fd)
            {
                programs.push(p);
            }
        }
    }
    programs.sort();
    programs.dedup();

    let mut rewritten = 0usize;
    let mut saved = 0i64;
    for id in programs {
        let Ok(Object::Stream(s)) = doc.get_object(id) else { continue };
        if !matches!(
            crate::filter_names(&s.dict).iter().map(|f| f.as_str()).collect::<Vec<_>>().as_slice(),
            [] | ["FlateDecode"]
        ) {
            continue;
        }
        let Ok(font) = inflate(s, MAX_INFLATE) else { continue };
        let before = s.content.len();
        if !matches!(be32(&font, 0), Some(0x0001_0000) | Some(0x7472_7565)) {
            continue; // not a TrueType sfnt ('true' or 1.0)
        }
        let Some(all) = tags(&font) else { continue };
        let substitution_ok = !keep_substitution.contains_key(&id);
        let dropped: Vec<[u8; 4]> = all
            .iter()
            .copied()
            .filter(|t| {
                UNUSED_ALWAYS.contains(&t) || (substitution_ok && UNUSED_WITH_TOUNICODE.contains(&t))
            })
            .collect();
        if dropped.is_empty() {
            continue;
        }
        let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        let mut ok = true;
        for tag in &all {
            if dropped.contains(tag) {
                continue;
            }
            let Some(mut content) = table(&font, tag).map(|t| t.to_vec()) else {
                ok = false;
                break;
            };
            if tag == b"head" {
                if content.len() < 12 {
                    ok = false;
                    break;
                }
                content[8..12].fill(0); // checkSumAdjustment, recomputed
            }
            tables.push((*tag, content));
        }
        if !ok {
            continue;
        }
        let rebuilt = assemble_font(&tables);
        let mut stream = lopdf::Stream::new(Dictionary::new(), rebuilt.clone());
        if stream.compress().is_err() || stream.content.len() >= before {
            continue;
        }
        let after = stream.content.len();
        let Some(Object::Stream(target)) = doc.objects.get_mut(&id) else { continue };
        target.dict.set("Length1", rebuilt.len() as i64);
        target.dict.set("Filter", Object::Name(b"FlateDecode".to_vec()));
        target.dict.remove(b"DecodeParms");
        target.dict.set("Length", after as i64);
        target.set_content(stream.content);
        target.allows_compression = false;
        rewritten += 1;
        saved += before as i64 - after as i64;
        let names: Vec<String> = dropped.iter().map(|t| String::from_utf8_lossy(t).trim().to_string()).collect();
        log.push(format!(
            "font program {} {}: dropped unused tables {}, {} -> {} bytes stored",
            id.0,
            id.1,
            names.join(","),
            before,
            after
        ));
    }
    (rewritten, saved)
}

/// FontFile2 programs: whether every user qualifies, the base font name
/// (subset tag removed), and whether a simple TrueType font (which looks
/// glyphs up through the program's cmap) uses it. None when a font
/// structure is unreadable.
fn program_users(doc: &Document) -> Option<HashMap<ObjectId, (bool, Vec<u8>, bool)>> {
    let mut users: HashMap<ObjectId, (bool, Vec<u8>, bool)> = HashMap::new();
    // Descriptors reached through a qualifying Type0 -> CIDFontType2 chain,
    // or used by a simple TrueType font.
    let mut good_descriptors: HashMap<ObjectId, ()> = HashMap::new();
    let mut simple_descriptors: HashMap<ObjectId, ()> = HashMap::new();
    let mut bad_descriptors: HashMap<ObjectId, ()> = HashMap::new();
    for obj in doc.objects.values() {
        let Object::Dictionary(font) = obj else {
            continue;
        };
        if !matches!(font.get(b"Type"), Ok(Object::Name(n)) if n == b"Font") {
            continue;
        }
        match font.get(b"Subtype") {
            Ok(Object::Name(n)) if n == b"Type0" => {
                let qualifies_parent = font.get(b"ToUnicode").is_ok();
                let Ok(Object::Array(desc)) =
                    font.get(b"DescendantFonts").and_then(|o| resolve(doc, o))
                else {
                    continue;
                };
                for d in desc {
                    let Ok(cid) = resolve_dict(doc, d) else {
                        continue;
                    };
                    let Ok(Object::Reference(fd)) = cid.get(b"FontDescriptor") else {
                        continue;
                    };
                    let is_type2 =
                        matches!(cid.get(b"Subtype"), Ok(Object::Name(n)) if n == b"CIDFontType2");
                    let identity = match cid.get(b"CIDToGIDMap") {
                        Err(_) => true,
                        Ok(Object::Name(n)) => n == b"Identity",
                        Ok(_) => false,
                    };
                    if qualifies_parent && is_type2 && identity {
                        good_descriptors.insert(*fd, ());
                    } else {
                        bad_descriptors.insert(*fd, ());
                    }
                }
            }
            Ok(Object::Name(n)) if n == b"CIDFontType2" => {
                // Judged through its Type0 parent above.
            }
            Ok(Object::Name(n)) if n == b"TrueType" => {
                if let Ok(Object::Reference(fd)) = font.get(b"FontDescriptor") {
                    simple_descriptors.insert(*fd, ());
                }
            }
            _ => {
                if let Ok(Object::Reference(fd)) = font.get(b"FontDescriptor") {
                    bad_descriptors.insert(*fd, ());
                }
            }
        }
    }
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(desc) = obj else {
            continue;
        };
        let Ok(Object::Reference(program)) = desc.get(b"FontFile2") else {
            continue;
        };
        let simple = simple_descriptors.contains_key(&id);
        let ok = (good_descriptors.contains_key(&id) || simple)
            && !bad_descriptors.contains_key(&id)
            && desc.get(b"CIDSet").is_err();
        let name = match desc.get(b"FontName") {
            Ok(Object::Name(n)) => match n.iter().position(|&b| b == b'+') {
                Some(p) => n[p + 1..].to_vec(),
                None => n.clone(),
            },
            _ => return None,
        };
        let entry = users.entry(*program).or_insert((true, name.clone(), false));
        entry.0 &= ok && entry.1 == name;
        entry.2 |= simple;
    }
    Some(users)
}

/// Point every FontDescriptor whose FontFile2 is one of `gone` at `keep`.
fn repoint_descriptors(doc: &mut Document, gone: &[ObjectId], keep: ObjectId) {
    for obj in doc.objects.values_mut() {
        let Object::Dictionary(desc) = obj else {
            continue;
        };
        if let Ok(Object::Reference(p)) = desc.get(b"FontFile2")
            && gone.contains(p)
        {
            desc.set("FontFile2", Object::Reference(keep));
        }
    }
}

struct Parsed<'a> {
    font: &'a [u8],
    glyf: &'a [u8],
    loca: Vec<usize>,
    metrics: Vec<(u16, u16)>,
}

fn parse(font: &[u8], count: usize) -> Option<Parsed<'_>> {
    if be32(font, 0)? != 0x0001_0000 {
        return None;
    }
    let (head_o, head_l) = find_table(font, b"head")?;
    let (hhea_o, hhea_l) = find_table(font, b"hhea")?;
    let (hmtx_o, hmtx_l) = find_table(font, b"hmtx")?;
    let (loca_o, _) = find_table(font, b"loca")?;
    let (glyf_o, glyf_l) = find_table(font, b"glyf")?;
    if head_l < 54 || hhea_l < 36 {
        return None;
    }
    let long = match be16(font, head_o + 50)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mut loca = Vec::with_capacity(count + 1);
    for i in 0..=count {
        let v = if long {
            be32(font, loca_o + i * 4)? as usize
        } else {
            be16(font, loca_o + i * 2)? as usize * 2
        };
        if v > glyf_l || loca.last().is_some_and(|&prev| v < prev) {
            return None;
        }
        loca.push(v);
    }
    let nhm = be16(font, hhea_o + 34)? as usize;
    if nhm == 0 || nhm > count {
        return None;
    }
    let mut metrics = Vec::with_capacity(count);
    for gid in 0..count {
        if gid < nhm {
            metrics.push((
                be16(font, hmtx_o + gid * 4)?,
                be16(font, hmtx_o + gid * 4 + 2)?,
            ));
        } else {
            let advance = be16(font, hmtx_o + (nhm - 1) * 4)?;
            let lsb_off = hmtx_o + nhm * 4 + (gid - nhm) * 2;
            let lsb = if lsb_off + 2 <= hmtx_o + hmtx_l {
                be16(font, lsb_off)?
            } else {
                0
            };
            metrics.push((advance, lsb));
        }
    }
    Some(Parsed {
        font,
        glyf: font.get(glyf_o..glyf_o + glyf_l)?,
        loca,
        metrics,
    })
}

fn tags(font: &[u8]) -> Option<Vec<[u8; 4]>> {
    let n = be16(font, 4)? as usize;
    (0..n)
        .map(|i| font.get(12 + i * 16..16 + i * 16)?.try_into().ok())
        .collect()
}

fn table<'a>(font: &'a [u8], tag: &[u8; 4]) -> Option<&'a [u8]> {
    let (o, l) = find_table(font, tag)?;
    font.get(o..o + l)
}

/// The union program, or None if the group is not provably compatible.
/// `cmap_used`: some user looks glyphs up through the cmap, so the cmap must
/// become the union too.
#[cfg(test)]
fn build_union(fonts: &[&[u8]], cmap_used: bool) -> Option<Vec<u8>> {
    build_union_why(fonts, cmap_used, &mut "")
}

/// build_union, recording in `why` the check under way when it gives up.
fn build_union_why(fonts: &[&[u8]], cmap_used: bool, why: &mut &'static str) -> Option<Vec<u8>> {
    *why = "unreadable table structure";
    let first = fonts[0];
    let count = be16(first, find_table(first, b"maxp")?.0 + 4)? as usize;
    let parsed: Vec<Parsed> = fonts
        .iter()
        .map(|f| parse(f, count))
        .collect::<Option<_>>()?;

    // Every table outside the rebuilt/carried/dropped sets must agree.
    // Tables no PDF consumer reads are dropped from the union (users here
    // are ToUnicode CIDFonts or simple fonts, whose text comes from their
    // /Encoding), so they neither need to agree nor to exist everywhere.
    *why = "different table sets";
    let droppable = |t: &[u8; 4]| {
        DROPPED.contains(&t) || UNUSED_ALWAYS.contains(&t) || UNUSED_WITH_TOUNICODE.contains(&t)
    };
    let kept_tags = |f: &[u8]| -> Option<Vec<[u8; 4]>> {
        let mut t: Vec<[u8; 4]> = tags(f)?.into_iter().filter(|t| !droppable(t)).collect();
        t.sort();
        Some(t)
    };
    let first_tags = kept_tags(first)?;
    for f in &fonts[1..] {
        if kept_tags(f)? != first_tags {
            return None;
        }
    }
    for tag in &first_tags {
        if REBUILT.contains(&tag) || CARRIED.contains(&tag) || DROPPED.contains(&tag) {
            continue;
        }
        let reference = table(first, tag)?;
        if fonts[1..].iter().any(|f| table(f, tag) != Some(reference)) {
            *why = match tag {
                b"cvt " | b"fpgm" | b"prep" => "hinting tables differ",
                b"EBDT" | b"EBLC" | b"EBSC" => "embedded bitmaps differ",
                b"vhea" | b"vmtx" => "vertical metrics differ",
                _ => "a shared table differs",
            };
            return None;
        }
    }
    let cmap = if cmap_used {
        // Glyph names: some viewers look simple-font glyphs up by name.
        *why = "glyph names (post) differ";
        let post = table(first, b"post");
        let nameless = |p: Option<&[u8]>| p.and_then(|t| be32(t, 0)) == Some(0x0003_0000);
        if !fonts
            .iter()
            .all(|f| table(f, b"post") == post || (nameless(post) && nameless(table(f, b"post"))))
        {
            return None;
        }
        *why = "cmaps disagree or use an unsupported format";
        Some(merged_cmap(fonts)?)
    } else {
        None
    };
    *why = "different unitsPerEm";
    let units = |f: &[u8]| table(f, b"head").and_then(|h| be16(h, 18));
    if fonts.iter().any(|f| units(f) != units(first)) {
        return None;
    }

    // Union of outlines: the first subset holding a glyph provides it; any
    // other holder must have the same bytes.
    *why = "an outline differs between subsets";
    let mut provider: Vec<Option<usize>> = vec![None; count];
    for (i, p) in parsed.iter().enumerate() {
        for gid in 0..count {
            let (s, e) = (p.loca[gid], p.loca[gid + 1]);
            if e == s {
                continue;
            }
            match provider[gid] {
                None => provider[gid] = Some(i),
                Some(j) => {
                    let q = &parsed[j];
                    if q.glyf[q.loca[gid]..q.loca[gid + 1]] != p.glyf[s..e] {
                        return None;
                    }
                }
            }
        }
    }
    // Each subset's composites must find all their components inside that
    // same subset; then the union draws them exactly as the subset did.
    *why = "a composite glyph's component is missing in its subset";
    for (i, p) in parsed.iter().enumerate() {
        let provided: BTreeSet<u16> = (0..count)
            .filter(|&g| provider[g] == Some(i))
            .map(|g| g as u16)
            .collect();
        let mut closure = provided.clone();
        add_composite_closure(p.font, &mut closure)?;
        let outlined = outlined_gids(p.font)?;
        if !closure.is_subset(&outlined) {
            return None;
        }
    }

    // Glyphs without an outline anywhere (the space, say) still carry an
    // advance, which text extraction uses to tell words apart: every subset
    // must agree on it (metric_compatible_subgroups groups them so).
    *why = "metrics of an outline-less glyph differ";
    let mut empty_metric: Vec<(u16, u16)> = Vec::with_capacity(count);
    for gid in 0..count {
        let m = parsed[0].metrics[gid];
        if provider[gid].is_none() && parsed.iter().any(|p| p.metrics[gid] != m) {
            return None;
        }
        empty_metric.push(m);
    }
    *why = "unreadable table structure";
    let mut glyf = Vec::new();
    let mut loca = Vec::with_capacity((count + 1) * 4);
    let mut hmtx = Vec::with_capacity(count * 4);
    for gid in 0..count {
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        let (src, metric) = match provider[gid] {
            Some(i) => {
                let p = &parsed[i];
                (&p.glyf[p.loca[gid]..p.loca[gid + 1]], p.metrics[gid])
            }
            None => (&[][..], empty_metric[gid]),
        };
        glyf.extend_from_slice(src);
        // Glyph data stays 4-byte aligned in the long-loca layout.
        glyf.resize(glyf.len().div_ceil(4) * 4, 0);
        hmtx.extend_from_slice(&metric.0.to_be_bytes());
        hmtx.extend_from_slice(&metric.1.to_be_bytes());
    }
    loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());

    // maxp: field-wise maxima (version 1.0 fields after numGlyphs).
    let mut maxp = table(first, b"maxp")?.to_vec();
    if maxp.len() >= 32 {
        for off in (6..32).step_by(2) {
            let m = fonts
                .iter()
                .filter_map(|f| table(f, b"maxp").and_then(|t| be16(t, off)))
                .max()?;
            maxp[off..off + 2].copy_from_slice(&m.to_be_bytes());
        }
    }
    // head: union bounding box, long loca, checksum recomputed on assembly.
    let mut head = table(first, b"head")?.to_vec();
    let field = |f: &[u8], off: usize| {
        table(f, b"head")
            .and_then(|t| be16(t, off))
            .map(|v| v as i16)
    };
    for (off, take_min) in [(36, true), (38, true), (40, false), (42, false)] {
        let vals = fonts
            .iter()
            .map(|f| field(f, off))
            .collect::<Option<Vec<i16>>>()?;
        let v = if take_min {
            *vals.iter().min()?
        } else {
            *vals.iter().max()?
        };
        head[off..off + 2].copy_from_slice(&v.to_be_bytes());
    }
    head[50..52].copy_from_slice(&1u16.to_be_bytes());
    head[8..12].fill(0);
    // hhea: one longHorMetric per glyph, extremes over the group.
    let mut hhea = table(first, b"hhea")?.to_vec();
    let hfield = |f: &[u8], off: usize| table(f, b"hhea").and_then(|t| be16(t, off));
    let adv_max = fonts
        .iter()
        .map(|f| hfield(f, 10))
        .collect::<Option<Vec<u16>>>()?;
    hhea[10..12].copy_from_slice(&adv_max.iter().max()?.to_be_bytes());
    for (off, take_min) in [(12, true), (14, true), (16, false)] {
        let vals = fonts
            .iter()
            .map(|f| hfield(f, off).map(|v| v as i16))
            .collect::<Option<Vec<i16>>>()?;
        let v = if take_min {
            *vals.iter().min()?
        } else {
            *vals.iter().max()?
        };
        hhea[off..off + 2].copy_from_slice(&v.to_be_bytes());
    }
    hhea[34..36].copy_from_slice(&(count as u16).to_be_bytes());

    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    for tag in tags(first)? {
        if droppable(&tag) {
            continue;
        }
        let content = match &tag {
            b"glyf" => glyf.clone(),
            b"loca" => loca.clone(),
            b"hmtx" => hmtx.clone(),
            b"maxp" => maxp.clone(),
            b"head" => head.clone(),
            b"hhea" => hhea.clone(),
            b"cmap" if cmap.is_some() => cmap.clone()?,
            _ => table(first, &tag)?.to_vec(),
        };
        tables.push((tag, content));
    }
    Some(assemble_font(&tables))
}

/// The union of the group's cmaps: the same (platform, encoding) subtables,
/// each mapping every code any subset maps, to the same glyph. None when the
/// subtable sets differ, a format is not one we read (0, 4, 6, 12), or two
/// subsets map one code to different glyphs.
///
/// Format 14 (Unicode variation sequences) is carried from the first subset:
/// it only applies to a base character followed by a variation selector,
/// which a PDF simple font's single-byte codes never form.
fn merged_cmap(fonts: &[&[u8]]) -> Option<Vec<u8>> {
    let mut budget = MAX_CMAP_ENTRIES;
    let tables: Vec<&[u8]> = fonts
        .iter()
        .map(|f| table(f, b"cmap"))
        .collect::<Option<_>>()?;
    let records = |t: &[u8]| -> Option<Vec<(u16, u16, usize)>> {
        let n = be16(t, 2)? as usize;
        let mut r: Vec<(u16, u16, usize)> = (0..n)
            .map(|i| {
                Some((
                    be16(t, 4 + i * 8)?,
                    be16(t, 6 + i * 8)?,
                    be32(t, 8 + i * 8)? as usize,
                ))
            })
            .collect::<Option<_>>()?;
        r.sort();
        Some(r)
    };
    let all: Vec<Vec<(u16, u16, usize)>> =
        tables.iter().map(|t| records(t)).collect::<Option<_>>()?;
    let keys: Vec<(u16, u16)> = all[0].iter().map(|&(p, e, _)| (p, e)).collect();
    if keys.is_empty()
        || all
            .iter()
            .any(|r| r.iter().map(|&(p, e, _)| (p, e)).collect::<Vec<_>>() != keys)
    {
        return None;
    }
    let mut subtables: Vec<((u16, u16), Vec<u8>)> = Vec::new();
    for (k, &(p, e)) in keys.iter().enumerate() {
        let formats: Vec<u16> = tables
            .iter()
            .zip(&all)
            .map(|(t, r)| be16(t, r[k].2))
            .collect::<Option<_>>()?;
        let off0 = all[0][k].2;
        if formats.contains(&14) {
            if formats.iter().any(|&f| f != 14) {
                return None;
            }
            let len = be32(tables[0], off0 + 2)? as usize;
            subtables.push(((p, e), tables[0].get(off0..off0 + len)?.to_vec()));
            continue;
        }
        let mut map: BTreeMap<u32, u16> = BTreeMap::new();
        for (t, r) in tables.iter().zip(&all) {
            for (code, gid) in read_subtable(t, r[k].2, &mut budget)? {
                match map.insert(code, gid) {
                    Some(prev) if prev != gid => return None,
                    _ => {}
                }
            }
        }
        let body = if formats.iter().all(|&f| f == 0) {
            format0(&map, be16(tables[0], off0 + 4)?)
        } else if formats.contains(&12) || map.keys().any(|&c| c > 0xFFFF) {
            format12(
                &map,
                be32(tables[0], off0 + 8)
                    .filter(|_| formats[0] == 12)
                    .unwrap_or(0),
            )
        } else {
            format4(
                &map,
                be16(tables[0], off0 + 4)
                    .filter(|_| formats[0] != 12)
                    .unwrap_or(0),
            )?
        };
        subtables.push(((p, e), body));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(subtables.len() as u16).to_be_bytes());
    let mut offset = 4 + 8 * subtables.len();
    for ((p, e), body) in &subtables {
        out.extend_from_slice(&p.to_be_bytes());
        out.extend_from_slice(&e.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        offset += body.len();
    }
    for (_, body) in &subtables {
        out.extend_from_slice(body);
    }
    Some(out)
}

/// Every (code, glyph) a format 0, 4, 6 or 12 subtable maps (glyph 0, the
/// "missing" glyph, counts as unmapped). Every enumerated code draws on
/// `budget`; None once it runs out, since segments and groups can repeat
/// and a few bytes would otherwise enumerate tens of thousands of codes.
fn read_subtable(t: &[u8], off: usize, budget: &mut usize) -> Option<Vec<(u32, u16)>> {
    let mut out = Vec::new();
    let spend = |budget: &mut usize, n: usize| -> Option<()> {
        *budget = budget.checked_sub(n)?;
        Some(())
    };
    match be16(t, off)? {
        0 => {
            spend(budget, 256)?;
            for c in 0..256usize {
                let g = *t.get(off + 6 + c)? as u16;
                if g != 0 {
                    out.push((c as u32, g));
                }
            }
        }
        4 => {
            let segx2 = be16(t, off + 6)? as usize;
            let ends = off + 14;
            let starts = ends + segx2 + 2;
            let deltas = starts + segx2;
            let ranges = deltas + segx2;
            for i in (0..segx2).step_by(2) {
                let (start, end) = (be16(t, starts + i)?, be16(t, ends + i)?);
                let delta = be16(t, deltas + i)?;
                let range_off = be16(t, ranges + i)? as usize;
                if start > end {
                    return None;
                }
                spend(budget, (end - start) as usize + 1)?;
                for c in start..=end {
                    if c == 0xFFFF {
                        break;
                    }
                    let g = if range_off == 0 {
                        c.wrapping_add(delta)
                    } else {
                        match be16(t, ranges + i + range_off + 2 * (c - start) as usize)? {
                            0 => 0,
                            g => g.wrapping_add(delta),
                        }
                    };
                    if g != 0 {
                        out.push((c as u32, g));
                    }
                }
            }
        }
        6 => {
            let first = be16(t, off + 6)? as u32;
            let count = be16(t, off + 8)? as usize;
            spend(budget, count)?;
            for i in 0..count {
                let g = be16(t, off + 10 + 2 * i)?;
                if g != 0 {
                    out.push((first + i as u32, g));
                }
            }
        }
        12 => {
            let groups = be32(t, off + 12)? as usize;
            for i in 0..groups {
                let at = off + 16 + 12 * i;
                let (start, end, g0) = (be32(t, at)?, be32(t, at + 4)?, be32(t, at + 8)?);
                if start > end || end > 0x10_FFFF {
                    return None;
                }
                spend(budget, (end - start) as usize + 1)?;
                for c in start..=end {
                    let g = u16::try_from(g0.checked_add(c - start)?).ok()?;
                    if g != 0 {
                        out.push((c, g));
                    }
                }
            }
        }
        _ => return None,
    }
    Some(out)
}

fn format0(map: &BTreeMap<u32, u16>, language: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(262);
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&262u16.to_be_bytes());
    out.extend_from_slice(&language.to_be_bytes());
    out.extend((0..256u32).map(|c| map.get(&c).copied().unwrap_or(0) as u8));
    out
}

/// Runs of consecutive codes mapping to consecutive glyphs.
fn runs(map: &BTreeMap<u32, u16>, below: u32) -> Vec<(u32, u32, u16)> {
    let mut segs: Vec<(u32, u32, u16)> = Vec::new(); // (start, end, first gid)
    for (&c, &g) in map.range(..below) {
        match segs.last_mut() {
            Some((s, e, g0)) if c == *e + 1 && g as u32 == *g0 as u32 + (c - *s) => *e = c,
            _ => segs.push((c, c, g)),
        }
    }
    segs
}

/// Format 4 with one delta segment per run, plus the required final 0xFFFF
/// segment. None if the table would overflow its 16-bit length.
fn format4(map: &BTreeMap<u32, u16>, language: u16) -> Option<Vec<u8>> {
    let mut segs = runs(map, 0xFFFF);
    segs.push((0xFFFF, 0xFFFF, 0));
    let n = segs.len();
    let length = u16::try_from(16 + 8 * n).ok()?;
    let mut out = Vec::new();
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&language.to_be_bytes());
    out.extend_from_slice(&((n * 2) as u16).to_be_bytes());
    let pow = 1u16 << (15 - (n as u16).leading_zeros());
    out.extend_from_slice(&(pow * 2).to_be_bytes());
    out.extend_from_slice(&(pow.trailing_zeros() as u16).to_be_bytes());
    out.extend_from_slice(&((n as u16) * 2 - pow * 2).to_be_bytes());
    for &(_, e, _) in &segs {
        out.extend_from_slice(&(e as u16).to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes());
    for &(s, _, _) in &segs {
        out.extend_from_slice(&(s as u16).to_be_bytes());
    }
    for &(s, _, g) in &segs {
        // The final segment maps 0xFFFF to glyph 0: delta 1 wraps it.
        let delta = if s == 0xFFFF {
            1
        } else {
            g.wrapping_sub(s as u16)
        };
        out.extend_from_slice(&delta.to_be_bytes());
    }
    for _ in &segs {
        out.extend_from_slice(&0u16.to_be_bytes());
    }
    Some(out)
}

/// Format 12: one sequential group per run.
fn format12(map: &BTreeMap<u32, u16>, language: u32) -> Vec<u8> {
    let segs = runs(map, 0x11_0000);
    let mut out = Vec::new();
    out.extend_from_slice(&12u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&((16 + 12 * segs.len()) as u32).to_be_bytes());
    out.extend_from_slice(&language.to_be_bytes());
    out.extend_from_slice(&(segs.len() as u32).to_be_bytes());
    for (s, e, g) in segs {
        out.extend_from_slice(&s.to_be_bytes());
        out.extend_from_slice(&e.to_be_bytes());
        out.extend_from_slice(&(g as u32).to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny sfnt with `outlines[gid]` as glyph data (empty = no outline).
    fn font(outlines: &[&[u8]], extra: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let count = outlines.len();
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        for g in outlines {
            loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
            glyf.extend_from_slice(g);
        }
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        let mut head = vec![0u8; 54];
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[50..52].copy_from_slice(&1u16.to_be_bytes());
        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&(count as u16).to_be_bytes());
        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&(count as u16).to_be_bytes());
        let mut hmtx = Vec::new();
        for gid in 0..count as u16 {
            hmtx.extend_from_slice(&(500 + gid).to_be_bytes());
            hmtx.extend_from_slice(&0u16.to_be_bytes());
        }
        let mut tables = vec![
            (*b"glyf", glyf),
            (*b"head", head),
            (*b"hhea", hhea),
            (*b"hmtx", hmtx),
            (*b"loca", loca),
            (*b"maxp", maxp),
        ];
        tables.extend_from_slice(extra);
        assemble_font(&tables)
    }

    /// A simple glyph: one contour, distinguishable by `tag`.
    fn glyph(tag: u8) -> Vec<u8> {
        let mut g = vec![0u8; 12];
        g[0..2].copy_from_slice(&1i16.to_be_bytes());
        g[11] = tag;
        g
    }

    #[test]
    fn repeated_cmap_ranges_hit_the_budget_instead_of_exhausting_memory() {
        // 1024 identical format-12 groups, each enumerating 65,536 codes:
        // 12 KiB of table that would expand to 67M pairs (seconds, ~1 GB).
        let groups = 1024usize;
        let mut sub = Vec::new();
        sub.extend_from_slice(&12u16.to_be_bytes());
        sub.extend_from_slice(&0u16.to_be_bytes());
        sub.extend_from_slice(&((16 + 12 * groups) as u32).to_be_bytes());
        sub.extend_from_slice(&0u32.to_be_bytes());
        sub.extend_from_slice(&(groups as u32).to_be_bytes());
        for _ in 0..groups {
            sub.extend_from_slice(&0u32.to_be_bytes());
            sub.extend_from_slice(&0xFFFFu32.to_be_bytes());
            sub.extend_from_slice(&0u32.to_be_bytes());
        }
        let mut t = Vec::new();
        for v in [0u16, 1, 3, 10] {
            t.extend_from_slice(&v.to_be_bytes());
        }
        t.extend_from_slice(&12u32.to_be_bytes());
        t.extend_from_slice(&sub);
        let f = font(&[&[]], &[(*b"cmap", t)]);
        let start = std::time::Instant::now();
        assert!(merged_cmap(&[&f, &f]).is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn a_font_with_implausibly_many_subsets_is_left_alone() {
        // Partitioning compares subsets pairwise; past MAX_SUBSETS_PER_FONT
        // a name is skipped before anything is compared.
        let mut doc = Document::with_version("1.5");
        for i in 0..=MAX_SUBSETS_PER_FONT {
            let f = font(&[&[], &glyph((i % 200) as u8)], &[]);
            let program = doc.add_object(lopdf::Stream::new(Dictionary::new(), f));
            let mut desc = Dictionary::new();
            desc.set("FontName", Object::Name(format!("T{i:05}+X").into_bytes()));
            desc.set("FontFile2", Object::Reference(program));
            let desc = doc.add_object(desc);
            let mut f = Dictionary::new();
            f.set("Type", Object::Name(b"Font".to_vec()));
            f.set("Subtype", Object::Name(b"TrueType".to_vec()));
            f.set("FontDescriptor", Object::Reference(desc));
            doc.add_object(f);
        }
        let mut log = Vec::new();
        assert_eq!(union_cid_truetype_subsets(&mut doc, &mut log).0, 0);
    }

    #[test]
    fn union_holds_every_subsets_outlines_at_their_ids() {
        let (a, b) = (glyph(1), glyph(2));
        let f1 = font(&[&[], &a, &[], &[]], &[]);
        let f2 = font(&[&[], &[], &[], &b], &[]);
        let union = build_union(&[&f1, &f2], false).expect("compatible");
        let p = parse(&union, 4).unwrap();
        assert_eq!(&p.glyf[p.loca[1]..p.loca[2]], a.as_slice());
        assert_eq!(&p.glyf[p.loca[3]..p.loca[3] + 12], b.as_slice());
        assert_eq!(p.loca[2], p.loca[3], "gid 2 stays empty");
        assert_eq!(outlined_gids(&union).unwrap(), BTreeSet::from([1, 3]));
    }

    fn cmap4(pairs: &[(u16, u16)]) -> Vec<u8> {
        let map: BTreeMap<u32, u16> = pairs.iter().map(|&(c, g)| (c as u32, g)).collect();
        let body = format4(&map, 0).unwrap();
        let mut t = Vec::new();
        t.extend_from_slice(&0u16.to_be_bytes());
        t.extend_from_slice(&1u16.to_be_bytes());
        t.extend_from_slice(&3u16.to_be_bytes());
        t.extend_from_slice(&1u16.to_be_bytes());
        t.extend_from_slice(&12u32.to_be_bytes());
        t.extend_from_slice(&body);
        t
    }

    #[test]
    fn cmaps_merge_when_simple_fonts_share_the_program() {
        let a = font(
            &[&[], &glyph(1), &[], &[]],
            &[(*b"cmap", cmap4(&[(0x41, 1)]))],
        );
        let b = font(
            &[&[], &[], &[], &glyph(3)],
            &[(*b"cmap", cmap4(&[(0x43, 3)]))],
        );
        let union = build_union(&[&a, &b], true).expect("consistent cmaps merge");
        let idx = CmapIndex::parse(&union);
        assert_eq!(idx.lookup(3, 1, 0x41), Some(1));
        assert_eq!(idx.lookup(3, 1, 0x43), Some(3));
        assert_eq!(idx.lookup(3, 1, 0x42), None);
        // The same code mapped to two different glyphs: refused.
        let c = font(
            &[&[], &[], &[], &glyph(3)],
            &[(*b"cmap", cmap4(&[(0x41, 3)]))],
        );
        assert!(build_union(&[&a, &c], true).is_none());
        // Without a cmap user, the first cmap is simply carried.
        assert!(build_union(&[&a, &c], false).is_some());
    }

    #[test]
    fn format4_round_trips_runs_and_gaps() {
        let pairs: Vec<(u16, u16)> =
            vec![(0x20, 3), (0x21, 4), (0x22, 5), (0x30, 9), (0x3042, 700)];
        let idx = CmapIndex::parse(&font(&[&[]], &[(*b"cmap", cmap4(&pairs))]));
        for (c, g) in &pairs {
            assert_eq!(idx.lookup(3, 1, *c as u32), Some(*g));
        }
        assert_eq!(idx.lookup(3, 1, 0x23), None);
        assert_eq!(idx.lookup(3, 1, 0xFFFF), None);
    }

    #[test]
    fn unused_tables_are_dropped_and_glyphs_kept() {
        let f = font(
            &[&[], &glyph(1)],
            &[(*b"GPOS", vec![7; 400]), (*b"GSUB", vec![8; 400]), (*b"fpgm", vec![1, 2])],
        );
        let mut doc = Document::with_version("1.5");
        let mut st = lopdf::Stream::new(Dictionary::new(), f.clone());
        st.compress().unwrap();
        let program = doc.add_object(st);
        let mut desc = Dictionary::new();
        desc.set("FontFile2", Object::Reference(program));
        doc.add_object(desc);
        let mut log = Vec::new();
        assert_eq!(strip_unused_font_tables(&mut doc, &mut log).0, 1);
        let Ok(Object::Stream(s)) = doc.get_object(program) else { panic!() };
        let out = s.decompressed_content().unwrap();
        let t = tags(&out).unwrap();
        assert!(!t.contains(b"GPOS") && !t.contains(b"GSUB") && t.contains(b"fpgm"));
        assert_eq!(table(&out, b"glyf"), table(&f, b"glyf"));
        assert_eq!(dict_int_len1(&s.dict), Some(out.len() as i64));
    }

    fn dict_int_len1(d: &Dictionary) -> Option<i64> {
        d.get(b"Length1").ok()?.as_i64().ok()
    }

    #[test]
    fn conflicting_outlines_or_tables_refuse_the_union() {
        let f1 = font(&[&[], &glyph(1)], &[]);
        let f2 = font(&[&[], &glyph(9)], &[]);
        assert!(build_union(&[&f1, &f2], false).is_none());
        // Same outlines, but a hinting program differs.
        let f1 = font(&[&[], &glyph(1)], &[(*b"fpgm", vec![1, 2])]);
        let f2 = font(&[&[], &glyph(1)], &[(*b"fpgm", vec![3, 4])]);
        assert!(build_union(&[&f1, &f2], false).is_none());
        // Carried tables (names, cmap) may differ.
        let f1 = font(&[&[], &glyph(1)], &[(*b"name", vec![1])]);
        let f2 = font(&[&[], &glyph(1)], &[(*b"name", vec![2])]);
        assert!(build_union(&[&f1, &f2], false).is_some());
    }
}
