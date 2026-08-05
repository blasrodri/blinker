//! Reading what a `.dylib` exports.
//!
//! # Why this exists
//!
//! Everything a link imports used to come from a `.tbd`: Apple ships text
//! stubs beside every SDK library, and a Rust program that touches nothing but
//! `libSystem` never needs anything else. So the linker read stubs, and a
//! resolved path that was not a stub was quietly skipped.
//!
//! That is fine until a project depends on C. `-lzstd` resolves to
//! `/opt/homebrew/lib/libzstd.dylib`, a real Mach-O with no `.tbd` anywhere
//! near it, and skipping it means every symbol it defines comes out undefined
//! — which is what happened to the first real-world project this was pointed
//! at, and to every project with a C or C++ dependency that is not in the SDK.
//!
//! A dylib carries its exports in one of two places, and both are read here:
//!
//! - `LC_DYLD_EXPORTS_TRIE` on anything built for macOS 12 or later, and
//! - the `export_off` range of `LC_DYLD_INFO`/`LC_DYLD_INFO_ONLY` before that.
//!
//! Neither is guaranteed to be present — a dylib linked with `-no_exported_
//! symbols`, or an old one, may carry only a symbol table — so a dylib whose
//! trie is missing or empty falls back to the external definitions in
//! `LC_SYMTAB`. That is what `nm -gU` reports and what `ld64` would bind
//! against.

use std::path::{Path, PathBuf};

use object::read::macho::{LoadCommandVariant, MachHeader, MachOFile64};
use object::{LittleEndian, Object, ObjectSymbol};

/// What a dylib offers a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DylibExports {
    /// The `LC_ID_DYLIB` name, which is what a binding against this library
    /// records and what dyld will look for at load time — *not* the path it
    /// was found at, which is usually a symlink and often a build directory.
    pub install_name: String,
    pub symbols: Vec<String>,
}

#[derive(Debug)]
pub enum DylibError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Not a Mach-O dynamic library. Callers use this to mean "try something
    /// else", so it must stay distinguishable from a dylib we failed to read.
    NotADylib {
        path: PathBuf,
    },
    Malformed {
        path: PathBuf,
        detail: String,
    },
}

