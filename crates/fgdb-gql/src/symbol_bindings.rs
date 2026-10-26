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
use fgdb_delta_types::catalog::{CatalogSymbol, CatalogSymbolKind, SymbolCatalog};

/// Resolve one pinned catalog through the ordinary native binder interface.
///
/// The host owns the catalog coordinate and authorization. A restricted host
/// supplies its stable symbol-visibility predicate before preparation; the
/// same predicate gates forward lookup, reverse lookup and enumeration. This
/// adapter does not acquire a snapshot, validate a token or grant authority.
pub struct CatalogSymbolResolver<'a> {
    catalog: &'a SymbolCatalog,
    visible: Option<&'a (dyn Fn(CatalogSymbol) -> bool + Send + Sync)>,
    zones: Option<crate::GqlTimeZones>,
}

impl core::fmt::Debug for CatalogSymbolResolver<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CatalogSymbolResolver")
            .field("catalog", &"[REDACTED]")
            .field("restricted", &self.visible.is_some())
            .field("time_zones", &self.zones.is_some())
            .finish()
    }
}

impl<'a> CatalogSymbolResolver<'a> {
    #[must_use]
    pub const fn new(catalog: &'a SymbolCatalog) -> Self {
        Self {
            catalog,
            visible: None,
            zones: None,
        }
    }

    #[must_use]
    pub fn with_visibility(
        mut self,
        visible: &'a (dyn Fn(CatalogSymbol) -> bool + Send + Sync),
    ) -> Self {
        self.visible = Some(visible);
        self
    }

    #[must_use]
    pub fn with_time_zones(mut self, zones: crate::GqlTimeZones) -> Self {
        self.zones = Some(zones);
        self
    }

    fn admits(&self, symbol: CatalogSymbol) -> bool {
        self.visible.is_none_or(|visible| visible(symbol))
    }
}

