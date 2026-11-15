//! Immutable name bindings published by the ordinary Chronicle transaction path.
//!
//! A catalog is graph state: its transition is a Schema delta in the same
//! capsule/marker stream as graph mutations. The manifest pins the committed
//! catalog generation, and recovery derives that generation from those effects.

use crate::*;
use std::collections::BTreeMap;
use fgdb_delta_types::catalog::{
    CatalogBindingError, CatalogSymbol, CatalogSymbolKind, SymbolCatalog,
};
use fgdb_strata::catalog::{
    CatalogError, CatalogRef, SchemaCatalog, SchemaTransition, SchemaTransitionVersion,
};

/// A catalog request failed before admission or during the ordinary durable
/// transaction/publication path. Write errors retain their committed/unknown
/// distinction; callers must not retry an unknown outcome as a fresh change.
#[derive(Debug)]
pub enum SymbolCatalogError {
    Read(ReadError),
    Write(Box<WriteError>),
    Binding(CatalogBindingError),
    Definition(CatalogError),
    Storage(Box<RebuildError>),
    Canonical(CanonicalError),
}

impl core::fmt::Display for SymbolCatalogError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
            Self::Binding(error) => error.fmt(f),
            Self::Definition(error) => error.fmt(f),
            Self::Storage(error) => error.fmt(f),
            Self::Canonical(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for SymbolCatalogError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Write(error) => Some(error.as_ref()),
            Self::Binding(error) => Some(error),
            Self::Definition(error) => Some(error),
            Self::Storage(error) => Some(error.as_ref()),
            Self::Canonical(error) => Some(error),
        }
    }
}

impl From<ReadError> for SymbolCatalogError {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<WriteError> for SymbolCatalogError {
    fn from(error: WriteError) -> Self {
        Self::Write(Box::new(error))
    }
}

impl From<StoreError> for SymbolCatalogError {
    fn from(error: StoreError) -> Self {
        Self::Storage(Box::new(RebuildError::Store(error)))
    }
}

impl From<CatalogError> for SymbolCatalogError {
    fn from(error: CatalogError) -> Self {
        Self::Definition(error)
    }
}

impl From<CatalogBindingError> for SymbolCatalogError {
    fn from(error: CatalogBindingError) -> Self {
        Self::Binding(error)
    }
}

impl From<CanonicalError> for SymbolCatalogError {
    fn from(error: CanonicalError) -> Self {
        Self::Canonical(error)
    }
}

/// Names and their root travel with the same immutable snapshot as the graph.
#[derive(Clone, Debug)]
pub(super) struct SchemaState {
    pub(super) reference: Option<CatalogRef>,
    pub(super) catalog: Arc<SchemaCatalog>,
    // Keep admitted generations flat: recursive predecessor chains can
    // overflow the stack when a long-lived handle releases its history.
    history: Arc<BTreeMap<CommitSeq, (CatalogRef, Arc<SchemaCatalog>)>>,
    origin_available: bool,
}

impl SchemaState {
    pub(super) fn empty() -> Self {
        Self {
            reference: None,
            catalog: Arc::new(SchemaCatalog::empty(GRAPH, BRANCH)),
            history: Arc::new(BTreeMap::new()),
            origin_available: true,
        }
    }

    /// Select an already-admitted generation without storage I/O. Historical
    /// preparation is synchronous and must never borrow a newer schema epoch.
    pub(super) fn at(&self, basis: CommitSeq) -> Result<Self, RebuildError> {
        if let Some((_, (reference, catalog))) = self.history.range(..basis).next_back() {
            return Ok(Self {
                reference: Some(*reference),
                catalog: Arc::clone(catalog),
                history: Arc::clone(&self.history),
                origin_available: self.origin_available,
            });
        }
        if self.origin_available {
            let mut empty = Self::empty();
            empty.history = Arc::clone(&self.history);
            return Ok(empty);
        }
        Err(invalid("historical catalog requires origin history materialization"))
    }

