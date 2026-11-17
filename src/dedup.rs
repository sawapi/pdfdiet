//! Merging byte-identical streams and value dictionaries, and pointing
//! references at the survivors.

use std::collections::HashMap;

use lopdf::{Dictionary, Document, Object, ObjectId};

/// Dictionary types that are pure values: they are only ever reached by
/// following a key or a resource name, never addressed by identity, so two
/// byte-identical ones are interchangeable.
///
/// This has to stay an allowlist. For most dictionary types object identity
/// is itself semantic: two byte-identical `/Type /OCG` dicts are two
/// independently toggleable layers, two identical `/Type /Page` dicts are two
/// pages (merging them makes `/Kids` hold the same reference twice and breaks
/// every `/Dest` aimed at the second one), and annotations, form fields and
/// named destinations are all addressed by reference. Merging any of those
/// silently changes what the document means, however identical their bytes.
const MERGEABLE_DICT_TYPES: [&[u8]; 5] = [
    b"Group",          // transparency group attributes, a `/Group` value
    b"ExtGState",      // graphics state parameters, named from /Resources
    b"Font",           // selected by resource name
    b"FontDescriptor", // a `/FontDescriptor` value
    b"Encoding",       // an `/Encoding` value
];

/// May two byte-identical copies of this dictionary be collapsed into one?
fn dict_is_mergeable(dict: &Dictionary) -> bool {
    matches!(dict.get(b"Type"),
        Ok(Object::Name(n)) if MERGEABLE_DICT_TYPES.contains(&n.as_slice()))
}

/// Streams are content, and content is safe to share — a page's `/Contents`,
/// an image, a font program are all addressed by name or by a single key.
/// The exceptions are streams that *are* file structure: merging two object
/// streams would drop every object the loser carried.
fn stream_is_mergeable(dict: &Dictionary) -> bool {
    !matches!(dict.get(b"Type"),
        Ok(Object::Name(n)) if n == b"ObjStm" || n == b"XRef" || n == b"Linearized")
}

/// Append a canonical, injective byte encoding of `obj` to `out`.
///
/// `format!("{obj:?}")` cannot be used for this. lopdf renders Name and String
/// payloads through `String::from_utf8_lossy`, so `Name(b"\x80")` and
/// `Name(b"\x81")` both print as `/\u{FFFD}`, and `Integer(100000000)` prints
/// exactly like `Real(1e8)`. Colliding keys make dedup merge unrelated objects
/// and redirect references to the wrong survivor. Every variant here gets a
/// distinct tag byte and every variable-length payload a length prefix, so
/// distinct objects always produce distinct keys.
fn write_object_key(obj: &Object, out: &mut Vec<u8>) {
    match obj {
        Object::Null => out.push(0),
        Object::Boolean(b) => out.extend_from_slice(&[1, *b as u8]),
        Object::Integer(i) => {
            out.push(2);
            out.extend_from_slice(&i.to_be_bytes());
        }
        // Bit pattern, not the decimal rendering: 1e8f32 must not key like
        // Integer(100000000), and -0.0 must not key like 0.0.
        Object::Real(r) => {
            out.push(3);
            out.extend_from_slice(&r.to_bits().to_be_bytes());
        }
        Object::Name(n) => {
            out.push(4);
            write_bytes_key(n, out);
        }
        Object::String(s, format) => {
            out.push(5);
            out.push(matches!(format, lopdf::StringFormat::Hexadecimal) as u8);
            write_bytes_key(s, out);
        }
        Object::Array(a) => {
            out.push(6);
            out.extend_from_slice(&(a.len() as u64).to_be_bytes());
            for item in a {
                write_object_key(item, out);
            }
        }
        Object::Dictionary(d) => {
            out.push(7);
            write_dict_key(d, out);
        }
        Object::Stream(s) => {
            out.push(8);
            write_dict_key(&s.dict, out);
            write_bytes_key(&s.content, out);
        }
        Object::Reference((number, generation)) => {
            out.push(9);
            out.extend_from_slice(&number.to_be_bytes());
            out.extend_from_slice(&generation.to_be_bytes());
        }
    }
}

