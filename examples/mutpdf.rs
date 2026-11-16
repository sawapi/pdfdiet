//! Structure-preserving corruption of a PDF's streams, for robustness runs.
//!
//! usage: cargo run --release --example mutpdf -- <in.pdf> <outdir> <n> <seed>
//!
//! Writes n loadable PDFs whose stream contents (images, fonts, content,
//! ...) or a few dictionary numbers (Width, Height, BitsPerComponent,
//! Length1, N, palette hival) are corrupted. Run pdfdiet over them and check
//! that nothing panics or hangs, and with tools/pdfcmp.swift that the output
//! opens and extracts the same text as the corrupted input: in Nov 2026 this
//! found a lenient content parse that let --subset-outlines drop glyphs a
//! viewer still draws, and Type1 programs whose Length1-3 disagreed with
//! their data being converted differently from how viewers read them.
use lopdf::{Document, Object, ObjectId};
struct Rng(u64);
impl Rng { fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 } fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize } }
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (src, out, n, seed) = (&a[1], &a[2], a[3].parse::<usize>().unwrap(), a[4].parse::<u64>().unwrap());
    let base = std::path::Path::new(src).file_stem().unwrap().to_string_lossy().to_string();
    let orig = Document::load(src).unwrap();
    let streams: Vec<ObjectId> = orig.objects.iter().filter(|(_, o)| matches!(o, Object::Stream(_))).map(|(id, _)| *id).collect();
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    for i in 0..n {
        let mut doc = orig.clone();
        let mut what = vec![];
        for _ in 0..1 + rng.below(3) {
            let id = streams[rng.below(streams.len())];
            let Some(Object::Stream(s)) = doc.objects.get_mut(&id) else { continue };
            let mut data = s.decompressed_content().unwrap_or_else(|_| s.content.clone());
            match rng.below(6) {
                0 => { for _ in 0..1 + rng.below(30) { if !data.is_empty() { let p = rng.below(data.len()); data[p] ^= 1 << rng.below(8); } } what.push("flip"); }
                1 => { let k = rng.below(data.len().max(1)); data.truncate(k); what.push("trunc"); }
                2 => { let extra = rng.below(5000); for _ in 0..extra { data.push(rng.next() as u8); } what.push("extend"); }
                3 => { if !data.is_empty() { let p = rng.below(data.len()); let l = rng.below(256).min(data.len() - p); for b in &mut data[p..p + l] { *b = 0xFF; } } what.push("ffrun"); }
                4 => { let keys = [b"Width".as_slice(), b"Height", b"BitsPerComponent", b"Length1", b"N"]; let k = keys[rng.below(keys.len())]; if s.dict.get(k).is_ok() { let v = [0i64, 1, 2, 3, 7, 255, 65535, 100000, -1][rng.below(9)]; s.dict.set(k, v); } what.push("dictnum"); }
                _ => { if let Ok(Object::Array(arr)) = s.dict.get_mut(b"ColorSpace") && let Some(x) = arr.get_mut(2) { *x = Object::Integer([0i64, 1, 255, 256, -5][rng.below(5)]); }
                    if let Ok(Object::Dictionary(dp)) = s.dict.get_mut(b"DecodeParms") { dp.set("Columns", [0i64, 1, 3, 100000][rng.below(4)]); dp.set("Colors", [0i64, 1, 3, 4, 9][rng.below(4)]); } what.push("params"); }
            }
            s.dict.remove(b"Filter"); s.dict.remove(b"DecodeParms"); s.set_content(data);
        }
        let path = format!("{out}/{base}_{i:03}_{}.pdf", what.join("-"));
        if doc.save(&path).is_err() { let _ = std::fs::remove_file(&path); }
    }
}
