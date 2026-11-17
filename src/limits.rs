//! Bounds on the work done for untrusted input: stream inflation, content
//! parsing and form recursion.

use lopdf::content::Content;
use lopdf::{Document, Object, ObjectId};

use crate::resolve;

/// Upper bound for inflating any one stream whose expected size is not
/// known in advance (font programs, content streams, JPEG data inside a
/// Flate wrapper), and for loading object streams. Deflate alone expands up
/// to ~1000x and filter chains multiply that, so every inflation is bounded;
/// real streams stay far below this.
pub(crate) const MAX_INFLATE: usize = 256 << 20;

/// Upper bound on the bytes a whole pass keeps inflated at once (the font
/// passes hold every candidate program side by side).
pub(crate) const MAX_PASS_BYTES: usize = 1 << 30;

/// Upper bound on one content stream's inflated size in the walkers (the
/// largest real page seen is 4.7 MB).
pub(crate) const MAX_CONTENT: usize = 32 << 20;
/// Parse cost (see `parse_cost`) a walk may hold at once: a page plus the
/// forms open inside it. lopdf takes ~650 bytes per parsed operation and
/// ~250-370 per array element, far more than the source (780k "q"
/// operations, 1.6 MB, peak at ~500 MB), so this, not the byte size, bounds
/// the walkers' memory. The costliest real page seen (two nested forms)
/// holds 0.41M.
pub(crate) const MAX_LIVE_COST: usize = 800_000;
/// Parse cost one walk of a document may spend in total, counting every
/// invocation of a reused form. Repeated large parses also grow the heap
/// (fragmentation, ~40-65 bytes per operation parsed), so this bounds
/// memory as well as time. The costliest real document seen spends 1.5M;
/// past this, pages take the walkers' conservative fallbacks.
pub(crate) const MAX_WALK_COST: usize = 6_000_000;

/// Upper estimate of what lopdf's parse of content bytes costs, in
/// operations: one per operator-like token (a run starting with a letter,
/// ' or "), half one per token or delimiter inside an array or dictionary,
/// an eighth one per other operand token (measured: "0 0 m" costs what "q"
/// does, while array elements take ~300 bytes each). String contents and inline image
/// data count too, which only overestimates.
fn parse_cost(data: &[u8]) -> usize {
    let (mut ops, mut in_array, mut other) = (0usize, 0usize, 0usize);
    let mut depth = 0usize;
    let mut in_run = false;
    let operand = |depth: usize, in_array: &mut usize, other: &mut usize| {
        if depth > 0 { *in_array += 1 } else { *other += 1 }
    };
    for &b in data {
        match b {
            b' ' | b'\n' | b'\r' | b'\t' | b'\x0c' | 0 => in_run = false,
            b'/' => {
                operand(depth, &mut in_array, &mut other);
                in_run = true; // the name's characters follow
            }
            // Arrays and dictionaries (and, overestimating, hex strings).
            b'[' | b']' | b'<' | b'>' => {
                operand(depth, &mut in_array, &mut other);
                depth = if matches!(b, b'[' | b'<') { depth + 1 } else { depth.saturating_sub(1) };
                in_run = false;
            }
            b'(' | b')' | b'{' | b'}' | b'%' => {
                operand(depth, &mut in_array, &mut other);
                in_run = false;
            }
            _ => {
                if !in_run {
                    if depth == 0 && (b.is_ascii_alphabetic() || b == b'\'' || b == b'"') {
                        ops += 1;
                    } else {
                        operand(depth, &mut in_array, &mut other);
                    }
                }
                in_run = true;
            }
        }
    }
    ops + in_array.div_ceil(2) + other.div_ceil(8)
}

/// Budgets for one content-stream walk of a document.
pub(crate) struct Walk {
    /// Form/glyph-procedure invocations left.
    pub(crate) forms: u32,
    live: usize,
    parsed: usize,
}

/// What one parse holds open in a Walk, to give back with `close`.
pub(crate) struct Opened(usize);

