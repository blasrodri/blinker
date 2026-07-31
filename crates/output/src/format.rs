//! Mach-O on-disk structures, write side only.
//!
//! Constants and layouts come from `<mach-o/loader.h>`, cross-checked against
//! the bytes of a real Rust executable so the values are what the toolchain
//! actually emits rather than what the header file permits:
//!
//! ```text
//! cffa edfe 0c00 0001 0000 0000 0200 0000
//! 1100 0000 3808 0000 8500 a000 0000 0000
//!  ^magic    ^cputype  ^subtype ^filetype
//!            ^ncmds=17 ^sizeofcmds=2104   ^flags=0x00a00085
//! ```
//!
//! Only the write side lives here. Reading Mach-O is the `macho` crate's job,
//! and it wraps a fuzzed parser rather than hand-rolling one — but emitting is
//! a different problem: there is no untrusted input, and the bytes must match
//! an exact expected shape.

/// 64-bit Mach-O magic, little-endian on disk as `cf fa ed fe`.
pub const MH_MAGIC_64: u32 = 0xfeed_facf;

/// `CPU_TYPE_ARM | CPU_ARCH_ABI64` — 16777228 as `otool` prints it.
pub const CPU_TYPE_ARM64: u32 = 0x0100_000C;
pub const CPU_SUBTYPE_ARM64_ALL: u32 = 0;

/// A fully linked executable.
pub const MH_EXECUTE: u32 = 0x2;

// Header flags. The real binary carries 0x00a00085, which is the union below.
/// No undefined references remain.
pub const MH_NOUNDEFS: u32 = 0x1;
/// The file participates in dynamic linking.
pub const MH_DYLDLINK: u32 = 0x4;
/// Uses two-level namespace bindings.
pub const MH_TWOLEVEL: u32 = 0x80;
/// Position independent — loadable at a random base address.
pub const MH_PIE: u32 = 0x0020_0000;
/// The image defines thread-local variable descriptors.
pub const MH_HAS_TLV_DESCRIPTORS: u32 = 0x0080_0000;
/// Safe to link into an app extension. Notably **not** set by the toolchain
/// for a Rust executable, which is why it is absent from [`EXECUTABLE_FLAGS`].
pub const MH_APP_EXTENSION_SAFE: u32 = 0x0200_0000;

/// The flag set a real Rust executable carries: `0x00a00085`.
///
/// Decomposed from the real header rather than assembled from what seemed
/// plausible — an earlier version of this constant included
/// [`MH_APP_EXTENSION_SAFE`] and the byte-for-byte test caught it.
pub const EXECUTABLE_FLAGS: u32 =
    MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL | MH_PIE | MH_HAS_TLV_DESCRIPTORS;

// Load command identifiers, in the order a real binary emits them.
pub const LC_SYMTAB: u32 = 0x2;
pub const LC_DYSYMTAB: u32 = 0xb;
pub const LC_LOAD_DYLIB: u32 = 0xc;
pub const LC_LOAD_DYLINKER: u32 = 0xe;
pub const LC_UUID: u32 = 0x1b;
pub const LC_SEGMENT_64: u32 = 0x19;

/// `SG_READ_ONLY`: the segment is made read-only after its fixups are applied.
///
/// Not decorative. dyld *refuses to load* an image whose `__DATA_CONST` lacks
/// it: "__DATA_CONST segment missing SG_READ_ONLY flag". The segment exists
/// precisely so that pointers dyld has to write once can be protected
/// afterwards, and an unflagged one means the linker did not understand that.
pub const SG_READ_ONLY: u32 = 0x10;
pub const LC_CODE_SIGNATURE: u32 = 0x1d;
pub const LC_FUNCTION_STARTS: u32 = 0x26;
pub const LC_DATA_IN_CODE: u32 = 0x29;
pub const LC_SOURCE_VERSION: u32 = 0x2A;
pub const LC_BUILD_VERSION: u32 = 0x32;

