//! CCITT Group 4 (ITU-T T.6) encoder for bilevel images.
//!
//! Implemented clean-room from the T.4/T.6 specifications. Only what pdfdiet
//! needs: pure two-dimensional (MMR) coding — the K = -1 case of PDF's
//! CCITTFaxDecode — terminated by an end-of-facsimile-block marker.
//!
//! Input rows are packed MSB-first with each row starting on a byte boundary
//! (the PDF image sample layout), with bit 1 meaning white. That matches
//! 1-bit DeviceGray samples and CCITTFaxDecode's default BlackIs1 = false,
//! so a decode of the output reproduces the input bits exactly.

/// MSB-first bit accumulator.
struct BitWriter {
    out: Vec<u8>,
    acc: u8,
    used: u8,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            used: 0,
        }
    }

    /// Append the `len` low bits of `bits`, most significant first.
    fn put(&mut self, bits: u32, len: u8) {
        for i in (0..len).rev() {
            self.acc = (self.acc << 1) | ((bits >> i) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.out.push(self.acc);
                self.acc = 0;
                self.used = 0;
            }
        }
    }

    /// Zero-pad the final partial byte.
    fn finish(mut self) -> Vec<u8> {
        if self.used > 0 {
            self.out.push(self.acc << (8 - self.used));
        }
        self.out
    }
}

/// (code length in bits, code bits) — T.4 modified Huffman run-length codes.
type Code = (u8, u16);

/// Terminating codes for white runs 0-63.
const WHITE_TERM: [Code; 64] = [
    (8, 0b00110101),
    (6, 0b000111),
    (4, 0b0111),
    (4, 0b1000),
    (4, 0b1011),
    (4, 0b1100),
    (4, 0b1110),
    (4, 0b1111),
    (5, 0b10011),
    (5, 0b10100),
    (5, 0b00111),
    (5, 0b01000),
    (6, 0b001000),
    (6, 0b000011),
    (6, 0b110100),
    (6, 0b110101),
    (6, 0b101010),
    (6, 0b101011),
    (7, 0b0100111),
    (7, 0b0001100),
    (7, 0b0001000),
    (7, 0b0010111),
    (7, 0b0000011),
    (7, 0b0000100),
    (7, 0b0101000),
    (7, 0b0101011),
    (7, 0b0010011),
    (7, 0b0100100),
    (7, 0b0011000),
    (8, 0b00000010),
    (8, 0b00000011),
    (8, 0b00011010),
    (8, 0b00011011),
    (8, 0b00010010),
    (8, 0b00010011),
    (8, 0b00010100),
    (8, 0b00010101),
    (8, 0b00010110),
    (8, 0b00010111),
    (8, 0b00101000),
    (8, 0b00101001),
    (8, 0b00101010),
    (8, 0b00101011),
    (8, 0b00101100),
    (8, 0b00101101),
    (8, 0b00000100),
    (8, 0b00000101),
    (8, 0b00001010),
    (8, 0b00001011),
    (8, 0b01010010),
    (8, 0b01010011),
    (8, 0b01010100),
    (8, 0b01010101),
    (8, 0b00100100),
    (8, 0b00100101),
    (8, 0b01011000),
    (8, 0b01011001),
    (8, 0b01011010),
    (8, 0b01011011),
    (8, 0b01001010),
    (8, 0b01001011),
    (8, 0b00110010),
    (8, 0b00110011),
    (8, 0b00110100),
];

/// Make-up codes for white runs 64, 128, ..., 1728.
const WHITE_MAKEUP: [Code; 27] = [
    (5, 0b11011),
    (5, 0b10010),
    (6, 0b010111),
    (7, 0b0110111),
    (8, 0b00110110),
    (8, 0b00110111),
    (8, 0b01100100),
    (8, 0b01100101),
    (8, 0b01101000),
    (8, 0b01100111),
    (9, 0b011001100),
    (9, 0b011001101),
    (9, 0b011010010),
    (9, 0b011010011),
    (9, 0b011010100),
    (9, 0b011010101),
    (9, 0b011010110),
    (9, 0b011010111),
    (9, 0b011011000),
    (9, 0b011011001),
    (9, 0b011011010),
    (9, 0b011011011),
    (9, 0b010011000),
    (9, 0b010011001),
    (9, 0b010011010),
    (6, 0b011000),
    (9, 0b010011011),
];

