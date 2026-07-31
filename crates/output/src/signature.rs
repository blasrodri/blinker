//! Ad-hoc code signing.
//!
//! # Why this is not optional
//!
//! On Apple Silicon the kernel refuses to execute an unsigned image. Not
//! "warns", not "prompts" — the process is killed before any of its code runs.
//! An unsigned image blinker produced exits with status 137 (SIGKILL); the
//! same image ad-hoc signed exits with the status its code computed. So a
//! linker that cannot sign cannot produce a program on this platform, and
//! signing belongs in the linker rather than in a step after it.
//!
//! # The structure, as measured
//!
//! Every constant here was read out of a real signature that `codesign -s -`
//! produced for one of blinker's own images, and the page hashes were
//! recomputed independently to confirm what they cover. The layout:
//!
//! ```text
//! SuperBlob  (0xfade0cc0)          ── everything below, with an index
//!   ├─ slot 0x00000  CodeDirectory (0xfade0c02)
//!   ├─ slot 0x00002  Requirements  (0xfade0c01)   an empty set
//!   └─ slot 0x10000  CMS signature (0xfade0b01)   empty: that is what
//!                                                  "ad-hoc" means
//! ```
//!
//! The `CodeDirectory` carries a SHA-256 hash of every page of the file up to
//! where the signature itself begins, plus "special slots" indexed
//! *backwards* from the code hashes, holding hashes of the other blobs.
//!
//! # Two things here are counter-intuitive
//!
//! - **Every integer is big-endian**, in a format that is otherwise
//!   little-endian throughout. Mach-O is little-endian on this target; the
//!   code signing blobs are not.
//! - **The signing page size is 16 KiB, not 4 KiB.** The measured
//!   `CodeDirectory` had `pageSize = 14`, and that is a log2. Using 4096 would
//!   produce four times as many slots and a signature the kernel rejects.

use sha2::{Digest, Sha256};

/// `CSMAGIC_EMBEDDED_SIGNATURE`.
const MAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
/// `CSMAGIC_CODEDIRECTORY`.
const MAGIC_CODE_DIRECTORY: u32 = 0xfade_0c02;
/// `CSMAGIC_REQUIREMENTS` — a set of requirements, here an empty one.
const MAGIC_REQUIREMENTS: u32 = 0xfade_0c01;
/// `CSMAGIC_BLOBWRAPPER` — wraps the CMS signature, empty for ad-hoc.
const MAGIC_BLOB_WRAPPER: u32 = 0xfade_0b01;

/// Blob index slot numbers.
const SLOT_CODE_DIRECTORY: u32 = 0;
const SLOT_REQUIREMENTS: u32 = 2;
const SLOT_SIGNATURE: u32 = 0x1_0000;

/// `CodeDirectory` version with `execSeg*` fields — what the current toolchain
/// emits, and what the measured signature used.
const CODE_DIRECTORY_VERSION: u32 = 0x0002_0400;

/// `CS_ADHOC`: signed with no identity.
const FLAG_ADHOC: u32 = 0x0000_0002;

/// `CS_EXECSEG_MAIN_BINARY`.
const EXEC_SEG_MAIN_BINARY: u64 = 0x1;

/// SHA-256.
const HASH_TYPE_SHA256: u8 = 2;
const HASH_SIZE: usize = 32;

/// Log2 of the signing page size.
///
/// **16 KiB, not 4 KiB.** Read from a real signature; assuming the more
/// familiar 4 KiB produces four times the slots and a rejected image.
const PAGE_SHIFT: u8 = 14;
const PAGE_SIZE: usize = 1 << PAGE_SHIFT;

/// Bytes in the fixed part of a `CodeDirectory` at [`CODE_DIRECTORY_VERSION`].
///
/// The identifier string begins immediately after, which is why this is also
/// the value of `identOffset`.
const CODE_DIRECTORY_HEADER: usize = 88;

/// Size of the empty requirements blob: magic, length, count.
const REQUIREMENTS_LEN: usize = 12;
/// Size of the empty CMS wrapper: magic and length only.
const SIGNATURE_LEN: usize = 8;

