//! A minimal length-checked binary codec.
//!
//! Hand-rolled rather than pulled from a crate because the cache is almost
//! entirely one thing — raw patched bytes — and no general codec makes copying
//! a `Vec<u8>` faster. What a dependency *would* add is a second definition of
//! the on-disk format, in a version that moves independently of `SCHEMA`.
//!
//! Every read is bounds-checked and returns `Option`, because the input is a
//! file that may have been truncated by an interrupted link, and a decoder
//! that indexes optimistically turns that into a panic inside the linker.

pub struct Encoder(Vec<u8>);

impl Encoder {
    pub fn new() -> Self {
        Encoder(Vec::new())
    }

    pub fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }

    pub fn bytes_raw(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

pub struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Decoder { bytes, at: 0 }
    }

    pub fn bytes_raw(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(length)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes_raw(4)?.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes_raw(8)?.try_into().ok()?))
    }

    /// Whether every byte was consumed.
    ///
    /// Checked at the end of a decode: a file with bytes left over decoded
    /// *something*, but not this structure, and accepting it would mean
    /// trusting a cache written by a different version that happened to start
    /// the same way.
    pub fn at_end(&self) -> bool {
        self.at == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip_in_order() {
        let mut out = Encoder::new();
        out.u32(0xdead_beef);
        out.u64(0x0123_4567_89ab_cdef);
        out.bytes_raw(b"tail");
        let bytes = out.finish();

        let mut input = Decoder::new(&bytes);
        assert_eq!(input.u32(), Some(0xdead_beef));
        assert_eq!(input.u64(), Some(0x0123_4567_89ab_cdef));
        assert_eq!(input.bytes_raw(4), Some(&b"tail"[..]));
        assert!(input.at_end());
    }

    #[test]
    fn reading_past_the_end_returns_none_rather_than_panicking() {
        let mut input = Decoder::new(&[1, 2, 3]);
        assert_eq!(input.u64(), None);
        assert_eq!(input.bytes_raw(4), None);
        assert_eq!(input.u32(), None);
    }

    /// A length field read from a corrupt file can be enormous; adding it to
    /// the cursor must not wrap around into a valid-looking range.
    #[test]
    fn an_absurd_length_cannot_wrap_the_cursor() {
        let mut input = Decoder::new(&[0; 8]);
        assert_eq!(input.bytes_raw(usize::MAX), None);
    }

    #[test]
    fn unconsumed_bytes_are_visible_as_not_at_end() {
        let mut input = Decoder::new(&[0; 8]);
        assert_eq!(input.u32(), Some(0));
        assert!(!input.at_end());
    }
}
