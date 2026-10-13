//! The `--bindings-file` text format shared by the `fgdb` CLI and `fgdbd`.
//!
//! There is no durable catalog yet, so every invocation names its symbols
//! explicitly. A bindings file is the same explicit mapping as repeated
//! `--label`/`--relation`/`--property` flags, written once: one binding per
//! line as `label <name>=<u32>`, `relation <name>=<u32>` or
//! `property <name>=<u32>`. Blank lines and lines starting with `#` are
//! ignored. The name is everything between the kind and the FIRST `=`, as for
//! the flags, with surrounding whitespace removed. Uniqueness of names and IDs
//! per kind is the host's rule, applied exactly as for flags.

use crate::GraphSymbolKind;

/// Largest bindings file a host reads, in bytes.
pub const MAX_SYMBOL_BINDINGS_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolBinding {
    pub kind: GraphSymbolKind,
    pub name: String,
    pub id: u32,
    /// One-based source line, for the host's duplicate diagnostics.
    pub line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolBindingsError {
    /// One-based line; zero for a whole-file refusal.
    pub line: usize,
    pub reason: &'static str,
}

impl core::fmt::Display for SymbolBindingsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line == 0 {
            write!(f, "bindings file: {}", self.reason)
        } else {
            write!(f, "bindings file line {}: {}", self.line, self.reason)
        }
    }
}
impl core::error::Error for SymbolBindingsError {}

/// Parse a complete bindings file. Refuses non-UTF-8 input, oversized input
/// and any line that is not a comment, blank, or one well-formed binding.
pub fn parse_symbol_bindings(bytes: &[u8]) -> Result<Vec<SymbolBinding>, SymbolBindingsError> {
    if bytes.len() > MAX_SYMBOL_BINDINGS_BYTES {
        return Err(SymbolBindingsError {
            line: 0,
            reason: "larger than 1 MiB",
        });
    }
    let text = core::str::from_utf8(bytes).map_err(|_| SymbolBindingsError {
        line: 0,
        reason: "not UTF-8",
    })?;
    let mut bindings = Vec::new();
    for (at, raw) in text.lines().enumerate() {
        let line = at + 1;
        let refuse = |reason| SymbolBindingsError { line, reason };
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (kind, rest) = trimmed
            .split_once(char::is_whitespace)
            .ok_or(refuse("expected `<kind> <name>=<u32>`"))?;
        let kind = match kind {
            "label" => GraphSymbolKind::Label,
            "relation" => GraphSymbolKind::Relation,
            "property" => GraphSymbolKind::Property,
            _ => return Err(refuse("kind must be label, relation or property")),
        };
        let (name, id) = rest
            .split_once('=')
            .ok_or(refuse("expected `<name>=<u32>`"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(refuse("empty name"));
        }
        let id = id.trim();
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(refuse("ID must be a decimal u32"));
        }
        let id = id.parse().map_err(|_| refuse("ID must be a decimal u32"))?;
        bindings.push(SymbolBinding {
            kind,
            name: name.to_owned(),
            id,
            line,
        });
    }
    Ok(bindings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_comments_blanks_and_first_equals_split() {
        let parsed = parse_symbol_bindings(
            b"# social graph\n\nlabel Person=1\n  relation KNOWS = 7\r\nproperty first name=2\n",
        )
        .unwrap();
        assert_eq!(
            parsed
                .into_iter()
                .map(|b| (b.kind, b.name, b.id, b.line))
                .collect::<Vec<_>>(),
            vec![
                (GraphSymbolKind::Label, "Person".to_owned(), 1, 3),
                (GraphSymbolKind::Relation, "KNOWS".to_owned(), 7, 4),
                (GraphSymbolKind::Property, "first name".to_owned(), 2, 5),
            ]
        );
        // The name ends at the FIRST `=`, as for the flags.
        assert_eq!(
            parse_symbol_bindings(b"label P=1\nproperty a=b=3\n"),
            Err(SymbolBindingsError {
                line: 2,
                reason: "ID must be a decimal u32"
            })
        );
        assert_eq!(parse_symbol_bindings(b"").unwrap(), Vec::new());
    }

    #[test]
    fn malformed_lines_refuse_with_their_line() {
        for (text, line, reason) in [
            (&b"label"[..], 1, "expected `<kind> <name>=<u32>`"),
            (b"\nedge R=1", 2, "kind must be label, relation or property"),
            (b"label Person", 1, "expected `<name>=<u32>`"),
            (b"label =1", 1, "empty name"),
            (b"label P=", 1, "ID must be a decimal u32"),
            (b"label P=-1", 1, "ID must be a decimal u32"),
            (b"label P=+1", 1, "ID must be a decimal u32"),
            (b"label P=4294967296", 1, "ID must be a decimal u32"),
            (b"\xff", 0, "not UTF-8"),
        ] {
            assert_eq!(
                parse_symbol_bindings(text),
                Err(SymbolBindingsError { line, reason }),
                "{text:?}"
            );
        }
        let oversized = vec![b'#'; MAX_SYMBOL_BINDINGS_BYTES + 1];
        assert_eq!(
            parse_symbol_bindings(&oversized),
            Err(SymbolBindingsError {
                line: 0,
                reason: "larger than 1 MiB"
            })
        );
        assert_eq!(
            parse_symbol_bindings(b"label P=4294967295").unwrap()[0].id,
            u32::MAX
        );
    }
}
