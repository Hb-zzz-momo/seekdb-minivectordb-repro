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

pub fn crc32c(data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the CPU supports SSE4.2, checked just above.
            return unsafe { crc32c_sse42(data) };
        }
    }
    crc32c_sw(data)
}

fn crc32c_sw(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};
    let mut c = !0u32 as u64;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        c = _mm_crc32_u64(c, u64::from_le_bytes(*w));
    }
    let mut c = c as u32;
    for &b in rest {
        c = _mm_crc32_u8(c, b);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_sw(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(crc32c(&long), crc32c_sw(&long));
    }
}