pub(crate) fn write_dict_key(dict: &Dictionary, out: &mut Vec<u8>) {
    out.extend_from_slice(&(dict.len() as u64).to_be_bytes());
    for (key, value) in dict {
        write_bytes_key(key, out);
        write_object_key(value, out);
    }
}

fn write_bytes_key(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Merge byte-identical stream objects (duplicated fonts, images, content
/// streams — common when figures are included as PDFs that each embed their
/// own copy of the same font) and rewrite every reference to the survivor.
/// Runs to a fixpoint so chains of duplicates (a stream whose dict references
/// another duplicated stream) also collapse.
///
/// Plain dictionaries are merged too, but only for the handful of types where
/// object identity provably carries no meaning — see `MERGEABLE_DICT_TYPES`.
pub(crate) fn dedup_streams(doc: &mut Document) -> (usize, i64) {
    let mut merged_total = 0usize;
    let mut saved_total = 0i64;
    for _pass in 0..4 {
        let mut canonical: HashMap<Vec<u8>, ObjectId> = HashMap::new();
        let mut remap: HashMap<ObjectId, ObjectId> = HashMap::new();
        let mut ids: Vec<ObjectId> = doc.objects.keys().copied().collect();
        ids.sort();
        for id in ids {
            // Objects referencing different sub-objects produce different keys,
            // so this can never merge objects with diverging dependencies.
            // Iterating to a fixpoint lets merges cascade: once two Group dicts
            // collapse into one, the form streams referencing them can become
            // identical too.
            let mut key = Vec::new();
            match doc.objects.get(&id) {
                Some(obj @ Object::Stream(stream)) if stream_is_mergeable(&stream.dict) => {
                    write_object_key(obj, &mut key)
                }
                Some(obj @ Object::Dictionary(dict)) if dict_is_mergeable(dict) => {
                    write_object_key(obj, &mut key)
                }
                _ => continue,
            }
            if let Some(&keep) = canonical.get(&key) {
                remap.insert(id, keep);
                if let Some(Object::Stream(s)) = doc.objects.get(&id) {
                    saved_total += s.content.len() as i64;
                }
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
    (merged_total, saved_total)
}

pub(crate) fn remap_refs(obj: &mut Object, remap: &HashMap<ObjectId, ObjectId>) {
    match obj {
        Object::Reference(r) => {
            if let Some(n) = remap.get(r) {
                *r = *n;
            }
        }
        Object::Array(a) => {
            for o in a.iter_mut() {
                remap_refs(o, remap);
            }
        }
        Object::Dictionary(d) => remap_refs_in_dict(d, remap),
        Object::Stream(s) => remap_refs_in_dict(&mut s.dict, remap),
        _ => {}
    }
}

pub(crate) fn remap_refs_in_dict(dict: &mut Dictionary, remap: &HashMap<ObjectId, ObjectId>) {
    for (_key, value) in dict.iter_mut() {
        remap_refs(value, remap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(obj: &Object) -> Vec<u8> {
        let mut out = Vec::new();
        write_object_key(obj, &mut out);
        out
    }

    fn typed_dict(type_name: &[u8], key: &str, value: Object) -> Object {
        let mut dict = Dictionary::new();
        dict.set("Type", Object::Name(type_name.to_vec()));
        dict.set(key, value);
        Object::Dictionary(dict)
    }

    /// The dedup key must separate objects the Debug rendering cannot.
    #[test]
    fn dedup_key_is_injective_where_debug_is_not() {
        // from_utf8_lossy maps every invalid byte to the same replacement char.
        let a = Object::Name(vec![0x80]);
        let b = Object::Name(vec![0x81]);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        let a = Object::String(vec![0x80], lopdf::StringFormat::Literal);
        let b = Object::String(vec![0x81], lopdf::StringFormat::Literal);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        // Integer and Real print identically for values with no fraction.
        let a = Object::Integer(100_000_000);
        let b = Object::Real(1e8);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_ne!(key_of(&a), key_of(&b));

        // Same bytes, different framing: the length prefixes keep them apart.
        let mut ab_c = Dictionary::new();
        ab_c.set("ab", Object::Name(b"c".to_vec()));
        let mut a_bc = Dictionary::new();
        a_bc.set("a", Object::Name(b"bc".to_vec()));
        assert_ne!(
            key_of(&Object::Dictionary(ab_c)),
            key_of(&Object::Dictionary(a_bc))
        );

        // The property dedup relies on: differing sub-references differ.
        assert_ne!(
            key_of(&Object::Reference((1, 0))),
            key_of(&Object::Reference((1, 1)))
        );
    }

    /// Two byte-identical `/Type /OCG` dicts are two independently toggleable
    /// layers. Merging them silently turns two layers into one.
    #[test]
    fn identity_bearing_dicts_are_not_merged() {
        for type_name in [b"OCG".as_slice(), b"Page", b"Annot", b"StructElem"] {
            let mut doc = Document::new();
            doc.objects
                .insert((1, 0), typed_dict(type_name, "Name", Object::string_literal("x")));
            doc.objects
                .insert((2, 0), typed_dict(type_name, "Name", Object::string_literal("x")));
            let mut owner = Dictionary::new();
            owner.set(
                "Kids",
                Object::Array(vec![Object::Reference((1, 0)), Object::Reference((2, 0))]),
            );
            doc.objects.insert((3, 0), Object::Dictionary(owner));

            let (merged, _) = dedup_streams(&mut doc);
            assert_eq!(merged, 0, "{} must not be merged", String::from_utf8_lossy(type_name));
            assert!(doc.objects.contains_key(&(2, 0)));
        }
    }

    /// Non-UTF-8 names used to collapse to one replacement character, so two
    /// different graphics states merged and half the page got the wrong one.
    #[test]
    fn dicts_differing_only_in_a_non_utf8_byte_survive() {
        let mut doc = Document::new();
        doc.objects
            .insert((1, 0), typed_dict(b"ExtGState", "BM", Object::Name(vec![0x80])));
        doc.objects
            .insert((2, 0), typed_dict(b"ExtGState", "BM", Object::Name(vec![0x81])));
        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 0);
        assert!(doc.objects.contains_key(&(2, 0)));
    }

    /// The win the allowlist exists to keep: 2,555 identical transparency
    /// group dicts in one paper's worth of figures.
    #[test]
    fn value_dicts_and_streams_still_merge() {
        let mut doc = Document::new();
        let group = || {
            let mut d = Dictionary::new();
            d.set("Type", Object::Name(b"Group".to_vec()));
            d.set("S", Object::Name(b"Transparency".to_vec()));
            Object::Dictionary(d)
        };
        doc.objects.insert((1, 0), group());
        doc.objects.insert((2, 0), group());
        let mut form = Dictionary::new();
        form.set("Group", Object::Reference((2, 0)));
        doc.objects
            .insert((3, 0), Object::Stream(lopdf::Stream::new(form, b"x".to_vec())));

        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 1);
        assert!(!doc.objects.contains_key(&(2, 0)));
        let Some(Object::Stream(s)) = doc.objects.get(&(3, 0)) else {
            panic!("form stream gone")
        };
        assert_eq!(s.dict.get(b"Group").unwrap(), &Object::Reference((1, 0)));
    }

    /// Object streams are file structure, not content: merging two would drop
    /// every object the loser carried.
    #[test]
    fn structural_streams_are_not_merged() {
        let mut doc = Document::new();
        for id in [(1, 0), (2, 0)] {
            let mut dict = Dictionary::new();
            dict.set("Type", Object::Name(b"ObjStm".to_vec()));
            doc.objects
                .insert(id, Object::Stream(lopdf::Stream::new(dict, b"payload".to_vec())));
        }
        let (merged, _) = dedup_streams(&mut doc);
        assert_eq!(merged, 0);
    }
}