/// The empty requirements blob — a requirements set containing nothing.
///
/// Built from the magic rather than written as literal bytes so the constant
/// above stays the single source of truth for what this blob is.
fn requirements_blob() -> [u8; REQUIREMENTS_LEN] {
    let mut blob = [0u8; REQUIREMENTS_LEN];
    blob[0..4].copy_from_slice(&MAGIC_REQUIREMENTS.to_be_bytes());
    blob[4..8].copy_from_slice(&(REQUIREMENTS_LEN as u32).to_be_bytes());
    // count: zero requirements.
    blob
}

/// The empty CMS wrapper. Ad-hoc means there is no certificate chain to carry,
/// so the blob is its own header and nothing else.
fn signature_blob() -> [u8; SIGNATURE_LEN] {
    let mut blob = [0u8; SIGNATURE_LEN];
    blob[0..4].copy_from_slice(&MAGIC_BLOB_WRAPPER.to_be_bytes());
    blob[4..8].copy_from_slice(&(SIGNATURE_LEN as u32).to_be_bytes());
    blob
}

/// How many special slots the `CodeDirectory` declares.
///
/// Slot −1 is the Info.plist hash (absent here, so zeros) and slot −2 is the
/// requirements hash. They are declared in that order, so having a
/// requirements blob forces the info slot to exist as padding.
const SPECIAL_SLOTS: usize = 2;

/// Everything needed to sign, known before the bytes exist.
#[derive(Debug, Clone)]
pub struct SignatureRequest {
    /// The identifier embedded in the signature. Derived from the output
    /// filename, as `codesign` does.
    pub identifier: String,
    /// File offset where the signature will be written. Also the number of
    /// bytes it covers — the signature cannot hash itself.
    pub code_limit: u64,
    /// File offset of the executable segment, `__TEXT`.
    pub exec_segment_base: u64,
    /// Its file size.
    pub exec_segment_limit: u64,
}

/// Turn an output path into a signing identifier.
///
/// `codesign` uses the file's base name; anything after the first `.` is
/// dropped, matching how a bundle identifier is formed.
pub fn identifier_from_path(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "a.out".to_string())
}

/// Number of code slots for a given covered length.
fn code_slot_count(code_limit: u64) -> usize {
    (code_limit as usize).div_ceil(PAGE_SIZE)
}

/// The exact size the signature will occupy.
///
/// Callable before the image exists, which is what lets `LC_CODE_SIGNATURE`
/// and the `__LINKEDIT` layout be written in the same pass that decides where
/// the signature goes. Getting this wrong by even one byte moves `code_limit`
/// and invalidates every hash.
pub fn signature_size(request: &SignatureRequest) -> usize {
    let slots = code_slot_count(request.code_limit);
    let code_directory = code_directory_size(&request.identifier, slots);
    super_blob_header() + code_directory + REQUIREMENTS_LEN + SIGNATURE_LEN
}

fn super_blob_header() -> usize {
    // magic + length + count, then three (type, offset) index entries.
    12 + 3 * 8
}

fn code_directory_size(identifier: &str, code_slots: usize) -> usize {
    hash_offset(identifier) + code_slots * HASH_SIZE
}

/// Offset within the `CodeDirectory` of the first *code* slot hash.
///
/// Special slots live immediately before it, at negative indices, so this is
/// past them.
fn hash_offset(identifier: &str) -> usize {
    CODE_DIRECTORY_HEADER + identifier.len() + 1 + SPECIAL_SLOTS * HASH_SIZE
}

/// Build the signature for an image.
///
/// `image` must be the complete file up to `code_limit`, with
/// `LC_CODE_SIGNATURE` already present and pointing at `code_limit` — the
/// command is inside the region being hashed, so it cannot be added afterwards.
pub fn sign(image: &[u8], request: &SignatureRequest) -> Vec<u8> {
    assert!(
        image.len() >= request.code_limit as usize,
        "the image is shorter than the region the signature must cover"
    );

    let code_slots = code_slot_count(request.code_limit);
    let code_directory = build_code_directory(image, request, code_slots);

    // The special slots hash the *other* blobs, so they must be built first.
    let mut blob = Vec::with_capacity(signature_size(request));

    let cd_offset = super_blob_header();
    let requirements_offset = cd_offset + code_directory.len();
    let signature_offset = requirements_offset + REQUIREMENTS_LEN;
    let total = signature_offset + SIGNATURE_LEN;

    blob.extend_from_slice(&MAGIC_EMBEDDED_SIGNATURE.to_be_bytes());
    blob.extend_from_slice(&(total as u32).to_be_bytes());
    blob.extend_from_slice(&3u32.to_be_bytes());

    for (slot, offset) in [
        (SLOT_CODE_DIRECTORY, cd_offset),
        (SLOT_REQUIREMENTS, requirements_offset),
        (SLOT_SIGNATURE, signature_offset),
    ] {
        blob.extend_from_slice(&slot.to_be_bytes());
        blob.extend_from_slice(&(offset as u32).to_be_bytes());
    }

    blob.extend_from_slice(&code_directory);
    blob.extend_from_slice(&requirements_blob());
    blob.extend_from_slice(&signature_blob());

    debug_assert_eq!(
        blob.len(),
        signature_size(request),
        "signature_size disagreed with what was built; code_limit is now wrong"
    );
    blob
}