impl Walk {
    pub(crate) fn new(forms: u32) -> Self {
        Walk { forms, live: 0, parsed: 0 }
    }

    /// Start a new page: nothing is open.
    pub(crate) fn page(&mut self) {
        self.live = 0;
    }

    /// Parse content bytes within both bounds. On success the cost counts
    /// as open until `close` (callers that abandon the walk need not).
    pub(crate) fn parse(&mut self, data: &[u8]) -> Option<(Content, Opened)> {
        let cost = parse_cost(data);
        let live = self.live.checked_add(cost).filter(|&v| v <= MAX_LIVE_COST)?;
        let parsed = self.parsed.checked_add(cost).filter(|&v| v <= MAX_WALK_COST)?;
        let content = Content::decode_strict(data).ok()?;
        (self.live, self.parsed) = (live, parsed);
        Some((content, Opened(cost)))
    }

    /// Inflate and parse a form or glyph-procedure stream.
    pub(crate) fn parse_stream(&mut self, stream: &lopdf::Stream) -> Option<(Content, Opened)> {
        self.parse(&inflate(stream, MAX_CONTENT).ok()?)
    }

    pub(crate) fn close(&mut self, opened: Opened) {
        self.live -= opened.0;
    }
}

/// Load options for untrusted input: object and cross-reference streams
/// are inflated with the shared bound.
pub fn load_options() -> lopdf::LoadOptions {
    lopdf::LoadOptions::with_max_decompressed_size(MAX_INFLATE)
}

/// Inflate a stream, refusing output beyond `limit` bytes.
pub(crate) fn inflate(stream: &lopdf::Stream, limit: usize) -> lopdf::Result<Vec<u8>> {
    stream.decompressed_content_with_limit(limit)
}

/// A page's content streams, concatenated, inflated with MAX_CONTENT
/// applied to each stream and to the total (a Contents array may name the
/// same stream any number of times). None if anything cannot be read.
pub(crate) fn page_content_bounded(doc: &Document, page_id: ObjectId) -> Option<Vec<u8>> {
    let page = doc.get_dictionary(page_id).ok()?;
    let mut data = Vec::new();
    let Ok(contents) = page.get(b"Contents") else { return Some(data) };
    let streams: Vec<&Object> = match resolve(doc, contents).ok()? {
        Object::Array(a) => a.iter().collect(),
        one => vec![one],
    };
    for s in streams {
        let Object::Stream(s) = resolve(doc, s).ok()? else { return None };
        data.extend(inflate(s, MAX_CONTENT).ok()?);
        data.push(b'\n');
        if data.len() > MAX_CONTENT {
            return None;
        }
    }
    Some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    use lopdf::Dictionary;

    #[test]
    fn content_walks_are_bounded_by_parse_cost() {
        // A 2 MB form of 1M "q" operations parses to ~650 MB: over the live
        // bound on its own, so the walk stops before lopdf parses it.
        let q = b"q\n".repeat(1_000_000);
        assert!(parse_cost(&q) > MAX_LIVE_COST);
        assert!(Walk::new(1).parse(&q).is_none());
        // Operators count one, array elements (and brackets) half, other
        // operands an eighth: 2 + 4/2 + 3/8 rounded up.
        assert_eq!(parse_cost(b"/Fm12 Do 0 0 m [1 2]"), 2 + 2 + 1);
        // A form under the live bound but invoked over and over: the total
        // bound stops the walk after a dozen parses, not 50k.
        let form = b"q\n".repeat(500_000);
        let mut walk = Walk::new(u32::MAX);
        let mut n = 0;
        while let Some((_c, opened)) = walk.parse(&form) {
            walk.close(opened);
            n += 1;
            assert!(n < 50_000);
        }
        assert_eq!(n, MAX_WALK_COST / 500_000);
    }

    #[test]
    fn inflation_is_bounded() {
        let mut st = lopdf::Stream::new(Dictionary::new(), vec![0u8; 1 << 20]);
        st.compress().unwrap();
        assert!(inflate(&st, 1 << 20).is_ok());
        assert!(inflate(&st, 1000).is_err());
    }
}
