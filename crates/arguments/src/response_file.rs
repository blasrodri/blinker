//! Response-file (`@path`) expansion.
//!
//! When a link has enough inputs to threaten the OS argument-length limit,
//! `rustc` writes the arguments to a file and passes `@<path>` instead. The
//! driver reads the file and splices its contents into the argument vector at
//! that position.
//!
//! Small links do not use them — the ground-truth capture from a minimal
//! `cargo build` contained none — but any workspace of real size will, so
//! expansion is part of M0 rather than something to discover later.
//!
//! # Tokenization
//!
//! Tokens are whitespace-separated, with three escapes honoured, matching the
//! behaviour of the Apple toolchain's driver:
//!
//! - `'single quoted'` — everything up to the closing quote is literal.
//! - `"double quoted"` — as above, but backslash escapes are processed.
//! - `\x` outside quotes — the next character is taken literally.
//!
//! An unterminated quote is an error rather than a silently-truncated token:
//! guessing here would drop or merge input paths.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Maximum depth of nested `@file` references.
///
/// Cycles are caught exactly via the visited set; this bound additionally stops
/// pathological non-cyclic nesting (a chain of distinct files) from exhausting
/// the stack.
const MAX_DEPTH: usize = 32;

#[derive(Debug)]
pub enum ResponseFileError {
    /// The `@`-referenced file could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A quoted token was never closed.
    UnterminatedQuote { path: PathBuf, quote: char },
    /// The file references itself, directly or transitively.
    Cycle { path: PathBuf },
    /// Nesting exceeded [`MAX_DEPTH`].
    TooDeep { path: PathBuf },
}