/// Terminating codes for black runs 0-63.
const BLACK_TERM: [Code; 64] = [
    (10, 0b0000110111),
    (3, 0b010),
    (2, 0b11),
    (2, 0b10),
    (3, 0b011),
    (4, 0b0011),
    (4, 0b0010),
    (5, 0b00011),
    (6, 0b000101),
    (6, 0b000100),
    (7, 0b0000100),
    (7, 0b0000101),
    (7, 0b0000111),
    (8, 0b00000100),
    (8, 0b00000111),
    (9, 0b000011000),
    (10, 0b0000010111),
    (10, 0b0000011000),
    (10, 0b0000001000),
    (11, 0b00001100111),
    (11, 0b00001101000),
    (11, 0b00001101100),
    (11, 0b00000110111),
    (11, 0b00000101000),
    (11, 0b00000010111),
    (11, 0b00000011000),
    (12, 0b000011001010),
    (12, 0b000011001011),
    (12, 0b000011001100),
    (12, 0b000011001101),
    (12, 0b000001101000),
    (12, 0b000001101001),
    (12, 0b000001101010),
    (12, 0b000001101011),
    (12, 0b000011010010),
    (12, 0b000011010011),
    (12, 0b000011010100),
    (12, 0b000011010101),
    (12, 0b000011010110),
    (12, 0b000011010111),
    (12, 0b000001101100),
    (12, 0b000001101101),
    (12, 0b000011011010),
    (12, 0b000011011011),
    (12, 0b000001010100),
    (12, 0b000001010101),
    (12, 0b000001010110),
    (12, 0b000001010111),
    (12, 0b000001100100),
    (12, 0b000001100101),
    (12, 0b000001010010),
    (12, 0b000001010011),
    (12, 0b000000100100),
    (12, 0b000000110111),
    (12, 0b000000111000),
    (12, 0b000000100111),
    (12, 0b000000101000),
    (12, 0b000001011000),
    (12, 0b000001011001),
    (12, 0b000000101011),
    (12, 0b000000101100),
    (12, 0b000001011010),
    (12, 0b000001100110),
    (12, 0b000001100111),
];

/// Make-up codes for black runs 64, 128, ..., 1728.
const BLACK_MAKEUP: [Code; 27] = [
    (10, 0b0000001111),
    (12, 0b000011001000),
    (12, 0b000011001001),
    (12, 0b000001011011),
    (12, 0b000000110011),
    (12, 0b000000110100),
    (12, 0b000000110101),
    (13, 0b0000001101100),
    (13, 0b0000001101101),
    (13, 0b0000001001010),
    (13, 0b0000001001011),
    (13, 0b0000001001100),
    (13, 0b0000001001101),
    (13, 0b0000001110010),
    (13, 0b0000001110011),
    (13, 0b0000001110100),
    (13, 0b0000001110101),
    (13, 0b0000001110110),
    (13, 0b0000001110111),
    (13, 0b0000001010010),
    (13, 0b0000001010011),
    (13, 0b0000001010100),
    (13, 0b0000001010101),
    (13, 0b0000001011010),
    (13, 0b0000001011011),
    (13, 0b0000001100100),
    (13, 0b0000001100101),
];

/// Extended make-up codes for runs 1792, 1856, ..., 2560 (either colour).
const EXT_MAKEUP: [Code; 13] = [
    (11, 0b00000001000),
    (11, 0b00000001100),
    (11, 0b00000001101),
    (12, 0b000000010010),
    (12, 0b000000010011),
    (12, 0b000000010100),
    (12, 0b000000010101),
    (12, 0b000000010110),
    (12, 0b000000010111),
    (12, 0b000000011100),
    (12, 0b000000011101),
    (12, 0b000000011110),
    (12, 0b000000011111),
];

/// Emit a run of `len` pixels of one colour as make-up + terminating codes.
fn put_run(w: &mut BitWriter, mut len: usize, white: bool) {
    // Runs beyond the largest make-up code repeat it; each repetition must
    // still be followed by more code, which the loop structure guarantees
    // (the final iteration always ends with a terminating code).
    while len > 2560 + 63 {
        let (n, bits) = EXT_MAKEUP[12];
        w.put(bits as u32, n);
        len -= 2560;
    }
    if len >= 64 {
        let steps = len / 64; // 1..=40
        let (n, bits) = if steps <= 27 {
            if white {
                WHITE_MAKEUP[steps - 1]
            } else {
                BLACK_MAKEUP[steps - 1]
            }
        } else {
            EXT_MAKEUP[steps - 28]
        };
        w.put(bits as u32, n);
        len -= steps * 64;
    }
    let (n, bits) = if white {
        WHITE_TERM[len]
    } else {
        BLACK_TERM[len]
    };
    w.put(bits as u32, n);
}

/// Positions where the row changes colour (a pixel differing from the one to
/// its left, with an imaginary white pixel before position 0). Because rows
/// notionally start white, the change at even index switches to black and at
/// odd index back to white.
fn changing_elements(row: &[u8], width: usize) -> Vec<usize> {
    let mut changes = Vec::new();
    let mut white = true;
    for p in 0..width {
        let bit_white = (row[p / 8] >> (7 - p % 8)) & 1 == 1;
        if bit_white != white {
            changes.push(p);
            white = bit_white;
        }
    }
    changes
}

/// First changing element strictly right of `a0` whose new colour is the
/// opposite of the current colour (`width` when there is none). Changes
/// alternate colours, so at most the immediately following entry needs to be
/// skipped to fix the parity.
fn next_opposite(changes: &[usize], a0: isize, cur_white: bool, width: usize) -> usize {
    let want_even = cur_white; // even-index changes switch to black
    let k = changes.partition_point(|&p| (p as isize) <= a0);
    let k = if (k % 2 == 0) == want_even { k } else { k + 1 };
    changes.get(k).copied().unwrap_or(width)
}