impl std::fmt::Display for DylibError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DylibError::Io { path, source } => write!(out, "{}: {source}", path.display()),
            DylibError::NotADylib { path } => {
                write!(out, "{}: not a Mach-O dynamic library", path.display())
            }
            DylibError::Malformed { path, detail } => write!(out, "{}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for DylibError {}

/// A Mach-O dynamic library, or a stub of one.
const MH_DYLIB: u32 = 0x6;
const MH_DYLIB_STUB: u32 = 0x9;

/// Read `path`'s install name and exported symbols.
pub fn read_dylib_exports(path: &Path) -> Result<DylibExports, DylibError> {
    let data = std::fs::read(path).map_err(|source| DylibError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    read_dylib_exports_from(&data, path)
}

/// The same, on bytes already in hand.
pub fn read_dylib_exports_from(data: &[u8], path: &Path) -> Result<DylibExports, DylibError> {
    // A universal binary carries several architectures; only ours is wanted,
    // and a Homebrew bottle on an Intel-and-Apple machine is routinely fat.
    let slice = arm64_slice(data, path)?;

    let header = object::macho::MachHeader64::<LittleEndian>::parse(slice, 0).map_err(|_| {
        DylibError::NotADylib {
            path: path.to_path_buf(),
        }
    })?;
    let kind = header.filetype(LittleEndian);
    if kind != MH_DYLIB && kind != MH_DYLIB_STUB {
        return Err(DylibError::NotADylib {
            path: path.to_path_buf(),
        });
    }

    let file =
        MachOFile64::<LittleEndian>::parse(slice).map_err(|error| DylibError::Malformed {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

    let install_name = install_name(header, slice, path)?;

    // The trie first: it is what dyld itself reads, and it is the only place
    // that records re-exports and aliases.
    let mut symbols: Vec<String> = file
        .exports()
        .map_err(|error| DylibError::Malformed {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?
        .iter()
        .filter_map(|export| std::str::from_utf8(export.name()).ok())
        .map(str::to_owned)
        .collect();

    // A dylib with no trie is not a dylib with no exports. Falling back rather
    // than reporting an empty library, because an empty library is
    // indistinguishable downstream from a library that defines nothing, and
    // the failure it produces — every symbol undefined — points at the caller
    // rather than at the library.
    if symbols.is_empty() {
        symbols = file
            .symbols()
            .filter(|symbol| symbol.is_global() && symbol.is_definition())
            .filter_map(|symbol| symbol.name().ok())
            .map(str::to_owned)
            .collect();
    }

    symbols.sort_unstable();
    symbols.dedup();
    Ok(DylibExports {
        install_name,
        symbols,
    })
}

/// Whether `path` looks like something [`read_dylib_exports`] can read.
///
/// A cheap magic-number check, so a caller can tell a Mach-O from a text stub
/// without reading either in full.
pub fn is_mach_o(data: &[u8]) -> bool {
    const MAGICS: [u32; 4] = [
        object::macho::MH_MAGIC_64,
        object::macho::MH_CIGAM_64,
        object::macho::FAT_MAGIC,
        object::macho::FAT_CIGAM,
    ];
    let Some(head) = data.get(..4) else {
        return false;
    };
    let magic = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    MAGICS.contains(&magic)
}

/// The arm64 slice of a universal binary, or the whole file if it is thin.
fn arm64_slice<'a>(data: &'a [u8], path: &Path) -> Result<&'a [u8], DylibError> {
    use object::read::macho::FatArch;

    let Some(head) = data.get(..4) else {
        return Err(DylibError::NotADylib {
            path: path.to_path_buf(),
        });
    };
    let magic = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    if magic != object::macho::FAT_MAGIC && magic != object::macho::FAT_MAGIC_64 {
        return Ok(data);
    }

    let malformed = |detail: &str| DylibError::Malformed {
        path: path.to_path_buf(),
        detail: detail.to_owned(),
    };
    let arches = object::read::macho::MachOFatFile64::parse(data)
        .map(|fat| {
            fat.arches()
                .iter()
                .map(|arch| (arch.cputype(), arch.file_range()))
                .collect::<Vec<_>>()
        })
        .or_else(|_| {
            object::read::macho::MachOFatFile32::parse(data).map(|fat| {
                fat.arches()
                    .iter()
                    .map(|arch| (arch.cputype(), arch.file_range()))
                    .collect::<Vec<_>>()
            })
        })
        .map_err(|error| malformed(&error.to_string()))?;

    let (_, (offset, size)) = arches
        .into_iter()
        .find(|(cpu, _)| *cpu == object::macho::CPU_TYPE_ARM64)
        .ok_or_else(|| malformed("a universal binary with no arm64 slice"))?;
    let start =
        usize::try_from(offset).map_err(|_| malformed("a slice past the end of the file"))?;
    let len = usize::try_from(size).map_err(|_| malformed("a slice past the end of the file"))?;
    data.get(start..start + len)
        .ok_or_else(|| malformed("a slice past the end of the file"))
}

/// The `LC_ID_DYLIB` name.
fn install_name(
    header: &object::macho::MachHeader64<LittleEndian>,
    data: &[u8],
    path: &Path,
) -> Result<String, DylibError> {
    let malformed = |detail: &str| DylibError::Malformed {
        path: path.to_path_buf(),
        detail: detail.to_owned(),
    };
    let mut commands = header
        .load_commands(LittleEndian, data, 0)
        .map_err(|error| malformed(&error.to_string()))?;
    while let Some(command) = commands
        .next()
        .map_err(|error| malformed(&error.to_string()))?
    {
        let Ok(LoadCommandVariant::IdDylib(dylib)) = command.variant() else {
            continue;
        };
        let name = command
            .string(LittleEndian, dylib.dylib.name)
            .map_err(|error| malformed(&error.to_string()))?;
        return std::str::from_utf8(name)
            .map(str::to_owned)
            .map_err(|_| malformed("an install name that is not UTF-8"));
    }
    // Every `MH_DYLIB` has one; a file that does not is not something to guess
    // an identity for, because the identity is what every import records.
    Err(malformed("a dynamic library with no LC_ID_DYLIB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real dylib, built here.
    ///
    /// The first version of this looked for `/usr/lib/libSystem.B.dylib` and
    /// skipped the test when it was absent — and it is *always* absent, because
    /// macOS keeps the system libraries in the dyld shared cache and not on
    /// disk. The test passed by returning early, which is the failure mode the
    /// thing it was testing already had.
    fn a_real_dylib(scratch: &Path, name: &str, install: &str) -> PathBuf {
        let source = scratch.join(format!("{name}.c"));
        let built = scratch.join(format!("lib{name}.dylib"));
        std::fs::write(&source, b"int exported_answer(void) { return 42; }\n").expect("write");
        let done = std::process::Command::new("cc")
            .args(["-dynamiclib", "-o"])
            .arg(&built)
            .arg(&source)
            .args(["-install_name", install])
            .output()
            .expect("cc should run");
        assert!(
            done.status.success(),
            "cc failed: {}",
            String::from_utf8_lossy(&done.stderr)
        );
        built
    }

    fn scratch(name: &str) -> PathBuf {
        let at = std::env::temp_dir().join(format!("blinker-dylib-test-{name}"));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("scratch");
        at
    }

    #[test]
    fn a_text_stub_is_not_a_dylib() {
        let stub = b"--- !tapi-tbd\ninstall-name: /usr/lib/libfoo.dylib\n";
        let error = read_dylib_exports_from(stub, Path::new("libfoo.tbd"));
        assert!(matches!(error, Err(DylibError::NotADylib { .. })));
        assert!(!is_mach_o(stub));
    }

    #[test]
    fn an_empty_file_is_not_a_dylib() {
        assert!(matches!(
            read_dylib_exports_from(&[], Path::new("empty")),
            Err(DylibError::NotADylib { .. })
        ));
        assert!(!is_mach_o(&[]));
    }

    #[test]
    fn a_real_dylib_reports_its_install_name_and_symbols() {
        let at = scratch("exports");
        let path = a_real_dylib(&at, "answer", "/nowhere/libanswer.1.dylib");
        let read = read_dylib_exports(&path).expect("the dylib should be readable");

        // The *recorded* name, not the path it was found at. A link binds
        // against what dyld will look for.
        assert_eq!(read.install_name, "/nowhere/libanswer.1.dylib");
        assert!(
            read.symbols.iter().any(|name| name == "_exported_answer"),
            "the exported function should be there, got {:?}",
            read.symbols
        );
        // Sorted and unique, which the caller relies on.
        assert!(read.symbols.windows(2).all(|pair| pair[0] < pair[1]));
        let _ = std::fs::remove_dir_all(&at);
    }

    /// The case the whole module exists for: a library with no `.tbd` beside
    /// it, resolved from a `-L` path, whose symbols a link needs.
    #[test]
    fn a_dylib_with_no_text_stub_beside_it_is_still_readable() {
        let at = scratch("nostub");
        let path = a_real_dylib(&at, "lonely", "/nowhere/liblonely.dylib");
        assert!(
            !at.join("liblonely.tbd").exists(),
            "the fixture must not have a stub, or it proves nothing"
        );
        let read = read_dylib_exports(&path).expect("readable without any stub");
        assert!(read.symbols.iter().any(|name| name == "_exported_answer"));
        let _ = std::fs::remove_dir_all(&at);
    }

    /// A universal binary is routine for Homebrew bottles, and only our slice
    /// is wanted.
    #[test]
    fn a_universal_binary_is_read_through_its_arm64_slice() {
        let at = scratch("fat");
        let source = at.join("fat.c");
        let built = at.join("libfat.dylib");
        std::fs::write(&source, b"int exported_answer(void) { return 42; }\n").expect("write");
        let done = std::process::Command::new("cc")
            .args(["-dynamiclib", "-arch", "arm64", "-arch", "x86_64", "-o"])
            .arg(&built)
            .arg(&source)
            .args(["-install_name", "/nowhere/libfat.dylib"])
            .output()
            .expect("cc should run");
        if !done.status.success() {
            // A machine with no x86_64 SDK cannot build one; that is not this
            // module's failure, and the thin case is covered above.
            let _ = std::fs::remove_dir_all(&at);
            return;
        }
        let read = read_dylib_exports(&built).expect("a universal binary should be readable");
        assert_eq!(read.install_name, "/nowhere/libfat.dylib");
        assert!(read.symbols.iter().any(|name| name == "_exported_answer"));
        let _ = std::fs::remove_dir_all(&at);
    }
}