impl std::fmt::Display for ResponseFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseFileError::Read { path, source } => {
                write!(f, "cannot read response file {}: {source}", path.display())
            }
            ResponseFileError::UnterminatedQuote { path, quote } => write!(
                f,
                "unterminated {quote} quote in response file {}",
                path.display()
            ),
            ResponseFileError::Cycle { path } => {
                write!(f, "response file cycle at {}", path.display())
            }
            ResponseFileError::TooDeep { path } => write!(
                f,
                "response files nested more than {MAX_DEPTH} deep at {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ResponseFileError {}

/// Expand every `@path` argument, recursively, preserving argument order.
///
/// Arguments that are not `@`-prefixed pass through untouched.
pub fn expand_response_files(argv: &[String]) -> Result<Vec<String>, ResponseFileError> {
    let mut out = Vec::with_capacity(argv.len());
    let mut visited = HashSet::new();
    for arg in argv {
        expand_one(arg, &mut out, &mut visited, 0)?;
    }
    Ok(out)
}

fn expand_one(
    arg: &str,
    out: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<(), ResponseFileError> {
    let Some(path_str) = arg.strip_prefix('@') else {
        out.push(arg.to_string());
        return Ok(());
    };

    let path = PathBuf::from(path_str);
    if depth >= MAX_DEPTH {
        return Err(ResponseFileError::TooDeep { path });
    }

    // Canonicalize so two spellings of the same file are recognized as a cycle.
    // If canonicalization fails the file is unreadable anyway, and the read
    // below produces the better error message.
    let key = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !visited.insert(key.clone()) {
        return Err(ResponseFileError::Cycle { path });
    }

    let contents = std::fs::read_to_string(&path).map_err(|source| ResponseFileError::Read {
        path: path.clone(),
        source,
    })?;

    for token in tokenize(&contents, &path)? {
        expand_one(&token, out, visited, depth + 1)?;
    }

    // Leaving the frame lets sibling branches reference the same file legally;
    // only a self-referential *chain* is a cycle.
    visited.remove(&key);
    Ok(())
}

/// Split response-file contents into arguments.
fn tokenize(contents: &str, path: &Path) -> Result<Vec<String>, ResponseFileError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = contents.chars();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            '\'' | '"' => {
                // A quote starts a token even when the quoted body is empty,
                // so `''` produces one empty argument rather than none.
                has_token = true;
                let quote = c;
                loop {
                    match chars.next() {
                        None => {
                            return Err(ResponseFileError::UnterminatedQuote {
                                path: path.to_path_buf(),
                                quote,
                            })
                        }
                        Some(c) if c == quote => break,
                        // Backslash escapes apply inside double quotes only,
                        // matching shell-style quoting.
                        Some('\\') if quote == '"' => {
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        Some(c) => current.push(c),
                    }
                }
            }
            '\\' => {
                has_token = true;
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }

    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    /// Create a scratch directory, panicking on failure — these tests have
    /// nothing useful to say if the filesystem is unavailable.
    fn scratch(tag: &str) -> Scratch {
        Scratch::dir(tag).expect("scratch directory")
    }

    /// Write a response file into `dir` and return its path.
    fn write(dir: &Scratch, name: &str, contents: &str) -> PathBuf {
        Scratch::write(dir, name, contents).expect("writable")
    }

    fn tok(s: &str) -> Vec<String> {
        tokenize(s, Path::new("test")).unwrap()
    }

    #[test]
    fn splits_on_arbitrary_whitespace() {
        assert_eq!(tok("a b\tc\nd\r\ne"), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn ignores_leading_and_trailing_whitespace() {
        assert_eq!(tok("  \n a b \t "), vec!["a", "b"]);
    }

    #[test]
    fn empty_content_yields_no_tokens() {
        assert!(tok("").is_empty());
        assert!(tok("   \n\t ").is_empty());
    }

    #[test]
    fn single_quotes_preserve_whitespace_and_backslashes() {
        // Backslash is literal inside single quotes — a path containing one
        // must survive unchanged.
        assert_eq!(tok(r"'a b' 'c\d'"), vec!["a b", r"c\d"]);
    }

    #[test]
    fn double_quotes_process_backslash_escapes() {
        assert_eq!(tok(r#""a b" "c\"d""#), vec!["a b", r#"c"d"#]);
    }

    #[test]
    fn backslash_escapes_whitespace_outside_quotes() {
        assert_eq!(tok(r"a\ b c"), vec!["a b", "c"]);
    }

    #[test]
    fn quotes_can_be_adjacent_within_one_token() {
        assert_eq!(tok(r#"-L"/some path"/lib"#), vec!["-L/some path/lib"]);
    }

    #[test]
    fn empty_quotes_produce_an_empty_argument() {
        // Distinguishing "" from absent matters: dropping it would shift every
        // subsequent positional argument.
        assert_eq!(tok(r#"a "" b"#), vec!["a", "", "b"]);
    }

    #[test]
    fn unterminated_quote_is_an_error_not_a_truncated_token() {
        for input in [r#""abc"#, r"'abc"] {
            let err = tokenize(input, Path::new("test")).unwrap_err();
            assert!(matches!(err, ResponseFileError::UnterminatedQuote { .. }));
        }
    }

    #[test]
    fn non_response_arguments_pass_through_unchanged() {
        let argv = vec!["-o".to_string(), "out".to_string()];
        assert_eq!(expand_response_files(&argv).unwrap(), argv);
    }

    #[test]
    fn expands_in_place_preserving_surrounding_order() {
        let dir = scratch("order");
        let rsp = write(&dir, "args.rsp", "b.o c.o");
        let argv = vec![
            "a.o".to_string(),
            format!("@{}", rsp.display()),
            "d.o".to_string(),
        ];
        assert_eq!(
            expand_response_files(&argv).unwrap(),
            vec!["a.o", "b.o", "c.o", "d.o"]
        );
    }

    #[test]
    fn expands_nested_response_files() {
        let dir = scratch("nested");
        let inner = write(&dir, "inner.rsp", "c.o d.o");
        let outer = write(&dir, "outer.rsp", &format!("b.o @{} e.o", inner.display()));
        let argv = vec![format!("@{}", outer.display())];
        assert_eq!(
            expand_response_files(&argv).unwrap(),
            vec!["b.o", "c.o", "d.o", "e.o"]
        );
    }

    #[test]
    fn detects_self_referential_cycle() {
        let dir = scratch("cycle");
        let path = dir.join("self.rsp");
        std::fs::write(&path, format!("@{}", path.display())).unwrap();
        let err = expand_response_files(&[format!("@{}", path.display())]).unwrap_err();
        assert!(matches!(err, ResponseFileError::Cycle { .. }));
    }

    #[test]
    fn detects_mutual_cycle() {
        let dir = scratch("mutual");
        let a = dir.join("a.rsp");
        let b = dir.join("b.rsp");
        std::fs::write(&a, format!("@{}", b.display())).unwrap();
        std::fs::write(&b, format!("@{}", a.display())).unwrap();
        let err = expand_response_files(&[format!("@{}", a.display())]).unwrap_err();
        assert!(matches!(err, ResponseFileError::Cycle { .. }));
    }

    #[test]
    fn same_file_referenced_twice_in_sequence_is_not_a_cycle() {
        // Two sibling references are legal; only a containment chain is a cycle.
        let dir = scratch("siblings");
        let rsp = write(&dir, "shared.rsp", "x.o");
        let argv = vec![format!("@{}", rsp.display()), format!("@{}", rsp.display())];
        assert_eq!(expand_response_files(&argv).unwrap(), vec!["x.o", "x.o"]);
    }

    #[test]
    fn missing_response_file_reports_the_path() {
        let err = expand_response_files(&["@/nonexistent/blinker/x.rsp".to_string()]).unwrap_err();
        match err {
            ResponseFileError::Read { path, .. } => {
                assert_eq!(path, PathBuf::from("/nonexistent/blinker/x.rsp"));
            }
            other => panic!("expected Read error, got {other:?}"),
        }
    }

    #[test]
    fn handles_realistic_quoted_paths_with_spaces() {
        let dir = scratch("paths");
        let rsp = write(
            &dir,
            "args.rsp",
            "'/Users/me/My Projects/a.o'\n\"/Users/me/My Projects/b.rlib\"\n-lSystem",
        );
        assert_eq!(
            expand_response_files(&[format!("@{}", rsp.display())]).unwrap(),
            vec![
                "/Users/me/My Projects/a.o",
                "/Users/me/My Projects/b.rlib",
                "-lSystem"
            ]
        );
    }
}
