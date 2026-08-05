//! Reading a `.tbd` without building a YAML tree.
//!
//! # Why this exists
//!
//! `libSystem.B.tbd` is 326 KB of YAML holding 9,264 exported symbols across
//! 40 documents, and a general YAML parser spends 3–6 ms on it: every scalar
//! becomes a `String`, every mapping a hash map, and the whole tree is built
//! so that seven keys per document can be read out of it and the rest
//! discarded. On the workspace's own link that parse was the largest single
//! item left in a cold link.
//!
//! # Why a scanner is safe here, and how that is established
//!
//! It is not safe on the strength of the format being simple. TBD v4 is YAML,
//! and a scanner that is *nearly* right silently under-reports what a library
//! exports — which does not fail loudly, it fails as an undefined symbol in
//! some program that happened to call the one function that went missing.
//!
//! Two things make it answerable:
//!
//! - **The scanner refuses what it does not recognise.** Every construct it
//!   has not been taught is [`Unsupported`], and [`super::parse_tbd`] falls
//!   back to the YAML parser rather than guessing. Being wrong requires
//!   misreading something it claims to understand, not merely meeting
//!   something new.
//! - **Agreement is checked against the real corpus.** The SDK ships 6,098
//!   `.tbd` files; the differential test parses every one of them both ways
//!   and asserts the results are equal — and that the fallback never fired.
//!   That is the oracle, and it is why the YAML parser stays a dependency.

use std::borrow::Cow;

use crate::{target::Target, SymbolSet, TbdDocument, TbdFile};
use std::path::Path;

/// A construct the scanner has not been taught.
///
/// Carries no detail on purpose: nothing acts on it but the fallback, and a
/// reason a caller cannot use is a reason to be tempted to log it.
#[derive(Debug)]
pub struct Unsupported;

/// A value on the right of a key: either a scalar or a flow sequence.
///
/// Two shapes rather than a general node type, because TBD v4 nests exactly
/// two levels — a document's keys, and the mappings inside its block
/// sequences — and a general tree is the thing being avoided.
enum Value<'a> {
    Scalar(Cow<'a, str>),
    List(Vec<Cow<'a, str>>),
}

impl<'a> Value<'a> {
    fn scalar(self) -> Option<Cow<'a, str>> {
        match self {
            Value::Scalar(text) => Some(text),
            Value::List(_) => None,
        }
    }

    fn list(self) -> Vec<Cow<'a, str>> {
        match self {
            Value::List(items) => items,
            Value::Scalar(_) => Vec::new(),
        }
    }
}

/// Parse a `.tbd`'s text, or report that it holds something unrecognised.
pub fn scan(text: &str, path: &Path) -> Result<TbdFile, Unsupported> {
    let lines: Vec<&str> = text.lines().collect();
    let mut documents = Vec::new();
    let mut at = 0;

    while at < lines.len() {
        let line = lines[at];
        if line.trim().is_empty() {
            at += 1;
            continue;
        }
        // `...` ends the stream; a bare `---` or `--- !tapi-tbd` starts a
        // document. Anything else at the top level is content outside a
        // document, which this does not model.
        if line.starts_with("...") {
            at += 1;
            continue;
        }
        if !line.starts_with("---") {
            return Err(Unsupported);
        }
        at += 1;
        if let Some(document) = scan_document(&lines, &mut at)? {
            documents.push(document);
        }
    }

    Ok(TbdFile {
        path: path.to_path_buf(),
        documents,
    })
}

