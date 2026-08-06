//! Building `__TEXT,__unwind_info`.
//!
//! # What it is for
//!
//! When a Rust program panics, `_Unwind_RaiseException` walks the stack asking
//! the unwinder how to restore each frame. On macOS the unwinder answers from
//! this section. Without it, the lookup fails, `rust_panic` gives up and calls
//! `abort` — which is exactly what a blinker-linked binary did: the panic
//! message printed, then `SIGABRT`, with no crash inside the unwinder because
//! nothing had gone wrong except that there was nothing to read.
//!
//! # Where the data comes from
//!
//! The compiler emits one `__LD,__compact_unwind` record per function: a
//! pointer to the function, its length, a 32-bit encoding describing how to
//! restore the frame, and optional personality and LSDA pointers. That section
//! is *input only* — ld64 consumes it and emits this one, which is why
//! `__LD,__compact_unwind` must never be copied into the output.
//!
//! # The shape
//!
//! A two-level table, so the unwinder can binary-search by address:
//!
//! ```text
//! header (28 bytes)
//! common encodings   (omitted here: an optimisation, not a requirement)
//! personalities      one 32-bit image offset each
//! index              one entry per page, plus a sentinel
//! LSDA index         (functionOffset, lsdaOffset) pairs
//! second-level pages REGULAR: (functionOffset, encoding) pairs
//! ```
//!
//! Every offset in the file is relative to the **section start**, while every
//! *function* offset is relative to the **image base**. Mixing those up
//! produces a table that parses and then misdirects the unwinder.

/// One function's unwind record, after addresses have been resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnwindEntry {
    /// Function address, relative to the image base.
    pub function_offset: u32,
    /// How long the function is.
    ///
    /// Read only to compute the sentinel, which is the address *past* the last
    /// function. Nothing else in the table needs it — an entry's range runs to
    /// wherever the next entry starts — which is why it was read and thrown
    /// away, and why the sentinel was the last function's *start* (finding
    /// 239). That makes the last function's range empty, so the unwinder finds
    /// no unwind info for any address inside it.
    pub length: u32,
    /// The compact encoding, without personality bits.
    pub encoding: u32,
    /// Personality routine, as an image-relative offset to its pointer slot.
    pub personality: Option<u32>,
    /// Language-specific data area, image-relative.
    pub lsda: Option<u32>,
}

/// `UNWIND_SECOND_LEVEL_REGULAR`.
const SECOND_LEVEL_REGULAR: u32 = 2;

/// Bits 28–29 hold a one-based personality index.
const PERSONALITY_SHIFT: u32 = 28;
const PERSONALITY_MASK: u32 = 0x3000_0000;

/// `UNWIND_HAS_LSDA`.
const HAS_LSDA: u32 = 0x4000_0000;

/// Bytes in the section header.
const HEADER_SIZE: usize = 28;
/// Bytes per index entry.
const INDEX_ENTRY_SIZE: usize = 12;
/// Bytes per LSDA index entry.
const LSDA_ENTRY_SIZE: usize = 8;
/// Bytes per second-level regular entry.
const REGULAR_ENTRY_SIZE: usize = 8;
/// Bytes in a second-level page header.
const PAGE_HEADER_SIZE: usize = 8;

/// How many entries fit in one second-level page.
///
/// A page is capped at 4 KiB because the index stores its offset in a `u16`
/// relative to the page start.
const ENTRIES_PER_PAGE: usize = (4096 - PAGE_HEADER_SIZE) / REGULAR_ENTRY_SIZE;

/// An upper bound on the size of a table with `records` entries.
///
/// Used to reserve space before any address is known, so it must never
/// under-estimate: every record is assumed to be distinct, and the maximum
/// three personalities are assumed present.
///
/// `lsdas` is how many of those records carry one. It used to be assumed to be
/// all of them, which is the same eight bytes a table entry costs — so the
/// reservation was twice the table for input that mostly has no LSDA, and the
/// section was padded out to it (finding 238).
pub fn upper_bound_size(records: usize, lsdas: usize) -> usize {
    let pages = records.div_ceil(ENTRIES_PER_PAGE).max(1);
    HEADER_SIZE
        + 3 * 4
        + (pages + 1) * INDEX_ENTRY_SIZE
        + lsdas.min(records) * LSDA_ENTRY_SIZE
        + pages * PAGE_HEADER_SIZE
        + records * REGULAR_ENTRY_SIZE
}