    pub(super) async fn load<V: Vfs>(
        cx: &CommitCx,
        store: &BlockStore<V>,
        reference: Option<CatalogRef>,
        published_at: CommitSeq,
    ) -> Result<Self, RebuildError> {
        cx.checkpoint().map_err(RebuildError::Interrupted)?;
        let Some(reference) = reference else {
            return Ok(Self::empty());
        };
        let catalog = store.get_catalog(cx, reference.root).await?;
        if (catalog.graph(), catalog.branch()) != (GRAPH, BRANCH)
            || catalog.epoch() != reference.epoch
            || catalog.changed_after().0 >= published_at.0
        {
            return Err(invalid("catalog does not belong to the selected publication"));
        }
        let catalog = Arc::new(catalog);
        let history = BTreeMap::from([(
            catalog.changed_after(),
            (reference, Arc::clone(&catalog)),
        )]);
        Ok(Self {
            reference: Some(reference),
            catalog,
            history: Arc::new(history),
            origin_available: false,
        })
    }

    /// Admit schema effects before D1, or replay the same admission from the
    /// committed capsule. The binding's predecessor is already identified;
    /// neither the template nor its objects names a future marker or sequence.
    pub(super) async fn apply_template<V: Vfs>(
        &self,
        cx: &CommitCx,
        store: &BlockStore<V>,
        template: &LogicalDeltaTemplate,
        predecessor: Option<MarkerRef>,
    ) -> Result<Self, RebuildError> {
        self.apply_entries(cx, store, template.coordinate_entries(), predecessor)
            .await
    }