/// First changing element strictly right of `pos` (`width` when there is none).
fn next_after(changes: &[usize], pos: usize, width: usize) -> usize {
    let k = changes.partition_point(|&p| p <= pos);
    changes.get(k).copied().unwrap_or(width)
}

/// Code one line against its reference line (T.6 two-dimensional coding).
fn code_line(w: &mut BitWriter, cur: &[usize], reference: &[usize], width: usize) {
    let mut a0: isize = -1; // imaginary white pixel just before the row
    let mut white = true;
    loop {
        let a1 = next_opposite(cur, a0, white, width);
        let b1 = next_opposite(reference, a0, white, width);
        let b2 = if b1 >= width {
            width
        } else {
            next_after(reference, b1, width)
        };
        if b2 < a1 {
            // Pass mode: the reference run ends before our change starts.
            w.put(0b0001, 4);
            a0 = b2 as isize;
        } else {
            let d = a1 as isize - b1 as isize;
            if d.abs() <= 3 {
                // Vertical mode: V0 / VR1-3 / VL1-3.
                let (n, bits) = match d {
                    0 => (1, 0b1),
                    1 => (3, 0b011),
                    2 => (6, 0b000011),
                    3 => (7, 0b0000011),
                    -1 => (3, 0b010),
                    -2 => (6, 0b000010),
                    _ => (7, 0b0000010),
                };
                w.put(bits, n);
                a0 = a1 as isize;
                white = !white;
            } else {
                // Horizontal mode: two explicit run lengths.
                let a2 = if a1 >= width {
                    width
                } else {
                    next_after(cur, a1, width)
                };
                w.put(0b001, 3);
                let start = if a0 < 0 { 0 } else { a0 as usize };
                put_run(w, a1 - start, white);
                put_run(w, a2 - a1, !white);
                a0 = a2 as isize;
            }
        }
        if a0 >= width as isize {
            break;
        }
    }
}

/// Encode packed 1-bit rows (bit 1 = white, rows byte-aligned, MSB first) as
/// a CCITT G4 stream, terminated by EOFB. `data` must hold at least
/// `height` rows of `ceil(width / 8)` bytes.
#[cfg(test)]
pub fn encode_g4(data: &[u8], width: usize, height: usize) -> Vec<u8> {
    encode_g4_within(data, width, height, usize::MAX).unwrap()
}

/// [`encode_g4`], giving up (`None`) once the output reaches `limit` bytes:
/// the encoding can be several times larger than the packed input for noisy
/// rows, and a result that is not smaller is discarded anyway.
pub fn encode_g4_within(data: &[u8], width: usize, height: usize, limit: usize) -> Option<Vec<u8>> {
    // Real assert: this precondition guards every row slice below, and the
    // callers validate it — release builds must not skip the check.
    assert!(width > 0 && data.len() >= width.div_ceil(8) * height);
    let stride = width.div_ceil(8);
    let mut w = BitWriter::new();
    let mut reference: Vec<usize> = Vec::new(); // imaginary all-white line
    for row in 0..height {
        let cur = changing_elements(&data[row * stride..(row + 1) * stride], width);
        code_line(&mut w, &cur, &reference, width);
        reference = cur;
        if w.out.len() >= limit {
            return None;
        }
    }
    // EOFB: two end-of-line codes.
    w.put(0b000000000001, 12);
    w.put(0b000000000001, 12);
    Some(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The vectors below are worked out by hand from the T.4/T.6 code tables.

    #[test]
    fn all_white_row() {
        // V0 (a1 = b1 = imaginary change at width), then EOFB:
        // 1 | 000000000001 | 000000000001
        assert_eq!(encode_g4(&[0xFF], 8, 1), vec![0x80, 0x08, 0x00, 0x80]);
    }

    #[test]
    fn all_black_row() {
        // Horizontal: 001, white run 0 (00110101), black run 8 (000101), EOFB.
        assert_eq!(
            encode_g4(&[0x00], 8, 1),
            vec![0x26, 0xA2, 0x80, 0x08, 0x00, 0x80]
        );
    }

    #[test]
    fn identical_rows_use_vertical_mode() {
        // Second row equal to the first codes as V0 per changing element.
        // Row: 4 white, 4 black (0xF0). Row 1: H(white 4, black 4);
        // row 2: V0 at the black edge, V0 at the imaginary width change.
        // 001 1011 011 | 1 1 | EOFB
        let bits = encode_g4(&[0xF0, 0xF0], 8, 2);
        assert_eq!(
            bits,
            vec![0b00110110, 0b11110000, 0b00000001, 0b00000000, 0b00010000]
        );
    }

    #[test]
    fn terminates_on_arbitrary_input() {
        // Pseudo-random rows: must terminate and stay within sane bounds.
        let mut data = Vec::new();
        let mut x: u32 = 0x12345678;
        for _ in 0..64 * 100 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((x >> 24) as u8);
        }
        let out = encode_g4(&data, 512, 100);
        assert!(!out.is_empty());
    }
}