/// Commands dyld must understand; it refuses to load a file carrying an
/// unrecognised one of these.
pub const LC_REQ_DYLD: u32 = 0x8000_0000;
pub const LC_DYLD_INFO_ONLY: u32 = 0x22 | LC_REQ_DYLD;
pub const LC_MAIN: u32 = 0x28 | LC_REQ_DYLD;

/// `PLATFORM_MACOS`, as carried in `LC_BUILD_VERSION`.
pub const PLATFORM_MACOS: u32 = 1;

/// The dynamic linker every macOS executable names.
pub const DYLD_PATH: &str = "/usr/lib/dyld";

// Section type and attribute bits from `<mach-o/loader.h>`.
pub const S_REGULAR: u32 = 0x0;
/// Zero-filled at load time, occupying no file bytes.
pub const S_ZEROFILL: u32 = 0x1;
/// Literal C strings, deduplicable by the linker.
pub const S_CSTRING_LITERALS: u32 = 0x2;
/// Non-lazy symbol pointers — the GOT.
pub const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
/// Lazy symbol pointers, patched on first call.
pub const S_LAZY_SYMBOL_POINTERS: u32 = 0x7;
/// Symbol stubs, the indirection a lazily-bound call goes through.
pub const S_SYMBOL_STUBS: u32 = 0x8;
/// Thread-local variable descriptors.
/// Initialised thread-local data — the template copied per thread.
pub const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
/// Zero-filled thread-local data.
pub const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
pub const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
/// Pointers to thread-local descriptors, the TLV analogue of `__got`.
pub const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;

/// Section contains executable instructions.
pub const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
/// Some machine instructions live here.
pub const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