    /// The same admission for an authenticated retained batch. This does not
    /// reconstruct a new template or give uncommitted rows ordering authority.
    pub(super) async fn apply_entries<V: Vfs>(
        &self,
        cx: &CommitCx,
        store: &BlockStore<V>,
        coordinates: &[CoordinateEntry],
        predecessor: Option<MarkerRef>,
    ) -> Result<Self, RebuildError> {
        let mut next = self.clone();
        let mut transitioned = false;
        for coordinate in coordinates {
            cx.checkpoint().map_err(RebuildError::Interrupted)?;
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            // This profile only adds immutable names/floors. A previously
            // prepared graph write remains compatible with an earlier epoch;
            // a future epoch or a transition over another basis cannot apply.
            if coordinate.schema_epoch.0 > self.catalog.epoch().0 {
                return Err(invalid("write requires a future catalog epoch"));
            }
            match coordinate.schema_transition {
                None => {
                    if coordinate.rows.iter().any(|row| matches!(row, DeltaRow::Schema { .. })) {
                        return Err(invalid("schema delta has no coordinate transition"));
                    }
                }
                Some(transition_oid) => {
                    if transitioned || coordinates.len() != 1 {
                        return Err(invalid("this catalog profile requires one schema coordinate"));
                    }
                    let [DeltaRow::Schema {
                        transition_oid: row_oid,
                        before_epoch,
                        after_epoch,
                    }] = coordinate.rows.as_slice()
                    else {
                        return Err(invalid("catalog transition must own its complete delta"));
                    };
                    if *row_oid != transition_oid
                        || *before_epoch != self.catalog.epoch()
                        || coordinate.schema_epoch != *before_epoch
                    {
                        return Err(invalid("catalog transition has a different before binding"));
                    }
                    let transition = store
                        .get_schema_transition(cx, SchemaTransitionVersion(transition_oid))
                        .await?;
                    if (transition.graph, transition.branch) != (GRAPH, BRANCH)
                        || transition.apply_basis != predecessor
                        || transition.before != self.reference
                        || transition.after.epoch != *after_epoch
                    {
                        return Err(invalid("catalog transition does not match the committed basis"));
                    }
                    let catalog = store.get_catalog(cx, transition.after.root).await?;
                    if catalog.epoch() != transition.after.epoch {
                        return Err(invalid("catalog object and transition epochs disagree"));
                    }
                    let basis = predecessor.map_or(CommitSeq::ORIGIN, |marker| marker.commit_seq);
                    self.catalog
                        .validate_successor(&catalog, basis)
                        .map_err(|error| RebuildError::Store(StoreError::MalformedCatalog(error)))?;
                    let catalog = Arc::new(catalog);
                    let mut history = Arc::clone(&self.history);
                    Arc::make_mut(&mut history).insert(
                        catalog.changed_after(),
                        (transition.after, Arc::clone(&catalog)),
                    );
                    next = Self {
                        reference: Some(transition.after),
                        catalog,
                        history,
                        origin_available: self.origin_available,
                    };
                    transitioned = true;
                }
            }
        }
        Ok(next)
    }
}

fn invalid(reason: &'static str) -> RebuildError {
    RebuildError::Store(StoreError::MalformedCatalog(CatalogError::InvalidTransition(reason)))
}

/// A marker already in the admitted, gap-free chain. Origin has no marker.
pub(super) fn marker_at(
    chain: &fgdb_chronicle::MarkerChain,
    at: CommitSeq,
) -> Option<MarkerRef> {
    let ordinal = usize::try_from(at.0.checked_sub(1)?).ok()?;
    let entry = chain.entries().get(ordinal)?;
    (entry.marker.commit_seq == at.0).then_some(MarkerRef {
        marker_oid: entry.marker_oid,
        commit_seq: at,
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Borrow the names pinned by this handle's current healthy generation.
    /// This privileged embedded API grants no new restricted-query authority.
    pub fn symbol_catalog(&self) -> Result<&SymbolCatalog, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.schema.catalog.symbols())
    }

    /// Select names from the same retained historical cut as a graph read.
    /// Later registrations cannot make names appear in an earlier cut.
    pub fn symbol_catalog_at(
        &self,
        at: CommitSeq,
    ) -> Result<SymbolCatalog, SymbolCatalogError> {
        self.ensure_readable()?;
        self.snapshot.check_frontier(at)?;
        Ok(self.snapshot.schema.catalog.symbols_at(at)?)
    }

    /// Register immutable names and allocator floors in one native transaction.
    ///
    /// Existing identical bindings are idempotent; conflicting names or IDs
    /// refuse before publication. Unmentioned bindings remain unchanged.
    /// A schema-only transaction advances Chronicle once without creating graph
    /// elements. This is an embedded/operator authority surface, not remote DDL.
    pub async fn register_symbols(
        &mut self,
        cx: &CommitCx,
        bindings: &SymbolCatalog,
    ) -> Result<CommitSeq, SymbolCatalogError> {
        self.register_symbols_with_faults(cx, bindings, None, None).await
    }

    /// The production catalog path with the same Chronicle/publication fault
    /// boundaries used by graph writes, for the integrated recovery matrix.
    #[doc(hidden)]
    pub async fn register_symbols_with_faults(
        &mut self,
        cx: &CommitCx,
        bindings: &SymbolCatalog,
        crash_at: Option<CrashPoint>,
        publication_failure: Option<DerivedPublicationStage>,
    ) -> Result<CommitSeq, SymbolCatalogError> {
        self.ensure_writable()?;
        cx.checkpoint()
            .map_err(RebuildError::Interrupted)
            .map_err(|error| SymbolCatalogError::Storage(Box::new(error)))?;
        let before = &self.snapshot.schema;
        let mut merged = before.catalog.symbols().clone();
        for (symbol, name) in bindings.bindings() {
            cx.checkpoint()
                .map_err(RebuildError::Interrupted)
                .map_err(|error| SymbolCatalogError::Storage(Box::new(error)))?;
            merged.bind(name, symbol)?;
        }
        for kind in [
            CatalogSymbolKind::Label,
            CatalogSymbolKind::Relation,
            CatalogSymbolKind::Property,
        ] {
            let ordinal = bindings.high_water_mark(kind);
            let symbol = match kind {
                CatalogSymbolKind::Label => CatalogSymbol::Label(LabelId(ordinal)),
                CatalogSymbolKind::Relation => CatalogSymbol::Relation(RelationId(ordinal)),
                CatalogSymbolKind::Property => CatalogSymbol::Property(PropertyKeyId(ordinal)),
            };
            merged.reserve_through(symbol);
        }
        let basis = self.snapshot.frontier;
        let Some(catalog) = before.catalog.successor(basis, &merged)? else {
            return Ok(basis);
        };
        let before_ref = before.reference;
        let before_epoch = before.catalog.epoch();
        let apply_basis = marker_at(self.coordinator.chain(), basis);
        if basis != CommitSeq::ORIGIN && apply_basis.is_none() {
            return Err(CatalogError::InvalidTransition("catalog basis is absent from Chronicle").into());
        }
        // These immutable objects are durable before the capsule can refer to
        // them. A failed/cancelled pre-D1 write leaves only unreferenced objects.
        let root = self.store.put_catalog(cx, &catalog).await?;
        let after = CatalogRef { root, epoch: catalog.epoch() };
        let transition = SchemaTransition {
            graph: GRAPH,
            branch: BRANCH,
            apply_basis,
            before: before_ref,
            after,
        };
        let transition = self.store.put_schema_transition(cx, &transition).await?;
        let template = LogicalDeltaTemplate::build(
            intent_semantics_oid(),
            [0u8; 32],
            vec![CoordinateEntry {
                graph: GRAPH,
                branch: BRANCH,
                relation: RelationId(0),
                schema_epoch: before_epoch,
                schema_transition: Some(transition.0),
                rows: vec![DeltaRow::Schema {
                    transition_oid: transition.0,
                    before_epoch,
                    after_epoch: after.epoch,
                }],
            }],
        )?;
        self.coordinator.set_validator(Box::new(
            FirstCommitterWinsValidator::default()
                .with_scalar_resolver(self.keys.scalar_resolver.clone()),
        ));
        Ok(self
            .commit_template(cx, template, crash_at, publication_failure, None)
            .await?)
    }
}

impl EmbeddedReadView {
    /// Borrow the catalog from this exact immutable graph generation.
    #[must_use]
    pub fn symbol_catalog(&self) -> &SymbolCatalog {
        self.snapshot.schema.catalog.symbols()
    }