impl crate::GraphSymbolResolver for CatalogSymbolResolver<'_> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<crate::GraphSymbol> {
        let kind = match kind {
            GraphSymbolKind::Label => CatalogSymbolKind::Label,
            GraphSymbolKind::Relation => CatalogSymbolKind::Relation,
            GraphSymbolKind::Property => CatalogSymbolKind::Property,
        };
        let symbol = self.catalog.resolve(kind, name)?;
        if !self.admits(symbol) {
            return None;
        }
        Some(match symbol {
            CatalogSymbol::Label(id) => crate::GraphSymbol::Label(id),
            CatalogSymbol::Relation(id) => crate::GraphSymbol::Relation(id),
            CatalogSymbol::Property(id) => crate::GraphSymbol::Property(id),
        })
    }

    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.zones.clone()
    }

    fn reverse_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
        let mut catalog = crate::ReverseSymbolCatalog::new();
        for (symbol, name) in self.catalog.bindings() {
            if self.admits(symbol) {
                match symbol {
                    CatalogSymbol::Label(id) => catalog.insert_label(id, name),
                    CatalogSymbol::Relation(id) => catalog.insert_relation(id, name),
                    CatalogSymbol::Property(id) => catalog.insert_property(id, name),
                }
            }
        }
        Some(catalog)
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        let symbol = CatalogSymbol::Label(id);
        self.admits(symbol)
            .then(|| self.catalog.name(symbol).map(str::to_owned))
            .flatten()
    }

    fn reverse_relation(&self, id: fgdb_delta_types::RelationId) -> Option<String> {
        let symbol = CatalogSymbol::Relation(id);
        self.admits(symbol)
            .then(|| self.catalog.name(symbol).map(str::to_owned))
            .flatten()
    }
}

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
    use crate::{GraphSymbol, GraphSymbolResolver, ReverseSymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};

    #[test]
    fn catalog_resolver_keeps_native_width_and_complete_unmentioned_names() {
        fn shared_host_state<T: Send + Sync>(_: &T) {}

        let ordinal = u64::from(u32::MAX) + 41;
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind_all([
                ("研究者", CatalogSymbol::Label(LabelId(ordinal))),
                ("MENTORS", CatalogSymbol::Relation(RelationId(ordinal))),
                (
                    "display name",
                    CatalogSymbol::Property(PropertyKeyId(ordinal)),
                ),
            ])
            .unwrap();
        let mut resolver = CatalogSymbolResolver::new(&catalog);
        shared_host_state(&catalog);
        shared_host_state(&resolver);
        for (kind, name, expected) in [
            (
                GraphSymbolKind::Label,
                "研究者",
                GraphSymbol::Label(LabelId(ordinal)),
            ),
            (
                GraphSymbolKind::Relation,
                "MENTORS",
                GraphSymbol::Relation(RelationId(ordinal)),
            ),
            (
                GraphSymbolKind::Property,
                "display name",
                GraphSymbol::Property(PropertyKeyId(ordinal)),
            ),
        ] {
            assert_eq!(resolver.resolve_symbol(kind, name), Some(expected));
        }
        assert_eq!(
            resolver.reverse_label(LabelId(ordinal)).as_deref(),
            Some("研究者")
        );
        assert_eq!(
            resolver.reverse_relation(RelationId(ordinal)).as_deref(),
            Some("MENTORS")
        );
        let mut expected = ReverseSymbolCatalog::new();
        expected.insert_label(LabelId(ordinal), "研究者");
        expected.insert_relation(RelationId(ordinal), "MENTORS");
        expected.insert_property(PropertyKeyId(ordinal), "display name");
        assert_eq!(
            ReverseSymbolCatalog::from_resolver(&mut resolver, "MATCH (n) RETURN n"),
            expected
        );
        assert!(resolver.time_zones().is_none());
    }

    #[test]
    fn hidden_catalog_entries_observe_like_absent_entries_through_every_resolver_seam() {
        let bindings = [
            ("Visible", CatalogSymbol::Label(LabelId(1))),
            ("Hidden", CatalogSymbol::Label(LabelId(2))),
            ("VISIBLE", CatalogSymbol::Relation(RelationId(1))),
            ("HIDDEN", CatalogSymbol::Relation(RelationId(2))),
            ("visible", CatalogSymbol::Property(PropertyKeyId(1))),
            ("hidden", CatalogSymbol::Property(PropertyKeyId(2))),
        ];
        let visible = |symbol: CatalogSymbol| symbol.ordinal() == 1;
        let mut full = SymbolCatalog::default();
        full.bind_all(bindings).unwrap();
        let mut absent = SymbolCatalog::default();
        absent
            .bind_all(bindings.into_iter().filter(|(_, symbol)| visible(*symbol)))
            .unwrap();
        let mut masked = CatalogSymbolResolver::new(&full).with_visibility(&visible);
        let mut without_hidden = CatalogSymbolResolver::new(&absent).with_visibility(&visible);
        for kind in [
            GraphSymbolKind::Label,
            GraphSymbolKind::Relation,
            GraphSymbolKind::Property,
        ] {
            for name in bindings.map(|(name, _)| name).into_iter().chain(["absent"]) {
                assert_eq!(
                    masked.resolve_symbol(kind, name),
                    without_hidden.resolve_symbol(kind, name)
                );
            }
        }
        for id in [0, 1, 2, u64::MAX] {
            assert_eq!(
                masked.reverse_label(LabelId(id)),
                without_hidden.reverse_label(LabelId(id))
            );
            assert_eq!(
                masked.reverse_relation(RelationId(id)),
                without_hidden.reverse_relation(RelationId(id))
            );
        }
        assert_eq!(masked.reverse_catalog(), without_hidden.reverse_catalog());
        let visible_catalog = masked.reverse_catalog().unwrap();
        assert_eq!(visible_catalog.labels.len(), 1);
        assert_eq!(visible_catalog.relations.len(), 1);
        assert_eq!(visible_catalog.properties.len(), 1);
        assert_eq!(
            visible_catalog
                .properties
                .get(&PropertyKeyId(1))
                .map(String::as_str),
            Some("visible")
        );
        assert!(!visible_catalog.properties.contains_key(&PropertyKeyId(2)));
        assert_eq!(format!("{masked:?}"), format!("{without_hidden:?}"));
    }

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