/// One document's keys, up to the next document or the end of the stream.
///
/// `None` for a document with no `install-name`: it is not a library stub, and
/// the YAML path skips it for the same reason.
fn scan_document(lines: &[&str], at: &mut usize) -> Result<Option<TbdDocument>, Unsupported> {
    let mut install_name = None;
    let mut targets = Vec::new();
    let mut current_version = None;
    let mut compatibility_version = None;
    let mut reexported_libraries = Vec::new();
    let mut exports = Vec::new();
    let mut reexports = Vec::new();

    while *at < lines.len() {
        let line = lines[*at];
        if line.trim().is_empty() {
            *at += 1;
            continue;
        }
        if line.starts_with("---") || line.starts_with("...") {
            break;
        }
        if indent_of(line) != 0 {
            return Err(Unsupported);
        }
        let (key, rest) = split_key(line).ok_or(Unsupported)?;

        if rest.is_empty() {
            *at += 1;
            let entries = scan_block_sequence(lines, at)?;
            match key {
                "reexported-libraries" => {
                    for entry in &entries {
                        reexported_libraries.extend(
                            find(entry, "libraries")
                                .into_iter()
                                .map(|name| name.into_owned()),
                        );
                    }
                }
                "exports" => exports = symbol_sets(&entries),
                "reexports" => reexports = symbol_sets(&entries),
                _ => {}
            }
            continue;
        }

        let value = scan_value(lines, at, rest)?;
        match key {
            "install-name" => install_name = value.scalar().map(Cow::into_owned),
            "current-version" => current_version = value.scalar().map(Cow::into_owned),
            "compatibility-version" => compatibility_version = value.scalar().map(Cow::into_owned),
            "targets" => targets = parse_targets(value.list()),
            _ => {}
        }
    }

    let Some(install_name) = install_name else {
        return Ok(None);
    };
    Ok(Some(TbdDocument {
        install_name,
        targets,
        current_version,
        compatibility_version,
        reexported_libraries,
        exports,
        reexports,
    }))
}

/// A block sequence of mappings: `  - key: value` with `    key: value` under
/// it. Every list-valued key in TBD v4 has this shape.
///
/// The two indents are fixed at 2 and 4 rather than merely increasing. YAML
/// permits any consistent indent; the SDK's writer emits these, and accepting
/// only what has been seen is what keeps the fallback honest.
#[allow(clippy::type_complexity)]
fn scan_block_sequence<'a>(
    lines: &[&'a str],
    at: &mut usize,
) -> Result<Vec<Vec<(&'a str, Value<'a>)>>, Unsupported> {
    let mut entries: Vec<Vec<(&'a str, Value<'a>)>> = Vec::new();

    while *at < lines.len() {
        let line = lines[*at];
        if line.trim().is_empty() {
            *at += 1;
            continue;
        }
        let indent = indent_of(line);
        if indent == 0 || line.starts_with("---") || line.starts_with("...") {
            break;
        }
        if indent != 2 || !line[2..].starts_with("- ") {
            return Err(Unsupported);
        }

        // The item's first pair sits on the `- ` line itself, at column 4.
        let mut pairs = Vec::new();
        let (key, rest) = split_key(&line[4..]).ok_or(Unsupported)?;
        if rest.is_empty() {
            return Err(Unsupported);
        }
        pairs.push((key, scan_value(lines, at, rest)?));

        // Then its remaining pairs, at the same column, until the next item or
        // the end of the sequence.
        while *at < lines.len() {
            let line = lines[*at];
            if line.trim().is_empty() {
                *at += 1;
                continue;
            }
            if indent_of(line) != 4 {
                break;
            }
            let (key, rest) = split_key(&line[4..]).ok_or(Unsupported)?;
            if rest.is_empty() {
                return Err(Unsupported);
            }
            pairs.push((key, scan_value(lines, at, rest)?));
        }
        entries.push(pairs);
    }

    Ok(entries)
}

