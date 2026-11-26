//! Immutable name bindings published by the ordinary Chronicle transaction path.
//!
//! A catalog is graph state: its transition is a Schema delta in the same
//! capsule/marker stream as graph mutations. The manifest pins the committed
//! catalog generation, and recovery derives that generation from those effects.

use crate::*;
use fgdb_delta_types::catalog::{
    CatalogBindingError, CatalogSymbol, CatalogSymbolKind, SymbolCatalog,
};
use fgdb_delta_types::zset::committed::CommittedSchemaCompatibility;
use fgdb_strata::catalog::{
    CatalogError, CatalogRef, SchemaCatalog, SchemaTransition, SchemaTransitionVersion,
};
use std::collections::{BTreeMap, BTreeSet};

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

/// An admitted generation may carry the exact immutable-name transition that
/// produced it. A checkpoint root alone proves the catalog, not that transition.
#[derive(Clone, Debug)]
struct AdmittedCatalogGeneration {
    reference: CatalogRef,
    catalog: Arc<SchemaCatalog>,
    transition: Option<SchemaTransitionVersion>,
}

/// Names and their root travel with the same immutable snapshot as the graph.
#[derive(Clone, Debug)]
pub(super) struct SchemaState {
    pub(super) reference: Option<CatalogRef>,
    pub(super) catalog: Arc<SchemaCatalog>,
    // Keep admitted generations flat: recursive predecessor chains can
    // overflow the stack when a long-lived handle releases its history.
    history: Arc<BTreeMap<CommitSeq, AdmittedCatalogGeneration>>,
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
        if let Some((_, generation)) = self.history.range(..basis).next_back() {
            return Ok(Self {
                reference: Some(generation.reference),
                catalog: Arc::clone(&generation.catalog),
                history: Arc::clone(&self.history),
                origin_available: self.origin_available,
            });
        }
        if self.origin_available {
            let mut empty = Self::empty();
            empty.history = Arc::clone(&self.history);
            return Ok(empty);
        }
        Err(invalid(
            "historical catalog requires origin history materialization",
        ))
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
            return Err(invalid(
                "catalog does not belong to the selected publication",
            ));
        }
        let catalog = Arc::new(catalog);
        let history = BTreeMap::from([(
            catalog.changed_after(),
            AdmittedCatalogGeneration {
                reference,
                catalog: Arc::clone(&catalog),
                transition: None,
            },
        )]);
        Ok(Self {
            reference: Some(reference),
            catalog,
            history: Arc::new(history),
            origin_available: false,
        })
    }

    /// Prove that this exact committed tick only extended immutable names or
    /// allocation floors. Beacon definitions select numeric IDs and therefore
    /// keep the same corpus across this change. This is not a compatibility
    /// judgment for metadata-returning queries or arbitrary schema effects.
    /// Callers supply an authenticated native delta batch; this compatibility
    /// check does not replace marker or payload authentication.
    ///
    /// Only apply_entries records a transition after validating both catalog
    /// generations and the ordered predecessor. Root-only opens deliberately
    /// lack that evidence; checkpoint catch-up retains its rebuild fallback.
    pub(super) fn admits_name_only_delta(&self, batch: &LogicalDeltaBatch) -> bool {
        let [coordinate] = batch.coordinate_entries() else {
            return false;
        };
        let [
            DeltaRow::Schema {
                transition_oid,
                before_epoch,
                after_epoch,
            },
        ] = coordinate.rows.as_slice()
        else {
            return false;
        };
        let sequence = batch.commit_seq();
        let Some(basis) = sequence.0.checked_sub(1).map(CommitSeq) else {
            return false;
        };
        let Some(generation) = self.history.get(&basis) else {
            return false;
        };
        batch.frontier() == sequence
            && batch.commit_marker_identity().commit_seq == sequence
            && (coordinate.graph, coordinate.branch) == (GRAPH, BRANCH)
            && coordinate.schema_transition == Some(*transition_oid)
            && coordinate.schema_epoch == *before_epoch
            && before_epoch.0.checked_add(1) == Some(after_epoch.0)
            && generation.transition == Some(SchemaTransitionVersion(*transition_oid))
            && generation.reference.epoch == *after_epoch
            && generation.catalog.epoch() == *after_epoch
            && after_epoch.0 <= self.catalog.epoch().0
            && generation.catalog.changed_after() == basis
    }

    /// Attest a live committed topology input after publication has verified
    /// its batch and installed the resulting catalog. A root loaded without
    /// its catalog history cannot certify the full lineage.
    pub(super) fn topology_compatibility<'a>(
        &self,
        cx: &CommitCx,
        batch: &'a LogicalDeltaBatch,
    ) -> Option<CommittedSchemaCompatibility<'a>> {
        if !self.origin_available {
            return None;
        }
        let has_catalog_effect = batch
            .coordinate_entries()
            .iter()
            .any(|entry| entry.schema_transition.is_some());
        if has_catalog_effect && !self.admits_name_only_delta(batch) {
            return None;
        }
        Some(CommittedSchemaCompatibility::attest_name_only_catalog(
            cx,
            batch,
            GRAPH,
            BRANCH,
            self.catalog.epoch(),
        ))
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
                    if coordinate
                        .rows
                        .iter()
                        .any(|row| matches!(row, DeltaRow::Schema { .. }))
                    {
                        return Err(invalid("schema delta has no coordinate transition"));
                    }
                }
                Some(transition_oid) => {
                    if transitioned || coordinates.len() != 1 {
                        return Err(invalid(
                            "this catalog profile requires one schema coordinate",
                        ));
                    }
                    let [
                        DeltaRow::Schema {
                            transition_oid: row_oid,
                            before_epoch,
                            after_epoch,
                        },
                    ] = coordinate.rows.as_slice()
                    else {
                        return Err(invalid("catalog transition must own its complete delta"));
                    };
                    if *row_oid != transition_oid
                        || *before_epoch != self.catalog.epoch()
                        || coordinate.schema_epoch != *before_epoch
                    {
                        return Err(invalid("catalog transition has a different before binding"));
                    }
                    // Prove Send for cold object reads here. A downstream
                    // authorized transaction can carry a local-only clock;
                    // its callers need not traverse the storage futures.
                    let read_transition: crate::SendFuture<'_, _> = Box::pin(
                        store.get_schema_transition(cx, SchemaTransitionVersion(transition_oid)),
                    );
                    let transition = read_transition.await?;
                    if (transition.graph, transition.branch) != (GRAPH, BRANCH)
                        || transition.apply_basis != predecessor
                        || transition.before != self.reference
                        || transition.after.epoch != *after_epoch
                    {
                        return Err(invalid(
                            "catalog transition does not match the committed basis",
                        ));
                    }
                    let read_catalog: crate::SendFuture<'_, _> =
                        Box::pin(store.get_catalog(cx, transition.after.root));
                    let catalog = read_catalog.await?;
                    if catalog.epoch() != transition.after.epoch {
                        return Err(invalid("catalog object and transition epochs disagree"));
                    }
                    let basis = predecessor.map_or(CommitSeq::ORIGIN, |marker| marker.commit_seq);
                    self.catalog
                        .validate_successor(&catalog, basis)
                        .map_err(|error| {
                            RebuildError::Store(StoreError::MalformedCatalog(error))
                        })?;
                    let catalog = Arc::new(catalog);
                    let mut history = Arc::clone(&self.history);
                    Arc::make_mut(&mut history).insert(
                        catalog.changed_after(),
                        AdmittedCatalogGeneration {
                            reference: transition.after,
                            catalog: Arc::clone(&catalog),
                            transition: Some(SchemaTransitionVersion(transition_oid)),
                        },
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

    /// Authenticate an origin replay with the same schema-transition rules as
    /// history admission, retaining only the current private catalog. The
    /// caller starts from `empty()` and checks the final reference against its
    /// admitted snapshot; this state never backs historical public reads.
    pub(super) async fn audit_entries<V: Vfs>(
        &mut self,
        cx: &CommitCx,
        store: &BlockStore<V>,
        coordinates: &[CoordinateEntry],
        predecessor: Option<MarkerRef>,
    ) -> Result<(), RebuildError> {
        let mut next = self
            .apply_entries(cx, store, coordinates, predecessor)
            .await?;
        if !next.history.is_empty() {
            Arc::make_mut(&mut next.history).clear();
        }
        *self = next;
        Ok(())
    }
}

