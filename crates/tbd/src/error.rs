//! `.tbd` parsing errors.

use std::path::PathBuf;

#[derive(Debug)]
pub enum TbdError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not YAML we can read.
    Malformed { path: PathBuf, detail: String },
    /// Valid YAML, but containing no library stub.
    ///
    /// Distinct from `Malformed` because the causes differ: a corrupt file
    /// versus a file that parsed fine and simply is not a stub.
    NoDocuments { path: PathBuf },
}

impl std::fmt::Display for TbdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TbdError::Io { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            TbdError::Malformed { path, detail } => {
                write!(f, "malformed .tbd {}: {detail}", path.display())
            }
            TbdError::NoDocuments { path } => write!(
                f,
                "{} contains no library stub (no install-name found)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TbdError {}
