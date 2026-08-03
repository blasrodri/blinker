//! Reading `.a` static archives and Rust `.rlib` files.
//!
//! # Why the index is separate from the members
//!
//! Archive semantics are order-sensitive and globally observable: which members
//! get pulled in depends on what is still undefined at the moment the archive
//! is reached. Pulling a member that should not have been pulled produces a
//! binary that looks fine and behaves wrongly.
//!
//! So this crate deliberately separates two things:
//!
//! - [`ArchiveIndex`] — cheap, cacheable metadata: what members exist, what
//!   symbols they define, where their bytes are. Parsed once per archive.
//! - Member *contents* — read only when the symbol resolver decides a member
//!   is actually needed.
//!
//! A real link reads 186 rlibs. Eagerly parsing every member of every one of
//! them would dominate link time and defeat the point of the cache that M4
//! builds on top of this.
//!
//! # What a Rust `.rlib` actually contains
//!
//! Observed in the toolchain's own `libstd`:
//!
//! ```text
//! __.SYMDEF            archive symbol table
//! lib.rmeta            Rust crate metadata — not a linker input
//! lib.rmeta-link       further Rust metadata — not a linker input
//! std-….rcgu.o         the actual object code
//! ```
//!
//! The metadata members must be skipped deliberately rather than by accident:
//! handing `lib.rmeta` to a Mach-O parser produces a confusing error about a
//! malformed object when the real answer is "that was never an object".

use std::path::{Path, PathBuf};

use object::read::archive::ArchiveFile;

mod error;
pub use error::ArchiveError;

/// Index of a member within its archive.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct MemberId(pub u32);

/// What an archive member is, and therefore what should be done with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemberKind {
    /// A Mach-O object file — a real linker input.
    MachOObject,
    /// Rust crate metadata (`lib.rmeta`, `lib.rmeta-link`). Carried in the
    /// archive for the compiler's benefit and skipped by the linker.
    RustMetadata,
    /// The archive symbol table (`__.SYMDEF`, `__.SYMDEF SORTED`). Consumed as
    /// an index rather than linked.
    SymbolTable,
    /// Something else. Recorded rather than assumed harmless, so an unexpected
    /// member shows up in diagnostics instead of vanishing.
    Unknown,
}

impl MemberKind {
    /// Classify a member by name.
    ///
    /// Name is the only signal available without reading the member's bytes,
    /// and for the metadata members it is the *correct* signal — they are
    /// identified by convention, not by content.
    pub fn classify(name: &str) -> Self {
        match name {
            "__.SYMDEF" | "__.SYMDEF SORTED" | "__.SYMDEF_64" | "__.SYMDEF_64 SORTED" => {
                MemberKind::SymbolTable
            }
            "lib.rmeta" | "lib.rmeta-link" | "rust.metadata.bin" => MemberKind::RustMetadata,
            other if other.ends_with(".o") => MemberKind::MachOObject,
            _ => MemberKind::Unknown,
        }
    }

    /// Whether this member should be offered to the symbol resolver.
    pub fn is_linkable(self) -> bool {
        self == MemberKind::MachOObject
    }
}

/// One member's identity and location, without its contents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveMember {
    pub id: MemberId,
    /// Member name as stored in the archive, with long-name encodings already
    /// resolved.
    pub name: String,
    pub kind: MemberKind,
    /// Offset of the member's data within the archive file.
    pub offset: u64,
    pub size: u64,
}

/// Everything about an archive except its members' contents.
///
/// Cheap to build, cheap to cache, and sufficient to decide which members a
/// link actually needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveIndex {
    pub path: PathBuf,
    pub members: Vec<ArchiveMember>,
    /// Symbol name → the member defining it, from the archive symbol table,
    /// **in the archive's own order**.
    ///
    /// Not sorted. It was, for a binary search this crate no longer performs;
    /// the linker builds one index across every archive instead. A consumer
    /// wanting a lookup should build its own, which is cheaper once and
    /// correct for the question it is actually asking.
    ///
    /// The order carries one guarantee, which is the one resolution needs: the
    /// first entry for a name is the earliest member defining it.
    ///
    /// Empty when the archive has no symbol table, which is not an error: the
    /// resolver then falls back to examining linkable members directly.
    pub symbol_map: Vec<(String, MemberId)>,
    pub file_size: u64,
}

