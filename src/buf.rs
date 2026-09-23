//! Minecraft `FriendlyByteBuf` wire primitives.
//!
//! Everything here parses untrusted bytes off the network, so every read is
//! bounds-checked and returns a `Result`. A panic inside a WASM plugin traps the
//! whole instance, so there is no `unwrap` in this module.

// A complete set of wire primitives; handlers land incrementally and each one
// needs a different subset.
#![allow(dead_code)]

use core::fmt;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// Ran off the end of the buffer.
    Eof,
    /// A VarInt/VarLong used more continuation bytes than the type allows.
    VarTooLong,
    /// A string field was not valid UTF-8.
    BadUtf8,
    /// A length-prefixed field declared more bytes than we are willing to allocate.
    TooLarge {
        what: &'static str,
        len: usize,
        max: usize,
    },
    /// Handler finished before consuming the whole packet.
    Trailing(usize),
    /// Well-formed bytes that are not the value the field needs.
    Unexpected(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eof => f.write_str("unexpected end of buffer"),
            Self::VarTooLong => f.write_str("varint too long"),
            Self::BadUtf8 => f.write_str("invalid utf-8"),
            Self::TooLarge { what, len, max } => {
                write!(f, "{what} too large: {len} > {max}")
            }
            Self::Trailing(n) => write!(f, "{n} trailing bytes left unread"),
            Self::Unexpected(what) => f.write_str(what),
        }
    }
}

/// Upper bound on a single string field, matching vanilla's 32767-char limit
/// (worst case 3 bytes per char, plus the terminator slack vanilla allows).
const MAX_STRING_BYTES: usize = 32767 * 3;

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Errors unless the whole buffer was consumed. Axiom kicks the player when a
    /// handler leaves bytes behind, so mirror that check at every call site.
    pub const fn expect_fully_read(&self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(Error::Trailing(self.remaining()))
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Eof)?;
        let slice = self.data.get(self.pos..end).ok_or(Error::Eof)?;
        self.pos = end;
        Ok(slice)
    }

    /// The rest of the buffer, consuming it.
    pub fn rest(&mut self) -> &'a [u8] {
        let slice = &self.data[self.pos..];
        self.pos = self.data.len();
        slice
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.pos).ok_or(Error::Eof)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn i8(&mut self) -> Result<i8> {
        self.u8().map(|b| b as i8)
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn i16(&mut self) -> Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u16(&mut self) -> Result<u16> {
        self.i16().map(|v| v as u16)
    }

    pub fn i32(&mut self) -> Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    pub fn f32(&mut self) -> Result<f32> {
        self.i32().map(f32::from_bits_signed)
    }

    pub fn f64(&mut self) -> Result<f64> {
        self.i64().map(|v| f64::from_bits(v as u64))
    }

    pub fn var_i32(&mut self) -> Result<i32> {
        let mut value: i32 = 0;
        for shift in 0..5 {
            let byte = self.u8()?;
            value |= i32::from(byte & 0x7F) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::VarTooLong)
    }

    /// A VarInt used as a count or length. Negative values are rejected rather
    /// than wrapping into a huge `usize`.
    pub fn var_len(&mut self, what: &'static str, max: usize) -> Result<usize> {
        let raw = self.var_i32()?;
        let len = usize::try_from(raw).map_err(|_| Error::TooLarge {
            what,
            len: 0,
            max,
        })?;
        if len > max {
            return Err(Error::TooLarge { what, len, max });
        }
        Ok(len)
    }

    pub fn var_i64(&mut self) -> Result<i64> {
        let mut value: i64 = 0;
        for shift in 0..10 {
            let byte = self.u8()?;
            value |= i64::from(byte & 0x7F) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::VarTooLong)
    }

    pub fn string(&mut self) -> Result<&'a str> {
        let len = self.var_len("string", MAX_STRING_BYTES)?;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| Error::BadUtf8)
    }

    /// A length-prefixed byte array.
    pub fn byte_array(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = self.var_len("byte array", max)?;
        self.take(len)
    }

    pub fn uuid(&mut self) -> Result<(u64, u64)> {
        let high = self.i64()? as u64;
        let low = self.i64()? as u64;
        Ok((high, low))
    }

    pub fn block_pos(&mut self) -> Result<(i32, i32, i32)> {
        Ok(unpack_block_pos(self.i64()?))
    }

    /// Reads the next byte without consuming it.
    pub fn peek_u8(&self) -> Result<u8> {
        self.data.get(self.pos).copied().ok_or(Error::Eof)
    }

    #[must_use]
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Everything between `start` and the current position.
    pub fn since(&self, start: usize) -> Result<&'a [u8]> {
        self.data.get(start..self.pos).ok_or(Error::Eof)
    }
}

