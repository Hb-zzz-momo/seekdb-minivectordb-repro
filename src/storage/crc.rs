//! CRC-32C (Castagnoli), hardware-accelerated with SSE4.2 when available.

const POLY: u32 = 0x82F6_3B78;

static TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// A CRC-32C over data that arrives in pieces: `update` with each piece in
/// order, then `finish`. The result does not depend on where the pieces are
/// cut.
#[derive(Debug, Clone, Copy)]
pub struct Crc32c(u32);

impl Default for Crc32c {
    fn default() -> Self {
        Crc32c(!0)
    }
}

impl Crc32c {
    pub fn new() -> Crc32c {
        Crc32c::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        #[cfg(target_arch = "x86_64")]
        {
            if std::is_x86_feature_detected!("sse4.2") {
                // SAFETY: the CPU supports SSE4.2, checked just above.
                self.0 = unsafe { update_sse42(self.0, data) };
                return;
            }
        }
        self.0 = update_sw(self.0, data);
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = Crc32c::new();
    c.update(data);
    c.finish()
}

fn update_sw(mut c: u32, data: &[u8]) -> u32 {
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn update_sse42(c: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut c = c as u64;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        c = _mm_crc32_u64(c, u64::from_le_bytes(*w));
    }
    let mut c = c as u32;
    for &b in rest {
        c = _mm_crc32_u8(c, b);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32c_sw(data: &[u8]) -> u32 {
        !update_sw(!0, data)
    }

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_sw(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(crc32c(&long), crc32c_sw(&long));
    }

    /// Wherever the data is cut, in two pieces or many, the sum is the same.
    #[test]
    fn pieces_give_the_same_sum() {
        let data: Vec<u8> = (0..100u32).map(|i| (i * 31 + 7) as u8).collect();
        let whole = crc32c(&data);
        for cut in 0..=data.len() {
            let mut c = Crc32c::new();
            c.update(&data[..cut]);
            c.update(&data[cut..]);
            assert_eq!(c.finish(), whole, "cut at {cut}");
        }
        for step in [1, 3, 7, 8, 9, 64] {
            let mut c = Crc32c::new();
            data.chunks(step).for_each(|piece| c.update(piece));
            assert_eq!(c.finish(), whole, "pieces of {step}");
        }
    }
}
