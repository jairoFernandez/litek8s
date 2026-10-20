//! Gorilla compression of one series' samples (Pelkonen et al., "Gorilla: A
//! Fast, Scalable, In-Memory Time Series Database", VLDB 2015), as
//! Prometheus's XOR chunks do it:
//!
//! - timestamps (milliseconds) as delta-of-delta in variable-width buckets:
//!   a sample on a regular grid costs one bit;
//! - values as the XOR with the previous one: an unchanged value costs one
//!   bit, a changed one only its meaningful bits.
//!
//! A chunk is `count: u16 | first t: i64 | first v: f64 | bits...`; the
//! count tells the reader where the bits end.

/// Appends bits to a byte vector, most significant bit first.
#[derive(Clone, Debug, Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 = the last byte is full or there is none).
    used: u8,
}

impl BitWriter {
    pub fn write_bit(&mut self, bit: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> self.used;
        }
        self.used = (self.used + 1) % 8;
    }

    /// The low `n` bits of `value`, most significant first.
    pub fn write_bits(&mut self, value: u64, n: u32) {
        let mut n = n;
        while n > 0 {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let free = 8 - self.used as u32;
            let take = free.min(n);
            let chunk = ((value >> (n - take)) & ((1u64 << take) - 1)) as u8;
            let last = self.bytes.len() - 1;
            self.bytes[last] |= chunk << (free - take);
            self.used = ((self.used as u32 + take) % 8) as u8;
            n -= take;
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub fn shrink(&mut self) {
        self.bytes.shrink_to_fit();
    }
}

pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        BitReader { bytes, pos: 0 }
    }

    pub fn read_bit(&mut self) -> Option<bool> {
        let byte = *self.bytes.get(self.pos / 8)?;
        let bit = byte & (0x80 >> (self.pos % 8)) != 0;
        self.pos += 1;
        Some(bit)
    }

    pub fn read_bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        let mut n = n;
        while n > 0 {
            let byte = *self.bytes.get(self.pos / 8)? as u64;
            let off = (self.pos % 8) as u32;
            let avail = 8 - off;
            let take = avail.min(n);
            let chunk = (byte >> (avail - take)) & ((1u64 << take) - 1);
            v = (v << take) | chunk;
            self.pos += take as usize;
            n -= take;
        }
        Some(v)
    }
}

/// Delta-of-delta buckets: (prefix bits, prefix length, value bits).
const DOD_BUCKETS: [(u64, u32, u32); 4] = [(0b10, 2, 14), (0b110, 3, 17), (0b1110, 4, 20), (0b1111, 4, 64)];

/// An open chunk being appended to.
#[derive(Clone, Debug, Default)]
pub struct Encoder {
    bits: BitWriter,
    count: u16,
    first_t: i64,
    first_v: u64,
    t: i64,
    delta: i64,
    v: u64,
    leading: u8,
    trailing: u8,
}

impl Encoder {
    pub fn count(&self) -> u16 {
        self.count
    }

    pub fn first_t(&self) -> i64 {
        self.first_t
    }

    pub fn last_t(&self) -> i64 {
        self.t
    }

    pub fn last_v(&self) -> f64 {
        f64::from_bits(self.v)
    }

    /// Heap bytes held.
    pub fn heap_bytes(&self) -> usize {
        self.bits.capacity()
    }

    pub fn shrink(&mut self) {
        self.bits.shrink();
    }

