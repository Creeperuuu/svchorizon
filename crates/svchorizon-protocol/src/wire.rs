use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("unexpected end of packet")]
    UnexpectedEof,
    #[error("VarInt is longer than five bytes")]
    VarIntTooLong,
    #[error("declared byte array length {declared} exceeds limit {limit}")]
    LengthLimit { declared: usize, limit: usize },
    #[error("invalid UTF-8")]
    InvalidUtf8,
    #[error("invalid boolean value {0}")]
    InvalidBoolean(u8),
    #[error("packet contains trailing bytes")]
    TrailingBytes,
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }
    /// Bytes consumed so far (to slice off the unconsumed remainder).
    pub fn offset(&self) -> usize {
        self.pos
    }
    /// Bytes left to read.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::UnexpectedEof)?;
        if end > self.data.len() {
            return Err(WireError::UnexpectedEof);
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(WireError::InvalidBoolean(v)),
        }
    }
    pub fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_be_bytes(
            self.take(4)?.try_into().expect("length checked"),
        ))
    }
    pub fn i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        ))
    }
    pub fn f32(&mut self) -> Result<f32, WireError> {
        Ok(f32::from_bits(u32::from_be_bytes(
            self.take(4)?.try_into().expect("length checked"),
        )))
    }
    pub fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_bits(u64::from_be_bytes(
            self.take(8)?.try_into().expect("length checked"),
        )))
    }
    pub fn uuid(&mut self) -> Result<Uuid, WireError> {
        Ok(Uuid::from_bytes(
            self.take(16)?.try_into().expect("length checked"),
        ))
    }
    pub fn varint(&mut self) -> Result<u32, WireError> {
        let mut value = 0u32;
        for index in 0..5 {
            let byte = self.u8()?;
            value |= u32::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(WireError::VarIntTooLong)
    }
    pub fn byte_array(&mut self, max: usize) -> Result<Vec<u8>, WireError> {
        let len = self.varint()? as usize;
        if len > max {
            return Err(WireError::LengthLimit {
                declared: len,
                limit: max,
            });
        }
        Ok(self.take(len)?.to_vec())
    }
    pub fn utf(&mut self, max_chars: usize) -> Result<String, WireError> {
        let bytes = self.byte_array(max_chars.saturating_mul(4))?;
        let value = String::from_utf8(bytes).map_err(|_| WireError::InvalidUtf8)?;
        if value.chars().count() > max_chars {
            return Err(WireError::LengthLimit {
                declared: value.chars().count(),
                limit: max_chars,
            });
        }
        Ok(value)
    }
    pub fn finish(self) -> Result<(), WireError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(WireError::TrailingBytes)
        }
    }
}

#[derive(Default)]
pub struct Writer {
    pub data: Vec<u8>,
}
impl Writer {
    pub fn u8(&mut self, value: u8) {
        self.data.push(value);
    }
    pub fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }
    pub fn i32(&mut self, value: i32) {
        self.data.extend_from_slice(&value.to_be_bytes());
    }
    pub fn i64(&mut self, value: i64) {
        self.data.extend_from_slice(&value.to_be_bytes());
    }
    pub fn f32(&mut self, value: f32) {
        self.data.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    pub fn f64(&mut self, value: f64) {
        self.data.extend_from_slice(&value.to_bits().to_be_bytes());
    }
    pub fn uuid(&mut self, value: Uuid) {
        self.data.extend_from_slice(value.as_bytes());
    }
    pub fn raw(&mut self, value: &[u8]) {
        self.data.extend_from_slice(value);
    }
    pub fn varint(&mut self, mut value: u32) {
        while value & !0x7f != 0 {
            self.u8(((value & 0x7f) as u8) | 0x80);
            value >>= 7;
        }
        self.u8(value as u8);
    }
    pub fn byte_array(&mut self, value: &[u8]) {
        self.varint(value.len() as u32);
        self.raw(value);
    }
    pub fn utf(&mut self, value: &str) {
        self.byte_array(value.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn varint_boundaries_round_trip() {
        for value in [0, 1, 127, 128, 255, 16_384, u32::MAX] {
            let mut w = Writer::default();
            w.varint(value);
            assert_eq!(Reader::new(&w.data).varint().unwrap(), value);
        }
    }
    #[test]
    fn uuid_network_byte_order() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let mut w = Writer::default();
        w.uuid(id);
        assert_eq!(w.data, 0x00112233445566778899aabbccddeeffu128.to_be_bytes());
        assert_eq!(Reader::new(&w.data).uuid().unwrap(), id);
    }
    #[test]
    fn rejects_oversized_array() {
        assert!(matches!(
            Reader::new(&[0xff, 0x7f]).byte_array(64),
            Err(WireError::LengthLimit { .. })
        ));
    }
}
