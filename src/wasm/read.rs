//! Byte reader for the WebAssembly binary format — LEB128 integers, floats, names. Every read is checked; a
//! truncated or malformed module is an error, never a panic.

use super::Error;

pub(super) type R<T> = Result<T, Error>;

#[derive(Clone)]
pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    pub fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    pub fn u8(&mut self) -> R<u8> {
        let b = *self.bytes.get(self.pos).ok_or_else(|| Error::new("unexpected end"))?;
        self.pos += 1;
        Ok(b)
    }

    pub fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| Error::new("unexpected end"))?;
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// A sub-reader over the next `n` bytes (a section or a function body).
    pub fn sub(&mut self, n: usize) -> R<Reader<'a>> {
        Ok(Reader::new(self.bytes(n)?))
    }

    fn uleb(&mut self, bits: u32) -> R<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            let left = bits - shift;
            if left < 7 && (b & 0x80 != 0 || (b as u32) >> left != 0) {
                return Err(Error::new("integer too large"));
            }
            result |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn sleb(&mut self, bits: u32) -> R<i64> {
        let mut result = 0i64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            let payload = b & 0x7F;
            let left = bits - shift;
            if left < 7 {
                // The last byte: no continuation, and the bits past the integer's width repeat its sign bit.
                let mask = (0x7Fu8 << (left - 1)) & 0x7F;
                if b & 0x80 != 0 || (payload & mask != 0 && payload & mask != mask) {
                    return Err(Error::new("integer too large"));
                }
            }
            result |= (payload as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && payload & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
    }

    pub fn u32(&mut self) -> R<u32> {
        Ok(self.uleb(32)? as u32)
    }

    /// A count of things that each take at least one more byte — bounded by what's left, so a lying count
    /// can't make us allocate much.
    pub fn count(&mut self) -> R<usize> {
        let n = self.u32()? as usize;
        if n > self.remaining() {
            return Err(Error::new("count larger than the section"));
        }
        Ok(n)
    }

    pub fn s32(&mut self) -> R<i32> {
        Ok(self.sleb(32)? as i32)
    }

    pub fn s33(&mut self) -> R<i64> {
        self.sleb(33)
    }

    pub fn s64(&mut self) -> R<i64> {
        self.sleb(64)
    }

    pub fn f32(&mut self) -> R<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().expect("4 bytes")))
    }

    pub fn f64(&mut self) -> R<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().expect("8 bytes")))
    }

    pub fn name(&mut self) -> R<&'a str> {
        let n = self.u32()? as usize;
        std::str::from_utf8(self.bytes(n)?).map_err(|_| Error::new("malformed UTF-8 name"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s32(b: &[u8]) -> R<i32> {
        Reader::new(b).s32()
    }
    fn u32(b: &[u8]) -> R<u32> {
        Reader::new(b).u32()
    }

    #[test]
    fn leb128_edges() {
        assert_eq!(u32(&[0xE5, 0x8E, 0x26]).unwrap(), 624485);
        assert_eq!(u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).unwrap(), u32::MAX);
        assert!(u32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]).is_err(), "bits past 32");
        assert!(u32(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]).is_err(), "too long");
        assert_eq!(s32(&[0x7F]).unwrap(), -1);
        assert_eq!(s32(&[0x80, 0x80, 0x80, 0x80, 0x78]).unwrap(), i32::MIN);
        assert_eq!(s32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]).unwrap(), i32::MAX);
        assert!(s32(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]).is_err(), "unused bits disagree with the sign");
        assert_eq!(
            Reader::new(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7F]).s64().unwrap(),
            i64::MIN
        );
        assert!(Reader::new(&[0x80]).u32().is_err(), "truncated");
    }
}