/// Vanilla packs a block position into a long as 26 bits of X, 26 of Z and 12 of Y,
/// each signed. Axiom uses the same packing for section keys in a block buffer.
#[must_use]
pub const fn unpack_block_pos(v: i64) -> (i32, i32, i32) {
    (
        (v >> 38) as i32,
        ((v << 52) >> 52) as i32,
        ((v << 26) >> 38) as i32,
    )
}

#[must_use]
pub const fn pack_block_pos(x: i32, y: i32, z: i32) -> i64 {
    ((x as i64 & 0x3FF_FFFF) << 38) | ((z as i64 & 0x3FF_FFFF) << 12) | (y as i64 & 0xFFF)
}

trait FromBitsSigned {
    fn from_bits_signed(v: i32) -> Self;
}

impl FromBitsSigned for f32 {
    fn from_bits_signed(v: i32) -> Self {
        Self::from_bits(v as u32)
    }
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            buf: Vec::with_capacity(n),
        }
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.bytes(&v.to_be_bytes())
    }

    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    pub fn var_i32(&mut self, v: i32) -> &mut Self {
        let mut value = v as u32;
        loop {
            if value & !0x7F == 0 {
                self.buf.push(value as u8);
                return self;
            }
            self.buf.push((value as u8 & 0x7F) | 0x80);
            value >>= 7;
        }
    }

    pub fn var_i64(&mut self, v: i64) -> &mut Self {
        let mut value = v as u64;
        loop {
            if value & !0x7F == 0 {
                self.buf.push(value as u8);
                return self;
            }
            self.buf.push((value as u8 & 0x7F) | 0x80);
            value >>= 7;
        }
    }

    pub fn string(&mut self, v: &str) -> &mut Self {
        self.var_i32(v.len() as i32).bytes(v.as_bytes())
    }

    pub fn byte_array(&mut self, v: &[u8]) -> &mut Self {
        self.var_i32(v.len() as i32).bytes(v)
    }

    pub fn uuid(&mut self, high: u64, low: u64) -> &mut Self {
        self.i64(high as i64).i64(low as i64)
    }

    pub fn block_pos(&mut self, x: i32, y: i32, z: i32) -> &mut Self {
        self.i64(pack_block_pos(x, y, z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Axiom asserts this exact bit pattern at startup; if our packing disagrees,
    /// every section key in a block buffer lands in the wrong place.
    #[test]
    fn block_pos_packing_matches_vanilla() {
        const MIN_POSITION_LONG: i64 =
            0b1000000000000000000000000010000000000000000000000000100000000000_u64 as i64;
        assert_eq!(pack_block_pos(-33_554_432, -2048, -33_554_432), MIN_POSITION_LONG);
        assert_eq!(unpack_block_pos(MIN_POSITION_LONG), (-33_554_432, -2048, -33_554_432));
    }

    #[test]
    fn block_pos_roundtrips_signed_components() {
        for pos in [(0, 0, 0), (1, -1, 1), (-30_000_000, 319, 30_000_000), (17, -64, -17)] {
            let (x, y, z) = pos;
            assert_eq!(unpack_block_pos(pack_block_pos(x, y, z)), pos);
        }
    }

    #[test]
    fn varint_roundtrips() {
        for v in [0, 1, -1, 127, 128, i32::MAX, i32::MIN] {
            let mut w = Writer::new();
            w.var_i32(v);
            let bytes = w.into_vec();
            assert_eq!(Reader::new(&bytes).var_i32(), Ok(v), "value {v}");
        }
    }

    #[test]
    fn reads_are_bounds_checked() {
        assert_eq!(Reader::new(&[]).i32(), Err(Error::Eof));
        assert_eq!(Reader::new(&[0xFF; 3]).i32(), Err(Error::Eof));
        assert_eq!(Reader::new(&[0xFF; 6]).var_i32(), Err(Error::VarTooLong));
        // A negative length must not wrap into a huge allocation.
        let mut r = Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        assert!(matches!(r.var_len("test", 16), Err(Error::TooLarge { .. })));
    }
}