fn invalid(reason: &'static str) -> RebuildError {
    RebuildError::Store(StoreError::MalformedCatalog(
        CatalogError::InvalidTransition(reason),
    ))
}

/// A marker already in the admitted, gap-free chain. Origin has no marker.
pub(super) fn marker_at(chain: &fgdb_chronicle::MarkerChain, at: CommitSeq) -> Option<MarkerRef> {
    let ordinal = usize::try_from(at.0.checked_sub(1)?).ok()?;
    let entry = chain.entries().get(ordinal)?;
    (entry.marker.commit_seq == at.0).then_some(MarkerRef {
        marker_oid: entry.marker_oid,
        commit_seq: at,
    })
}

/// Why a current-only catalog open could not admit a published generation.
#[derive(Debug)]
pub enum SymbolCatalogOpenError {
    /// The ordinary database identity, storage, or catalog admission failed.
    Open(Box<OpenError>),
    /// No checkpoint is current with the observed committed chain. A writer
    /// must publish or recover it before a current-only reader can proceed.
    RecoveryRequired,
}

impl core::fmt::Display for SymbolCatalogOpenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Open(error) => error.fmt(f),
            Self::RecoveryRequired => f.write_str(
                "symbol metadata requires a current published checkpoint; database recovery is required",
            ),
        }
    }
}

impl core::error::Error for SymbolCatalogOpenError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error.as_ref()),
            Self::RecoveryRequired => None,
        }
    }
}

impl From<OpenError> for SymbolCatalogOpenError {
    fn from(error: OpenError) -> Self {
        Self::Open(Box::new(error))
    }
}

/// Immutable catalog metadata from one admitted database generation.
///
/// Trusted embedded hosts can prepare name resolvers without materializing
/// graph payloads. The view pins its selected frontier and catalog, and owns
/// no writer lease.
#[derive(Clone)]
pub struct SymbolCatalogReadView {
    catalog: Arc<SchemaCatalog>,
    frontier: CommitSeq,
}

impl core::fmt::Debug for SymbolCatalogReadView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SymbolCatalogReadView")
            .field("frontier", &self.frontier)
            .field("epoch", &self.catalog.epoch())
            .finish_non_exhaustive()
    }
}

impl SymbolCatalogReadView {
    pub(super) async fn from_published<V: Vfs>(
        cx: &CommitCx,
        published: &PublishedRead<V>,
    ) -> Result<Self, OpenError> {
        let schema = SchemaState::load(
            cx,
            &published.store,
            published.schema,
            published.published_at,
        )
        .await?;
        Ok(Self {
            catalog: schema.catalog,
            frontier: published.published_at,
        })
    }

    /// The exact published frontier selected when this view was opened.
    #[must_use]
    pub fn frontier(&self) -> CommitSeq {
        self.frontier
    }

    /// Borrow the immutable names selected with this view's frontier.
    #[must_use]
    pub fn symbol_catalog(&self) -> &SymbolCatalog {
        self.catalog.symbols()
    }

    /// Resolve names at a historical cut within this view's pinned frontier.
    pub fn symbol_catalog_at(&self, at: CommitSeq) -> Result<SymbolCatalog, SymbolCatalogError> {
        if at.0 > self.frontier.0 {
            return Err(ReadError::BeyondFrontier {
                asked: at,
                frontier: self.frontier,
            }
            .into());
        }
        Ok(self.catalog.symbols_at(at)?)
    }
}

