//! Font-program fuzzing for the font passes (merge, strip, union, subset,
//! CFF subset, Type1 conversion), run as an ignored test:
//!
//!   FONTFUZZ_SEEDS="a.pdf b.pdf" FONTFUZZ_ITERS=2000 \
//!     cargo test --release fontfuzz -- --ignored --nocapture
//!
//! Unlike examples/mutpdf.rs, which corrupts any stream of a whole PDF, this
//! corrupts only embedded font programs (with sfnt/CFF-aware mutations) and
//! runs only the font passes, so thousands of cases reach deep into the
//! parsers per minute. A watchdog aborts on a case that runs too long or
//! whose RSS grows past a bound, printing the case to replay
//! (FONTFUZZ_ONLY=<seed file index>:<case>). Panics are caught and counted.
use crate::{cffsubset, fontunion, merge_font_subsets, subset, type1};
use lopdf::{Document, Object, ObjectId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Seed document with images emptied (the font passes never read them) and
/// the ids of its font program streams.
fn prepare(path: &str) -> Option<(Document, Vec<ObjectId>)> {
    let mut doc = Document::load_with_options(path, crate::load_options()).ok()?;
    let mut fonts = Vec::new();
    for o in doc.objects.values() {
        if let Object::Dictionary(d) = o {
            for k in [b"FontFile".as_slice(), b"FontFile2", b"FontFile3"] {
                if let Ok(Object::Reference(id)) = d.get(k) {
                    fonts.push(*id);
                }
            }
        }
    }
    fonts.sort();
    fonts.dedup();
    for o in doc.objects.values_mut() {
        if let Object::Stream(s) = o
            && s.dict.get(b"Subtype").and_then(|v| v.as_name()).ok() == Some(b"Image".as_slice())
        {
            s.set_content(Vec::new());
            s.dict.remove(b"Filter");
            s.dict.remove(b"DecodeParms");
        }
    }
    for id in &fonts {
        if let Some(Object::Stream(s)) = doc.objects.get_mut(id)
            && let Ok(data) = s.decompressed_content()
        {
            s.dict.remove(b"Filter");
            s.dict.remove(b"DecodeParms");
            s.set_content(data);
        }
    }
    (!fonts.is_empty()).then_some((doc, fonts))
}

const INTERESTING16: [u16; 8] = [0, 1, 2, 0x7FFF, 0x8000, 0xFFFF, 0x00FF, 0x0100];
const INTERESTING32: [u32; 7] = [0, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF, 0x0001_0000, 0x00FF_FFFF];

fn mutate(rng: &mut Rng, d: &mut Vec<u8>) {
    if d.is_empty() {
        return;
    }
    // Bias towards the front: sfnt table directories and CFF headers/INDEX
    // offsets live there.
    let pos = |rng: &mut Rng, len: usize| {
        if rng.below(2) == 0 { rng.below(len.min(512)) } else { rng.below(len) }
    };
    for _ in 0..1 + rng.below(4) {
        let len = d.len();
        if len == 0 {
            return;
        }
        match rng.below(8) {
            0 => {
                let p = pos(rng, len);
                d[p] ^= 1 << rng.below(8);
            }
            1 => {
                let p = pos(rng, len);
                if p + 2 <= len {
                    d[p..p + 2].copy_from_slice(&INTERESTING16[rng.below(8)].to_be_bytes());
                }
            }
            2 => {
                let p = pos(rng, len);
                if p + 4 <= len {
                    d[p..p + 4].copy_from_slice(&INTERESTING32[rng.below(7)].to_be_bytes());
                }
            }
            3 => {
                let p = pos(rng, len);
                d[p] = rng.next() as u8;
            }
            4 => d.truncate(rng.below(len)),
            5 => {
                // Copy a chunk over another place (self-similar structures).
                let (a, b) = (rng.below(len), rng.below(len));
                let l = rng.below(64).min(len - a).min(len - b);
                let chunk = d[a..a + l].to_vec();
                d[b..b + l].copy_from_slice(&chunk);
            }
            6 => {
                let p = rng.below(len);
                let l = rng.below(32);
                for _ in 0..l {
                    d.insert(p, rng.next() as u8);
                }
            }
            _ => {
                let p = rng.below(len);
                let l = rng.below(256).min(len - p);
                d.drain(p..p + l);
            }
        }
    }
}

fn rss_kb() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn run_passes(doc: &mut Document) {
    let mut log = Vec::new();
    merge_font_subsets(doc, &mut log);
    fontunion::strip_unused_font_tables(doc, &mut log);
    fontunion::union_cid_truetype_subsets(doc, &mut log);
    subset::subset_truetype_fonts(doc, &mut log, true);
    cffsubset::subset_cff_cid_fonts(doc, &mut log);
    type1::convert_type1_fonts(doc, &mut log);
}

#[test]
#[ignore]
fn fontfuzz() {
    let seeds: Vec<String> =
        std::env::var("FONTFUZZ_SEEDS").unwrap_or_default().split_whitespace().map(String::from).collect();
    let iters: usize = std::env::var("FONTFUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let only: Option<(usize, usize)> = std::env::var("FONTFUZZ_ONLY").ok().and_then(|v| {
        let (a, b) = v.split_once(':')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    let max_secs: u64 = 20;
    let max_rss_kb: u64 = 1_500_000;

    // Watchdog: current case id and its start time.
    let current = Arc::new(AtomicU64::new(u64::MAX));
    let started = Arc::new(AtomicU64::new(0));
    let epoch = Instant::now();
    {
        let (current, started) = (current.clone(), started.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let c = current.load(Ordering::SeqCst);
            if c == u64::MAX {
                continue;
            }
            let ran = epoch.elapsed().as_millis() as u64 - started.load(Ordering::SeqCst);
            let rss = rss_kb();
            if ran > max_secs * 1000 || rss > max_rss_kb {
                eprintln!("WATCHDOG case {}:{} ran {ran} ms, rss {} MB", c >> 32, c & 0xFFFF_FFFF, rss / 1024);
                std::process::abort();
            }
        });
    }
    std::panic::set_hook(Box::new(|_| {}));
    let mut panics = Vec::new();
    let mut slowest = (0u128, 0usize, 0usize);
    for (si, path) in seeds.iter().enumerate() {
        let Some((base, fonts)) = prepare(path) else {
            eprintln!("{path}: no font programs, skipped");
            continue;
        };
        let t0 = Instant::now();
        for i in 0..iters {
            if only.is_some_and(|(a, b)| (a, b) != (si, i)) {
                continue;
            }
            let mut rng = Rng(((si as u64) << 40 ^ i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut doc = base.clone();
            for _ in 0..1 + rng.below(3) {
                let id = fonts[rng.below(fonts.len())];
                if let Some(Object::Stream(s)) = doc.objects.get_mut(&id) {
                    let mut data = std::mem::take(&mut s.content);
                    mutate(&mut rng, &mut data);
                    if rng.below(8) == 0 {
                        let k = [b"Length1".as_slice(), b"Length2", b"Length3"][rng.below(3)];
                        s.dict.set(k, [0i64, 1, 6, -1, 1 << 30][rng.below(5)]);
                    }
                    s.set_content(data);
                }
            }
            current.store((si as u64) << 32 | i as u64, Ordering::SeqCst);
            started.store(epoch.elapsed().as_millis() as u64, Ordering::SeqCst);
            let c0 = Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_passes(&mut doc)));
            let el = c0.elapsed().as_millis();
            if el > slowest.0 {
                slowest = (el, si, i);
            }
            current.store(u64::MAX, Ordering::SeqCst);
            if let Err(e) = r {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                eprintln!("PANIC case {si}:{i}: {msg}");
                panics.push((si, i, msg));
            }
        }
        eprintln!("{path}: {} fonts, {iters} cases in {:.1}s", fonts.len(), t0.elapsed().as_secs_f64());
    }
    eprintln!("slowest case {}:{} {} ms; peak rss now {} MB", slowest.1, slowest.2, slowest.0, rss_kb() / 1024);
    let _ = std::panic::take_hook();
    assert!(panics.is_empty(), "{} panics", panics.len());
}