    /// Resolve a historical catalog within this view's pinned frontier.
    pub fn symbol_catalog_at(
        &self,
        at: CommitSeq,
    ) -> Result<SymbolCatalog, SymbolCatalogError> {
        self.snapshot.check_frontier(at)?;
        Ok(self.snapshot.schema.catalog.symbols_at(at)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::AsyncWriteExt;
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, algebra::GraphValue};
    use fgdb_types::PurposeContexts;

    const LABEL: LabelId = LabelId(7);
    const PROPERTY: PropertyKeyId = PropertyKeyId(11);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x51; 32],
            DatabaseSecurityNamespaceId([0x52; 32]),
            [0x53; 32],
        )
    }

    fn bindings() -> SymbolCatalog {
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind_all([
                ("Person", CatalogSymbol::Label(LABEL)),
                ("KNOWS", CatalogSymbol::Relation(RelationId(3))),
                ("score", CatalogSymbol::Property(PROPERTY)),
            ])
            .unwrap();
        catalog
    }

    fn vertex(id: u128, score: i64) -> WriteBatch {
        let mut write = WriteBatch::new(RelationId(3));
        write.create_vertex(
            VId(id),
            vec![LABEL],
            vec![(PROPERTY, CanonicalScalar::Int(score))],
        );
        write
    }

    #[test]
    fn names_share_graph_publication_pins_history_and_reopen_without_external_bindings() {
        let ((), report) = run_async_under_lab(0x5ca1_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let origin = db.pinned_read_view().unwrap();
            let first = db.register_symbols(&cx, &bindings()).await.unwrap();
            assert_eq!(first, CommitSeq(1));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.heads.next_birth_ordinal, 0);
            assert_eq!(db.snapshot.schema.catalog.epoch(), SchemaEpoch(1));
            let manifest = db.manifest().unwrap();
            let slot = db.slot_generation;
            assert_eq!(db.register_symbols(&cx, &bindings()).await.unwrap(), first);
            assert_eq!(db.manifest().unwrap(), manifest);
            assert_eq!(db.slot_generation, slot);
            assert_eq!(db.coordinator.chain().entries().len(), 1);
            let pinned = db.pinned_read_view().unwrap();
            let graph = db.write(&cx, vertex(1, 42)).await.unwrap();
            assert_eq!(graph, CommitSeq(2));
            let mut later = SymbolCatalog::default();
            later.bind("Later", CatalogSymbol::Label(LabelId(8))).unwrap();
            let last = db.register_symbols(&cx, &later).await.unwrap();
            assert_eq!(last, CommitSeq(3));
            assert!(origin.symbol_catalog().is_empty());
            assert_eq!(pinned.symbol_catalog(), &bindings());
            assert_eq!(
                db.symbol_catalog_at(graph).unwrap().resolve(CatalogSymbolKind::Label, "Later"),
                None
            );
            assert_eq!(
                db.symbol_catalog_at(last).unwrap().resolve(CatalogSymbolKind::Label, "Later"),
                Some(CatalogSymbol::Label(LabelId(8)))
            );
            assert!(db.symbol_catalog_at(CommitSeq(4)).is_err());
            let mut conflict = SymbolCatalog::default();
            conflict.bind("Other", CatalogSymbol::Label(LABEL)).unwrap();
            assert!(matches!(
                db.register_symbols(&cx, &conflict).await,
                Err(SymbolCatalogError::Binding(_))
            ));
            assert_eq!(db.frontier().unwrap(), last);
            let query = || GqlQueryPolicy::new(100, 100, 100_000, 100_000);
            let expected = QueryResult::Rows {
                columns: vec!["value".to_owned()],
                rows: vec![vec![QueryValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(42),
                ))]],
            };
            assert_eq!(
                db.query(
                    &contexts.query(),
                    "MATCH (n:Person) RETURN n.score AS value",
                    &GqlParameters::new(),
                    CatalogSymbolResolver::new(db.symbol_catalog().unwrap()),
                    query(),
                )
                .unwrap(),
                expected
            );
            db.compact(&cx).await.unwrap();
            let names = db.symbol_catalog().unwrap().clone();
            let reader = Database::open_read_view_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(reader.symbol_catalog(), &names);
            assert_eq!(reader.frontier(), last);
            drop(db);
            let reopened = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.symbol_catalog().unwrap(), &names);
            assert_eq!(
                reopened.query(
                    &contexts.query(),
                    "MATCH (n:Person) RETURN n.score AS value",
                    &GqlParameters::new(),
                    CatalogSymbolResolver::new(reopened.symbol_catalog().unwrap()),
                    query(),
                ).unwrap(),
                expected
            );
            let manifest = reopened.manifest().unwrap();
            drop(reopened);
            let rebuilt = Database::bind_with_vfs(&cx, vfs, &path, keys(), true)
                .await
                .unwrap();
            assert_eq!(rebuilt.symbol_catalog().unwrap(), &names);
            assert_eq!(rebuilt.manifest().unwrap(), manifest);
            assert_eq!(rebuilt.frontier().unwrap(), last);
            assert_eq!(pinned.symbol_catalog(), &bindings());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_preparation_retains_floor_only_epochs_before_and_after_lazy_reopen() {
        let ((), report) = run_async_under_lab(0x5ca1_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.register_symbols(&cx, &bindings()).await.unwrap();
            let basis = db.write(&cx, vertex(1, 1)).await.unwrap();
            let expected = db.prepare_write(vertex(2, 2)).unwrap().template;
            let first_ref = db.snapshot.schema.reference;
            let mut floors = SymbolCatalog::default();
            floors.reserve_through(CatalogSymbol::Property(PropertyKeyId(1000)));
            db.register_symbols(&cx, &floors).await.unwrap();
            assert_eq!(db.snapshot.schema.catalog.epoch(), SchemaEpoch(2));
            assert_ne!(db.snapshot.schema.reference, first_ref);
            let current = Arc::clone(&db.snapshot);
            let prepared = db.prepare_write_at(basis, vertex(2, 2)).unwrap();
            assert_eq!(prepared.template, expected);
            assert!(Arc::ptr_eq(&db.snapshot, &current));
            assert_eq!(prepared.template.coordinate_entries()[0].schema_epoch, SchemaEpoch(1));
            drop(prepared);
            drop(current);
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert!(db.prepare_write_at(basis, vertex(2, 2)).is_err());
            db.ensure_delta_window(&cx, CommitSeq::ORIGIN).await.unwrap();
            assert_eq!(db.snapshot.schema.at(basis).unwrap().reference, first_ref);
            let prepared = db.prepare_write_at(basis, vertex(2, 2)).unwrap();
            assert_eq!(prepared.template, expected);
            let committed = db.commit_prepared(&cx, prepared).await.unwrap();
            assert_eq!(committed, CommitSeq(4));
            assert!(db.vertex(VId(2)).unwrap().is_some());
            assert_eq!(db.snapshot.schema.catalog.epoch(), SchemaEpoch(2));
            assert_eq!(
                db.symbol_catalog().unwrap().high_water_mark(CatalogSymbolKind::Property),
                1000
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn catalog_publication_failures_recover_the_committed_generation_once() {
        let ((), report) = run_async_under_lab(0x5ca1_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for stage in [
                DerivedPublicationStage::FoldCommittedTemplate,
                DerivedPublicationStage::SealPartition,
                DerivedPublicationStage::PublishEdgeBlocks,
                DerivedPublicationStage::PublishVertexPatches,
                DerivedPublicationStage::PublishPartitionRoot,
                DerivedPublicationStage::PublishManifest,
                DerivedPublicationStage::PublishRootSlot,
                DerivedPublicationStage::RefreshEdgeSnapshot,
                DerivedPublicationStage::RefreshVertexSnapshot,
            ] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                let pinned = db.pinned_read_view().unwrap();
                let error = db
                    .register_symbols_with_faults(&cx, &bindings(), None, Some(stage))
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    SymbolCatalogError::Write(source)
                        if matches!(*source, WriteError::CommittedNeedsRecovery { .. })
                ), "{stage:?}");
                assert!(db.symbol_catalog().is_err());
                assert!(pinned.symbol_catalog().is_empty());
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), CommitSeq(1), "{stage:?}");
                assert_eq!(reopened.symbol_catalog().unwrap(), &bindings(), "{stage:?}");
                assert_eq!(reopened.register_symbols(&cx, &bindings()).await.unwrap(), CommitSeq(1));
                let manifest = reopened.manifest().unwrap();
                drop(reopened);
                let rebuilt = Database::bind_with_vfs(&cx, vfs, &path, keys(), true)
                    .await
                    .unwrap();
                assert_eq!(rebuilt.symbol_catalog().unwrap(), &bindings(), "{stage:?}");
                assert_eq!(rebuilt.manifest().unwrap(), manifest, "{stage:?}");
                assert_eq!(rebuilt.coordinator.chain().entries().len(), 1);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn uncommitted_catalog_objects_never_acquire_names_or_advance_the_epoch() {
        let ((), report) = run_async_under_lab(0x5ca1_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for crash in [
                CrashPoint::BeforeCapsule,
                CrashPoint::AfterCapsuleBeforeD1,
                CrashPoint::AfterD1,
            ] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                assert!(db
                    .register_symbols_with_faults(&cx, &bindings(), Some(crash), None)
                    .await
                    .is_err());
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), CommitSeq::ORIGIN, "{crash:?}");
                assert!(reopened.symbol_catalog().unwrap().is_empty());
                assert_eq!(reopened.snapshot.schema.catalog.epoch(), SchemaEpoch(0));
                assert_eq!(reopened.register_symbols(&cx, &bindings()).await.unwrap(), CommitSeq(1));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn a_forged_catalog_apply_basis_refuses_before_the_capsule_and_keeps_graph_state() {
        let ((), report) = run_async_under_lab(0x5ca1_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            db.write(&cx, vertex(1, 1)).await.unwrap();
            let basis = db.frontier().unwrap();
            let catalog = db.snapshot.schema.catalog.successor(basis, &bindings()).unwrap().unwrap();
            let catalog_root = db.store.put_catalog(&cx, &catalog).await.unwrap();
            let transition = SchemaTransition {
                graph: GRAPH,
                branch: BRANCH,
                apply_basis: None,
                before: None,
                after: CatalogRef { root: catalog_root, epoch: SchemaEpoch(1) },
            };
            let transition = db.store.put_schema_transition(&cx, &transition).await.unwrap();
            let template = LogicalDeltaTemplate::build(
                intent_semantics_oid(),
                [0; 32],
                vec![CoordinateEntry {
                    graph: GRAPH,
                    branch: BRANCH,
                    relation: RelationId(0),
                    schema_epoch: SchemaEpoch(0),
                    schema_transition: Some(transition.0),
                    rows: vec![DeltaRow::Schema {
                        transition_oid: transition.0,
                        before_epoch: SchemaEpoch(0),
                        after_epoch: SchemaEpoch(1),
                    }],
                }],
            ).unwrap();
            assert!(matches!(
                db.commit_template(&cx, template, None, None, None).await,
                Err(WriteError::CatalogAdmission(_))
            ));
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.coordinator.chain().entries().len(), 1);
            assert!(db.symbol_catalog().unwrap().is_empty());
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert_eq!(db.register_symbols(&cx, &bindings()).await.unwrap(), CommitSeq(2));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn scrub_fences_missing_or_corrupt_current_and_historical_schema_objects() {
        let ((), report) = run_async_under_lab(0x5ca1_0006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for target in 0..3 {
                for missing in [false, true] {
                    let vfs = MemVfs::new().unwrap();
                    let path = vfs.database_dir();
                    let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    db.register_symbols(&cx, &bindings()).await.unwrap();
                    let first = db.snapshot.schema.reference.unwrap();
                    let transition = db.snapshot.delta_index.since(CommitSeq::ORIGIN)
                        .unwrap().next().unwrap().coordinate_entries()[0]
                        .schema_transition.unwrap();
                    db.write(&cx, vertex(1, 1)).await.unwrap();
                    let mut floors = SymbolCatalog::default();
                    floors.reserve_through(CatalogSymbol::Property(PropertyKeyId(1000)));
                    db.register_symbols(&cx, &floors).await.unwrap();
                    let latest = db.snapshot.schema.reference.unwrap();
                    let object = match target {
                        0 => latest.root.0,
                        1 => first.root.0,
                        _ => transition,
                    };
                    let clean = db.scrub(&cx).await.unwrap();
                    assert!(clean.metadata_lost.is_empty());
                    assert!(clean.metadata_clean.contains(&object));
                    let object_path = db.store.path(object);
                    if missing {
                        let unavailable = object_path.with_extension("unavailable");
                        cx.with_restriction_async(vfs.rename(&object_path, &unavailable))
                            .await
                            .unwrap();
                    } else {
                        cx.with_restriction_async(async {
                            let mut file = vfs.open(
                                &object_path,
                                &OpenOptions::new().write(true).truncate(true),
                            ).await?;
                            file.write_all(b"damaged schema object").await?;
                            file.sync_all().await
                        })
                        .await
                        .unwrap();
                    }
                    let summary = db.scrub(&cx).await.unwrap();
                    assert!(
                        summary.metadata_lost.iter().any(|lost| lost.object_id == object),
                        "target={target}, missing={missing}"
                    );
                    assert!(db.symbol_catalog().is_err());
                    assert!(db.vertices().is_err());
                    drop(db);
                    if target == 0 {
                        assert!(Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                            .await.is_err());
                    }
                    assert!(
                        Database::bind_with_vfs(&cx, vfs, &path, keys(), true).await.is_err(),
                        "full recovery must authenticate target={target}, missing={missing}"
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
