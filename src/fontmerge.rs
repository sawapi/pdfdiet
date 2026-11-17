//! Merging duplicate subsets of the same TrueType font, and byte-identical
//! font dictionaries.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

use crate::dedup::{remap_refs, remap_refs_in_dict, write_dict_key};
use crate::limits::{MAX_INFLATE, MAX_PASS_BYTES, inflate};
use crate::resolve;

/// Merge semantically duplicated TrueType font subsets.
///
/// Figures saved as PDF each embed their own subset of the same font, and
/// subsetters bake the random six-letter subset tag (`SKOJEB+ArialMT`) into
/// the font program's `name` table, so the copies are near- but not
/// byte-identical and slip past `dedup_streams`. Two font programs are merged
/// only when they are byte-identical after neutralizing exactly that tag (and
/// the checksums derived from it) — anything else about them must match, so a
/// merge provably cannot change rendering.
///
/// When programs merge, the losing subsets' FontDescriptor `FontName` and
/// Font `BaseFont` tags are rewritten to the surviving subset's tag; the
/// dictionaries frequently become fully identical then and are merged too.
pub(crate) fn merge_font_subsets(doc: &mut Document, log: &mut Vec<String>) -> (usize, i64) {
    // FontFile2 stream -> descriptors referencing it.
    let mut descs_of_file: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else { continue };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"FontDescriptor") {
            continue;
        }
        if let Ok(Object::Reference(fid)) = dict.get(b"FontFile2") {
            descs_of_file.entry(*fid).or_default().push(id);
        }
    }
    for descs in descs_of_file.values_mut() {
        descs.sort();
    }

    // Group the font programs by their tag-neutralized bytes.
    let mut file_ids: Vec<ObjectId> = descs_of_file.keys().copied().collect();
    file_ids.sort();
    let mut groups: HashMap<Vec<u8>, Vec<ObjectId>> = HashMap::new();
    // Every key is a whole program: bound what the pass holds at once.
    let mut held = 0usize;
    for fid in file_ids {
        let Ok(Object::Stream(stream)) = doc.get_object(fid) else {
            continue;
        };
        let Ok(data) = inflate(stream, MAX_INFLATE) else { continue };
        held += data.len();
        if held > MAX_PASS_BYTES {
            break;
        }
        if let Some(key) = truetype_tag_neutralized(&data) {
            groups.entry(key).or_default().push(fid);
        }
    }

    let mut file_remap: HashMap<ObjectId, ObjectId> = HashMap::new();
    // Descriptor -> surviving subset tag, for the name rewrite pass.
    let mut retagged: HashMap<ObjectId, Vec<u8>> = HashMap::new();
    let mut saved = 0i64;
    let mut group_keys: Vec<&Vec<u8>> = groups.keys().filter(|k| groups[*k].len() > 1).collect();
    group_keys.sort();
    for key in group_keys {
        let members = &groups[key];
        let canon = members[0];
        let Some(canon_name) = descriptor_font_name(doc, descs_of_file[&canon][0]) else {
            continue;
        };
        let Some((canon_tag, canon_base)) = split_subset_name(&canon_name) else {
            continue;
        };
        let (canon_tag, canon_base) = (canon_tag.to_vec(), canon_base.to_vec());
        let mut merged_here = 0usize;
        for &dup in &members[1..] {
            // Every descriptor of the duplicate must carry a subset name with
            // the same base font; otherwise leave that copy alone.
            let dup_descs: Vec<ObjectId> = descs_of_file[&dup]
                .iter()
                .filter(|&&d| {
                    descriptor_font_name(doc, d)
                        .as_deref()
                        .and_then(split_subset_name)
                        .is_some_and(|(_tag, base)| base == canon_base)
                })
                .copied()
                .collect();
            if dup_descs.len() != descs_of_file[&dup].len() {
                continue;
            }
            if let Ok(Object::Stream(s)) = doc.get_object(dup) {
                saved += s.content.len() as i64;
            }
            file_remap.insert(dup, canon);
            for d in dup_descs {
                retagged.insert(d, canon_tag.clone());
            }
            merged_here += 1;
        }
        if merged_here > 0 {
            log.push(format!(
                "font {}: merged {} tag-equivalent subset programs",
                String::from_utf8_lossy(&canon_base),
                merged_here
            ));
        }
    }
    if file_remap.is_empty() {
        return (0, 0);
    }

    // Point every reference at the surviving programs and drop the duplicates.
    let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
    for id in &all_ids {
        if file_remap.contains_key(id) {
            continue;
        }
        if let Some(obj) = doc.objects.get_mut(id) {
            remap_refs(obj, &file_remap);
        }
    }
    remap_refs_in_dict(&mut doc.trailer, &file_remap);
    for dup in file_remap.keys() {
        doc.objects.remove(dup);
    }
    let merged_programs = file_remap.len();

    // Rewrite the subset tags so each descriptor matches the program it now
    // embeds: FontName on the descriptor itself, BaseFont on fonts that
    // reference it (directly, or through DescendantFonts for Type0 parents).
    let mut renames: Vec<(ObjectId, &[u8], Vec<u8>)> = Vec::new();
    for (&desc, new_tag) in &retagged {
        renames.push((desc, b"FontName", new_tag.clone()));
    }
    for (&id, obj) in &doc.objects {
        let Object::Dictionary(dict) = obj else { continue };
        if !matches!(dict.get(b"Type"), Ok(Object::Name(n)) if n == b"Font") {
            continue;
        }
        let desc = match dict.get(b"FontDescriptor") {
            Ok(Object::Reference(d)) => Some(*d),
            _ => type0_descendant_descriptor(doc, dict),
        };
        if let Some(new_tag) = desc.and_then(|d| retagged.get(&d)) {
            renames.push((id, b"BaseFont", new_tag.clone()));
        }
    }
    for (id, key, new_tag) in renames {
        let Some(Object::Dictionary(dict)) = doc.objects.get_mut(&id) else {
            continue;
        };
        let name = match dict.get(key) {
            Ok(Object::Name(n)) => n.clone(),
            _ => continue,
        };
        if let Some((_tag, base)) = split_subset_name(&name) {
            let mut renamed = new_tag;
            renamed.push(b'+');
            renamed.extend_from_slice(base);
            dict.set(key, Object::Name(renamed));
        }
    }

    // The retagged dictionaries are often now byte-identical to the survivors'
    // — collapse those as well.
    let dicts_merged = dedup_font_dicts(doc);
    if dicts_merged > 0 {
        log.push(format!("merged {dicts_merged} identical font dictionaries"));
    }

    (merged_programs, saved)
}