    /// Append a sample; `t` must be after the previous one.
    pub fn push(&mut self, t: i64, v: f64) {
        let bits = v.to_bits();
        match self.count {
            0 => {
                self.first_t = t;
                self.first_v = bits;
                self.t = t;
                self.v = bits;
                self.leading = 0xff;
            }
            1 => {
                self.delta = t - self.t;
                self.bits.write_bits(self.delta as u64, 32);
                self.t = t;
                self.write_value(bits);
            }
            _ => {
                let delta = t - self.t;
                let dod = delta - self.delta;
                if dod == 0 {
                    self.bits.write_bit(false);
                } else {
                    for (prefix, plen, vbits) in DOD_BUCKETS {
                        let fits = vbits == 64 || (-(1i64 << (vbits - 1)) + 1..=(1i64 << (vbits - 1))).contains(&dod);
                        if fits {
                            self.bits.write_bits(prefix, plen);
                            let mask = if vbits == 64 { u64::MAX } else { (1u64 << vbits) - 1 };
                            self.bits.write_bits(dod as u64 & mask, vbits);
                            break;
                        }
                    }
                }
                self.delta = delta;
                self.t = t;
                self.write_value(bits);
            }
        }
        self.count += 1;
    }

    fn write_value(&mut self, bits: u64) {
        let xor = bits ^ self.v;
        self.v = bits;
        if xor == 0 {
            self.bits.write_bit(false);
            return;
        }
        self.bits.write_bit(true);
        let leading = (xor.leading_zeros() as u8).min(31);
        let trailing = xor.trailing_zeros() as u8;
        if self.leading != 0xff && leading >= self.leading && trailing >= self.trailing {
            self.bits.write_bit(false);
            let sig = 64 - self.leading as u32 - self.trailing as u32;
            self.bits.write_bits(xor >> self.trailing, sig);
        } else {
            self.leading = leading;
            self.trailing = trailing;
            self.bits.write_bit(true);
            self.bits.write_bits(leading as u64, 5);
            let sig = 64 - leading as u32 - trailing as u32;
            // 64 significant bits do not fit in 6 bits: 0 stands for 64.
            self.bits.write_bits((sig % 64) as u64, 6);
            self.bits.write_bits(xor >> trailing, sig);
        }
    }

    /// The chunk as bytes (the encoder stays usable).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.bits.bytes().len());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.first_t.to_le_bytes());
        out.extend_from_slice(&self.first_v.to_le_bytes());
        out.extend_from_slice(self.bits.bytes());
        out
    }

    pub fn new() -> Self {
        Encoder::default()
    }
}

/// Decode a chunk into samples.
pub fn decode(chunk: &[u8], out: &mut Vec<(i64, f64)>) -> bool {
    decode_range(chunk, i64::MIN, i64::MAX, out)
}

/// Decode the samples of a chunk with `mint <= t <= maxt`.
pub fn decode_range(chunk: &[u8], mint: i64, maxt: i64, out: &mut Vec<(i64, f64)>) -> bool {
    if chunk.len() < 18 {
        return false;
    }
    let count = u16::from_le_bytes([chunk[0], chunk[1]]);
    let mut t = i64::from_le_bytes(chunk[2..10].try_into().unwrap());
    let mut v = u64::from_le_bytes(chunk[10..18].try_into().unwrap());
    if count == 0 {
        return true;
    }
    let push = |t: i64, v: u64, out: &mut Vec<(i64, f64)>| {
        if t >= mint && t <= maxt {
            out.push((t, f64::from_bits(v)));
        }
    };
    push(t, v, out);
    let mut r = BitReader::new(&chunk[18..]);
    let mut delta = 0i64;
    let (mut leading, mut trailing) = (0u32, 0u32);
    for i in 1..count {
        if t > maxt {
            return true;
        }
        if i == 1 {
            let Some(d) = r.read_bits(32) else { return false };
            delta = d as i64;
        } else {
            let mut ones = 0;
            while ones < 4 {
                match r.read_bit() {
                    Some(true) => ones += 1,
                    Some(false) => break,
                    None => return false,
                }
            }
            if ones > 0 {
                let (_, _, vbits) = DOD_BUCKETS[ones - 1];
                let Some(raw) = r.read_bits(vbits) else { return false };
                let dod = if vbits == 64 {
                    raw as i64
                } else {
                    // Sign-extend.
                    let shift = 64 - vbits;
                    ((raw << shift) as i64) >> shift
                };
                delta += dod;
            }
        }
        t += delta;
        match r.read_bit() {
            None => return false,
            Some(false) => {}
            Some(true) => {
                match r.read_bit() {
                    None => return false,
                    Some(false) => {}
                    Some(true) => {
                        let (Some(l), Some(s)) = (r.read_bits(5), r.read_bits(6)) else { return false };
                        leading = l as u32;
                        let sig = if s == 0 { 64 } else { s as u32 };
                        trailing = 64 - leading - sig;
                    }
                }
                let sig = 64 - leading - trailing;
                let Some(bits) = r.read_bits(sig) else { return false };
                v ^= bits << trailing;
            }
        }
        push(t, v, out);
    }
    true
}

