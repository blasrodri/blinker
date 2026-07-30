//! Target triples as `.tbd` files spell them, and the rule for matching them.
//!
//! # The finding that shapes this module
//!
//! `libSystem.B.tbd` — the stub every Rust link resolves `-lSystem` against —
//! declares its targets as:
//!
//! ```text
//! targets: [ x86_64-macos, x86_64-maccatalyst, arm64e-macos, arm64e-maccatalyst ]
//! ```
//!
//! There is no `arm64-macos` in that list. Yet a plain arm64 binary links
//! against it every time, which was confirmed empirically rather than assumed:
//! `cc -arch arm64` produces a binary that `lipo -archs` reports as `arm64` and
//! `otool -L` shows linked to `/usr/lib/libSystem.B.dylib`.
//!
//! So **exact target matching would reject libSystem and fail every link.**
//! `arm64e` is `arm64` plus pointer authentication; the two share a symbol set,
//! and the toolchain treats an `arm64e` stub as satisfying an `arm64` link.
//! [`Architecture::is_compatible_with`] encodes that, and nothing else — the
//! rule is deliberately narrow, because widening it would start accepting
//! stubs for architectures that genuinely do not match.

use std::fmt;

/// A CPU architecture as spelled in a `.tbd` target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Architecture {
    Arm64,
    /// arm64 with pointer authentication. Distinct from [`Architecture::Arm64`]
    /// but link-compatible with it.
    Arm64e,
    X86_64,
    /// Haswell-and-later x86_64.
    X86_64h,
    /// 32-bit ARM and anything else we do not link against. Kept so a target
    /// list parses completely instead of failing on an entry we simply filter
    /// out.
    Other,
}

impl Architecture {
    pub fn parse(text: &str) -> Self {
        match text {
            "arm64" => Architecture::Arm64,
            "arm64e" => Architecture::Arm64e,
            "x86_64" => Architecture::X86_64,
            "x86_64h" => Architecture::X86_64h,
            _ => Architecture::Other,
        }
    }

    /// Whether a stub built for `self` can satisfy a link targeting `wanted`.
    ///
    /// The only cross-architecture case is arm64 ↔ arm64e, and it exists
    /// because the system stubs require it (see the module docs). Everything
    /// else must match exactly.
    pub fn is_compatible_with(self, wanted: Architecture) -> bool {
        use Architecture::*;
        match (self, wanted) {
            (a, b) if a == b => true,
            (Arm64, Arm64e) | (Arm64e, Arm64) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Architecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Architecture::Arm64 => "arm64",
            Architecture::Arm64e => "arm64e",
            Architecture::X86_64 => "x86_64",
            Architecture::X86_64h => "x86_64h",
            Architecture::Other => "other",
        })
    }
}

/// The platform half of a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Platform {
    MacOs,
    /// iOS apps running on macOS. Present in every system stub and never what
    /// we want, so it must be distinguished from `MacOs` rather than folded in.
    MacCatalyst,
    IOs,
    IOsSimulator,
    TvOs,
    WatchOs,
    Other,
}

impl Platform {
    pub fn parse(text: &str) -> Self {
        match text {
            "macos" => Platform::MacOs,
            "maccatalyst" => Platform::MacCatalyst,
            "ios" => Platform::IOs,
            "ios-simulator" => Platform::IOsSimulator,
            "tvos" => Platform::TvOs,
            "watchos" => Platform::WatchOs,
            _ => Platform::Other,
        }
    }
}

/// One `arch-platform` entry from a `targets:` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Target {
    pub architecture: Architecture,
    pub platform: Platform,
}

impl Target {
    /// Parse `arm64e-macos` and friends.
    ///
    /// Split on the *first* hyphen only: platforms themselves contain hyphens
    /// (`ios-simulator`), so splitting on all of them would mangle them.
    pub fn parse(text: &str) -> Option<Self> {
        let (arch, platform) = text.trim().split_once('-')?;
        if arch.is_empty() || platform.is_empty() {
            return None;
        }
        Some(Target {
            architecture: Architecture::parse(arch),
            platform: Platform::parse(platform),
        })
    }

    /// Whether this target can satisfy a link for `wanted`.
    ///
    /// Platform must match exactly; architecture uses the compatibility rule.
    pub fn satisfies(self, wanted: Target) -> bool {
        self.platform == wanted.platform
            && self.architecture.is_compatible_with(wanted.architecture)
    }

