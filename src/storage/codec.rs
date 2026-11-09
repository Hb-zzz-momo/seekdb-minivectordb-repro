//! Little-endian fixed-width and LEB128 varint encoding for log records.

use crate::bail;
use crate::db_err;
use crate::error::DbResult;

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_i64(buf: &mut Vec<u8>, v: i64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

/// Bounds-checked cursor; every read past the end is a `Corrupt` error.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn take(&mut self, n: usize) -> DbResult<&'a [u8]> {
        if self.remaining() < n {
            bail!(Corrupt, "record truncated: need {} bytes, have {}", n, self.remaining());
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> DbResult<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> DbResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> DbResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> DbResult<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> DbResult<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> DbResult<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    pub fn varint(&mut self) -> DbResult<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                bail!(Corrupt, "varint too long");
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn bytes(&mut self) -> DbResult<&'a [u8]> {
        let n = self.varint()?;
        let n = usize::try_from(n).map_err(|_| db_err!(Corrupt, "length {} too large", n))?;
        self.take(n)
    }

    pub fn str(&mut self) -> DbResult<&'a str> {
        std::str::from_utf8(self.bytes()?).map_err(|_| db_err!(Corrupt, "invalid UTF-8 in record"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut b = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            put_varint(&mut b, v);
        }
        put_i64(&mut b, -5);
        put_str(&mut b, "héllo");
        put_u32(&mut b, 7);
        let mut r = Reader::new(&b);
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            assert_eq!(r.varint().unwrap(), v);
        }
        assert_eq!(r.i64().unwrap(), -5);
        assert_eq!(r.str().unwrap(), "héllo");
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.remaining(), 0);
        assert!(r.u8().is_err());
        assert!(Reader::new(&[0xff; 11]).varint().is_err());
        assert!(Reader::new(&[5, b'a']).str().is_err());
    }
}