/// A little-endian byte sink.
///
/// Mach-O is written in host order for the target, which is little-endian on
/// every platform blinker supports. Centralising the writes keeps that
/// assumption in one place rather than scattered across `to_le_bytes` calls.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Writer {
            bytes: Vec::with_capacity(capacity),
        }
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(value);
        self
    }

    /// Write a fixed-width name field, truncating and zero-padding.
    ///
    /// Mach-O section and segment names are exactly 16 bytes, NUL-padded but
    /// *not* NUL-terminated when they fill the field. Truncating rather than
    /// erroring matches the toolchain, which does the same.
    pub fn name16(&mut self, name: &str) -> &mut Self {
        let mut field = [0u8; 16];
        let source = name.as_bytes();
        let take = source.len().min(16);
        field[..take].copy_from_slice(&source[..take]);
        self.bytes(&field)
    }

    /// Write a NUL-terminated string padded to a 4-byte boundary.
    ///
    /// Used by the load commands that carry a path; the padding keeps the
    /// following command aligned.
    pub fn c_string_padded(&mut self, value: &str) -> &mut Self {
        self.bytes(value.as_bytes());
        self.bytes(&[0]);
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        self
    }

    /// Pad with zeroes until the length is a multiple of `alignment`.
    pub fn align_to(&mut self, alignment: usize) -> &mut Self {
        while alignment > 0 && !self.bytes.len().is_multiple_of(alignment) {
            self.bytes.push(0);
        }
        self
    }

    /// Pad with zeroes until exactly `length` bytes have been written.
    ///
    /// Returns `false` if more than `length` bytes are already present, which
    /// means a size was computed wrongly upstream — a caller must not silently
    /// carry on with an overlong region.
    pub fn pad_to(&mut self, length: usize) -> bool {
        if self.bytes.len() > length {
            return false;
        }
        self.bytes.resize(length, 0);
        true
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Overwrite four bytes at `offset`, for fields patched after the fact.
    ///
    /// Sizes that are only known once later content is written — `ncmds`,
    /// `sizeofcmds` — are reserved and then filled in here.
    pub fn patch_u32(&mut self, offset: usize, value: u32) -> bool {
        let Some(slice) = self.bytes.get_mut(offset..offset + 4) else {
            return false;
        };
        slice.copy_from_slice(&value.to_le_bytes());
        true
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

/// The Mach-O header, 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachHeader {
    pub cpu_type: u32,
    pub cpu_subtype: u32,
    pub file_type: u32,
    pub command_count: u32,
    pub command_size: u32,
    pub flags: u32,
}

impl MachHeader {
    /// Size on disk. The header is fixed-width, so this is a constant.
    pub const SIZE: usize = 32;

    /// A header for an arm64 executable.
    pub fn executable() -> Self {
        MachHeader {
            cpu_type: CPU_TYPE_ARM64,
            cpu_subtype: CPU_SUBTYPE_ARM64_ALL,
            file_type: MH_EXECUTE,
            // Filled in once the load commands have been emitted.
            command_count: 0,
            command_size: 0,
            flags: EXECUTABLE_FLAGS,
        }
    }

    pub fn write(&self, writer: &mut Writer) {
        writer
            .u32(MH_MAGIC_64)
            .u32(self.cpu_type)
            .u32(self.cpu_subtype)
            .u32(self.file_type)
            .u32(self.command_count)
            .u32(self.command_size)
            .u32(self.flags)
            // `reserved`, which must be zero.
            .u32(0);
    }

    /// Byte offset of `ncmds` within the header, for patching.
    pub const COMMAND_COUNT_OFFSET: usize = 16;
    /// Byte offset of `sizeofcmds` within the header, for patching.
    pub const COMMAND_SIZE_OFFSET: usize = 20;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_little_endian_scalars() {
        let mut w = Writer::new();
        w.u32(0x1234_5678);
        assert_eq!(w.as_slice(), &[0x78, 0x56, 0x34, 0x12]);

        let mut w = Writer::new();
        w.u64(0x0102_0304_0506_0708);
        assert_eq!(w.as_slice(), &[8, 7, 6, 5, 4, 3, 2, 1]);
    }

    /// The exact 32 bytes a real Rust executable starts with.
    #[test]
    fn header_matches_a_real_binary_byte_for_byte() {
        let mut header = MachHeader::executable();
        header.command_count = 17;
        header.command_size = 2104;

        let mut w = Writer::new();
        header.write(&mut w);

        let expected: [u8; 32] = [
            0xcf, 0xfa, 0xed, 0xfe, // magic
            0x0c, 0x00, 0x00, 0x01, // cputype  = 0x0100000C
            0x00, 0x00, 0x00, 0x00, // cpusubtype
            0x02, 0x00, 0x00, 0x00, // filetype = MH_EXECUTE
            0x11, 0x00, 0x00, 0x00, // ncmds = 17
            0x38, 0x08, 0x00, 0x00, // sizeofcmds = 2104
            0x85, 0x00, 0xa0, 0x00, // flags = 0x00a00085
            0x00, 0x00, 0x00, 0x00, // reserved
        ];
        assert_eq!(w.as_slice(), &expected);
    }

    #[test]
    fn header_is_exactly_thirty_two_bytes() {
        let mut w = Writer::new();
        MachHeader::executable().write(&mut w);
        assert_eq!(w.len(), MachHeader::SIZE);
    }

    /// The flag set must be exactly what the toolchain emits; a missing MH_PIE
    /// would silently disable address-space randomisation.
    #[test]
    fn executable_flags_match_the_real_value() {
        // The exact value a real Rust executable carries, and the exact set
        // of bits that composes it. Losing MH_PIE here would silently disable
        // address-space randomisation.
        assert_eq!(EXECUTABLE_FLAGS, 0x00a0_0085);
        assert_eq!(
            EXECUTABLE_FLAGS,
            MH_NOUNDEFS | MH_DYLDLINK | MH_TWOLEVEL | MH_PIE | MH_HAS_TLV_DESCRIPTORS
        );
        assert_eq!(
            EXECUTABLE_FLAGS & MH_APP_EXTENSION_SAFE,
            0,
            "the toolchain does not set MH_APP_EXTENSION_SAFE"
        );
    }

    #[test]
    fn names_are_padded_to_sixteen_bytes() {
        let mut w = Writer::new();
        w.name16("__text");
        assert_eq!(w.len(), 16);
        assert_eq!(&w.as_slice()[..6], b"__text");
        assert!(w.as_slice()[6..].iter().all(|&b| b == 0));
    }

    /// A name filling the field is not NUL-terminated — truncation matches the
    /// toolchain rather than being an error.
    #[test]
    fn names_longer_than_the_field_are_truncated() {
        let mut w = Writer::new();
        w.name16("__a_very_long_section_name");
        assert_eq!(w.len(), 16);
        assert_eq!(&w.as_slice()[..16], b"__a_very_long_se");
    }

    #[test]
    fn c_strings_are_terminated_and_padded_to_four_bytes() {
        let mut w = Writer::new();
        w.c_string_padded(DYLD_PATH);
        // 13 bytes + NUL = 14, padded to 16.
        assert_eq!(w.len(), 16);
        assert_eq!(&w.as_slice()[..13], DYLD_PATH.as_bytes());
        assert_eq!(w.as_slice()[13], 0);
    }

    #[test]
    fn a_string_already_on_a_boundary_still_gets_its_terminator() {
        let mut w = Writer::new();
        w.c_string_padded("abc");
        assert_eq!(w.len(), 4, "3 chars + NUL is already aligned");
        assert_eq!(w.as_slice(), b"abc\0");
    }

    #[test]
    fn align_to_pads_with_zeroes() {
        let mut w = Writer::new();
        w.bytes(&[1, 2, 3]).align_to(8);
        assert_eq!(w.len(), 8);
        assert_eq!(&w.as_slice()[3..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn align_to_is_a_no_op_when_already_aligned() {
        let mut w = Writer::new();
        w.u64(0).align_to(8);
        assert_eq!(w.len(), 8);
    }

    #[test]
    fn pad_to_extends_to_the_requested_length() {
        let mut w = Writer::new();
        w.u32(1);
        assert!(w.pad_to(16));
        assert_eq!(w.len(), 16);
    }

    /// An overlong region means a size was computed wrongly upstream. Silently
    /// carrying on would produce a file whose load commands disagree with its
    /// contents.
    #[test]
    fn pad_to_refuses_to_shrink() {
        let mut w = Writer::new();
        w.u64(0).u64(0);
        assert!(!w.pad_to(8), "should refuse to truncate");
        assert_eq!(w.len(), 16, "content must be left intact");
    }

    #[test]
    fn patching_rewrites_a_reserved_field() {
        // ncmds and sizeofcmds are only known after the commands are emitted.
        let mut w = Writer::new();
        MachHeader::executable().write(&mut w);

        assert!(w.patch_u32(MachHeader::COMMAND_COUNT_OFFSET, 17));
        assert!(w.patch_u32(MachHeader::COMMAND_SIZE_OFFSET, 2104));

        assert_eq!(&w.as_slice()[16..20], &17u32.to_le_bytes());
        assert_eq!(&w.as_slice()[20..24], &2104u32.to_le_bytes());
    }

    #[test]
    fn patching_past_the_end_fails_rather_than_growing_the_buffer() {
        let mut w = Writer::new();
        w.u32(0);
        assert!(!w.patch_u32(100, 1));
        assert_eq!(w.len(), 4);
    }

    #[test]
    fn dyld_required_commands_carry_the_required_bit() {
        // dyld refuses to load a file carrying an unrecognised LC_REQ_DYLD
        // command, which is exactly why these two set it.
        assert_eq!(LC_MAIN & LC_REQ_DYLD, LC_REQ_DYLD);
        assert_eq!(LC_DYLD_INFO_ONLY & LC_REQ_DYLD, LC_REQ_DYLD);
        assert_eq!(LC_SYMTAB & LC_REQ_DYLD, 0);
    }
}