    /// The target a normal Rust `aarch64-apple-darwin` build wants.
    pub fn aarch64_macos() -> Self {
        Target {
            architecture: Architecture::Arm64,
            platform: Platform::MacOs,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:?}", self.architecture, self.platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_target_spellings_the_sdk_uses() {
        let cases = [
            ("arm64-macos", Architecture::Arm64, Platform::MacOs),
            ("arm64e-macos", Architecture::Arm64e, Platform::MacOs),
            ("x86_64-macos", Architecture::X86_64, Platform::MacOs),
            (
                "arm64e-maccatalyst",
                Architecture::Arm64e,
                Platform::MacCatalyst,
            ),
            ("x86_64h-macos", Architecture::X86_64h, Platform::MacOs),
        ];
        for (text, arch, platform) in cases {
            let target = Target::parse(text).unwrap_or_else(|| panic!("{text} parses"));
            assert_eq!(target.architecture, arch, "{text}");
            assert_eq!(target.platform, platform, "{text}");
        }
    }

    /// Platforms contain hyphens, so the split must take only the first one.
    #[test]
    fn parses_platforms_containing_hyphens() {
        let target = Target::parse("arm64-ios-simulator").expect("parses");
        assert_eq!(target.architecture, Architecture::Arm64);
        assert_eq!(target.platform, Platform::IOsSimulator);
    }

    #[test]
    fn rejects_malformed_targets_rather_than_guessing() {
        for text in ["", "arm64", "-macos", "arm64-"] {
            assert!(Target::parse(text).is_none(), "{text:?} should not parse");
        }
    }

    #[test]
    fn unknown_architectures_and_platforms_parse_as_other() {
        // A target list must parse completely; unrecognised entries are
        // filtered out later rather than failing the file.
        let target = Target::parse("riscv64-freebsd").expect("parses");
        assert_eq!(target.architecture, Architecture::Other);
        assert_eq!(target.platform, Platform::Other);
    }

    /// The rule that keeps every link working. `libSystem.B.tbd` offers only
    /// `arm64e-macos`, and an arm64 link must accept it.
    #[test]
    fn arm64_and_arm64e_are_link_compatible() {
        assert!(Architecture::Arm64e.is_compatible_with(Architecture::Arm64));
        assert!(Architecture::Arm64.is_compatible_with(Architecture::Arm64e));
    }

    #[test]
    fn the_compatibility_rule_is_narrow() {
        // Widening it would start accepting stubs that genuinely do not match.
        assert!(!Architecture::X86_64.is_compatible_with(Architecture::Arm64));
        assert!(!Architecture::Arm64.is_compatible_with(Architecture::X86_64));
        assert!(!Architecture::X86_64h.is_compatible_with(Architecture::Arm64));
        assert!(!Architecture::Other.is_compatible_with(Architecture::Arm64));
    }

    #[test]
    fn identical_architectures_are_always_compatible() {
        for arch in [
            Architecture::Arm64,
            Architecture::Arm64e,
            Architecture::X86_64,
            Architecture::X86_64h,
        ] {
            assert!(arch.is_compatible_with(arch));
        }
    }

    /// The whole point: this is the exact target list libSystem declares, and
    /// an arm64 macOS link must find a match in it.
    #[test]
    fn libsystems_declared_targets_satisfy_an_arm64_macos_link() {
        let declared = [
            "x86_64-macos",
            "x86_64-maccatalyst",
            "arm64e-macos",
            "arm64e-maccatalyst",
        ];
        let wanted = Target::aarch64_macos();
        let matched: Vec<&str> = declared
            .iter()
            .filter(|t| Target::parse(t).is_some_and(|t| t.satisfies(wanted)))
            .copied()
            .collect();

        assert_eq!(
            matched,
            vec!["arm64e-macos"],
            "an arm64 macOS link should match arm64e-macos and nothing else"
        );
    }

    /// maccatalyst must not satisfy a macos link even at the same architecture.
    #[test]
    fn platform_must_match_exactly() {
        let wanted = Target::aarch64_macos();
        let catalyst = Target::parse("arm64e-maccatalyst").expect("parses");
        assert!(!catalyst.satisfies(wanted));

        let ios = Target::parse("arm64-ios").expect("parses");
        assert!(!ios.satisfies(wanted));
    }
}