/// Read the value that starts at `rest`, advancing `at` past every line it
/// occupies. A flow sequence spans as many lines as it needs; `libSystem`'s
/// re-export list is 39 libraries over 20 of them.
fn scan_value<'a>(
    lines: &[&'a str],
    at: &mut usize,
    rest: &'a str,
) -> Result<Value<'a>, Unsupported> {
    if !rest.starts_with('[') {
        *at += 1;
        return Ok(Value::Scalar(unquote(rest)?));
    }

    let mut items = Vec::new();
    let mut pending = &rest[1..];
    loop {
        let closed = flow_items(pending, &mut items)?;
        *at += 1;
        if closed {
            return Ok(Value::List(items));
        }
        // An item split across a line boundary would be a YAML plain scalar
        // folded onto the next line with a space, and folding is not modelled.
        // A line that continues the sequence ends on the comma that separates
        // its last item from the next.
        let tail = pending.trim_end();
        if !tail.is_empty() && !tail.ends_with(',') && !tail.ends_with('[') {
            return Err(Unsupported);
        }
        pending = lines.get(*at).ok_or(Unsupported)?.trim();
    }
}

/// Pull comma-separated items out of one line of a flow sequence, reporting
/// whether the `]` that ends it was on this line.
fn flow_items<'a>(line: &'a str, items: &mut Vec<Cow<'a, str>>) -> Result<bool, Unsupported> {
    let bytes = line.as_bytes();
    let mut start = 0;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            // A quoted scalar swallows commas and brackets, which is the whole
            // reason this is a scan and not a `split(',')`.
            b'\'' | b'"' => {
                let quote = bytes[at];
                at += 1;
                loop {
                    let Some(next) = bytes[at..].iter().position(|b| *b == quote) else {
                        return Err(Unsupported);
                    };
                    at += next + 1;
                    // `''` inside a single-quoted scalar is one quote, not the
                    // end of it.
                    if quote == b'\'' && bytes.get(at) == Some(&b'\'') {
                        at += 1;
                        continue;
                    }
                    break;
                }
            }
            b',' => {
                push_item(&line[start..at], items)?;
                at += 1;
                start = at;
            }
            b']' => {
                push_item(&line[start..at], items)?;
                return Ok(true);
            }
            _ => at += 1,
        }
    }
    push_item(&line[start..], items)?;
    Ok(false)
}

/// Add one flow item, ignoring the empty text a trailing comma leaves behind.
fn push_item<'a>(text: &'a str, items: &mut Vec<Cow<'a, str>>) -> Result<(), Unsupported> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    items.push(unquote(text)?);
    Ok(())
}

/// Strip the quotes from a scalar, if it has any.
///
/// Single quotes are YAML's literal form, where `''` is one quote. Double
/// quotes carry backslash escapes, which are not modelled — a double-quoted
/// scalar containing one is refused rather than mis-read.
fn unquote(text: &str) -> Result<Cow<'_, str>, Unsupported> {
    let text = text.trim();
    let mut characters = text.chars();
    match (characters.next(), text.len() >= 2, characters.next_back()) {
        (Some('\''), true, Some('\'')) => {
            let inner = &text[1..text.len() - 1];
            Ok(match inner.contains("''") {
                true => Cow::Owned(inner.replace("''", "'")),
                false => Cow::Borrowed(inner),
            })
        }
        (Some('"'), true, Some('"')) => {
            let inner = &text[1..text.len() - 1];
            match inner.contains('\\') {
                true => Err(Unsupported),
                false => Ok(Cow::Borrowed(inner)),
            }
        }
        // An unterminated quote is not a plain scalar that happens to start
        // with one.
        (Some('\'' | '"'), _, _) => Err(Unsupported),
        _ => Ok(Cow::Borrowed(text)),
    }
}

/// Split `key: rest` at the first colon, requiring a plain YAML key.
///
/// A key is `[a-z0-9-]+` in every TBD document; refusing anything else is what
/// keeps a colon inside a value from being read as a key boundary.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return None;
    }
    Some((key, line[colon + 1..].trim()))
}

/// How many spaces a line begins with. Tabs are not indentation in YAML and
/// are not accepted as any here either.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// The value of `key` in one block-sequence entry, as a list.
fn find<'a>(entry: &[(&str, Value<'a>)], key: &str) -> Vec<Cow<'a, str>> {
    for (name, value) in entry {
        if *name == key {
            return match value {
                Value::List(items) => items.clone(),
                Value::Scalar(_) => Vec::new(),
            };
        }
    }
    Vec::new()
}

