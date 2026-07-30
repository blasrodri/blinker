//! Archive parsing errors.

use std::path::PathBuf;

#[derive(Debug)]
pub enum ArchiveError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Not an archive, or an archive we cannot read.
    Malformed { path: PathBuf, detail: String },
    /// A member header claims a range outside the file.
    ///
    /// Reachable from a truncated or corrupted archive, and must be refused
    /// rather than clamped: a short read would hand the Mach-O parser a
    /// truncated object and turn a clear error into a confusing one.
    MemberOutOfBounds { path: PathBuf, member: String },
    /// More members than a `u32` ID can address.
    TooManyMembers { path: PathBuf, count: usize },
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            ArchiveError::Malformed { path, detail } => {
                write!(f, "malformed archive {}: {detail}", path.display())
            }
            ArchiveError::MemberOutOfBounds { path, member } => write!(
                f,
                "member `{member}` of {} lies outside the file",
                path.display()
            ),
            ArchiveError::TooManyMembers { path, count } => write!(
                f,
                "{} has {count} members, more than can be addressed",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ArchiveError {}
