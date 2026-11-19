//! Immutable name catalogs and the schema effects that publish them.
//!
//! Names reuse `SymbolCatalog`'s exact UTF-8 bijections. These objects acquire
//! authority only when a Chronicle Schema row names their transition; storing
//! an object alone never changes a database. A manifest retains the selected
//! catalog and historical views admit each binding strictly after its known
//! apply-basis sequence. No object contains its future assigned commit sequence.

use fgdb_delta_types::catalog::{CatalogBindingError, CatalogSymbol, CatalogSymbolKind, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{BranchId, CommitSeq, GraphId, LogicalObjectKind, LogicalObjectKindCode, MarkerRef, ObjectId};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use std::collections::BTreeMap;

pub const CATALOG_MAGIC: [u8; 4] = *b"FGSC";
pub const TRANSITION_MAGIC: [u8; 4] = *b"FGST";
pub const CATALOG_FORMAT_V1: u16 = 1;
pub const CATALOG_OBJECT_KIND: u16 = 0x058e;
pub const TRANSITION_OBJECT_KIND: u16 = 0x042d;
pub const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CATALOG_BINDINGS: usize = 65_536;
pub const MAX_CATALOG_NAME_BYTES: usize = 1024 * 1024;
pub const MAX_TRANSITION_BYTES: usize = 160;
const CATALOG_HEADER_BYTES: usize = 4 + 2 + 16 + 16 + 8 + 8 + 24 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct CatalogVersion(pub ObjectId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SchemaTransitionVersion(pub ObjectId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogRef {
    pub root: CatalogVersion,
    pub epoch: SchemaEpoch,
}

impl CatalogRef {
    pub fn validate(self) -> Result<(), CatalogError> {
        if self.root.0 == ObjectId([0; 32]) || self.epoch.0 == 0 {
            return Err(CatalogError::InvalidEpoch);
        }
        Ok(())
    }
}

/// A complete immutable name generation, including the allocation floors of
/// unnamed existing data. Introspection must still use the caller's policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaCatalog {
    graph: GraphId,
    branch: BranchId,
    epoch: SchemaEpoch,
    changed_after: CommitSeq,
    symbols: SymbolCatalog,
    introduced: BTreeMap<CatalogSymbol, CommitSeq>,
}

impl LogicalObjectKind for SchemaCatalog {
    const OBJECT_KIND: LogicalObjectKindCode = LogicalObjectKindCode::SchemaCatalog;
}

impl SchemaCatalog {
    #[must_use]
    pub fn empty(graph: GraphId, branch: BranchId) -> Self {
        Self {
            graph,
            branch,
            epoch: SchemaEpoch(0),
            changed_after: CommitSeq::ORIGIN,
            symbols: SymbolCatalog::default(),
            introduced: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn graph(&self) -> GraphId { self.graph }
    #[must_use]
    pub const fn branch(&self) -> BranchId { self.branch }
    #[must_use]
    pub const fn epoch(&self) -> SchemaEpoch { self.epoch }
    #[must_use]
    pub const fn changed_after(&self) -> CommitSeq { self.changed_after }
    #[must_use]
    pub fn symbols(&self) -> &SymbolCatalog { &self.symbols }
    #[must_use]
    pub fn introduced_after(&self, symbol: CatalogSymbol) -> Option<CommitSeq> {
        self.introduced.get(&symbol).copied()
    }

    /// Construct an off-side monotone successor. Identical declarations and
    /// allocator floors are a no-op and do not consume an epoch or sequence.
    /// `basis` is the already-published predecessor. The owner verifies it
    /// against the actual predecessor of the ordered Schema effect.
    pub fn successor(
        &self,
        basis: CommitSeq,
        proposed: &SymbolCatalog,
    ) -> Result<Option<Self>, CatalogError> {
        self.validate()?;
        validate_symbols(proposed)?;
        if proposed == &self.symbols {
            return Ok(None);
        }
        if self.epoch.0 > 0 && basis <= self.changed_after {
            return Err(CatalogError::InvalidSequence);
        }
        for (symbol, name) in self.symbols.bindings() {
            if proposed.name(symbol) != Some(name) {
                return Err(CatalogError::NotExtension);
            }
        }
        for kind in KINDS {
            if proposed.high_water_mark(kind) < self.symbols.high_water_mark(kind) {
                return Err(CatalogError::NotExtension);
            }
        }
        let epoch = SchemaEpoch(self.epoch.0.checked_add(1).ok_or(CatalogError::InvalidEpoch)?);
        let mut introduced = self.introduced.clone();
        for (symbol, _) in proposed.bindings() {
            introduced.entry(symbol).or_insert(basis);
        }
        let next = Self {
            graph: self.graph,
            branch: self.branch,
            epoch,
            changed_after: basis,
            symbols: proposed.clone(),
            introduced,
        };
        next.validate()?;
        Ok(Some(next))
    }

    /// Recovery checks the actual transition, not only each object's digest.
    /// Old names, ordinals, introduction sequences and floors cannot change.
    pub fn validate_successor(
        &self,
        next: &Self,
        basis: CommitSeq,
    ) -> Result<(), CatalogError> {
        if self.graph != next.graph || self.branch != next.branch {
            return Err(CatalogError::CoordinateMismatch);
        }
        let expected = self.successor(basis, &next.symbols)?
            .ok_or(CatalogError::NotExtension)?;
        if expected != *next {
            return Err(CatalogError::NotExtension);
        }
        Ok(())
    }

    /// A historical binding projection with conservative allocation floors.
    /// Later names stay hidden, while the current generation's floors prevent
    /// this owned projection from suggesting reuse of later or unnamed IDs.
    /// It does not claim to reconstruct the exact historical allocator state.
    pub fn symbols_at(&self, at: CommitSeq) -> Result<SymbolCatalog, CatalogError> {
        if at > self.changed_after {
            return Ok(self.symbols.clone());
        }
        let mut symbols = SymbolCatalog::default();
        for (symbol, name) in self.symbols.bindings() {
            if self.introduced[&symbol] < at {
                symbols.bind(name, symbol).map_err(CatalogError::Binding)?;
            }
        }
        for kind in KINDS {
            symbols.reserve_through(symbol_for_kind(kind, self.symbols.high_water_mark(kind)));
        }
        Ok(symbols)
    }

    fn validate(&self) -> Result<(), CatalogError> {
        validate_symbols(&self.symbols)?;
        if self.introduced.len() != self.symbols.len() {
            return Err(CatalogError::NonCanonical);
        }
        if self.epoch.0 == 0 {
            if self.changed_after != CommitSeq::ORIGIN
                || self.symbols != SymbolCatalog::default()
                || !self.introduced.is_empty()
            {
                return Err(CatalogError::InvalidEpoch);
            }
        } else {
            if self.epoch.0 > self.changed_after.0.saturating_add(1) {
                return Err(CatalogError::InvalidSequence);
            }
            if self.symbols == SymbolCatalog::default() {
                return Err(CatalogError::NonCanonical);
            }
        }
        for (symbol, _) in self.symbols.bindings() {
            let at = self.introduced.get(&symbol).ok_or(CatalogError::NonCanonical)?;
            if *at > self.changed_after {
                return Err(CatalogError::InvalidSequence);
            }
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CatalogError> {
        self.validate()?;
        let mut size = CATALOG_HEADER_BYTES;
        for (_, name) in self.symbols.bindings() {
            size = size.checked_add(1 + 8 + 8 + 4 + name.len()).ok_or(CatalogError::Bounds)?;
        }
        if size > MAX_CATALOG_BYTES { return Err(CatalogError::Bounds); }
        let mut out = Vec::new();
        out.try_reserve_exact(size).map_err(|_| CatalogError::Bounds)?;
        out.extend_from_slice(&CATALOG_MAGIC);
        out.extend_from_slice(&CATALOG_FORMAT_V1.to_be_bytes());
        out.extend_from_slice(&self.graph.0.to_be_bytes());
        out.extend_from_slice(&self.branch.0.to_be_bytes());
        out.extend_from_slice(&self.epoch.0.to_be_bytes());
        out.extend_from_slice(&self.changed_after.0.to_be_bytes());
        for kind in KINDS {
            out.extend_from_slice(&self.symbols.high_water_mark(kind).to_be_bytes());
        }
        out.extend_from_slice(&(self.symbols.len() as u32).to_be_bytes());
        for (symbol, name) in self.symbols.bindings() {
            out.push(symbol_tag(symbol));
            out.extend_from_slice(&symbol.ordinal().to_be_bytes());
            out.extend_from_slice(&self.introduced[&symbol].0.to_be_bytes());
            out.extend_from_slice(&(name.len() as u32).to_be_bytes());
            out.extend_from_slice(name.as_bytes());
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        if bytes.len() > MAX_CATALOG_BYTES { return Err(CatalogError::Bounds); }
        let mut r = Reader::new(bytes);
        r.header(CATALOG_MAGIC)?;
        let graph = GraphId(r.u128()?);
        let branch = BranchId(r.u128()?);
        let epoch = SchemaEpoch(r.u64()?);
        let changed_after = CommitSeq(r.u64()?);
        let floors = [r.u64()?, r.u64()?, r.u64()?];
        let count = r.u32()? as usize;
        if count > MAX_CATALOG_BINDINGS { return Err(CatalogError::Bounds); }
        let mut symbols = SymbolCatalog::default();
        let mut introduced = BTreeMap::new();
        let mut previous: Option<(CatalogSymbolKind, &str)> = None;
        for _ in 0..count {
            let tag = r.u8()?;
            let ordinal = r.u64()?;
            let symbol = symbol_from_tag(tag, ordinal)?;
            let at = CommitSeq(r.u64()?);
            let len = r.u32()? as usize;
            if len == 0 || len > MAX_CATALOG_NAME_BYTES { return Err(CatalogError::Bounds); }
            let name = core::str::from_utf8(r.take(len)?).map_err(|_| CatalogError::NonCanonical)?;
            if previous.is_some_and(|old| old >= (symbol.kind(), name)) {
                return Err(CatalogError::NonCanonical);
            }
            previous = Some((symbol.kind(), name));
            symbols.bind(name, symbol).map_err(CatalogError::Binding)?;
            introduced.insert(symbol, at);
        }
        r.finish()?;
        for (kind, floor) in KINDS.into_iter().zip(floors) {
            if floor < symbols.high_water_mark(kind) { return Err(CatalogError::NonCanonical); }
            symbols.reserve_through(symbol_for_kind(kind, floor));
        }
        let catalog = Self { graph, branch, epoch, changed_after, symbols, introduced };
        catalog.validate()?;
        Ok(catalog)
    }
}

const KINDS: [CatalogSymbolKind; 3] = [
    CatalogSymbolKind::Label, CatalogSymbolKind::Relation, CatalogSymbolKind::Property,
];

fn validate_symbols(symbols: &SymbolCatalog) -> Result<(), CatalogError> {
    if symbols.len() > MAX_CATALOG_BINDINGS { return Err(CatalogError::Bounds); }
    let mut size = CATALOG_HEADER_BYTES;
    for (_, name) in symbols.bindings() {
        if name.is_empty() || name.len() > MAX_CATALOG_NAME_BYTES {
            return Err(CatalogError::Bounds);
        }
        size = size.checked_add(21 + name.len()).ok_or(CatalogError::Bounds)?;
        if size > MAX_CATALOG_BYTES { return Err(CatalogError::Bounds); }
    }
    Ok(())
}

fn symbol_tag(symbol: CatalogSymbol) -> u8 {
    match symbol {
        CatalogSymbol::Label(_) => 1,
        CatalogSymbol::Relation(_) => 2,
        CatalogSymbol::Property(_) => 3,
    }
}

fn symbol_for_kind(kind: CatalogSymbolKind, ordinal: u64) -> CatalogSymbol {
    match kind {
        CatalogSymbolKind::Label => CatalogSymbol::Label(LabelId(ordinal)),
        CatalogSymbolKind::Relation => CatalogSymbol::Relation(RelationId(ordinal)),
        CatalogSymbolKind::Property => CatalogSymbol::Property(PropertyKeyId(ordinal)),
    }
}

fn symbol_from_tag(tag: u8, ordinal: u64) -> Result<CatalogSymbol, CatalogError> {
    match tag {
        1 => Ok(CatalogSymbol::Label(LabelId(ordinal))),
        2 => Ok(CatalogSymbol::Relation(RelationId(ordinal))),
        3 => Ok(CatalogSymbol::Property(PropertyKeyId(ordinal))),
        _ => Err(CatalogError::InvalidTag),
    }
}

/// The immutable effect named by one existing `DeltaRow::Schema`.
/// Neither this object nor its catalog names its future marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaTransition {
    pub graph: GraphId,
    pub branch: BranchId,
    pub apply_basis: Option<MarkerRef>,
    pub before: Option<CatalogRef>,
    pub after: CatalogRef,
}

impl LogicalObjectKind for SchemaTransition {
    const OBJECT_KIND: LogicalObjectKindCode = LogicalObjectKindCode::SchemaTransition;
}

impl SchemaTransition {
    pub fn validate(&self) -> Result<(), CatalogError> {
        self.after.validate()?;
        let before_epoch = match self.before {
            Some(before) => {
                before.validate()?;
                if before.root == self.after.root { return Err(CatalogError::NotExtension); }
                before.epoch.0
            }
            None => 0,
        };
        if before_epoch.checked_add(1) != Some(self.after.epoch.0) {
            return Err(CatalogError::InvalidEpoch);
        }
        if let Some(basis) = self.apply_basis {
            if basis.marker_oid == ObjectId([0; 32]) || basis.commit_seq == CommitSeq::ORIGIN {
                return Err(CatalogError::InvalidSequence);
            }
        } else if self.before.is_some() {
            return Err(CatalogError::InvalidSequence);
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CatalogError> {
        self.validate()?;
        let mut out = Vec::with_capacity(MAX_TRANSITION_BYTES);
        out.extend_from_slice(&TRANSITION_MAGIC);
        out.extend_from_slice(&CATALOG_FORMAT_V1.to_be_bytes());
        out.extend_from_slice(&self.graph.0.to_be_bytes());
        out.extend_from_slice(&self.branch.0.to_be_bytes());
        match self.apply_basis {
            Some(basis) => {
                out.push(1);
                out.extend_from_slice(&basis.marker_oid.0);
                out.extend_from_slice(&basis.commit_seq.0.to_be_bytes());
            }
            None => out.push(0),
        }
        match self.before {
            Some(before) => { out.push(1); write_ref(&mut out, before); }
            None => out.push(0),
        }
        write_ref(&mut out, self.after);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        if bytes.len() > MAX_TRANSITION_BYTES { return Err(CatalogError::Bounds); }
        let mut r = Reader::new(bytes);
        r.header(TRANSITION_MAGIC)?;
        let graph = GraphId(r.u128()?);
        let branch = BranchId(r.u128()?);
        let apply_basis = match r.u8()? {
            0 => None,
            1 => Some(MarkerRef { marker_oid: r.oid()?, commit_seq: CommitSeq(r.u64()?) }),
            _ => return Err(CatalogError::InvalidTag),
        };
        let before = match r.u8()? {
            0 => None,
            1 => Some(r.catalog_ref()?),
            _ => return Err(CatalogError::InvalidTag),
        };
        let after = r.catalog_ref()?;
        r.finish()?;
        let transition = Self { graph, branch, apply_basis, before, after };
        transition.validate()?;
        Ok(transition)
    }
}

fn write_ref(out: &mut Vec<u8>, reference: CatalogRef) {
    out.extend_from_slice(&reference.root.0.0);
    out.extend_from_slice(&reference.epoch.0.to_be_bytes());
}

pub fn catalog_id(key: &[u8; 32], namespace: DatabaseSecurityNamespaceId, bytes: &[u8]) -> ObjectId {
    object_id(key, namespace, CATALOG_OBJECT_KIND, bytes)
}

pub fn transition_id(key: &[u8; 32], namespace: DatabaseSecurityNamespaceId, bytes: &[u8]) -> ObjectId {
    object_id(key, namespace, TRANSITION_OBJECT_KIND, bytes)
}

fn object_id(key: &[u8; 32], namespace: DatabaseSecurityNamespaceId, kind: u16, bytes: &[u8]) -> ObjectId {
    ObjectId(fgdb_crypto::logical_object_id(key, &namespace.0, &kind.to_le_bytes(), bytes).0)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    NotAnObject,
    UnsupportedFormat { format: u16 },
    Truncated,
    TrailingBytes,
    Bounds,
    InvalidTag,
    NonCanonical,
    InvalidEpoch,
    InvalidSequence,
    CoordinateMismatch,
    NotExtension,
    Binding(CatalogBindingError),
    IdentityMismatch { expected: ObjectId, actual: ObjectId },
    InvalidTransition(&'static str),
}

impl core::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidTransition(reason) => write!(f, "invalid schema transition: {reason}"),
            Self::NotAnObject => f.write_str("not a schema catalog object"),
            Self::UnsupportedFormat { format } => write!(f, "unsupported schema catalog format {format}"),
            Self::Truncated => f.write_str("truncated schema catalog object"),
            Self::TrailingBytes => f.write_str("trailing schema catalog bytes"),
            Self::Bounds => f.write_str("schema catalog exceeds its format bounds"),
            Self::InvalidTag => f.write_str("invalid schema catalog tag"),
            Self::NonCanonical => f.write_str("noncanonical schema catalog"),
            Self::InvalidEpoch => f.write_str("invalid schema catalog epoch"),
            Self::InvalidSequence => f.write_str("invalid schema catalog sequence"),
            Self::CoordinateMismatch => f.write_str("schema catalog coordinate mismatch"),
            Self::NotExtension => f.write_str("schema catalog is not an immutable extension"),
            Self::Binding(error) => error.fmt(f),
            Self::IdentityMismatch { expected, actual } => write!(f, "schema object {actual:?} does not match {expected:?}"),
        }
    }
}
impl core::error::Error for CatalogError {}

struct Reader<'a> { bytes: &'a [u8], at: usize }
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, at: 0 } }
    fn take(&mut self, len: usize) -> Result<&'a [u8], CatalogError> {
        let end = self.at.checked_add(len).ok_or(CatalogError::Bounds)?;
        let bytes = self.bytes.get(self.at..end).ok_or(CatalogError::Truncated)?;
        self.at = end;
        Ok(bytes)
    }
    fn header(&mut self, magic: [u8; 4]) -> Result<(), CatalogError> {
        if self.take(4)? != magic { return Err(CatalogError::NotAnObject); }
        let format = u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| CatalogError::Truncated)?);
        if format != CATALOG_FORMAT_V1 { return Err(CatalogError::UnsupportedFormat { format }); }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8, CatalogError> { Ok(self.take(1)?[0]) }
    fn u32(&mut self) -> Result<u32, CatalogError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| CatalogError::Truncated)?))
    }
    fn u64(&mut self) -> Result<u64, CatalogError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| CatalogError::Truncated)?))
    }
    fn u128(&mut self) -> Result<u128, CatalogError> {
        Ok(u128::from_be_bytes(self.take(16)?.try_into().map_err(|_| CatalogError::Truncated)?))
    }
    fn oid(&mut self) -> Result<ObjectId, CatalogError> {
        Ok(ObjectId(self.take(32)?.try_into().map_err(|_| CatalogError::Truncated)?))
    }
    fn catalog_ref(&mut self) -> Result<CatalogRef, CatalogError> {
        Ok(CatalogRef { root: CatalogVersion(self.oid()?), epoch: SchemaEpoch(self.u64()?) })
    }
    fn finish(&self) -> Result<(), CatalogError> {
        if self.at == self.bytes.len() { Ok(()) } else { Err(CatalogError::TrailingBytes) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_roundtrip_keeps_historical_names_unicode_and_native_ordinals() {
        let empty = SchemaCatalog::empty(GraphId(1), BranchId(2));
        let mut symbols = SymbolCatalog::default();
        symbols.bind("研究者", CatalogSymbol::Label(LabelId(u64::MAX))).unwrap();
        symbols.bind("name", CatalogSymbol::Property(PropertyKeyId(2))).unwrap();
        symbols.reserve_through(CatalogSymbol::Relation(RelationId(91)));
        let first = empty.successor(CommitSeq(4), &symbols).unwrap().unwrap();
        let bytes = first.canonical_bytes().unwrap();
        assert_eq!(SchemaCatalog::decode(&bytes).unwrap(), first);
        assert!(first.symbols_at(CommitSeq(3)).unwrap().is_empty());
        symbols.bind("MENTORS", CatalogSymbol::Relation(RelationId(92))).unwrap();
        let second = first.successor(CommitSeq(8), &symbols).unwrap().unwrap();
        first.validate_successor(&second, CommitSeq(8)).unwrap();
        assert_eq!(second.symbols_at(CommitSeq(4)).unwrap().resolve(
            CatalogSymbolKind::Relation, "MENTORS"), None);
        assert_eq!(second.symbols_at(CommitSeq(9)).unwrap(), symbols);
        assert!(second.successor(CommitSeq(8), &symbols).unwrap().is_none());
        assert_eq!(SchemaCatalog::decode(&empty.canonical_bytes().unwrap()).unwrap(), empty);
        for len in 0..bytes.len() {
            assert!(SchemaCatalog::decode(&bytes[..len]).is_err(), "prefix {len}");
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(SchemaCatalog::decode(&trailing), Err(CatalogError::TrailingBytes));
    }

    #[test]
    fn genesis_binding_has_no_future_sequence_and_decode_admits_bounds_before_names() {
        let empty = SchemaCatalog::empty(GraphId(1), BranchId(1));
        let mut symbols = SymbolCatalog::default();
        symbols.bind("first", CatalogSymbol::Label(LabelId(1))).unwrap();
        let first = empty.successor(CommitSeq::ORIGIN, &symbols).unwrap().unwrap();
        assert_eq!(first.changed_after(), CommitSeq::ORIGIN);
        assert_eq!(first.introduced_after(CatalogSymbol::Label(LabelId(1))), Some(CommitSeq::ORIGIN));
        assert!(first.symbols_at(CommitSeq::ORIGIN).unwrap().is_empty());
        assert_eq!(first.symbols_at(CommitSeq(1)).unwrap(), symbols);
        let bytes = first.canonical_bytes().unwrap();
        let mut corrupt = bytes.clone();
        corrupt[78..82].copy_from_slice(&((MAX_CATALOG_BINDINGS + 1) as u32).to_be_bytes());
        assert_eq!(SchemaCatalog::decode(&corrupt), Err(CatalogError::Bounds));
        corrupt = bytes.clone();
        corrupt[82] = 4;
        assert_eq!(SchemaCatalog::decode(&corrupt), Err(CatalogError::InvalidTag));
        corrupt = bytes;
        corrupt[99..103].copy_from_slice(&((MAX_CATALOG_NAME_BYTES + 1) as u32).to_be_bytes());
        assert_eq!(SchemaCatalog::decode(&corrupt), Err(CatalogError::Bounds));
    }

    #[test]
    fn catalog_refuses_rebinding_removed_names_floors_and_forged_introductions() {
        let empty = SchemaCatalog::empty(GraphId(1), BranchId(1));
        let mut symbols = SymbolCatalog::default();
        symbols.bind("Person", CatalogSymbol::Label(LabelId(3))).unwrap();
        symbols.reserve_through(CatalogSymbol::Relation(RelationId(99)));
        let first = empty.successor(CommitSeq(2), &symbols).unwrap().unwrap();
        assert_eq!(first.successor(CommitSeq(3), &SymbolCatalog::default()), Err(CatalogError::NotExtension));
        let mut conflicting = SymbolCatalog::default();
        conflicting.bind("Other", CatalogSymbol::Label(LabelId(3))).unwrap();
        assert_eq!(first.successor(CommitSeq(3), &conflicting), Err(CatalogError::NotExtension));
        let mut lower_floor = SymbolCatalog::default();
        lower_floor.bind("Person", CatalogSymbol::Label(LabelId(3))).unwrap();
        assert_eq!(first.successor(CommitSeq(3), &lower_floor), Err(CatalogError::NotExtension));
        symbols.bind("later", CatalogSymbol::Property(PropertyKeyId(5))).unwrap();
        assert_eq!(first.successor(CommitSeq(2), &symbols), Err(CatalogError::InvalidSequence));
        assert_eq!(first.successor(CommitSeq(1), &symbols), Err(CatalogError::InvalidSequence));
        let second = first.successor(CommitSeq(3), &symbols).unwrap().unwrap();
        assert_eq!(first.validate_successor(&second, CommitSeq(4)), Err(CatalogError::NotExtension));
        let mut forged = second.clone();
        forged.introduced.insert(CatalogSymbol::Label(LabelId(3)), CommitSeq(3));
        assert_eq!(first.validate_successor(&forged, CommitSeq(3)), Err(CatalogError::NotExtension));
        let mut wrong_coordinate = second;
        wrong_coordinate.branch = BranchId(2);
        assert_eq!(first.validate_successor(&wrong_coordinate, CommitSeq(3)), Err(CatalogError::CoordinateMismatch));
    }

    #[test]
    fn historical_names_keep_safe_floors_including_floor_only_generations() {
        let empty = SchemaCatalog::empty(GraphId(7), BranchId(9));
        let mut symbols = SymbolCatalog::default();
        symbols.reserve_through(CatalogSymbol::Relation(RelationId(73)));
        let first = empty.successor(CommitSeq::ORIGIN, &symbols).unwrap().unwrap();
        assert_eq!(first.epoch(), SchemaEpoch(1));
        assert!(first.symbols().is_empty());
        let mut later = symbols.clone();
        later.bind("LATER", CatalogSymbol::Relation(RelationId(74))).unwrap();
        later.reserve_through(CatalogSymbol::Property(PropertyKeyId(u64::MAX)));
        let second = first.successor(CommitSeq(3), &later).unwrap().unwrap();
        first.validate_successor(&second, CommitSeq(3)).unwrap();
        assert_eq!(SchemaCatalog::decode(&second.canonical_bytes().unwrap()).unwrap(), second);
        for at in [CommitSeq::ORIGIN, CommitSeq(1), CommitSeq(3)] {
            let mut historical = second.symbols_at(at).unwrap();
            assert!(historical.is_empty());
            assert_eq!(historical.resolve(CatalogSymbolKind::Relation, "LATER"), None);
            assert_eq!(historical.high_water_mark(CatalogSymbolKind::Relation), 74);
            assert_eq!(historical.high_water_mark(CatalogSymbolKind::Property), u64::MAX);
            assert_eq!(
                historical.intern(CatalogSymbolKind::Relation, "SAFE").unwrap(),
                CatalogSymbol::Relation(RelationId(75)),
            );
            assert_eq!(
                historical.intern(CatalogSymbolKind::Property, "exhausted"),
                Err(CatalogBindingError::OrdinalsExhausted { kind: CatalogSymbolKind::Property }),
            );
        }
        assert_eq!(second.symbols_at(CommitSeq(4)).unwrap(), later);
    }

    #[test]
    fn transition_roundtrip_preserves_basis_and_refuses_epoch_or_sequence_forgery() {
        let transition = SchemaTransition {
            graph: GraphId(1), branch: BranchId(2),
            apply_basis: Some(MarkerRef { marker_oid: ObjectId([7; 32]), commit_seq: CommitSeq(9) }),
            before: Some(CatalogRef { root: CatalogVersion(ObjectId([1; 32])), epoch: SchemaEpoch(3) }),
            after: CatalogRef { root: CatalogVersion(ObjectId([2; 32])), epoch: SchemaEpoch(4) },
        };
        let bytes = transition.canonical_bytes().unwrap();
        assert_eq!(bytes.len(), MAX_TRANSITION_BYTES);
        assert_eq!(SchemaTransition::decode(&bytes).unwrap(), transition);
        for len in 0..bytes.len() { assert!(SchemaTransition::decode(&bytes[..len]).is_err()); }
        let mut wrong = transition.clone();
        wrong.after.epoch = SchemaEpoch(5);
        assert_eq!(wrong.canonical_bytes(), Err(CatalogError::InvalidEpoch));
        wrong = transition;
        wrong.apply_basis = None;
        assert_eq!(wrong.canonical_bytes(), Err(CatalogError::InvalidSequence));
        let catalog = SchemaCatalog::empty(GraphId(1), BranchId(1)).canonical_bytes().unwrap();
        let ns = DatabaseSecurityNamespaceId([5; 32]);
        assert_ne!(catalog_id(&[1; 32], ns, &catalog), transition_id(&[1; 32], ns, &catalog));
        assert_ne!(catalog_id(&[1; 32], ns, &catalog), catalog_id(&[2; 32], ns, &catalog));
    }
}