impl ArchiveIndex {
    /// Members that are real linker inputs, in archive order.
    ///
    /// Order is preserved because archive resolution is order-sensitive.
    pub fn linkable_members(&self) -> impl Iterator<Item = &ArchiveMember> {
        self.members.iter().filter(|m| m.kind.is_linkable())
    }

    pub fn member(&self, id: MemberId) -> Option<&ArchiveMember> {
        self.members.get(id.0 as usize)
    }

    /// Which member defines `symbol`, according to the archive symbol table.
    ///
    /// # Why this binary-searches
    ///
    /// Scanning is fine for one question and quadratic for the way a linker
    /// actually asks: every still-undefined name, against every archive, once
    /// per extraction round. `libstd.rlib` lists tens of thousands of symbols,
    /// and on blinker's own binary that scan was **25 ms of a 148 ms link** —
    /// the second-largest cost in it, inside a function that reads like a
    /// lookup (finding 78).
    ///
    /// A scan, and not the binary search this used to be. The table is no
    /// longer sorted — see `index_archive` — because the linker stopped asking
    /// one archive at a time and the sort was costing more than every lookup
    /// it served. A caller with many names to resolve should build an index
    /// across the archives it has, as `load_objects` does, rather than call
    /// this in a loop; that is the mistake finding 78 was about.
    pub fn member_defining(&self, symbol: &str) -> Option<MemberId> {
        // The first entry wins, exactly as a forward scan of the archive's own
        // order would have: a name may be listed more than once.
        self.symbol_map
            .iter()
            .find(|(name, _)| name == symbol)
            .map(|(_, id)| *id)
    }

    /// Members skipped as non-linkable, for diagnostics.
    pub fn skipped_members(&self) -> impl Iterator<Item = &ArchiveMember> {
        self.members.iter().filter(|m| !m.kind.is_linkable())
    }

    /// Whether this looks like a Rust `.rlib` rather than a plain `.a`.
    pub fn is_rlib(&self) -> bool {
        self.members
            .iter()
            .any(|m| m.kind == MemberKind::RustMetadata)
    }
}

/// Build an index from an archive's bytes.
pub fn index_archive(data: &[u8], path: &Path) -> Result<ArchiveIndex, ArchiveError> {
    let archive = ArchiveFile::parse(data).map_err(|e| ArchiveError::Malformed {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;

    let mut members = Vec::new();
    for (index, member) in archive.members().enumerate() {
        let member = member.map_err(|e| ArchiveError::Malformed {
            path: path.to_path_buf(),
            detail: format!("member {index}: {e}"),
        })?;

        if index > u32::MAX as usize {
            return Err(ArchiveError::TooManyMembers {
                path: path.to_path_buf(),
                count: index,
            });
        }

        // Member names are bytes on disk. A non-UTF-8 name is recoverable —
        // we only need it to classify and to report — so it is replaced
        // lossily rather than failing the whole archive.
        let name = String::from_utf8_lossy(member.name()).into_owned();
        let (offset, size) = member.file_range();

        members.push(ArchiveMember {
            id: MemberId(index as u32),
            kind: MemberKind::classify(&name),
            name,
            offset,
            size,
        });
    }

    // Left in the archive's own order. It was sorted here, for a binary search
    // that no longer happens: the linker builds one map across every archive
    // and reads this table by scanning it once (see `load_objects`). Sorting
    // 73,095 `(String, MemberId)` pairs by text — every comparison two pointer
    // chases — cost 9-14 ms on rust-analyzer's largest rlib, on the one thread
    // that archive was being indexed on, to order a table nothing binary
    // searched (finding 181).
    //
    // The order the linker does depend on is unchanged. It takes the *first*
    // entry for a name, and the archive symbol table already lists members in
    // archive order, so the first occurrence names the same member the stable
    // sort put first.
    let symbol_map = parse_symbol_map(&archive, &members);

    Ok(ArchiveIndex {
        path: path.to_path_buf(),
        members,
        symbol_map,
        file_size: data.len() as u64,
    })
}

/// Read the archive symbol table, mapping each symbol to its defining member.
///
/// A missing or unreadable symbol table is not an error: it costs speed, not
/// correctness, because the resolver can always fall back to scanning members.
fn parse_symbol_map<'d>(
    archive: &ArchiveFile<'d, &'d [u8]>,
    members: &[ArchiveMember],
) -> Vec<(String, MemberId)> {
    let Ok(Some(symbols)) = archive.symbols() else {
        return Vec::new();
    };

    let mut map = Vec::new();
    for symbol in symbols.flatten() {
        let Ok(name) = std::str::from_utf8(symbol.name()) else {
            continue;
        };
        // The table addresses members by the offset of their *header*, while
        // our IDs index the member list. Resolve through `object` and match on
        // the member's data range, which is the one identity both sides agree
        // on.
        let Ok(member) = archive.member(symbol.offset()) else {
            continue;
        };
        let (offset, _) = member.file_range();
        if let Some(found) = members.iter().find(|m| m.offset == offset) {
            map.push((name.to_string(), found.id));
        }
    }
    map
}

/// Read one member's bytes out of an archive.
///
/// The laziness that makes indexing cheap: contents are fetched only once the
/// resolver has decided a member is needed.
pub fn member_data<'d>(
    data: &'d [u8],
    member: &ArchiveMember,
    path: &Path,
) -> Result<&'d [u8], ArchiveError> {
    let start = member.offset as usize;
    let end =
        start
            .checked_add(member.size as usize)
            .ok_or_else(|| ArchiveError::MemberOutOfBounds {
                path: path.to_path_buf(),
                member: member.name.clone(),
            })?;

    data.get(start..end)
        .ok_or_else(|| ArchiveError::MemberOutOfBounds {
            path: path.to_path_buf(),
            member: member.name.clone(),
        })
}