/// For a Type0 font dictionary, the FontDescriptor of its descendant CIDFont.
fn type0_descendant_descriptor(doc: &Document, dict: &Dictionary) -> Option<ObjectId> {
    let a = match resolve(doc, dict.get(b"DescendantFonts").ok()?).ok()? {
        Object::Array(a) => a,
        _ => return None,
    };
    let cid = match resolve(doc, a.first()?).ok()? {
        Object::Dictionary(d) => d,
        _ => return None,
    };
    match cid.get(b"FontDescriptor") {
        Ok(Object::Reference(d)) => Some(*d),
        _ => None,
    }
}

/// Merge byte-identical Font / FontDescriptor dictionaries. Restricted to
/// those two types: fonts are selected by reference from resource dicts, so
/// object identity carries no meaning for them (unlike e.g. page dicts).
fn dedup_font_dicts(doc: &mut Document) -> usize {
    let mut merged_total = 0usize;
    // FontDescriptors merge on the first pass, which can make their Font
    // dictionaries identical on the next; a few passes reach the fixpoint.
    for _pass in 0..4 {
        let mut canonical: HashMap<Vec<u8>, ObjectId> = HashMap::new();
        let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
        let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        ids.sort();
        for id in ids {
            let Some(Object::Dictionary(dict)) = doc.objects.get(&id) else {
                continue;
            };
            if !matches!(dict.get(b"Type"),
                Ok(Object::Name(n)) if n == b"Font" || n == b"FontDescriptor")
            {
                continue;
            }
            // Canonical bytes, not the Debug string, which collides on
            // non-UTF-8 names (a subset tag is ASCII, but /BaseFont and
            // /Differences entries need not be).
            let mut key = Vec::new();
            write_dict_key(dict, &mut key);
            if let Some(&keep) = canonical.get(&key) {
                remap.insert(id, keep);
            } else {
                canonical.insert(key, id);
            }
        }
        if remap.is_empty() {
            break;
        }
        let all_ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        for id in all_ids {
            if remap.contains_key(&id) {
                continue;
            }
            if let Some(obj) = doc.objects.get_mut(&id) {
                remap_refs(obj, &remap);
            }
        }
        remap_refs_in_dict(&mut doc.trailer, &remap);
        for dup in remap.keys() {
            doc.objects.remove(dup);
        }
        merged_total += remap.len();
    }
    merged_total
}