impl Database<UnixVfs> {
    /// Open catalog metadata only when its publication is current.
    ///
    /// This path takes no writer lease, performs no recovery, and never
    /// materializes graph payloads. Bounded and sealed query hosts can use it
    /// before their own graph admission without acquiring a resident fallback.
    pub async fn open_current_symbol_catalog(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<SymbolCatalogReadView, SymbolCatalogOpenError> {
        Self::open_current_symbol_catalog_with_vfs(cx, UnixVfs::new(), path, keys).await
    }

    /// Open immutable symbol metadata for a trusted embedded host.
    ///
    /// Uses the same committed-chain and manifest selection as
    /// [`Self::open_read_view`]. A current publication reads only its metadata
    /// and catalog, without materializing graph blocks or vertex patches.
    /// An interrupted publication follows the existing read-view recovery
    /// path, including its last-published-generation behavior under a live
    /// writer's lease.
    pub async fn open_symbol_catalog(
        cx: &CommitCx,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<SymbolCatalogReadView, OpenError> {
        Self::open_symbol_catalog_with_vfs(cx, UnixVfs::new(), path, keys).await
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// [`Database::open_current_symbol_catalog`] through an explicit filesystem.
    #[doc(hidden)]
    pub async fn open_current_symbol_catalog_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<SymbolCatalogReadView, SymbolCatalogOpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        match Self::read_current_generation(cx, &vfs, path, &keys, None).await? {
            ReadGeneration::Current(published) => {
                SymbolCatalogReadView::from_published(cx, &published)
                    .await
                    .map_err(Into::into)
            }
            ReadGeneration::Lagging(_) | ReadGeneration::Unselected => {
                Err(SymbolCatalogOpenError::RecoveryRequired)
            }
        }
    }

    /// [`Database::open_symbol_catalog`] through an explicit filesystem.
    #[doc(hidden)]
    pub async fn open_symbol_catalog_with_vfs(
        cx: &CommitCx,
        vfs: V,
        path: impl AsRef<Path>,
        keys: DatabaseKeys,
    ) -> Result<SymbolCatalogReadView, OpenError> {
        let path = path.as_ref();
        require_database_dir(cx, &vfs, path).await?;
        match Self::read_current_generation(cx, &vfs, path, &keys, None).await? {
            ReadGeneration::Current(published) => {
                SymbolCatalogReadView::from_published(cx, &published).await
            }
            ReadGeneration::Lagging(_) | ReadGeneration::Unselected => {
                // Healing and live-writer arbitration already belong to this
                // opener. Retain only its admitted metadata after it returns.
                let view = Self::open_read_view_with_vfs(cx, vfs, path, keys).await?;
                Ok(SymbolCatalogReadView {
                    catalog: Arc::clone(&view.snapshot.schema.catalog),
                    frontier: view.frontier(),
                })
            }
        }
    }

    /// Borrow the names pinned by this handle's current healthy generation.
    /// This privileged embedded API grants no new restricted-query authority.
    pub fn symbol_catalog(&self) -> Result<&SymbolCatalog, ReadError> {
        self.ensure_readable()?;
        Ok(self.snapshot.schema.catalog.symbols())
    }

    /// Select names from the same retained historical cut as a graph read.
    /// Later registrations cannot make names appear in an earlier cut.
    pub fn symbol_catalog_at(&self, at: CommitSeq) -> Result<SymbolCatalog, SymbolCatalogError> {
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
        self.register_symbols_with_faults(cx, bindings, None, None)
            .await
    }

    /// Register names without requiring callers to choose their numeric IDs.
    ///
    /// Explicit bindings are admitted first. New names then receive the next
    /// unspent ordinal in canonical namespace/name order, independent of request
    /// order or duplicates. Existing names keep their IDs. The complete request
    /// publishes as one schema transaction, and no allocated ID escapes before
    /// that transaction succeeds. Read [`Self::symbol_catalog`] after success to
    /// obtain the committed bindings.
    ///
    /// Before allocating, this method authenticates the complete retained
    /// Chronicle history and reserves every numeric symbol it has used,
    /// including labels and properties later removed and deleted elements.
    /// This also covers numeric graph writes made since a previous registration.
    /// Batches are consumed one at a time; omitted history is not installed in
    /// the database's retained delta window. Allocation therefore costs a
    /// retained-history traversal, not a scan of only the live graph.
    /// Unavailable or explicitly retired origin history refuses allocation;
    /// it must never be treated as an empty namespace.
    /// Requests that introduce no automatic names need no history traversal.
    ///
    /// Like [`Self::register_symbols`], this is a privileged embedded/operator
    /// surface and grants no remote schema authority.
    pub async fn register_symbol_names(
        &mut self,
        cx: &CommitCx,
        explicit: &SymbolCatalog,
        names: &[(CatalogSymbolKind, &str)],
    ) -> Result<CommitSeq, SymbolCatalogError> {
        self.ensure_writable()?;
        let checkpoint = || {
            cx.checkpoint().map_err(|error| {
                SymbolCatalogError::Storage(Box::new(RebuildError::Interrupted(error)))
            })
        };
        checkpoint()?;
        let mut merged = self.snapshot.schema.catalog.symbols().clone();
        merge_symbol_bindings(&mut merged, explicit, checkpoint)?;
        let mut requested = BTreeSet::new();
        let mut needs_allocation = false;
        for &(kind, name) in names {
            checkpoint()?;
            if name.is_empty() {
                return Err(CatalogBindingError::EmptyName.into());
            }
            needs_allocation |= merged.resolve(kind, name).is_none();
            requested.insert((kind, name));
        }
        if needs_allocation {
            self.visit_delta_history(cx, CommitSeq::ORIGIN, |_, batch| {
                checkpoint()?;
                for coordinate in batch.coordinate_entries() {
                    checkpoint()?;
                    if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                        continue;
                    }
                    merged.reserve_through(CatalogSymbol::Relation(coordinate.relation));
                    for row in &coordinate.rows {
                        reserve_delta_symbols(&mut merged, row, checkpoint)?;
                    }
                }
                Ok::<(), SymbolCatalogError>(())
            })
            .await
            .map_err(|error| match error {
                crate::delta_replay::DeltaHistoryError::Observer(error) => error,
                crate::delta_replay::DeltaHistoryError::Source(error) => {
                    match WriteError::from(error) {
                        WriteError::PreparedHistory(error) => {
                            SymbolCatalogError::Storage(Box::new(RebuildError::Index {
                                commit_seq: CommitSeq::ORIGIN.0,
                                error,
                            }))
                        }
                        WriteError::HistoryRebuild(error) => SymbolCatalogError::Storage(error),
                        error => SymbolCatalogError::Write(Box::new(error)),
                    }
                }
            })?;
            for (kind, name) in requested {
                checkpoint()?;
                merged.intern(kind, name)?;
            }
        }
        checkpoint()?;
        self.register_symbols(cx, &merged).await
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
        merge_symbol_bindings(&mut merged, bindings, || {
            cx.checkpoint()
                .map_err(RebuildError::Interrupted)
                .map_err(|error| SymbolCatalogError::Storage(Box::new(error)))
        })?;
        let basis = self.snapshot.frontier;
        let Some(catalog) = before.catalog.successor(basis, &merged)? else {
            return Ok(basis);
        };
        let before_ref = before.reference;
        let before_epoch = before.catalog.epoch();
        let apply_basis = marker_at(self.coordinator.chain(), basis);
        if basis != CommitSeq::ORIGIN && apply_basis.is_none() {
            return Err(
                CatalogError::InvalidTransition("catalog basis is absent from Chronicle").into(),
            );
        }
        // These immutable objects are durable before the capsule can refer to
        // them. A failed/cancelled pre-D1 write leaves only unreferenced objects.
        let root = self.store.put_catalog(cx, &catalog).await?;
        let after = CatalogRef {
            root,
            epoch: catalog.epoch(),
        };
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

fn merge_symbol_bindings(
    merged: &mut SymbolCatalog,
    bindings: &SymbolCatalog,
    mut checkpoint: impl FnMut() -> Result<(), SymbolCatalogError>,
) -> Result<(), SymbolCatalogError> {
    for (symbol, name) in bindings.bindings() {
        checkpoint()?;
        merged.bind(name, symbol)?;
    }
    for kind in [
        CatalogSymbolKind::Label,
        CatalogSymbolKind::Relation,
        CatalogSymbolKind::Property,
    ] {
        checkpoint()?;
        let ordinal = bindings.high_water_mark(kind);
        let symbol = match kind {
            CatalogSymbolKind::Label => CatalogSymbol::Label(LabelId(ordinal)),
            CatalogSymbolKind::Relation => CatalogSymbol::Relation(RelationId(ordinal)),
            CatalogSymbolKind::Property => CatalogSymbol::Property(PropertyKeyId(ordinal)),
        };
        merged.reserve_through(symbol);
    }
    Ok(())
}

/// Reserve from committed effects, including negative effects. Looking only at
/// surviving elements would reissue the ordinals of removed labels/properties
/// or retired elements. Poll inside variable-width rows as well as between rows.
fn reserve_delta_symbols<E>(
    symbols: &mut SymbolCatalog,
    row: &DeltaRow,
    mut checkpoint: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    checkpoint()?;
    match row {
        DeltaRow::CreateVertex { labels, props, .. } => {
            for &label in labels {
                checkpoint()?;
                symbols.reserve_through(CatalogSymbol::Label(label));
            }
            for &(property, _) in props {
                checkpoint()?;
                symbols.reserve_through(CatalogSymbol::Property(property));
            }
        }
        DeltaRow::CreateEdge {
            relation, props, ..
        } => {
            symbols.reserve_through(CatalogSymbol::Relation(*relation));
            for &(property, _) in props {
                checkpoint()?;
                symbols.reserve_through(CatalogSymbol::Property(property));
            }
        }
        DeltaRow::LabelMembership { label, .. } => {
            symbols.reserve_through(CatalogSymbol::Label(*label));
        }
        DeltaRow::Property { property, .. } | DeltaRow::Counter { property, .. } => {
            symbols.reserve_through(CatalogSymbol::Property(*property));
        }
        DeltaRow::Escrow {
            subject_property, ..
        } => {
            if let Some(property) = subject_property {
                symbols.reserve_through(CatalogSymbol::Property(*property));
            }
        }
        DeltaRow::DeleteVertex { .. }
        | DeltaRow::DeleteEdge { .. }
        | DeltaRow::ValidTime { .. }
        | DeltaRow::Sketch { .. }
        | DeltaRow::Schema { .. }
        | DeltaRow::Constraint { .. } => {}
    }
    Ok(())
}

impl EmbeddedReadView {
    /// Borrow the catalog from this exact immutable graph generation.
    #[must_use]
    pub fn symbol_catalog(&self) -> &SymbolCatalog {
        self.snapshot.schema.catalog.symbols()
    }

    /// Resolve a historical catalog within this view's pinned frontier.
    pub fn symbol_catalog_at(&self, at: CommitSeq) -> Result<SymbolCatalog, SymbolCatalogError> {
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
    fn name_only_delta_admission_requires_the_exact_validated_transition() {
        let ((), report) = run_async_under_lab(0x5ca1_0021, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let first = db.register_symbols(&cx, &bindings()).await.unwrap();
            let first_batch = db.snapshot.delta_index.get(first).unwrap().clone();
            let mut later = SymbolCatalog::default();
            later.reserve_through(CatalogSymbol::Property(PropertyKeyId(99)));
            let last = db.register_symbols(&cx, &later).await.unwrap();
            let last_batch = db.snapshot.delta_index.get(last).unwrap().clone();
            assert!(db.snapshot.schema.admits_name_only_delta(&first_batch));
            assert!(db.snapshot.schema.admits_name_only_delta(&last_batch));
            let historical = db.snapshot.schema.at(first).unwrap();
            assert!(historical.admits_name_only_delta(&first_batch));
            assert!(!historical.admits_name_only_delta(&last_batch));

            for corruption in 0..7 {
                let mut coordinates = first_batch.coordinate_entries().to_vec();
                let coordinate = &mut coordinates[0];
                match corruption {
                    0 => coordinate.schema_transition = None,
                    1 => {
                        let oid = fgdb_types::ids::ObjectId([0xa7; 32]);
                        coordinate.schema_transition = Some(oid);
                        let DeltaRow::Schema { transition_oid, .. } = &mut coordinate.rows[0]
                        else {
                            unreachable!("the committed catalog tick has one Schema row");
                        };
                        *transition_oid = oid;
                    }
                    2 => coordinate.schema_epoch.0 += 1,
                    3 => {
                        let DeltaRow::Schema { after_epoch, .. } = &mut coordinate.rows[0] else {
                            unreachable!("the committed catalog tick has one Schema row");
                        };
                        after_epoch.0 += 1;
                    }
                    4 => coordinate.rows.push(coordinate.rows[0].clone()),
                    5 => coordinate.graph = GraphId(GRAPH.0 + 1),
                    _ => coordinates.push(coordinates[0].clone()),
                }
                let forged = LogicalDeltaBatch::from_parts_for_test(
                    coordinates,
                    *first_batch.source_template_digest(),
                    first_batch.commit_marker_identity(),
                    first,
                    first,
                );
                assert!(
                    !db.snapshot.schema.admits_name_only_delta(&forged),
                    "corruption {corruption} must not acquire a catalog proof"
                );
            }

            let wrong_cut = LogicalDeltaBatch::from_parts_for_test(
                first_batch.coordinate_entries().to_vec(),
                *first_batch.source_template_digest(),
                first_batch.commit_marker_identity(),
                last,
                last,
            );
            assert!(!db.snapshot.schema.admits_name_only_delta(&wrong_cut));

            // Loading an authenticated root does not prove which transition
            // produced it. A checkpoint consumer must rebuild or authenticate
            // the missing history, rather than inventing schema compatibility.
            let loaded = SchemaState::load(&cx, &db.store, db.snapshot.schema.reference, last)
                .await
                .unwrap();
            assert!(!loaded.admits_name_only_delta(&first_batch));
            assert!(!loaded.admits_name_only_delta(&last_batch));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn automatic_names_publish_once_in_canonical_order_and_drive_native_queries() {
        let ((), report) = run_async_under_lab(0x5ca1_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let high = u64::from(u32::MAX) + 20;
            let mut explicit = SymbolCatalog::default();
            explicit
                .bind("score", CatalogSymbol::Property(PropertyKeyId(high)))
                .unwrap();
            explicit.reserve_through(CatalogSymbol::Label(LabelId(high)));
            let names = [
                (CatalogSymbolKind::Property, "Ω"),
                (CatalogSymbolKind::Label, "Ω"),
                (CatalogSymbolKind::Label, "Alpha"),
                (CatalogSymbolKind::Relation, "KNOWS"),
                (CatalogSymbolKind::Property, "alpha"),
                (CatalogSymbolKind::Property, "score"),
                (CatalogSymbolKind::Label, "Alpha"),
            ];
            let mut expected = explicit.clone();
            expected
                .bind_all([
                    ("Alpha", CatalogSymbol::Label(LabelId(high + 1))),
                    ("Ω", CatalogSymbol::Label(LabelId(high + 2))),
                    ("KNOWS", CatalogSymbol::Relation(RelationId(1))),
                    ("alpha", CatalogSymbol::Property(PropertyKeyId(high + 1))),
                    ("Ω", CatalogSymbol::Property(PropertyKeyId(high + 2))),
                ])
                .unwrap();
            for reverse in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                let origin = db.pinned_read_view().unwrap();
                let mut requested = names;
                if reverse {
                    requested.reverse();
                }
                let registered = db
                    .register_symbol_names(&cx, &explicit, &requested)
                    .await
                    .unwrap();
                assert_eq!(registered, CommitSeq(1));
                assert_eq!(db.coordinator.chain().entries().len(), 1);
                assert_eq!(db.symbol_catalog().unwrap(), &expected);
                assert!(origin.symbol_catalog().is_empty());
                assert!(db.symbol_catalog_at(CommitSeq::ORIGIN).unwrap().is_empty());
                let manifest = db.manifest().unwrap();
                let generation = db.slot_generation;
                assert_eq!(
                    db.register_symbol_names(&cx, &explicit, &names)
                        .await
                        .unwrap(),
                    registered
                );
                assert_eq!(db.manifest().unwrap(), manifest);
                assert_eq!(db.slot_generation, generation);
                let mut write = WriteBatch::new(RelationId(1));
                write.create_vertex(
                    VId(1),
                    vec![LabelId(high + 1)],
                    vec![(PropertyKeyId(high), CanonicalScalar::Int(42))],
                );
                db.write(&cx, write).await.unwrap();
                assert_eq!(
                    db.query(
                        &contexts.query(),
                        "MATCH (n:Alpha) RETURN n.score AS value",
                        &GqlParameters::new(),
                        CatalogSymbolResolver::new(db.symbol_catalog().unwrap()),
                        GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                    )
                    .unwrap(),
                    QueryResult::Rows {
                        columns: vec!["value".to_owned()],
                        rows: vec![vec![QueryValue::Value(GraphValue::Scalar(
                            CanonicalScalar::Int(42),
                        ))]],
                    }
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn automatic_names_reserve_removed_numeric_history_after_compaction_and_reopen() {
        let ((), report) = run_async_under_lab(0x5ca1_0012, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut create = WriteBatch::new(RelationId(51));
            create.create_vertex(
                VId(1),
                vec![LabelId(17)],
                vec![(PropertyKeyId(21), CanonicalScalar::Int(1))],
            );
            create.create_vertex(VId(2), vec![], vec![]);
            create.add_edge(
                EId(1),
                VId(1),
                VId(2),
                vec![(PropertyKeyId(61), CanonicalScalar::Int(2))],
            );
            db.write(&cx, create).await.unwrap();
            let mut change = WriteBatch::new(RelationId(51));
            change.set_vertex_label(VId(1), LabelId(77), true);
            change.set_vertex_property(VId(1), PropertyKeyId(101), Some(CanonicalScalar::Int(3)));
            change.set_edge_property(EId(1), PropertyKeyId(111), Some(CanonicalScalar::Int(4)));
            db.write(&cx, change).await.unwrap();
            let mut clear = WriteBatch::new(RelationId(51));
            clear.set_vertex_label(VId(1), LabelId(77), false);
            clear.set_vertex_property(VId(1), PropertyKeyId(101), None);
            clear.set_edge_property(EId(1), PropertyKeyId(111), None);
            db.write(&cx, clear).await.unwrap();
            let mut retire = WriteBatch::new(RelationId(51));
            retire.delete_vertex(VId(1));
            retire.delete_vertex(VId(2));
            let before = db.write(&cx, retire).await.unwrap();
            assert!(db.vertices().unwrap().is_empty());
            db.compact(&cx).await.unwrap();
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(db.delta_materialized_after, before);
            let before_registration = db.pinned_read_view().unwrap();
            let names = [
                (CatalogSymbolKind::Label, "Person"),
                (CatalogSymbolKind::Relation, "KNOWS"),
                (CatalogSymbolKind::Property, "score"),
            ];
            let registered = db
                .register_symbol_names(&cx, &SymbolCatalog::default(), &names)
                .await
                .unwrap();
            assert_eq!(registered, CommitSeq(before.0 + 1));
            assert_eq!(db.delta_materialized_after, before);
            assert_eq!(
                db.delta_index().unwrap().retained_after_commit_seq(),
                before
            );
            assert_eq!(db.delta_since(before).unwrap().count(), 1);
            assert!(matches!(
                db.delta_since(CommitSeq::ORIGIN),
                Err(ReadError::DeltaCursorRetired { retained_after, .. })
                    if retained_after == before
            ));
            assert_eq!(before_registration.frontier(), before);
            assert!(before_registration.symbol_catalog().is_empty());
            let mut expected = SymbolCatalog::default();
            expected
                .bind_all([
                    ("Person", CatalogSymbol::Label(LabelId(78))),
                    ("KNOWS", CatalogSymbol::Relation(RelationId(52))),
                    ("score", CatalogSymbol::Property(PropertyKeyId(112))),
                ])
                .unwrap();
            assert_eq!(db.symbol_catalog().unwrap(), &expected);
            assert!(db.symbol_catalog_at(before).unwrap().is_empty());
            assert!(db.vertices().unwrap().is_empty());
            let pinned = db.pinned_read_view().unwrap();

            // Native numeric writers remain supported. A later registration
            // must include their symbols even when those elements are gone.
            let mut numeric = WriteBatch::new(RelationId(90));
            numeric.create_vertex(
                VId(3),
                vec![LabelId(200)],
                vec![(PropertyKeyId(300), CanonicalScalar::Int(5))],
            );
            db.write(&cx, numeric).await.unwrap();
            let mut retire = WriteBatch::new(RelationId(90));
            retire.delete_vertex(VId(3));
            db.write(&cx, retire).await.unwrap();
            db.compact(&cx).await.unwrap();
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let reopened_floor = db.delta_materialized_after;
            db.register_symbol_names(
                &cx,
                &SymbolCatalog::default(),
                &[
                    (CatalogSymbolKind::Label, "Later"),
                    (CatalogSymbolKind::Relation, "NEXT"),
                    (CatalogSymbolKind::Property, "later"),
                ],
            )
            .await
            .unwrap();
            assert_eq!(db.delta_materialized_after, reopened_floor);
            assert_eq!(
                db.delta_index().unwrap().retained_after_commit_seq(),
                reopened_floor
            );
            assert_eq!(db.delta_since(reopened_floor).unwrap().count(), 1);
            expected
                .bind_all([
                    ("Later", CatalogSymbol::Label(LabelId(201))),
                    ("NEXT", CatalogSymbol::Relation(RelationId(91))),
                    ("later", CatalogSymbol::Property(PropertyKeyId(301))),
                ])
                .unwrap();
            assert_eq!(db.symbol_catalog().unwrap(), &expected);
            assert_eq!(
                db.symbol_catalog_at(registered)
                    .unwrap()
                    .resolve(CatalogSymbolKind::Label, "Later"),
                None
            );
            assert_eq!(pinned.symbol_catalog().len(), 3);
            let published = db.pinned_read_view().unwrap();
            let catalog = db.snapshot.schema.reference.unwrap();
            drop(db);
            let checkpoint = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(checkpoint.manifest().unwrap(), published.manifest());
            drop(checkpoint);
            let rebuilt = Database::bind_with_vfs(&cx, vfs, &path, keys(), true)
                .await
                .unwrap();
            // A full replay can choose a different block layout after compaction.
            // The catalog object and every committed graph version are durable.
            assert_eq!(rebuilt.snapshot.schema.reference, Some(catalog));
            assert_eq!(rebuilt.symbol_catalog().unwrap(), &expected);
            assert_eq!(rebuilt.frontier().unwrap(), published.frontier());
            for seq in 0..=published.frontier().0 {
                let cut = CommitSeq(seq);
                assert_eq!(
                    rebuilt.symbol_catalog_at(cut).unwrap(),
                    published.symbol_catalog_at(cut).unwrap()
                );
                assert_eq!(
                    rebuilt.vertices_at(cut).unwrap(),
                    published.vertices_at(cut).unwrap()
                );
                assert_eq!(
                    rebuilt.edges_at(cut).unwrap(),
                    published.edges_at(cut).unwrap()
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn automatic_names_authenticate_prior_schema_objects_without_installing_history() {
        let ((), report) = run_async_under_lab(0x5ca1_0015, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for prior_transition in [false, true] {
                for missing in [false, true] {
                    let vfs = MemVfs::new().unwrap();
                    let path = vfs.database_dir();
                    let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    db.register_symbols(&cx, &bindings()).await.unwrap();
                    let first = db.snapshot.schema.reference.unwrap();
                    let transition = db
                        .snapshot
                        .delta_index
                        .since(CommitSeq::ORIGIN)
                        .unwrap()
                        .next()
                        .unwrap()
                        .coordinate_entries()[0]
                        .schema_transition
                        .unwrap();
                    let mut numeric = WriteBatch::new(RelationId(51));
                    numeric.create_vertex(
                        VId(1),
                        vec![LabelId(77)],
                        vec![(PropertyKeyId(111), CanonicalScalar::Int(1))],
                    );
                    db.write(&cx, numeric).await.unwrap();
                    let mut retire = WriteBatch::new(RelationId(51));
                    retire.delete_vertex(VId(1));
                    db.write(&cx, retire).await.unwrap();
                    let mut floors = SymbolCatalog::default();
                    floors.reserve_through(CatalogSymbol::Label(LabelId(60)));
                    floors.reserve_through(CatalogSymbol::Property(PropertyKeyId(100)));
                    let before = db.register_symbols(&cx, &floors).await.unwrap();
                    assert_ne!(db.snapshot.schema.reference.unwrap(), first);
                    db.compact(&cx).await.unwrap();
                    drop(db);

                    // Opening the current checkpoint only needs its current
                    // catalog. Origin allocation must also authenticate the
                    // older schema objects without retaining that history.
                    let mut db = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    assert_eq!(db.delta_materialized_after, before);
                    assert!(db.delta_index().unwrap().is_empty());
                    assert!(!db.snapshot.schema.origin_available);
                    let snapshot = Arc::clone(&db.snapshot);
                    let pinned = db.pinned_read_view().unwrap();
                    let catalog = db.symbol_catalog().unwrap().clone();
                    let manifest = db.manifest().unwrap();
                    let slot_generation = db.slot_generation;
                    let chain_len = db.coordinator.chain().entries().len();
                    let object = if prior_transition {
                        transition
                    } else {
                        first.root.0
                    };
                    let object_path = db.store.path(object);
                    let clean = cx
                        .with_restriction_async(vfs.read(&object_path))
                        .await
                        .unwrap();
                    let unavailable = object_path.with_extension("schema-audit-unavailable");
                    if missing {
                        cx.with_restriction_async(vfs.rename(&object_path, &unavailable))
                            .await
                            .unwrap();
                    } else {
                        cx.with_restriction_async(async {
                            let mut file = vfs
                                .open(&object_path, &OpenOptions::new().write(true).truncate(true))
                                .await?;
                            file.write_all(b"damaged historical schema object").await?;
                            file.sync_all().await
                        })
                        .await
                        .unwrap();
                    }
                    let names = [
                        (CatalogSymbolKind::Label, "Later"),
                        (CatalogSymbolKind::Relation, "NEXT"),
                        (CatalogSymbolKind::Property, "later"),
                    ];
                    let error = db
                        .register_symbol_names(&cx, &SymbolCatalog::default(), &names)
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(&error, SymbolCatalogError::Storage(source)
                            if matches!(source.as_ref(), RebuildError::Store(_))),
                        "prior_transition={prior_transition}, missing={missing}: {error:?}"
                    );
                    assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                    assert_eq!(db.frontier().unwrap(), before);
                    assert_eq!(
                        db.state(),
                        DatabaseState::Healthy {
                            published_frontier: before
                        }
                    );
                    assert_eq!(db.manifest().unwrap(), manifest);
                    assert_eq!(db.slot_generation, slot_generation);
                    assert_eq!(db.coordinator.chain().entries().len(), chain_len);
                    assert_eq!(db.symbol_catalog().unwrap(), &catalog);
                    assert_eq!(db.delta_materialized_after, before);
                    assert!(db.delta_index().unwrap().is_empty());
                    assert_eq!(
                        db.delta_index().unwrap().retained_after_commit_seq(),
                        before
                    );
                    assert!(db.vertices().unwrap().is_empty());
                    assert_eq!(pinned.frontier(), before);
                    assert_eq!(pinned.symbol_catalog(), &catalog);

                    if missing {
                        cx.with_restriction_async(vfs.rename(&unavailable, &object_path))
                            .await
                            .unwrap();
                    } else {
                        cx.with_restriction_async(async {
                            let mut file = vfs
                                .open(&object_path, &OpenOptions::new().write(true).truncate(true))
                                .await?;
                            file.write_all(&clean).await?;
                            file.sync_all().await
                        })
                        .await
                        .unwrap();
                    }
                    let registered = db
                        .register_symbol_names(&cx, &SymbolCatalog::default(), &names)
                        .await
                        .unwrap();
                    assert_eq!(registered, CommitSeq(before.0 + 1));
                    let mut expected = catalog.clone();
                    expected
                        .bind_all([
                            ("Later", CatalogSymbol::Label(LabelId(78))),
                            ("NEXT", CatalogSymbol::Relation(RelationId(52))),
                            ("later", CatalogSymbol::Property(PropertyKeyId(112))),
                        ])
                        .unwrap();
                    assert_eq!(db.symbol_catalog().unwrap(), &expected);
                    assert_eq!(db.delta_materialized_after, before);
                    assert_eq!(
                        db.delta_index().unwrap().retained_after_commit_seq(),
                        before
                    );
                    assert_eq!(db.delta_index().unwrap().len(), 1);
                    assert!(!db.snapshot.schema.origin_available);
                    assert_eq!(
                        db.snapshot.schema.history.len(),
                        snapshot.schema.history.len() + 1
                    );
                    assert_eq!(pinned.frontier(), before);
                    assert_eq!(pinned.symbol_catalog(), &catalog);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn automatic_name_requests_refuse_conflicts_exhaustion_and_missing_history_atomically() {
        let ((), report) = run_async_under_lab(0x5ca1_0013, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            db.register_symbols(&cx, &bindings()).await.unwrap();
            let before = db.frontier().unwrap();
            let manifest = db.manifest().unwrap();
            let catalog = db.symbol_catalog().unwrap().clone();
            let mut explicit = SymbolCatalog::default();
            explicit
                .bind("Conflict", CatalogSymbol::Label(LABEL))
                .unwrap();
            assert!(matches!(
                db.register_symbol_names(
                    &cx,
                    &explicit,
                    &[(CatalogSymbolKind::Property, "candidate")],
                )
                .await,
                Err(SymbolCatalogError::Binding(
                    CatalogBindingError::OrdinalConflict { .. }
                ))
            ));
            assert!(matches!(
                db.register_symbol_names(
                    &cx,
                    &SymbolCatalog::default(),
                    &[
                        (CatalogSymbolKind::Label, "candidate"),
                        (CatalogSymbolKind::Property, "")
                    ],
                )
                .await,
                Err(SymbolCatalogError::Binding(CatalogBindingError::EmptyName))
            ));
            let mut explicit = SymbolCatalog::default();
            explicit
                .bind("explicit", CatalogSymbol::Label(LabelId(100)))
                .unwrap();
            explicit.reserve_through(CatalogSymbol::Property(PropertyKeyId(u64::MAX)));
            assert!(matches!(
                db.register_symbol_names(
                    &cx,
                    &explicit,
                    &[
                        (CatalogSymbolKind::Label, "candidate"),
                        (CatalogSymbolKind::Property, "new")
                    ],
                )
                .await,
                Err(SymbolCatalogError::Binding(
                    CatalogBindingError::OrdinalsExhausted {
                        kind: CatalogSymbolKind::Property,
                    }
                ))
            ));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.manifest().unwrap(), manifest);
            assert_eq!(db.symbol_catalog().unwrap(), &catalog);
            assert_eq!(db.coordinator.chain().entries().len(), 1);

            // Explicit retention is not permission to guess the missing
            // high-water marks from current graph contents.
            Arc::make_mut(&mut db.snapshot)
                .delta_index
                .retire_prefix(before)
                .unwrap();
            assert!(matches!(
                db.register_symbol_names(
                    &cx,
                    &SymbolCatalog::default(),
                    &[(CatalogSymbolKind::Label, "candidate")],
                ).await,
                Err(SymbolCatalogError::Storage(error))
                    if matches!(*error, RebuildError::Index {
                        error: IndexError::CursorRetired { .. }, ..
                    })
            ));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.manifest().unwrap(), manifest);
            assert_eq!(db.symbol_catalog().unwrap(), &catalog);
            // Redeclarations allocate nothing and remain usable without an
            // origin-history reload, including the exact no-op sequence.
            assert_eq!(
                db.register_symbol_names(
                    &cx,
                    &SymbolCatalog::default(),
                    &[(CatalogSymbolKind::Label, "Person")],
                )
                .await
                .unwrap(),
                before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn cancelled_automatic_registration_preserves_names_frontier_and_publication() {
        let ((), report) = run_async_under_lab(0x5ca1_0014, |root| async move {
            let mut handle = root
                .spawn(move |child| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&child);
                    let cx = contexts.commit();
                    let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                    db.write(&cx, vertex(1, 42)).await.unwrap();
                    let snapshot = Arc::clone(&db.snapshot);
                    let generation = db.slot_generation;
                    let state = db.state;
                    child.set_cancel_reason(asupersync::CancelReason::user(
                        "cancel symbol registration before allocation",
                    ));
                    assert!(matches!(
                        db.register_symbol_names(
                            &cx,
                            &bindings(),
                            &[(CatalogSymbolKind::Label, "Next")],
                        ).await,
                        Err(SymbolCatalogError::Storage(error))
                            if matches!(*error, RebuildError::Interrupted(_))
                    ));
                    assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                    assert_eq!(db.slot_generation, generation);
                    assert_eq!(db.state, state);
                    assert!(db.symbol_catalog().unwrap().is_empty());
                    assert_eq!(db.frontier().unwrap(), CommitSeq(1));
                })
                .unwrap();
            assert_eq!(handle.join(&root).await, Ok(()));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn symbol_history_census_checks_cancellation_inside_wide_rows() {
        let row = DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 0,
            labels: vec![LabelId(10), LabelId(20), LabelId(30)],
            props: vec![
                (
                    PropertyKeyId(40),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(1)),
                ),
                (
                    PropertyKeyId(50),
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(2)),
                ),
            ],
            valid_time: None,
        };
        let original = SymbolCatalog::default();
        // A candidate is private until the full census and allocation succeed.
        // Interruption at each inner boundary must propagate immediately.
        for stop in 1..=6 {
            let mut candidate = original.clone();
            let mut calls = 0;
            assert_eq!(
                reserve_delta_symbols(&mut candidate, &row, || {
                    calls += 1;
                    if calls == stop {
                        Err("interrupted")
                    } else {
                        Ok(())
                    }
                }),
                Err("interrupted")
            );
            assert_eq!(calls, stop);
            assert_eq!(original.high_water_mark(CatalogSymbolKind::Label), 0);
            assert_eq!(original.high_water_mark(CatalogSymbolKind::Property), 0);
        }
        let mut admitted = original;
        reserve_delta_symbols(&mut admitted, &row, || Ok::<(), ()>(())).unwrap();
        assert_eq!(admitted.high_water_mark(CatalogSymbolKind::Label), 30);
        assert_eq!(admitted.high_water_mark(CatalogSymbolKind::Property), 50);
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
            later
                .bind("Later", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            let last = db.register_symbols(&cx, &later).await.unwrap();
            assert_eq!(last, CommitSeq(3));
            assert!(origin.symbol_catalog().is_empty());
            assert_eq!(pinned.symbol_catalog(), &bindings());
            assert_eq!(
                db.symbol_catalog_at(graph)
                    .unwrap()
                    .resolve(CatalogSymbolKind::Label, "Later"),
                None
            );
            assert_eq!(
                db.symbol_catalog_at(last)
                    .unwrap()
                    .resolve(CatalogSymbolKind::Label, "Later"),
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
                reopened
                    .query(
                        &contexts.query(),
                        "MATCH (n:Person) RETURN n.score AS value",
                        &GqlParameters::new(),
                        CatalogSymbolResolver::new(reopened.symbol_catalog().unwrap()),
                        query(),
                    )
                    .unwrap(),
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
            assert_eq!(
                prepared.template.coordinate_entries()[0].schema_epoch,
                SchemaEpoch(1)
            );
            drop(prepared);
            drop(current);
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert!(db.prepare_write_at(basis, vertex(2, 2)).is_err());
            db.ensure_delta_window(&cx, CommitSeq::ORIGIN)
                .await
                .unwrap();
            assert_eq!(db.snapshot.schema.at(basis).unwrap().reference, first_ref);
            let prepared = db.prepare_write_at(basis, vertex(2, 2)).unwrap();
            assert_eq!(prepared.template, expected);
            let committed = db.commit_prepared(&cx, prepared).await.unwrap();
            assert_eq!(committed, CommitSeq(4));
            assert!(db.vertex(VId(2)).unwrap().is_some());
            assert_eq!(db.snapshot.schema.catalog.epoch(), SchemaEpoch(2));
            assert_eq!(
                db.symbol_catalog()
                    .unwrap()
                    .high_water_mark(CatalogSymbolKind::Property),
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
                assert!(
                    matches!(
                        error,
                        SymbolCatalogError::Write(source)
                            if matches!(*source, WriteError::CommittedNeedsRecovery { .. })
                    ),
                    "{stage:?}"
                );
                assert!(db.symbol_catalog().is_err());
                assert!(pinned.symbol_catalog().is_empty());
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), CommitSeq(1), "{stage:?}");
                assert_eq!(reopened.symbol_catalog().unwrap(), &bindings(), "{stage:?}");
                assert_eq!(
                    reopened.register_symbols(&cx, &bindings()).await.unwrap(),
                    CommitSeq(1)
                );
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
                assert!(
                    db.register_symbols_with_faults(&cx, &bindings(), Some(crash), None)
                        .await
                        .is_err()
                );
                drop(db);
                let mut reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.frontier().unwrap(), CommitSeq::ORIGIN, "{crash:?}");
                assert!(reopened.symbol_catalog().unwrap().is_empty());
                assert_eq!(reopened.snapshot.schema.catalog.epoch(), SchemaEpoch(0));
                assert_eq!(
                    reopened.register_symbols(&cx, &bindings()).await.unwrap(),
                    CommitSeq(1)
                );
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
            let catalog = db
                .snapshot
                .schema
                .catalog
                .successor(basis, &bindings())
                .unwrap()
                .unwrap();
            let catalog_root = db.store.put_catalog(&cx, &catalog).await.unwrap();
            let transition = SchemaTransition {
                graph: GRAPH,
                branch: BRANCH,
                apply_basis: None,
                before: None,
                after: CatalogRef {
                    root: catalog_root,
                    epoch: SchemaEpoch(1),
                },
            };
            let transition = db
                .store
                .put_schema_transition(&cx, &transition)
                .await
                .unwrap();
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
            )
            .unwrap();
            assert!(matches!(
                db.commit_template(&cx, template, None, None, None).await,
                Err(WriteError::CatalogAdmission(_))
            ));
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(db.coordinator.chain().entries().len(), 1);
            assert!(db.symbol_catalog().unwrap().is_empty());
            assert!(db.vertex(VId(1)).unwrap().is_some());
            assert_eq!(
                db.register_symbols(&cx, &bindings()).await.unwrap(),
                CommitSeq(2)
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_views_pin_latest_and_historical_names_without_a_writer_lease() {
        let ((), report) = run_async_under_lab(0x5ca1_0007, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let origin = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(origin.frontier(), CommitSeq::ORIGIN);
            assert!(origin.symbol_catalog().is_empty());

            let first = db.register_symbols(&cx, &bindings()).await.unwrap();
            let graph = db.write(&cx, vertex(1, 42)).await.unwrap();
            let pinned = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(pinned.frontier(), graph);
            assert_eq!(pinned.symbol_catalog(), &bindings());
            assert!(
                pinned
                    .symbol_catalog_at(CommitSeq::ORIGIN)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(pinned.symbol_catalog_at(first).unwrap(), bindings());

            let mut later = SymbolCatalog::default();
            later
                .bind("Later", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            let last = db.register_symbols(&cx, &later).await.unwrap();
            let current = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(current.frontier(), last);
            assert_eq!(current.symbol_catalog(), db.symbol_catalog().unwrap());
            let historical = current.symbol_catalog_at(graph).unwrap();
            let mut expected = bindings();
            expected.reserve_through(CatalogSymbol::Label(LabelId(8)));
            assert_eq!(historical, expected);
            assert_eq!(historical.high_water_mark(CatalogSymbolKind::Label), 8);
            assert_eq!(historical.resolve(CatalogSymbolKind::Label, "Later"), None);
            assert_eq!(
                current
                    .symbol_catalog_at(last)
                    .unwrap()
                    .resolve(CatalogSymbolKind::Label, "Later"),
                Some(CatalogSymbol::Label(LabelId(8)))
            );
            assert_eq!(pinned.frontier(), graph);
            assert_eq!(pinned.symbol_catalog(), &bindings());
            assert!(matches!(
                pinned.symbol_catalog_at(last),
                Err(SymbolCatalogError::Read(ReadError::BeyondFrontier {
                    asked,
                    frontier,
                })) if asked == last && frontier == graph
            ));
            assert!(origin.symbol_catalog().is_empty());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn current_metadata_admission_does_not_materialize_damaged_graph_payloads() {
        let ((), report) = run_async_under_lab(0x5ca1_0008, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.register_symbols(&cx, &bindings()).await.unwrap();
            let graph = db.write(&cx, vertex(1, 42)).await.unwrap();
            let selected = db.store.get_root(&cx, db.snapshot.root).await.unwrap();
            let patch = selected
                .vertex_patches
                .first()
                .expect("graph vertex payload");
            let object_path = db.store.path(patch.patch_id);
            cx.with_restriction_async(async {
                let mut file = vfs
                    .open(&object_path, &OpenOptions::new().write(true).truncate(true))
                    .await?;
                file.write_all(b"damaged graph payload").await?;
                file.sync_all().await
            })
            .await
            .unwrap();

            let metadata = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(metadata.frontier(), graph);
            assert_eq!(metadata.symbol_catalog(), &bindings());
            let current =
                Database::open_current_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
            assert_eq!(current.frontier(), graph);
            assert_eq!(current.symbol_catalog(), metadata.symbol_catalog());
            assert!(
                Database::open_read_view_with_vfs(&cx, vfs, &path, keys())
                    .await
                    .is_err()
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_open_preserves_live_writer_lag_and_heals_after_publication_failure() {
        let ((), report) = run_async_under_lab(0x5ca1_0009, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            db.register_symbols(&cx, &bindings()).await.unwrap();
            let published = db.write(&cx, vertex(1, 42)).await.unwrap();
            let mut later = SymbolCatalog::default();
            later
                .bind("Later", CatalogSymbol::Label(LabelId(8)))
                .unwrap();
            let error = db
                .register_symbols_with_faults(
                    &cx,
                    &later,
                    None,
                    Some(DerivedPublicationStage::PublishRootSlot),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                SymbolCatalogError::Write(source)
                    if matches!(*source, WriteError::CommittedNeedsRecovery { .. })
            ));

            // The live owner retains healing authority. Readers can still
            // bind the previously published catalog and its exact frontier.
            let lagging = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(lagging.frontier(), published);
            assert_eq!(lagging.symbol_catalog(), &bindings());
            drop(db);

            // Once the lease is released, ordinary read-view recovery admits
            // the committed successor before metadata is returned.
            let healed = Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(healed.frontier(), CommitSeq(published.0 + 1));
            assert_eq!(
                healed
                    .symbol_catalog()
                    .resolve(CatalogSymbolKind::Label, "Later"),
                Some(CatalogSymbol::Label(LabelId(8)))
            );
            let historical = healed.symbol_catalog_at(published).unwrap();
            let mut expected = bindings();
            expected.reserve_through(CatalogSymbol::Label(LabelId(8)));
            assert_eq!(historical, expected);
            assert_eq!(historical.high_water_mark(CatalogSymbolKind::Label), 8);
            assert_eq!(historical.resolve(CatalogSymbolKind::Label, "Later"), None);
            assert_eq!(lagging.frontier(), published);
            assert_eq!(lagging.symbol_catalog(), &bindings());
            let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.frontier().unwrap(), healed.frontier());
            assert_eq!(reopened.symbol_catalog().unwrap(), healed.symbol_catalog());
            assert_eq!(reopened.coordinator.chain().entries().len(), 3);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn current_only_metadata_refuses_unpublished_cuts_without_healing_or_graph_reads() {
        let ((), report) = run_async_under_lab(0x5ca1_0010, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for missing_slot in [false, true] {
                let vfs = MemVfs::new().unwrap();
                let path = vfs.database_dir();
                let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                    .await
                    .unwrap();
                db.register_symbols(&cx, &bindings()).await.unwrap();
                let published = db.write(&cx, vertex(1, 42)).await.unwrap();
                let selected = db.store.get_root(&cx, db.snapshot.root).await.unwrap();
                let patch = selected
                    .vertex_patches
                    .first()
                    .expect("graph vertex payload");
                let graph_path = db.store.path(patch.patch_id);
                let graph_unavailable = graph_path.with_extension("metadata-test-unavailable");
                let slot_path = path.join(fgdb_chronicle::store::ROOT_FILE_NAME);
                let slot_before = cx
                    .with_restriction_async(vfs.read(&slot_path))
                    .await
                    .unwrap();
                if missing_slot {
                    let slot_unavailable = slot_path.with_extension("metadata-test-unavailable");
                    cx.with_restriction_async(vfs.rename(&slot_path, &slot_unavailable))
                        .await
                        .unwrap();
                } else {
                    let mut later = SymbolCatalog::default();
                    later
                        .bind("Later", CatalogSymbol::Label(LabelId(8)))
                        .unwrap();
                    assert!(matches!(
                        db.register_symbols_with_faults(
                            &cx,
                            &later,
                            None,
                            Some(DerivedPublicationStage::PublishRootSlot),
                        ).await,
                        Err(SymbolCatalogError::Write(source))
                            if matches!(*source, WriteError::CommittedNeedsRecovery { .. })
                    ));
                }
                drop(db);
                // The writer lease is free, so a recovering opener could heal.
                // Hide a graph payload too: strict metadata admission must
                // refuse the publication before any graph read or repair.
                cx.with_restriction_async(vfs.rename(&graph_path, &graph_unavailable))
                    .await
                    .unwrap();
                assert!(matches!(
                    Database::open_current_symbol_catalog_with_vfs(
                        &cx, vfs.clone(), &path, keys(),
                    ).await,
                    Err(SymbolCatalogOpenError::RecoveryRequired)
                ));
                assert!(matches!(
                    cx.with_restriction_async(vfs.symlink_metadata(&graph_path)).await,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ));
                if missing_slot {
                    assert!(matches!(
                        cx.with_restriction_async(vfs.read(&slot_path)).await,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound
                    ));
                } else {
                    assert_eq!(
                        cx.with_restriction_async(vfs.read(&slot_path))
                            .await
                            .unwrap(),
                        slot_before
                    );
                }

                // Restore only the test payload. Ordinary metadata opening
                // retains the existing healing contract for either slot state.
                cx.with_restriction_async(vfs.rename(&graph_unavailable, &graph_path))
                    .await
                    .unwrap();
                let healed =
                    Database::open_symbol_catalog_with_vfs(&cx, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                assert_eq!(
                    healed.frontier(),
                    if missing_slot {
                        published
                    } else {
                        CommitSeq(published.0 + 1)
                    }
                );
                assert_eq!(
                    healed
                        .symbol_catalog()
                        .resolve(CatalogSymbolKind::Label, "Person"),
                    Some(CatalogSymbol::Label(LABEL))
                );
                assert_eq!(
                    healed
                        .symbol_catalog()
                        .resolve(CatalogSymbolKind::Label, "Later"),
                    if missing_slot {
                        None
                    } else {
                        Some(CatalogSymbol::Label(LabelId(8)))
                    }
                );
                let current =
                    Database::open_current_symbol_catalog_with_vfs(&cx, vfs, &path, keys())
                        .await
                        .unwrap();
                assert_eq!(current.frontier(), healed.frontier());
                assert_eq!(current.symbol_catalog(), healed.symbol_catalog());
            }
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
                    let transition = db
                        .snapshot
                        .delta_index
                        .since(CommitSeq::ORIGIN)
                        .unwrap()
                        .next()
                        .unwrap()
                        .coordinate_entries()[0]
                        .schema_transition
                        .unwrap();
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
                            let mut file = vfs
                                .open(&object_path, &OpenOptions::new().write(true).truncate(true))
                                .await?;
                            file.write_all(b"damaged schema object").await?;
                            file.sync_all().await
                        })
                        .await
                        .unwrap();
                    }
                    let summary = db.scrub(&cx).await.unwrap();
                    assert!(
                        summary
                            .metadata_lost
                            .iter()
                            .any(|lost| lost.object_id == object),
                        "target={target}, missing={missing}"
                    );
                    assert!(db.symbol_catalog().is_err());
                    assert!(db.vertices().is_err());
                    drop(db);
                    if target == 0 {
                        assert!(
                            Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                                .await
                                .is_err()
                        );
                    }
                    assert!(
                        Database::bind_with_vfs(&cx, vfs, &path, keys(), true)
                            .await
                            .is_err(),
                        "full recovery must authenticate target={target}, missing={missing}"
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