/// Build the section contents.
///
/// `entries` need not be sorted; they are sorted here, because the unwinder
/// binary-searches and an unsorted table silently returns wrong answers rather
/// than failing.
pub fn build(mut entries: Vec<UnwindEntry>) -> Vec<u8> {
    entries.sort_by_key(|e| e.function_offset);
    entries.dedup_by_key(|e| e.function_offset);

    // Personalities are referenced by a one-based index packed into the
    // encoding, so at most three can exist. More than that is a link that
    // cannot be represented, but for Rust there is exactly one.
    let mut personalities: Vec<u32> = Vec::new();
    for entry in &entries {
        if let Some(personality) = entry.personality {
            if !personalities.contains(&personality) {
                personalities.push(personality);
            }
        }
    }
    personalities.truncate(3);

    let pages: Vec<&[UnwindEntry]> = entries.chunks(ENTRIES_PER_PAGE).collect();
    let lsda_entries: Vec<&UnwindEntry> = entries.iter().filter(|e| e.lsda.is_some()).collect();

    // Offsets are laid out before anything is written: the header has to name
    // where each later array begins.
    let personalities_offset = HEADER_SIZE;
    let index_offset = personalities_offset + personalities.len() * 4;
    // One index entry per page, plus a sentinel marking the end.
    let index_count = pages.len() + 1;
    let lsda_offset = index_offset + index_count * INDEX_ENTRY_SIZE;
    let pages_offset = lsda_offset + lsda_entries.len() * LSDA_ENTRY_SIZE;

    let mut out = Vec::new();
    let push = |value: u32, out: &mut Vec<u8>| out.extend_from_slice(&value.to_le_bytes());

    push(1, &mut out); // version
    push(HEADER_SIZE as u32, &mut out); // common encodings offset
    push(0, &mut out); // common encodings count — omitted
    push(personalities_offset as u32, &mut out);
    push(personalities.len() as u32, &mut out);
    push(index_offset as u32, &mut out);
    push(index_count as u32, &mut out);
    debug_assert_eq!(out.len(), HEADER_SIZE);

    for personality in &personalities {
        push(*personality, &mut out);
    }

    // Index: one entry per page. Each names where its page and its slice of
    // the LSDA index begin.
    let mut lsda_cursor = lsda_offset;
    let mut page_cursor = pages_offset;
    for page in &pages {
        push(page[0].function_offset, &mut out);
        push(page_cursor as u32, &mut out);
        push(lsda_cursor as u32, &mut out);
        lsda_cursor += page.iter().filter(|e| e.lsda.is_some()).count() * LSDA_ENTRY_SIZE;
        page_cursor += PAGE_HEADER_SIZE + page.len() * REGULAR_ENTRY_SIZE;
    }

    // The sentinel gives the end of the last function and terminates the
    // search. Its page offset is zero, which is how the unwinder recognises it.
    //
    // Past the end, not at the start: the unwinder takes the last index entry
    // whose function offset is `<= pc`, so a sentinel sitting *on* the last
    // function makes that function's range empty and its unwind info
    // unreachable. With one function in the table nothing is covered at all,
    // which is how a C++ `throw` in `main` walked out of the program.
    let end = entries
        .last()
        .map(|e| e.function_offset.saturating_add(e.length))
        .unwrap_or(0);
    push(end, &mut out);
    push(0, &mut out);
    push(lsda_cursor as u32, &mut out);
    debug_assert_eq!(out.len(), lsda_offset);

    for entry in &lsda_entries {
        push(entry.function_offset, &mut out);
        push(entry.lsda.expect("filtered"), &mut out);
    }
    debug_assert_eq!(out.len(), pages_offset);

    for page in &pages {
        push(SECOND_LEVEL_REGULAR, &mut out);
        // entryPageOffset and entryCount are u16s packed into one word.
        out.extend_from_slice(&(PAGE_HEADER_SIZE as u16).to_le_bytes());
        out.extend_from_slice(&(page.len() as u16).to_le_bytes());

        for entry in page.iter() {
            push(entry.function_offset, &mut out);

            // The personality index and the LSDA flag live in the encoding,
            // and the compiler leaves both clear — assigning them is the
            // linker's job, because only it knows the final personality set.
            let mut encoding = entry.encoding & !(PERSONALITY_MASK | HAS_LSDA);
            if let Some(personality) = entry.personality {
                if let Some(index) = personalities.iter().position(|p| *p == personality) {
                    encoding |= ((index as u32) + 1) << PERSONALITY_SHIFT;
                }
            }
            if entry.lsda.is_some() {
                encoding |= HAS_LSDA;
            }
            push(encoding, &mut out);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(offset: u32) -> UnwindEntry {
        UnwindEntry {
            function_offset: offset,
            length: 4,
            encoding: 0x0400_0000,
            personality: None,
            lsda: None,
        }
    }

    fn header(data: &[u8]) -> (u32, u32, u32, u32, u32, u32, u32) {
        let read = |i: usize| u32::from_le_bytes(data[i * 4..i * 4 + 4].try_into().unwrap());
        (
            read(0),
            read(1),
            read(2),
            read(3),
            read(4),
            read(5),
            read(6),
        )
    }

    #[test]
    fn the_header_declares_version_one() {
        let data = build(vec![entry(0x100)]);
        assert_eq!(header(&data).0, 1);
    }

    /// The unwinder binary-searches, so an unsorted table does not fail — it
    /// answers the wrong question.
    #[test]
    fn entries_are_sorted_by_function_offset() {
        let data = build(vec![entry(0x300), entry(0x100), entry(0x200)]);
        let (_, _, _, _, _, index_offset, _) = header(&data);
        let first_function = u32::from_le_bytes(
            data[index_offset as usize..index_offset as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(first_function, 0x100);
    }

    /// There is always one more index entry than there are pages.
    #[test]
    fn the_index_ends_with_a_sentinel() {
        let data = build(vec![entry(0x100), entry(0x200)]);
        let (_, _, _, _, _, index_offset, index_count) = header(&data);
        assert_eq!(index_count, 2, "one page plus the sentinel");

        let sentinel = index_offset as usize + INDEX_ENTRY_SIZE;
        let page_offset = u32::from_le_bytes(data[sentinel + 4..sentinel + 8].try_into().unwrap());
        assert_eq!(page_offset, 0, "the sentinel's page offset marks the end");
    }

    /// More entries than fit in a 4 KiB page must produce more pages.
    #[test]
    fn a_large_table_is_split_across_pages() {
        let entries: Vec<_> = (0..ENTRIES_PER_PAGE * 2 + 5)
            .map(|i| entry(i as u32 * 4))
            .collect();
        let data = build(entries);
        let (_, _, _, _, _, _, index_count) = header(&data);
        assert_eq!(index_count, 4, "three pages plus the sentinel");
    }

    /// A personality is recorded once and referenced by a one-based index in
    /// the encoding.
    #[test]
    fn personalities_are_deduplicated_and_indexed_from_one() {
        let mut a = entry(0x100);
        a.personality = Some(0x9000);
        let mut b = entry(0x200);
        b.personality = Some(0x9000);
        let data = build(vec![a, b]);

        let (_, _, _, personality_offset, personality_count, _, _) = header(&data);
        assert_eq!(
            personality_count, 1,
            "the same personality twice is one entry"
        );
        let value = u32::from_le_bytes(
            data[personality_offset as usize..personality_offset as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(value, 0x9000);

        // First page's first entry: encoding must carry index 1.
        let page_start = data.len() - 2 * REGULAR_ENTRY_SIZE - PAGE_HEADER_SIZE;
        let encoding = u32::from_le_bytes(
            data[page_start + PAGE_HEADER_SIZE + 4..page_start + PAGE_HEADER_SIZE + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            (encoding & PERSONALITY_MASK) >> PERSONALITY_SHIFT,
            1,
            "personality index is one-based"
        );
    }

    /// An entry with an LSDA must set the flag and appear in the LSDA index.
    #[test]
    fn an_lsda_sets_the_flag_and_is_indexed() {
        let mut a = entry(0x100);
        a.lsda = Some(0x7000);
        let data = build(vec![a, entry(0x200)]);

        let (_, _, _, _, _, index_offset, _) = header(&data);
        // The first index entry names where this page's LSDA slice starts.
        let lsda_offset = u32::from_le_bytes(
            data[index_offset as usize + 8..index_offset as usize + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        let function = u32::from_le_bytes(data[lsda_offset..lsda_offset + 4].try_into().unwrap());
        let lsda = u32::from_le_bytes(data[lsda_offset + 4..lsda_offset + 8].try_into().unwrap());
        assert_eq!((function, lsda), (0x100, 0x7000));
    }

    /// Two records for the same function would make the search ambiguous.
    #[test]
    fn duplicate_function_offsets_are_collapsed() {
        let data = build(vec![entry(0x100), entry(0x100), entry(0x200)]);
        let page_start = data.len() - 2 * REGULAR_ENTRY_SIZE - PAGE_HEADER_SIZE;
        let count = u16::from_le_bytes(data[page_start + 6..page_start + 8].try_into().unwrap());
        assert_eq!(count, 2);
    }

    #[test]
    fn an_empty_table_still_produces_a_valid_header() {
        let data = build(Vec::new());
        let (version, _, _, _, _, _, index_count) = header(&data);
        assert_eq!(version, 1);
        assert_eq!(index_count, 1, "just the sentinel");
    }
}

#[cfg(test)]
mod sentinel_tests {
    use super::*;

    /// The sentinel must be *past* the last function, not on it.
    ///
    /// The unwinder takes the last index entry whose function offset is at or
    /// below the pc it is asking about. A sentinel sitting on the last
    /// function's start makes that function's range empty, so its unwind info
    /// cannot be found — and with one function in the table, nothing can
    /// (finding 239).
    #[test]
    fn the_sentinel_is_past_the_end_of_the_last_function() {
        let entries = vec![UnwindEntry {
            function_offset: 0x1000,
            length: 0x40,
            encoding: 0x0400_0000,
            personality: None,
            lsda: None,
        }];
        let data = build(entries);
        let read = |i: usize| u32::from_le_bytes(data[i..i + 4].try_into().unwrap());
        let index_offset = read(20) as usize;
        let index_count = read(24) as usize;
        assert_eq!(index_count, 2, "one page and a sentinel");
        let first = read(index_offset);
        let sentinel = read(index_offset + INDEX_ENTRY_SIZE);
        assert_eq!(first, 0x1000);
        assert_eq!(
            sentinel, 0x1040,
            "the sentinel is on the function rather than after it, so its range is empty"
        );
        assert!(
            sentinel > first,
            "a sentinel at or below the first entry covers nothing at all"
        );
    }
}

#[cfg(test)]
mod bound_tests {
    use super::*;

    fn table_of(count: usize, with_lsda: impl Fn(usize) -> bool) -> Vec<UnwindEntry> {
        (0..count)
            .map(|i| UnwindEntry {
                function_offset: i as u32 * 4,
                length: 4,
                encoding: 0,
                personality: Some(0x1000 + (i as u32 % 3)),
                lsda: with_lsda(i).then_some(0x2000 + i as u32),
            })
            .collect()
    }

    /// The reservation must never be smaller than what is built, for any shape
    /// of input — the link fails outright if it is.
    ///
    /// Every proportion of LSDA-carrying records is tried, not only all of
    /// them: the bound is now told how many there are, so "none", "some" and
    /// "all" are three different sums and only one of them used to be
    /// exercised.
    #[test]
    fn the_upper_bound_is_never_exceeded() {
        let counts = [1usize, 2, 100, ENTRIES_PER_PAGE, ENTRIES_PER_PAGE + 1, 3000];
        type Shape = (&'static str, fn(usize) -> bool);
        let shapes: [Shape; 4] = [
            ("none", |_| false),
            ("all", |_| true),
            ("every other", |i| i % 2 == 0),
            ("one", |i| i == 0),
        ];
        for count in counts {
            for (name, has_lsda) in shapes {
                let entries = table_of(count, has_lsda);
                let lsdas = entries.iter().filter(|e| e.lsda.is_some()).count();
                let built = build(entries).len();
                let bound = upper_bound_size(count, lsdas);
                assert!(
                    built <= bound,
                    "{count} records with {name} LSDA: built {built} bytes, reserved {bound}"
                );
            }
        }
    }

    /// And it must be *tight*, or the difference is written out as zeroes.
    ///
    /// It was not: assuming every record carried an LSDA reserved an extra
    /// eight bytes each, which is exactly what a table entry costs — so a
    /// table of records without LSDAs was reserved at twice its size, and
    /// pulsevm carried 1.55 MB of padding (finding 238).
    #[test]
    fn the_upper_bound_is_not_twice_the_table() {
        let entries = table_of(3000, |_| false);
        let built = build(entries).len();
        let bound = upper_bound_size(3000, 0);
        assert!(
            bound < built + built / 8,
            "reserved {bound} for a {built}-byte table"
        );
    }
}