fn descriptor_font_name(doc: &Document, desc: ObjectId) -> Option<Vec<u8>> {
    match doc.get_dictionary(desc).ok()?.get(b"FontName") {
        Ok(Object::Name(n)) => Some(n.clone()),
        _ => None,
    }
}

/// Split a subset font name like `SKOJEB+ArialMT` into (tag, base name).
fn split_subset_name(name: &[u8]) -> Option<(&[u8], &[u8])> {
    if name.len() > 7 && name[..6].iter().all(u8::is_ascii_uppercase) && name[6] == b'+' {
        Some((&name[..6], &name[7..]))
    } else {
        None
    }
}

/// Comparison key for a TrueType font program with the subset tag neutralized.
///
/// Every `name`-table string starting with a six-uppercase-letter tag and '+'
/// (ASCII or UTF-16BE) gets the tag overwritten with `AAAAAA`, and the fields
/// derived from those bytes — the `name` and `head` directory checksums and
/// `head.checkSumAdjustment` — are zeroed. Every other byte is compared
/// strictly, so equal keys prove the fonts differ at most in their random
/// subset tag. Returns None when the data does not parse as TrueType, in
/// which case no merging is attempted.
fn truetype_tag_neutralized(data: &[u8]) -> Option<Vec<u8>> {
    let version = be32(data, 0)?;
    if version != 0x0001_0000 && version != u32::from_be_bytes(*b"true") {
        return None;
    }
    let num_tables = be16(data, 4)? as usize;
    let mut out = data.to_vec();
    let mut name = None;
    let mut head = None;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        let tag = data.get(rec..rec + 4)?;
        let off = be32(data, rec + 8)? as usize;
        let len = be32(data, rec + 12)? as usize;
        if off.checked_add(len)? > data.len() {
            return None;
        }
        match tag {
            b"name" => {
                out[rec + 4..rec + 8].fill(0);
                name = Some((off, len));
            }
            b"head" => {
                if len < 12 {
                    return None;
                }
                out[rec + 4..rec + 8].fill(0);
                head = Some(off);
            }
            _ => {}
        }
    }
    if let Some(off) = head {
        out[off + 8..off + 12].fill(0); // checkSumAdjustment
    }
    let (noff, nlen) = name?;
    if nlen < 6 {
        return None;
    }
    let count = be16(data, noff + 2)? as usize;
    let str_base = noff + be16(data, noff + 4)? as usize;
    for i in 0..count {
        let rec = noff + 6 + i * 12;
        let len = be16(data, rec + 8)? as usize;
        let off = be16(data, rec + 10)? as usize;
        let start = str_base.checked_add(off)?;
        let end = start.checked_add(len)?;
        // Strings pointing outside the name table are left as-is; they then
        // participate in the strict byte comparison instead.
        if end > (noff + nlen).min(data.len()) {
            continue;
        }
        neutralize_subset_tag(&mut out[start..end]);
    }
    Some(out)
}

/// If `s` starts with a six-uppercase-letter subset tag followed by '+'
/// (ASCII or UTF-16BE), overwrite the tag letters with 'A'.
fn neutralize_subset_tag(s: &mut [u8]) {
    if s.len() >= 7 && s[..6].iter().all(u8::is_ascii_uppercase) && s[6] == b'+' {
        s[..6].fill(b'A');
    } else if s.len() >= 14
        && s[..14].chunks(2).all(|c| c[0] == 0)
        && s[1..12].iter().step_by(2).all(u8::is_ascii_uppercase)
        && s[13] == b'+'
    {
        for i in 0..6 {
            s[i * 2 + 1] = b'A';
        }
    }
}