/// Index an archive from disk.
pub fn index_archive_file(path: &Path) -> Result<(ArchiveIndex, Vec<u8>), ArchiveError> {
    let data = std::fs::read(path).map_err(|source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let index = index_archive(&data, path)?;
    Ok((index, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_members_a_real_rlib_contains() {
        // Exactly what the toolchain's own libstd rlib holds.
        assert_eq!(MemberKind::classify("__.SYMDEF"), MemberKind::SymbolTable);
        assert_eq!(MemberKind::classify("lib.rmeta"), MemberKind::RustMetadata);
        assert_eq!(
            MemberKind::classify("lib.rmeta-link"),
            MemberKind::RustMetadata
        );
        assert_eq!(
            MemberKind::classify("std-4f24f0876fd27385.std.7648081134ee26d8-cgu.0.rcgu.o"),
            MemberKind::MachOObject
        );
    }

    #[test]
    fn classifies_symbol_table_spellings() {
        for name in [
            "__.SYMDEF",
            "__.SYMDEF SORTED",
            "__.SYMDEF_64",
            "__.SYMDEF_64 SORTED",
        ] {
            assert_eq!(
                MemberKind::classify(name),
                MemberKind::SymbolTable,
                "{name} is a symbol table"
            );
        }
    }

    /// Rust metadata must be skipped deliberately. Handing `lib.rmeta` to a
    /// Mach-O parser yields a confusing "malformed object" error when the real
    /// answer is that it was never an object.
    #[test]
    fn rust_metadata_is_not_linkable() {
        assert!(!MemberKind::classify("lib.rmeta").is_linkable());
        assert!(!MemberKind::classify("lib.rmeta-link").is_linkable());
        assert!(!MemberKind::classify("__.SYMDEF").is_linkable());
        assert!(MemberKind::classify("thing.o").is_linkable());
    }

    #[test]
    fn unrecognised_members_are_recorded_rather_than_assumed_harmless() {
        assert_eq!(MemberKind::classify("surprise.txt"), MemberKind::Unknown);
        assert!(!MemberKind::classify("surprise.txt").is_linkable());
    }

    fn index_with(members: Vec<(&str, MemberKind)>) -> ArchiveIndex {
        ArchiveIndex {
            path: PathBuf::from("/x.rlib"),
            members: members
                .into_iter()
                .enumerate()
                .map(|(i, (name, kind))| ArchiveMember {
                    id: MemberId(i as u32),
                    name: name.to_string(),
                    kind,
                    offset: i as u64 * 100,
                    size: 100,
                })
                .collect(),
            symbol_map: Vec::new(),
            file_size: 400,
        }
    }

    #[test]
    fn linkable_members_exclude_metadata_and_preserve_order() {
        // Order matters: archive resolution is order-sensitive, so filtering
        // must not reorder what survives.
        let index = index_with(vec![
            ("__.SYMDEF", MemberKind::SymbolTable),
            ("lib.rmeta", MemberKind::RustMetadata),
            ("a.o", MemberKind::MachOObject),
            ("b.o", MemberKind::MachOObject),
        ]);
        let names: Vec<&str> = index.linkable_members().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a.o", "b.o"]);
    }

    #[test]
    fn skipped_members_are_reportable() {
        let index = index_with(vec![
            ("__.SYMDEF", MemberKind::SymbolTable),
            ("lib.rmeta", MemberKind::RustMetadata),
            ("a.o", MemberKind::MachOObject),
        ]);
        assert_eq!(index.skipped_members().count(), 2);
    }

    #[test]
    fn detects_an_rlib_by_its_metadata_member() {
        let rlib = index_with(vec![
            ("lib.rmeta", MemberKind::RustMetadata),
            ("a.o", MemberKind::MachOObject),
        ]);
        let plain = index_with(vec![("a.o", MemberKind::MachOObject)]);
        assert!(rlib.is_rlib());
        assert!(!plain.is_rlib());
    }

    #[test]
    fn resolves_a_symbol_to_its_defining_member() {
        let mut index = index_with(vec![("a.o", MemberKind::MachOObject)]);
        index.symbol_map = vec![("_main".to_string(), MemberId(0))];
        assert_eq!(index.member_defining("_main"), Some(MemberId(0)));
        assert_eq!(index.member_defining("_absent"), None);
    }

    #[test]
    fn rejects_data_that_is_not_an_archive() {
        let err = index_archive(b"definitely not an archive", Path::new("/x.a")).unwrap_err();
        assert!(matches!(err, ArchiveError::Malformed { .. }));
    }

    #[test]
    fn rejects_empty_input() {
        assert!(index_archive(&[], Path::new("/x.a")).is_err());
    }

    #[test]
    fn member_data_rejects_a_range_outside_the_file() {
        let data = vec![0u8; 100];
        let member = ArchiveMember {
            id: MemberId(0),
            name: "a.o".into(),
            kind: MemberKind::MachOObject,
            offset: 50,
            size: 500,
        };
        let err = member_data(&data, &member, Path::new("/x.a")).unwrap_err();
        assert!(matches!(err, ArchiveError::MemberOutOfBounds { .. }));
    }

    #[test]
    fn member_data_rejects_an_offset_that_overflows() {
        let data = vec![0u8; 100];
        let member = ArchiveMember {
            id: MemberId(0),
            name: "a.o".into(),
            kind: MemberKind::MachOObject,
            offset: u64::MAX,
            size: u64::MAX,
        };
        assert!(member_data(&data, &member, Path::new("/x.a")).is_err());
    }

    #[test]
    fn member_data_returns_exactly_the_member_range() {
        let data: Vec<u8> = (0..100u8).collect();
        let member = ArchiveMember {
            id: MemberId(0),
            name: "a.o".into(),
            kind: MemberKind::MachOObject,
            offset: 10,
            size: 5,
        };
        let bytes = member_data(&data, &member, Path::new("/x.a")).expect("in range");
        assert_eq!(bytes, &[10, 11, 12, 13, 14]);
    }

    #[test]
    fn index_round_trips_through_serde() {
        // The index is what M4 caches, so it must survive serialization.
        let index = index_with(vec![
            ("lib.rmeta", MemberKind::RustMetadata),
            ("a.o", MemberKind::MachOObject),
        ]);
        let json = serde_json::to_string(&index).expect("serializes");
        let back: ArchiveIndex = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(index, back);
    }
}