fn parse_targets(items: Vec<Cow<'_, str>>) -> Vec<Target> {
    items
        .iter()
        .filter_map(|item| Target::parse(item))
        .collect()
}

/// Turn `exports:`/`reexports:` entries into symbol sets.
///
/// The key list and the "targets or nothing" rule are the YAML path's, because
/// the two have to agree exactly and the differential test says so.
fn symbol_sets(entries: &[Vec<(&str, Value<'_>)>]) -> Vec<SymbolSet> {
    let mut sets = Vec::new();
    for entry in entries {
        let targets = parse_targets(find(entry, "targets"));
        if targets.is_empty() {
            continue;
        }
        let mut symbols = Vec::new();
        for key in ["symbols", "objc-classes", "objc-ivars", "weak-symbols"] {
            symbols.extend(find(entry, key).into_iter().map(Cow::into_owned));
        }
        if !symbols.is_empty() {
            sets.push(SymbolSet { targets, symbols });
        }
    }
    sets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(text: &str) -> TbdFile {
        scan(text, Path::new("/x.tbd")).expect("scans")
    }

    #[test]
    fn a_flow_sequence_spanning_lines_is_one_list() {
        let file = scanned(
            "--- !tapi-tbd\ntargets:         [ arm64e-macos ]\n\
             install-name:    '/usr/lib/a.dylib'\nexports:\n  \
             - targets:         [ arm64e-macos ]\n    \
             symbols:         [ _a, _b, \n                       _c ]\n",
        );
        assert_eq!(file.documents[0].exports[0].symbols, ["_a", "_b", "_c"]);
    }

    #[test]
    fn a_quoted_symbol_may_contain_a_comma_or_a_bracket() {
        let file = scanned(
            "--- !tapi-tbd\ntargets:         [ arm64e-macos ]\n\
             install-name:    '/usr/lib/a.dylib'\nexports:\n  \
             - targets:         [ arm64e-macos ]\n    \
             symbols:         [ '_a,b', '_c]d', '_it''s' ]\n",
        );
        assert_eq!(
            file.documents[0].exports[0].symbols,
            ["_a,b", "_c]d", "_it's"]
        );
    }

    /// The refusal is the safety property: an unmodelled construct has to
    /// reach the YAML parser rather than be guessed at.
    #[test]
    fn constructs_it_was_not_taught_are_refused_rather_than_guessed() {
        for text in [
            // A nested block sequence inside a sequence item.
            "--- !tapi-tbd\nexports:\n  - targets:\n      - arm64e-macos\n",
            // An indent the SDK's writer does not emit.
            "--- !tapi-tbd\nexports:\n   - targets:         [ arm64e-macos ]\n",
            // A double-quoted scalar carrying an escape.
            "--- !tapi-tbd\ninstall-name:    \"/usr/lib/a\\tb.dylib\"\n",
            // Content before any document marker.
            "install-name: '/usr/lib/a.dylib'\n",
            // An unterminated flow sequence.
            "--- !tapi-tbd\ntargets:         [ arm64e-macos\n",
        ] {
            assert!(
                scan(text, Path::new("/x.tbd")).is_err(),
                "silently accepted {text:?}"
            );
        }
    }

    /// A document with no `install-name` is not a library stub. The YAML path
    /// skips it, so this must too, and the file around it must still parse.
    #[test]
    fn a_document_without_an_install_name_is_skipped() {
        let file = scanned(
            "--- !tapi-tbd\ntbd-version:     4\n--- !tapi-tbd\n\
             install-name:    '/usr/lib/a.dylib'\n...\n",
        );
        assert_eq!(file.documents.len(), 1);
        assert_eq!(file.documents[0].install_name, "/usr/lib/a.dylib");
    }
}
