//! Reading an input by mapping it rather than copying it.
//!
//! `std::fs::read` allocates a buffer and copies the file into it. For a link
//! that is 60 MB of copying from pages the kernel already has, and it is the
//! top of the profile: `fs::read::inner`, `read` and `open` together outweigh
//! every other named cost including hashing.
//!
//! `mmap` skips the copy. The bytes stay in the page cache and the linker reads
//! them where they are, which also means a second link of the same unchanged
//! input costs no I/O at all rather than costing another copy — the property a
//! persistent linker needs and cannot get from `read`.
//!
//! # What this trades away
//!
//! A mapped file that changes underneath the mapping does not return an error;
//! it delivers `SIGBUS` on the next touched page, which no `Result` can carry.
//! The trade is deliberate and bounded: these are inputs a build system wrote
//! before invoking the linker and does not touch again until the link returns.
//! Every linker that maps its inputs — `ld64`, `lld`, `mold` — takes the same
//! one.
//!
//! Small files stay on the heap. A mapping costs a syscall and a page-table
//! entry, and below a page it buys nothing; `symbols.o` in a Rust link is
//! 2.7 KB.

use std::os::unix::io::AsRawFd;
use std::path::Path;

/// Files at or above this size are mapped; smaller ones are read.
const MAP_THRESHOLD: usize = 64 * 1024;

/// A read-only mapping of a whole file.
pub struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// The mapping is read-only and never aliased mutably, so sharing it across
// threads is sound. `*mut c_void` is not `Send`/`Sync` on its own, which is
// what these assertions are overriding — the pointer is an owned resource, not
// a borrow into anything else.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map `path`, or return `None` if it should be read instead.
    ///
    /// `None` rather than an error for every reason a mapping might not be the
    /// right tool — too small, not mappable, a filesystem that will not — so
    /// the caller has exactly one fallback path and no decisions to duplicate.
    pub fn open(path: &Path) -> Option<Mapping> {
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        if len < MAP_THRESHOLD {
            return None;
        }
        // SAFETY: `fd` is open for reading for the duration of the call, and
        // the kernel chooses the address. A successful return owns the region
        // until `munmap`, which `Drop` performs exactly once.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        // The descriptor is not needed once the mapping exists; the mapping
        // holds its own reference to the file.
        drop(file);
        Some(Mapping { ptr, len })
    }
}

impl std::ops::Deref for Mapping {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` and `len` describe a live mapping this value owns, and
        // the region is readable for that whole length.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region this value owns, once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

impl std::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mapping({} bytes)", self.len)
    }
}

/// A file's bytes, however they were obtained.
#[derive(Debug)]
pub enum Backing {
    Heap(Vec<u8>),
    Mapped(Mapping),
}

impl std::ops::Deref for Backing {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Backing::Heap(bytes) => bytes,
            Backing::Mapped(mapping) => mapping,
        }
    }
}

/// Read `path`, mapping it when that is worthwhile.
pub fn read(path: &Path) -> std::io::Result<Backing> {
    match Mapping::open(path) {
        Some(mapping) => Ok(Backing::Mapped(mapping)),
        None => std::fs::read(path).map(Backing::Heap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    fn write(scratch: &Scratch, name: &str, len: usize) -> std::path::PathBuf {
        let path = scratch.join(name);
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).expect("written");
        path
    }

    /// The property: mapped or read, the caller sees the same bytes.
    #[test]
    fn a_mapped_file_reads_back_exactly() {
        let scratch = Scratch::dir("mapping-large").expect("scratch");
        let path = write(&scratch, "large.bin", MAP_THRESHOLD * 3 + 17);
        let expected = std::fs::read(&path).expect("read");

        let backing = read(&path).expect("mapped");
        assert!(matches!(backing, Backing::Mapped(_)), "it was not mapped");
        assert_eq!(&*backing, &expected[..]);
    }

    /// Below the threshold it falls back, and the bytes are still right.
    #[test]
    fn a_small_file_is_read_onto_the_heap() {
        let scratch = Scratch::dir("mapping-small").expect("scratch");
        let path = write(&scratch, "small.bin", 2_700);
        let backing = read(&path).expect("read");
        assert!(matches!(backing, Backing::Heap(_)), "it was mapped");
        assert_eq!(backing.len(), 2_700);
    }

    /// An empty file has nothing to map and must not become an error —
    /// `mmap` rejects a zero length, which is the case the fallback exists for.
    #[test]
    fn an_empty_file_is_not_an_error() {
        let scratch = Scratch::dir("mapping-empty").expect("scratch");
        let path = scratch.join("empty.bin");
        std::fs::write(&path, []).expect("written");
        assert!(read(&path).expect("read").is_empty());
    }

    /// A missing file is still an error, mapped or not.
    #[test]
    fn a_missing_file_is_an_error() {
        let scratch = Scratch::dir("mapping-missing").expect("scratch");
        assert!(read(&scratch.join("nope.bin")).is_err());
    }

    /// The mapping outlives the descriptor it was made from, which is why
    /// `open` drops the file. If it did not, every input would hold an open
    /// descriptor for the length of the link.
    #[test]
    fn the_bytes_survive_the_descriptor_closing() {
        let scratch = Scratch::dir("mapping-fd").expect("scratch");
        let path = write(&scratch, "held.bin", MAP_THRESHOLD * 2);
        let backing = read(&path).expect("mapped");
        // Deleting the file leaves the mapping valid: it holds the inode.
        std::fs::remove_file(&path).expect("removed");
        assert_eq!(backing[0], 0);
        assert_eq!(backing[250], 250);
    }
}