/// (count, first t, last t) of an encoded chunk without decoding it all
/// (last t needs a decode; callers keep it next to the chunk instead).
pub fn header(chunk: &[u8]) -> Option<(u16, i64)> {
    if chunk.len() < 18 {
        return None;
    }
    Some((u16::from_le_bytes([chunk[0], chunk[1]]), i64::from_le_bytes(chunk[2..10].try_into().ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(samples: &[(i64, f64)]) -> usize {
        let mut e = Encoder::new();
        for (t, v) in samples {
            e.push(*t, *v);
        }
        let bytes = e.encode();
        let mut out = Vec::new();
        assert!(decode(&bytes, &mut out));
        assert_eq!(out.len(), samples.len());
        for (a, b) in out.iter().zip(samples) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.to_bits(), b.1.to_bits(), "value at {}", a.0);
        }
        bytes.len()
    }

    #[test]
    fn bits_roundtrip() {
        let mut w = BitWriter::default();
        w.write_bit(true);
        w.write_bits(0b10110, 5);
        w.write_bits(u64::MAX, 64);
        w.write_bits(3, 2);
        let mut r = BitReader::new(w.bytes());
        assert_eq!(r.read_bit(), Some(true));
        assert_eq!(r.read_bits(5), Some(0b10110));
        assert_eq!(r.read_bits(64), Some(u64::MAX));
        assert_eq!(r.read_bits(2), Some(3));
    }

    #[test]
    fn regular_constant_series_costs_about_two_bits_a_sample() {
        let samples: Vec<_> = (0..120).map(|i| (1_700_000_000_000 + i * 10_000, 42.0)).collect();
        let size = roundtrip(&samples);
        assert!(size < 18 + 4 + 1 + 32, "{size} bytes");
    }

    #[test]
    fn jittery_timestamps_and_changing_values() {
        let mut t = 1_700_000_000_000i64;
        let mut samples = Vec::new();
        let mut x = 7u64;
        for i in 0..500 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            t += 10_000 + (x >> 60) as i64 - 8 + if i == 100 { 3_600_000 } else { 0 } + if i == 200 { -9_000 } else { 0 };
            let v = match i % 5 {
                0 => (x >> 11) as f64 / 1e3,
                1 => -1.5,
                2 => f64::NAN,
                3 => f64::INFINITY,
                _ => i as f64 * 0.1,
            };
            samples.push((t, v));
        }
        roundtrip(&samples);
        // Huge gaps take the 64-bit bucket.
        roundtrip(&[(0, 1.0), (10, 2.0), (1 << 40, 3.0), ((1 << 40) + 5, 0.0)]);
        roundtrip(&[(5, 1.0)]);
        roundtrip(&[]);
    }

    #[test]
    fn counter_compresses() {
        let samples: Vec<_> = (0..120).map(|i| (i * 10_000, (i * 1234) as f64)).collect();
        let size = roundtrip(&samples);
        assert!(size < 120 * 3, "{size}");
    }

    #[test]
    fn range_decode() {
        let mut e = Encoder::new();
        for i in 0..10 {
            e.push(i * 10, i as f64);
        }
        let mut out = Vec::new();
        decode_range(&e.encode(), 25, 60, &mut out);
        assert_eq!(out.iter().map(|s| s.0).collect::<Vec<_>>(), vec![30, 40, 50, 60]);
    }
}