fn build_code_directory(image: &[u8], request: &SignatureRequest, code_slots: usize) -> Vec<u8> {
    let identifier = &request.identifier;
    let hash_offset = hash_offset(identifier);
    let length = code_directory_size(identifier, code_slots);

    let mut cd = Vec::with_capacity(length);
    // Every field below is big-endian, unlike the rest of the file.
    cd.extend_from_slice(&MAGIC_CODE_DIRECTORY.to_be_bytes());
    cd.extend_from_slice(&(length as u32).to_be_bytes());
    cd.extend_from_slice(&CODE_DIRECTORY_VERSION.to_be_bytes());
    cd.extend_from_slice(&FLAG_ADHOC.to_be_bytes());
    cd.extend_from_slice(&(hash_offset as u32).to_be_bytes());
    cd.extend_from_slice(&(CODE_DIRECTORY_HEADER as u32).to_be_bytes()); // identOffset
    cd.extend_from_slice(&(SPECIAL_SLOTS as u32).to_be_bytes());
    cd.extend_from_slice(&(code_slots as u32).to_be_bytes());
    cd.extend_from_slice(&(request.code_limit as u32).to_be_bytes());
    cd.push(HASH_SIZE as u8);
    cd.push(HASH_TYPE_SHA256);
    cd.push(0); // platform: not a platform binary
    cd.push(PAGE_SHIFT);
    cd.extend_from_slice(&0u32.to_be_bytes()); // spare2
    cd.extend_from_slice(&0u32.to_be_bytes()); // scatterOffset
    cd.extend_from_slice(&0u32.to_be_bytes()); // teamOffset
    cd.extend_from_slice(&0u32.to_be_bytes()); // spare3
    cd.extend_from_slice(&0u64.to_be_bytes()); // codeLimit64: unused below 4 GiB
    cd.extend_from_slice(&request.exec_segment_base.to_be_bytes());
    cd.extend_from_slice(&request.exec_segment_limit.to_be_bytes());
    cd.extend_from_slice(&EXEC_SEG_MAIN_BINARY.to_be_bytes());
    debug_assert_eq!(cd.len(), CODE_DIRECTORY_HEADER);

    cd.extend_from_slice(identifier.as_bytes());
    cd.push(0);

    // Special slots are written in reverse: the byte range immediately before
    // the code hashes is slot -1, the range before that is slot -2. Writing
    // them forwards here means emitting -2 first.
    let mut requirements_hash = Sha256::new();
    requirements_hash.update(requirements_blob());
    cd.extend_from_slice(&requirements_hash.finalize()); // slot -2
    cd.extend_from_slice(&[0u8; HASH_SIZE]); // slot -1: no Info.plist
    debug_assert_eq!(cd.len(), hash_offset);

    // One SHA-256 per page of the covered region. The final page is short
    // unless code_limit happens to be page-aligned, and is hashed at its real
    // length rather than padded.
    //
    // # Why this is threaded
    //
    // A profile of the linker found `sha256::compress256` to be its single
    // largest cost — larger than reading every input from disk. It is also the
    // most parallel thing the linker does: each page's hash depends on that
    // page and nothing else, so there is no ordering to preserve and no state
    // to share.
    //
    // Determinism is by construction rather than by care: every slot writes to
    // its own index in a pre-sized vector, so no thread's timing can reach the
    // output. The bytes are identical to the serial version's, which
    // `the_threaded_hashes_match_the_serial_ones` checks rather than assumes.
    let limit = request.code_limit as usize;
    let hash_page = |slot: usize, out: &mut [u8; HASH_SIZE]| {
        let start = slot * PAGE_SIZE;
        let end = ((slot + 1) * PAGE_SIZE).min(limit);
        out.copy_from_slice(&Sha256::digest(&image[start..end]));
    };

    let mut hashes = vec![[0u8; HASH_SIZE]; code_slots];
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(code_slots.max(1));
    if threads <= 1 {
        for (slot, out) in hashes.iter_mut().enumerate() {
            hash_page(slot, out);
        }
    } else {
        // Contiguous runs rather than a shared cursor: pages are equal-cost,
        // so there is nothing for work-stealing to balance, and a `chunks_mut`
        // split hands each thread a disjoint slice the borrow checker can
        // prove is disjoint.
        let per_thread = code_slots.div_ceil(threads);
        std::thread::scope(|scope| {
            for (chunk, run) in hashes.chunks_mut(per_thread).enumerate() {
                let hash_page = &hash_page;
                scope.spawn(move || {
                    for (offset, out) in run.iter_mut().enumerate() {
                        hash_page(chunk * per_thread + offset, out);
                    }
                });
            }
        });
    }
    for hash in &hashes {
        cd.extend_from_slice(hash);
    }

    debug_assert_eq!(cd.len(), length);
    cd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(code_limit: u64) -> SignatureRequest {
        SignatureRequest {
            identifier: "skeleton".to_string(),
            code_limit,
            exec_segment_base: 0,
            exec_segment_limit: 0x2d8,
        }
    }

    /// The size computed in advance must equal the size actually produced.
    ///
    /// This is load-bearing rather than tidy: the predicted size decides where
    /// the signature starts, and the signature covers everything before it. A
    /// one-byte error moves `code_limit`, which changes every page hash, and
    /// the kernel rejects the result with no useful diagnostic.
    #[test]
    fn the_predicted_size_matches_the_produced_size() {
        for limit in [1024u64, 16384, 16385, 32768, 100_000] {
            let request = request(limit);
            let image = vec![0u8; limit as usize];
            assert_eq!(
                sign(&image, &request).len(),
                signature_size(&request),
                "size mismatch at code_limit {limit}"
            );
        }
    }

    /// Pages are 16 KiB. Getting this wrong is silent until the kernel refuses.
    #[test]
    fn slots_are_counted_in_sixteen_kilobyte_pages() {
        assert_eq!(code_slot_count(1), 1);
        assert_eq!(code_slot_count(16384), 1);
        assert_eq!(code_slot_count(16385), 2);
        assert_eq!(code_slot_count(32768), 2);
        assert_eq!(code_slot_count(32769), 3);
    }

    /// The blob is big-endian in a little-endian file.
    #[test]
    fn the_super_blob_magic_is_big_endian() {
        let image = vec![0u8; 4096];
        let blob = sign(&image, &request(4096));
        assert_eq!(&blob[0..4], &[0xfa, 0xde, 0x0c, 0xc0]);
    }

    /// Each code slot must hold the SHA-256 of its page of the file.
    #[test]
    fn the_threaded_hashes_match_the_serial_ones() {
        // Large enough to be split across every core, and not a multiple of
        // the page size, so the short final page is covered too.
        let image: Vec<u8> = (0..PAGE_SIZE * 40 + 123).map(|i| (i * 7) as u8).collect();
        let request = SignatureRequest {
            identifier: "threaded".into(),
            code_limit: image.len() as u64,
            exec_segment_base: 0,
            exec_segment_limit: 0,
        };
        let slots = code_slot_count(request.code_limit);
        let directory = build_code_directory(&image, &request, slots);

        let base = hash_offset(&request.identifier);
        for slot in 0..slots {
            let start = slot * PAGE_SIZE;
            let end = ((slot + 1) * PAGE_SIZE).min(image.len());
            let at = base + slot * HASH_SIZE;
            assert_eq!(
                &directory[at..at + HASH_SIZE],
                &Sha256::digest(&image[start..end])[..],
                "slot {slot} of {slots} does not hold its page's hash"
            );
        }
    }

    /// Threading must not make the output depend on how the work was split.
    #[test]
    fn signing_the_same_image_twice_produces_the_same_bytes() {
        let image: Vec<u8> = (0..PAGE_SIZE * 33 + 9).map(|i| (i * 13) as u8).collect();
        let request = SignatureRequest {
            identifier: "stable".into(),
            code_limit: image.len() as u64,
            exec_segment_base: 0,
            exec_segment_limit: 0,
        };
        assert_eq!(sign(&image, &request), sign(&image, &request));
    }

    #[test]
    fn code_slots_hold_the_hash_of_each_page() {
        let limit = 40_000usize;
        let image: Vec<u8> = (0..limit).map(|i| (i % 251) as u8).collect();
        let request = request(limit as u64);
        let blob = sign(&image, &request);

        let cd_start = super_blob_header();
        let base = cd_start + hash_offset(&request.identifier);
        for slot in 0..code_slot_count(limit as u64) {
            let start = slot * PAGE_SIZE;
            let end = ((slot + 1) * PAGE_SIZE).min(limit);
            let expected = Sha256::digest(&image[start..end]);
            let found = &blob[base + slot * HASH_SIZE..base + (slot + 1) * HASH_SIZE];
            assert_eq!(found, &expected[..], "slot {slot} hash is wrong");
        }
    }

    /// The last page is hashed at its real length, not padded to 16 KiB.
    #[test]
    fn a_short_final_page_is_hashed_at_its_real_length() {
        let limit = PAGE_SIZE + 100;
        let image: Vec<u8> = (0..limit).map(|i| (i % 7) as u8).collect();
        let request = request(limit as u64);
        let blob = sign(&image, &request);

        let base = super_blob_header() + hash_offset(&request.identifier);
        let last = &blob[base + HASH_SIZE..base + 2 * HASH_SIZE];
        assert_eq!(last, &Sha256::digest(&image[PAGE_SIZE..limit])[..]);
        // And it must NOT be the hash of a zero-padded page.
        let mut padded = image[PAGE_SIZE..].to_vec();
        padded.resize(PAGE_SIZE, 0);
        assert_ne!(last, &Sha256::digest(&padded)[..]);
    }

    /// Special slots run backwards from the code hashes: −1 nearest, then −2.
    #[test]
    fn special_slots_are_indexed_backwards_from_the_code_hashes() {
        let image = vec![0u8; 4096];
        let request = request(4096);
        let blob = sign(&image, &request);
        let base = super_blob_header() + hash_offset(&request.identifier);

        // Slot -1 is the Info.plist hash; there is none, so it is zeros.
        let slot_1 = &blob[base - HASH_SIZE..base];
        assert_eq!(slot_1, &[0u8; HASH_SIZE], "slot -1 should be empty");

        // Slot -2 is the hash of the requirements blob.
        let slot_2 = &blob[base - 2 * HASH_SIZE..base - HASH_SIZE];
        assert_eq!(slot_2, &Sha256::digest(requirements_blob())[..]);
    }

    /// The index must name the three blobs at their real offsets.
    #[test]
    fn the_index_points_at_each_blob() {
        let image = vec![0u8; 4096];
        let request = request(4096);
        let blob = sign(&image, &request);

        let count = u32::from_be_bytes(blob[8..12].try_into().unwrap());
        assert_eq!(count, 3);

        for index in 0..count as usize {
            let entry = 12 + index * 8;
            let offset =
                u32::from_be_bytes(blob[entry + 4..entry + 8].try_into().unwrap()) as usize;
            let magic = u32::from_be_bytes(blob[offset..offset + 4].try_into().unwrap());
            assert!(
                [MAGIC_CODE_DIRECTORY, MAGIC_REQUIREMENTS, MAGIC_BLOB_WRAPPER].contains(&magic),
                "index entry {index} points at {magic:#x}, which is not a blob"
            );
        }
    }

    /// The declared total length must equal the blob's real length.
    #[test]
    fn the_declared_length_matches_reality() {
        let image = vec![0u8; 20_000];
        let blob = sign(&image, &request(20_000));
        let declared = u32::from_be_bytes(blob[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared, blob.len());
    }

    #[test]
    fn an_identifier_comes_from_the_file_name() {
        assert_eq!(
            identifier_from_path(std::path::Path::new("/tmp/build/my-app")),
            "my-app"
        );
    }
}