fn be16(data: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(off..off + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(off..off + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal TrueType font: head + name tables, one MacRoman name record
    /// carrying `<tag>+Foo`, plus a fake glyf table with arbitrary bytes.
    fn tiny_font(tag: &[u8; 6], checksum_seed: u8, glyf: &[u8]) -> Vec<u8> {
        let mut name_table = Vec::new();
        let string = [tag.as_slice(), b"+Foo"].concat();
        name_table.extend_from_slice(&0u16.to_be_bytes()); // format
        name_table.extend_from_slice(&1u16.to_be_bytes()); // count
        name_table.extend_from_slice(&18u16.to_be_bytes()); // stringOffset
        for v in [1u16, 0, 0, 6, string.len() as u16, 0] {
            name_table.extend_from_slice(&v.to_be_bytes());
        }
        name_table.extend_from_slice(&string);

        let mut head_table = vec![0u8; 54];
        head_table[8..12].copy_from_slice(&[checksum_seed; 4]); // checkSumAdjustment

        let tables: [(&[u8; 4], &[u8]); 3] =
            [(b"glyf", glyf), (b"head", &head_table), (b"name", &name_table)];
        let mut out = Vec::new();
        out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        out.extend_from_slice(&(tables.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0u8; 6]); // searchRange etc., irrelevant here
        let mut offset = 12 + tables.len() * 16;
        for (tag, data) in &tables {
            out.extend_from_slice(*tag);
            // Only the name table's checksum depends on the subset tag.
            let cksum = if *tag == b"name" { checksum_seed } else { 0xAB };
            out.extend_from_slice(&[cksum; 4]);
            out.extend_from_slice(&(offset as u32).to_be_bytes());
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            offset += data.len();
        }
        for (_tag, data) in &tables {
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn same_font_different_tag_matches() {
        let a = tiny_font(b"SKOJEB", 0x11, b"outlines");
        let b = tiny_font(b"RZEDQD", 0x22, b"outlines");
        assert_eq!(
            truetype_tag_neutralized(&a).unwrap(),
            truetype_tag_neutralized(&b).unwrap()
        );
    }

    #[test]
    fn different_outlines_do_not_match() {
        let a = tiny_font(b"SKOJEB", 0x11, b"outlines");
        let b = tiny_font(b"SKOJEB", 0x11, b"OUTLINES");
        assert_ne!(
            truetype_tag_neutralized(&a).unwrap(),
            truetype_tag_neutralized(&b).unwrap()
        );
    }

    #[test]
    fn non_truetype_is_rejected() {
        assert!(truetype_tag_neutralized(b"%!PS-AdobeFont").is_none());
        assert!(truetype_tag_neutralized(b"").is_none());
    }

    #[test]
    fn tag_neutralization_forms() {
        let mut ascii = b"SKOJEB+ArialMT".to_vec();
        neutralize_subset_tag(&mut ascii);
        assert_eq!(ascii, b"AAAAAA+ArialMT");

        let mut utf16: Vec<u8> = "SKOJEB+A".bytes().flat_map(|c| [0, c]).collect();
        neutralize_subset_tag(&mut utf16);
        assert_eq!(utf16, "AAAAAA+A".bytes().flat_map(|c| [0, c]).collect::<Vec<u8>>());

        let mut untagged = b"ArialMT".to_vec();
        neutralize_subset_tag(&mut untagged);
        assert_eq!(untagged, b"ArialMT");

        assert_eq!(
            split_subset_name(b"SKOJEB+ArialMT"),
            Some((b"SKOJEB".as_slice(), b"ArialMT".as_slice()))
        );
        assert_eq!(split_subset_name(b"ArialMT"), None);
        assert_eq!(split_subset_name(b"skojeb+ArialMT"), None);
    }
}
