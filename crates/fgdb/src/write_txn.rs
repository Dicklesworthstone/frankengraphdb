use crate::{
    Database, EdgeRecord, GqlError, PendingRow, PreparedWrite, ReadError, RelationBind, VertexRow,
    WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{
    Acquired, CanonicalScalar, CommitCx, CommitSeq, EId, ObligationAcquireError, ObligationId,
    PurposeObligation, TxnCx, VId,
};

/// Failure to prepare or finish the bounded one-batch write transaction.
#[derive(Debug)]
pub enum WriteTxnError {
    NoPreparedWrite,
    Finished,
    RelationMismatch {
        expected: RelationId,
        found: RelationId,
    },
    SnapshotAdvanced {
        pinned: CommitSeq,
        live: CommitSeq,
    },
    Read(ReadError),
    Gql(GqlError),
    Write(WriteError),
}

impl core::fmt::Display for WriteTxnError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoPreparedWrite => formatter.write_str("write transaction has no batch"),
            Self::Finished => formatter.write_str("write transaction is already finished"),
            Self::RelationMismatch { expected, found } => write!(
                formatter,
                "write transaction relation mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::SnapshotAdvanced { pinned, live } => write!(
                formatter,
                "write transaction pinned {pinned:?}, but the live snapshot advanced to {live:?}"
            ),
            Self::Read(source) => write!(formatter, "could not read the pinned snapshot: {source}"),
            Self::Gql(source) => write!(formatter, "transaction GQL failed: {source}"),
            Self::Write(source) => write!(formatter, "write transaction failed: {source}"),
        }
    }
}

impl core::error::Error for WriteTxnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(source) => Some(source),
            Self::Gql(source) => Some(source),
            Self::Write(source) => Some(source),
            Self::NoPreparedWrite
            | Self::Finished
            | Self::RelationMismatch { .. }
            | Self::SnapshotAdvanced { .. } => None,
        }
    }
}

impl From<ReadError> for WriteTxnError {
    fn from(source: ReadError) -> Self {
        Self::Read(source)
    }
}

impl From<WriteError> for WriteTxnError {
    fn from(source: WriteError) -> Self {
        Self::Write(source)
    }
}

impl From<GqlError> for WriteTxnError {
    fn from(source: GqlError) -> Self {
        Self::Gql(source)
    }
}

/// Write batches staged against a snapshot pinned by a [`TxnCx`].
///
/// This is deliberately not SSI: after each write it combines same-relation
/// batches in call order and refreshes one prepared template against the
/// pinned basis. Commit delegates that retained template's verdict to
/// [`Database::commit_prepared`].
pub struct WriteTxn {
    basis: CommitSeq,
    staged: Vec<WriteBatch>,
    prepared: Option<PreparedWrite>,
    read_set: std::cell::RefCell<std::collections::BTreeSet<ElementId>>,
    match_expansions: std::cell::RefCell<std::collections::BTreeSet<(VId, RelationId)>>,
    pin: Option<PurposeObligation<Acquired>>,
}

impl WriteTxn {
    pub(crate) fn begin(
        basis: CommitSeq,
        txn: &TxnCx,
        obligation_id: ObligationId,
    ) -> Result<Self, ObligationAcquireError> {
        let pin = txn.pin_snapshot(obligation_id)?;
        Ok(Self {
            basis,
            staged: Vec::new(),
            prepared: None,
            read_set: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            match_expansions: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            pin: Some(pin),
        })
    }

    /// The snapshot frontier retained for this transaction.
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Stage a same-relation batch against this transaction's pinned snapshot.
    pub fn write<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let live = database.frontier()?;
        if live != self.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            });
        }

        if let Some(expected) = self.staged.first().map(|staged| staged.relation)
            && batch.relation != expected
        {
            return Err(WriteTxnError::RelationMismatch {
                expected,
                found: batch.relation,
            });
        }
        self.staged.push(batch);
        let combined = Self::combined_batch(&self.staged)
            .expect("a batch was staged immediately before combination");
        let prepared = match database.prepare_write(combined) {
            Ok(prepared) => prepared,
            Err(source) => {
                self.staged.pop();
                return Err(WriteTxnError::Write(source));
            }
        };
        debug_assert_eq!(prepared.basis(), self.basis);
        self.prepared = Some(prepared);
        Ok(())
    }

    /// Read one vertex from the pinned durable basis plus this transaction's
    /// staged row-order overlay. This performs no preparation or publication.
    pub fn vertex<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
    ) -> Result<Option<VertexRow>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let live = database.frontier()?;
        let mut overlay = if live == self.basis {
            database.vertex(vid)?
        } else {
            database.vertex_at(vid, self.basis)?
        };

        let mut intent_ordinal = 0u64;
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            intent_ordinal = intent_ordinal
                .checked_add(1)
                .expect("a transaction cannot stage 2^64 rows");
            match pending {
                PendingRow::Vertex {
                    vid: row_vid,
                    labels,
                    props,
                    ensure: _,
                } if *row_vid == vid && overlay.is_none() => {
                    let mut labels = labels.clone();
                    let mut props = props.clone();
                    crate::sort_write_labels_and_props(&mut labels, &mut props);
                    overlay = Some(VertexRow {
                        vid,
                        birth_ordinal: intent_ordinal,
                        created_at: self.basis,
                        retired_at: None,
                        labels,
                        props,
                    });
                }
                PendingRow::DeleteVertex { vid: row_vid, .. } if *row_vid == vid => {
                    overlay = None;
                }
                PendingRow::SetLabel {
                    vid: row_vid,
                    label,
                    member,
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        match row.labels.binary_search(label) {
                            Ok(at) if !member => {
                                row.labels.remove(at);
                            }
                            Err(at) if *member => row.labels.insert(at, *label),
                            Ok(_) | Err(_) => {}
                        }
                    }
                }
                PendingRow::SetProperty {
                    vid: row_vid,
                    key,
                    value,
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        Self::overlay_property(&mut row.props, *key, value.as_ref());
                    }
                }
                PendingRow::CompareAndSet {
                    elem: ElementId::Vertex(row_vid),
                    key,
                    expected,
                    value,
                    ..
                } if *row_vid == vid => {
                    if let Some(row) = overlay.as_mut() {
                        let actual = row
                            .props
                            .binary_search_by_key(key, |(property, _)| *property)
                            .ok()
                            .map(|at| &row.props[at].1);
                        if actual == expected.as_deref() {
                            Self::overlay_property(&mut row.props, *key, Some(value.as_ref()));
                        }
                    }
                }
                PendingRow::Vertex { .. }
                | PendingRow::Edge { .. }
                | PendingRow::DeleteEdge { .. }
                | PendingRow::DeleteVertex { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetEdgeProperty { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }
        self.read_set
            .borrow_mut()
            .insert(ElementId::Vertex(vid));
        Ok(overlay)
    }

    /// Read every vertex from the pinned basis through this transaction's
    /// staged row-order overlay, sorted by vertex identity.
    pub fn vertices<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut vids: std::collections::BTreeSet<VId> = database
            .vertices_at(self.basis)?
            .into_iter()
            .map(|row| row.vid)
            .collect();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                    vids.insert(*vid);
                }
                PendingRow::Edge { .. }
                | PendingRow::DeleteEdge { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetEdgeProperty { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }

        let mut rows = Vec::new();
        for vid in vids {
            if let Some(row) = self.vertex(database, vid)? {
                rows.push(row);
            }
        }
        rows.sort_by_key(|row| row.vid);
        self.read_set
            .borrow_mut()
            .extend(rows.iter().map(|row| ElementId::Vertex(row.vid)));
        Ok(rows)
    }

    /// Read one edge from the pinned durable basis plus this transaction's
    /// staged create/delete overlay, without publishing the transaction.
    pub fn edge<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<EdgeRecord>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut overlay = database.edge_at(eid, self.basis)?;
        let mut observed_sources = std::collections::BTreeSet::new();
        let mut deleted_vertices = std::collections::BTreeSet::new();
        if let Some(record) = &overlay {
            observed_sources.insert(record.entry.src);
        }

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid: row_eid,
                        src,
                        dst,
                        props,
                        ensure: _,
                    } if *row_eid == eid => {
                        let mut props = props.clone();
                        crate::sort_write_props(&mut props);
                        observed_sources.insert(*src);
                        overlay = Some(EdgeRecord {
                            entry: AdjacencyEntry {
                                src: *src,
                                relation: batch.relation,
                                dst: *dst,
                                eid,
                                created_at: self.basis,
                                retired_at: None,
                            },
                            props,
                        });
                    }
                    PendingRow::DeleteEdge { eid: row_eid, .. } if *row_eid == eid => {
                        overlay = None;
                    }
                    PendingRow::SetEdgeProperty {
                        eid: row_eid,
                        key,
                        value,
                    } if *row_eid == eid => {
                        if let Some(record) = overlay.as_mut() {
                            Self::overlay_property(&mut record.props, *key, value.as_ref());
                        }
                    }
                    PendingRow::CompareAndSet {
                        elem: ElementId::Edge(row_eid),
                        key,
                        expected,
                        value,
                        ..
                    } if *row_eid == eid => {
                        if let Some(record) = overlay.as_mut() {
                            let actual = record
                                .props
                                .binary_search_by_key(key, |(property, _)| *property)
                                .ok()
                                .map(|at| &record.props[at].1);
                            if actual == expected.as_deref() {
                                Self::overlay_property(
                                    &mut record.props,
                                    *key,
                                    Some(value.as_ref()),
                                );
                            }
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        if overlay.as_ref().is_some_and(|record| {
                            record.entry.src == *vid || record.entry.dst == *vid
                        }) {
                            deleted_vertices.insert(*vid);
                            overlay = None;
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::DeleteEdge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Edge(eid));
        read_set.extend(observed_sources.into_iter().map(ElementId::Vertex));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        Ok(overlay)
    }

    /// Read every edge from the pinned basis through this transaction's
    /// staged row-order overlay, sorted by edge identity.
    pub fn edges<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<EdgeRecord>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut eids: std::collections::BTreeSet<EId> = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|record| record.entry.eid)
            .collect();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Edge { eid, .. }
                | PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. }
                | PendingRow::CompareAndSet {
                    elem: ElementId::Edge(eid),
                    ..
                } => {
                    eids.insert(*eid);
                }
                PendingRow::Vertex { .. }
                | PendingRow::DeleteVertex { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }

        let mut rows = Vec::new();
        for eid in eids {
            if let Some(record) = self.edge(database, eid)? {
                rows.push(record);
            }
        }
        rows.sort_by_key(|record| record.entry.eid);

        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(
            rows.iter()
                .map(|record| ElementId::Edge(record.entry.eid)),
        );
        read_set.extend(
            rows.iter()
                .map(|record| ElementId::Vertex(record.entry.src)),
        );
        drop(read_set);
        self.match_expansions.borrow_mut().extend(
            rows.iter()
                .map(|record| (record.entry.src, record.entry.relation)),
        );
        Ok(rows)
    }

    /// Read the pinned neighbours of one relation through staged edge
    /// creates and deletes. Destinations retain the database API's sorted,
    /// deduplicated result shape even when parallel edges exist.
    pub fn neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut destinations: std::collections::BTreeSet<VId> = database
            .neighbours_at(src, relation, self.basis)?
            .into_iter()
            .collect();
        let mut matching_edges: std::collections::BTreeMap<EId, VId> = database
            .edges_at(self.basis)?
            .into_iter()
            .filter_map(|record| {
                (record.entry.src == src && record.entry.relation == relation)
                    .then_some((record.entry.eid, record.entry.dst))
            })
            .collect();
        let mut observed_edges: std::collections::BTreeSet<EId> =
            matching_edges.keys().copied().collect();
        let mut deleted_vertices = std::collections::BTreeSet::new();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid,
                        src: edge_src,
                        dst,
                        ensure,
                        ..
                    } if *edge_src == src && batch.relation == relation => {
                        if !ensure || !destinations.contains(dst) {
                            matching_edges.insert(*eid, *dst);
                            destinations.insert(*dst);
                            observed_edges.insert(*eid);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        if let Some(dst) = matching_edges.remove(eid)
                            && !matching_edges.values().any(|other| *other == dst)
                        {
                            destinations.remove(&dst);
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        let affected = if *vid == src {
                            matching_edges.clear();
                            destinations.clear();
                            true
                        } else if matching_edges.values().any(|dst| *dst == *vid) {
                            matching_edges.retain(|_, dst| *dst != *vid);
                            destinations.remove(vid);
                            true
                        } else {
                            false
                        };
                        if affected {
                            deleted_vertices.insert(*vid);
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Vertex(src));
        read_set.extend(observed_edges.into_iter().map(ElementId::Edge));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        drop(read_set);
        self.match_expansions.borrow_mut().insert((src, relation));
        Ok(destinations.into_iter().collect())
    }

    /// Read the pinned incoming neighbours of one relation through staged
    /// edge creates, edge deletes, and vertex-delete cascades.
    pub fn in_neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        dst: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut sources: std::collections::BTreeSet<VId> = database
            .in_neighbours_at(dst, relation, self.basis)?
            .into_iter()
            .collect();
        let mut matching_edges: std::collections::BTreeMap<EId, VId> = database
            .edges_at(self.basis)?
            .into_iter()
            .filter_map(|record| {
                (record.entry.dst == dst && record.entry.relation == relation)
                    .then_some((record.entry.eid, record.entry.src))
            })
            .collect();
        let mut observed_edges: std::collections::BTreeSet<EId> =
            matching_edges.keys().copied().collect();
        let mut deleted_sources = std::collections::BTreeSet::new();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid,
                        src,
                        dst: edge_dst,
                        ensure,
                        ..
                    } if *edge_dst == dst && batch.relation == relation => {
                        if !ensure || !sources.contains(src) {
                            matching_edges.insert(*eid, *src);
                            sources.insert(*src);
                            observed_edges.insert(*eid);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        if let Some(src) = matching_edges.remove(eid)
                            && !matching_edges.values().any(|other| *other == src)
                        {
                            sources.remove(&src);
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        if *vid == dst {
                            matching_edges.clear();
                            sources.clear();
                        } else if matching_edges.values().any(|src| *src == *vid) {
                            matching_edges.retain(|_, src| *src != *vid);
                            sources.remove(vid);
                            deleted_sources.insert(*vid);
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Vertex(dst));
        read_set.extend(observed_edges.into_iter().map(ElementId::Edge));
        read_set.extend(deleted_sources.into_iter().map(ElementId::Vertex));
        Ok(sources.into_iter().collect())
    }

    /// Execute the pinned MATCH expansion over the durable basis plus staged
    /// vertex and edge mutations, without publishing the transaction.
    pub fn execute_gql<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: &str,
        bind: &RelationBind,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let plan = bind.bind(src).map_err(|error| {
            WriteTxnError::Gql(match error {
                fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
                unbound => GqlError::Bind(unbound),
            })
        })?;

        // The node-only scan face (fgdb-w5-parsers-nje.7), over the OVERLAY
        // vertex rows so staged labels are visible and staged deletes hide:
        // no edge relation means no edge machinery at all. The binder makes
        // an unlabeled node-only plan unrepresentable, so the missing-label
        // arm fails closed to no rows. Result rows are vertex observations,
        // recorded exactly as the edge face records its own. `src_prop`
        // (fgdb-w5-parsers-nje.11) is the same integer-property test as
        // live `node_scan`; no-WHERE overlay scans still ignore props.
        let Some(edge_relation) = plan.relation else {
            let Some(label) = plan.src_label else {
                return Ok(Vec::new());
            };
            let mut vids: Vec<VId> = self
                .vertices(database)?
                .into_iter()
                .filter(|row| row.labels.contains(&label))
                .filter(|row| {
                    let equal = match plan.src_prop {
                        None => true,
                        Some((key, value)) => {
                            let wanted = CanonicalScalar::Int(value);
                            row.props
                                .iter()
                                .any(|(property, scalar)| *property == key && *scalar == wanted)
                        }
                    };
                    let not_equal = match plan.src_prop_ne {
                        None => true,
                        Some((key, value)) => row.props.iter().any(|(property, scalar)| {
                            *property == key
                                && matches!(
                                    scalar,
                                    CanonicalScalar::Int(actual) if *actual != value
                                )
                        }),
                    };
                    equal && not_equal
                })
                .map(|row| row.vid)
                .collect();
            vids.sort_unstable();
            vids.dedup();
            let vids = crate::apply_limit(&plan, vids);
            self.read_set
                .borrow_mut()
                .extend(vids.iter().copied().map(ElementId::Vertex));
            return Ok(vids);
        };

        let mut observed = std::collections::BTreeSet::new();
        let mut vertices = std::collections::BTreeSet::new();
        let mut edges: std::collections::BTreeMap<
            fgdb_types::EId,
            (VId, RelationId, VId),
        > = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|row| {
                observed.insert(ElementId::Edge(row.entry.eid));
                observed.insert(ElementId::Vertex(row.entry.src));
                observed.insert(ElementId::Vertex(row.entry.dst));
                vertices.insert(row.entry.src);
                vertices.insert(row.entry.dst);
                (
                    row.entry.eid,
                    (row.entry.src, row.entry.relation, row.entry.dst),
                )
            })
            .collect();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Vertex { vid, .. } => {
                        observed.insert(ElementId::Vertex(*vid));
                        vertices.insert(*vid);
                    }
                    PendingRow::Edge {
                        eid,
                        src,
                        dst,
                        ensure,
                        ..
                    } => {
                        observed.insert(ElementId::Edge(*eid));
                        observed.insert(ElementId::Vertex(*src));
                        observed.insert(ElementId::Vertex(*dst));
                        vertices.insert(*src);
                        vertices.insert(*dst);
                        let triple = (*src, batch.relation, *dst);
                        if !ensure || !edges.values().any(|existing| *existing == triple) {
                            edges.insert(*eid, triple);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        observed.insert(ElementId::Edge(*eid));
                        edges.remove(eid);
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        observed.insert(ElementId::Vertex(*vid));
                        vertices.remove(vid);
                        edges.retain(|_, (src, _, dst)| *src != *vid && *dst != *vid);
                    }
                    PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        // One kernel (fgdb-gql-one-kernel-7y17): the same
        // `execute_bound_plan_over` expansion the autocommit path uses, fed
        // the OVERLAY edge view instead of the live fold. Every edge left in
        // `edges` has both endpoints in `vertices` (deletes retire the edge
        // with the vertex above), so expanding each overlay-live source over
        // its overlay out-edges in the plan's relation is exactly the old
        // whole-map filter — but through the shared row discipline, so a
        // CGSE change cannot drift the two surfaces apart.
        // Node-label predicates constrain the MATCH itself, independent of
        // projection (fgdb-w5-parsers-nje.5, corrected law), read through the
        // OVERLAY vertex face so a staged label is visible and a staged
        // delete hides the row: src_label drops anchors before expansion,
        // dst_label drops hop-1 step results below. Unlabeled plans consult
        // no vertex row at all.
        let label_holders = |label: fgdb_delta_types::LabelId| -> Result<
            std::collections::BTreeSet<VId>,
            WriteTxnError,
        > {
            let mut holders = std::collections::BTreeSet::new();
            for vid in vertices.iter().copied() {
                if self
                    .vertex(database, vid)?
                    .is_some_and(|row| row.labels.contains(&label))
                {
                    holders.insert(vid);
                }
            }
            Ok(holders)
        };
        let src_labeled = plan.src_label.map(&label_holders).transpose()?;
        let dst_labeled = plan.dst_label.map(&label_holders).transpose()?;
        let prop_holders = |key: fgdb_delta_types::PropertyKeyId, value: i64| -> Result<
            std::collections::BTreeSet<VId>,
            WriteTxnError,
        > {
            let wanted = CanonicalScalar::Int(value);
            let mut holders = std::collections::BTreeSet::new();
            for vid in vertices.iter().copied() {
                if self.vertex(database, vid)?.is_some_and(|row| {
                    row.props
                        .iter()
                        .any(|(property, scalar)| *property == key && *scalar == wanted)
                }) {
                    holders.insert(vid);
                }
            }
            Ok(holders)
        };
        let src_prop_ok = match plan.src_prop {
            None => None,
            Some((key, value)) => Some(prop_holders(key, value)?),
        };
        let src_prop_ne_ok = match plan.src_prop_ne {
            None => None,
            Some((key, value)) => {
                let mut holders = std::collections::BTreeSet::new();
                for vid in vertices.iter().copied() {
                    if self.vertex(database, vid)?.is_some_and(|row| {
                        row.props.iter().any(|(property, scalar)| {
                            *property == key
                                && matches!(
                                    scalar,
                                    CanonicalScalar::Int(actual) if *actual != value
                                )
                        })
                    }) {
                        holders.insert(vid);
                    }
                }
                Some(holders)
            }
        };
        let dst_prop_ok = match plan.dst_prop {
            None => None,
            Some((key, value)) => Some(prop_holders(key, value)?),
        };
        let anchors: Vec<VId> = vertices
            .iter()
            .copied()
            .filter(|anchor| {
                src_labeled
                    .as_ref()
                    .is_none_or(|labeled| labeled.contains(anchor))
                    && src_prop_ok
                        .as_ref()
                        .is_none_or(|holders| holders.contains(anchor))
                    && src_prop_ne_ok
                        .as_ref()
                        .is_none_or(|holders| holders.contains(anchor))
            })
            .collect();
        let destinations = crate::execute_bound_plan_over(
            &plan,
            anchors,
            |source, relation| {
                // One orientation-aware neighbour walk serves every face
                // (fgdb-w5-parsers-nje.2 one-hop, fgdb-gql-two-hop-8pfw,
                // fgdb-gql-undir-2hop-7mrc): a directed plan expands
                // edge-flow only; an undirected plan expands both
                // orientations — the overlay twin of gql_exec's
                // per-direction adjacency builders.
                let undirected = plan.direction == fgdb_gql::EdgeDirection::Undirected;
                // The incoming TWO-hop chain reverse-composes: every step
                // walks against edge flow (fgdb-w5-parsers-nje.4). Incoming
                // ONE-hop stays forward — the parser normalized its variable
                // roles, exactly as on the durable faces.
                let reverse = plan.direction == fgdb_gql::EdgeDirection::Incoming
                    && plan.hop2_relation.is_some();
                let step = |anchor: VId, step_relation: fgdb_delta_types::RelationId| -> Vec<VId> {
                    edges
                        .values()
                        .filter_map(|(edge_src, edge_relation, edge_dst)| {
                            if *edge_relation != step_relation {
                                return None;
                            }
                            if !reverse && *edge_src == anchor && vertices.contains(edge_dst) {
                                Some(*edge_dst)
                            } else if (reverse || undirected)
                                && *edge_dst == anchor
                                && vertices.contains(edge_src)
                            {
                                Some(*edge_src)
                            } else {
                                None
                            }
                        })
                        .collect()
                };
                // WHERE a <> b and WHERE a = b filter exactly the hop-1
                // step, mirroring gql_exec (fgdb-gql-where-neq-v476,
                // fgdb-w5-parsers-nje.6): inequality drops staged or durable
                // self-loops before projection or composition; equality
                // keeps only them.
                let mut vias = step(source, relation);
                if plan.neq.is_some() {
                    vias.retain(|via| *via != source);
                }
                if plan.eq.is_some() {
                    vias.retain(|via| *via == source);
                }
                if let Some(labeled) = dst_labeled.as_ref() {
                    vias.retain(|via| labeled.contains(via));
                }
                if let Some(holders) = dst_prop_ok.as_ref() {
                    vias.retain(|via| holders.contains(via));
                }
                let Some(hop2_relation) = plan.hop2_relation else {
                    return Ok(vias);
                };
                // Two-hop composition, projection-shaped exactly as gql_exec
                // composes the durable adjacency: the via projection keeps
                // intermediates that continue, every other projection expands
                // to the composed hop-2 neighbours.
                Ok(match plan.projection {
                    fgdb_gql::ReturnProjection::Destination => vias
                        .into_iter()
                        .filter(|via| !step(*via, hop2_relation).is_empty())
                        .collect(),
                    fgdb_gql::ReturnProjection::Source
                    | fgdb_gql::ReturnProjection::Hop2Destination => vias
                        .into_iter()
                        .flat_map(|via| step(via, hop2_relation))
                        .collect(),
                })
            },
        )?;
        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(observed);
        // MATCH result materialization is itself a vertex observation. Keep
        // this explicit even when the scan already recorded the same vertex:
        // future narrower expansion kernels must not silently lose result-row
        // read dependencies.
        read_set.extend(destinations.iter().copied().map(ElementId::Vertex));
        drop(read_set);
        self.match_expansions
            .borrow_mut()
            .extend(vertices.iter().copied().map(|src| (src, edge_relation)));
        Ok(destinations)
    }

    /// Execute the pinned overlay MATCH and certify its bound plan against the
    /// transaction basis rather than the database's potentially newer frontier.
    pub fn execute_gql_certified<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, crate::GqlPlanCertificate), WriteTxnError> {
        let plan = bind.bind(src).map_err(|error| {
            WriteTxnError::Gql(match error {
                fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
                unbound => GqlError::Bind(unbound),
            })
        })?;
        let rows = self.execute_gql(database, src, bind)?;
        let certificate = crate::gql_cert::certify(&plan, self.basis());
        Ok((rows, certificate))
    }

    pub fn gql_plan_certificate(
        &self,
        src: &str,
        bind: &RelationBind,
    ) -> Result<crate::GqlPlanCertificate, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let plan = bind.bind(src).map_err(|error| {
            WriteTxnError::Gql(match error {
                fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
                unbound => GqlError::Bind(unbound),
            })
        })?;
        Ok(crate::gql_cert::certify(&plan, self.basis()))
    }

    /// Commit the prepared batch exactly as derived, then release the pin.
    pub async fn commit<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.commit_with_crash(database, cx, None).await
    }

    /// Commit through the production crash-point path, then release the pin
    /// regardless of whether the prepared write committed or was refused.
    pub async fn commit_with_crash<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &CommitCx,
        crash_at: Option<fgdb_chronicle::commit::CrashPoint>,
    ) -> Result<CommitSeq, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        if self.prepared.is_none() {
            self.release_pin();
            return Err(WriteTxnError::NoPreparedWrite);
        }
        let conflict = match self.read_conflict(database) {
            Ok(conflict) => conflict,
            Err(source) => {
                self.release_pin();
                return Err(WriteTxnError::Read(source));
            }
        };
        if let Some((element, committed_at)) = conflict {
            self.release_pin();
            return Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                detail: format!(
                    "read-set element {element:?} was written at {committed_at:?} after pinned basis {:?}",
                    self.basis
                ),
            }));
        }
        let prepared = self
            .prepared
            .take()
            .expect("the prepared write was checked immediately above");
        self.staged.clear();

        let result = database
            .commit_prepared_with_crash(cx, prepared, crash_at)
            .await
            .map_err(WriteTxnError::Write);
        self.release_pin();
        result
    }

    /// End the transaction without publishing its prepared batch.
    pub fn abort(mut self) {
        self.staged.clear();
        self.prepared = None;
        self.release_pin();
    }

    fn combined_batch(staged: &[WriteBatch]) -> Option<WriteBatch> {
        let mut batches = staged.iter().cloned();
        let mut combined = batches.next()?;
        for mut batch in batches {
            debug_assert_eq!(batch.relation, combined.relation);
            combined.rows.append(&mut batch.rows);
        }
        Some(combined)
    }

    fn overlay_property(
        props: &mut Vec<(fgdb_delta_types::PropertyKeyId, fgdb_types::CanonicalScalar)>,
        key: fgdb_delta_types::PropertyKeyId,
        value: Option<&fgdb_types::CanonicalScalar>,
    ) {
        match props.binary_search_by_key(&key, |(property, _)| *property) {
            Ok(at) => match value {
                Some(value) => props[at].1 = value.clone(),
                None => {
                    props.remove(at);
                }
            },
            Err(at) => {
                if let Some(value) = value {
                    props.insert(at, (key, value.clone()));
                }
            }
        }
    }

    fn read_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Option<(ElementId, CommitSeq)>, ReadError> {
        let read_set = self.read_set.borrow();
        let match_expansions = self.match_expansions.borrow();
        if read_set.is_empty() && match_expansions.is_empty() {
            return Ok(None);
        }
        for batch in database.delta_since(self.basis)? {
            let mut touched = std::collections::BTreeSet::new();
            let mut endpoints = std::collections::BTreeSet::new();
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    if let fgdb_delta_types::DeltaRow::CreateEdge {
                        eid,
                        src,
                        relation,
                        ..
                    } = row
                        && match_expansions.contains(&(*src, *relation))
                    {
                        return Ok(Some((ElementId::Edge(*eid), batch.commit_seq())));
                    }
                    crate::adjacency_endpoints(row, &mut endpoints);
                    crate::touched_elements(row, &mut touched);
                }
            }
            if let Some(element) = endpoints
                .into_iter()
                .find(|element| read_set.contains(element))
            {
                return Ok(Some((element, batch.commit_seq())));
            }
            if let Some(element) = touched.into_iter().find(|element| read_set.contains(element)) {
                return Ok(Some((element, batch.commit_seq())));
            }
        }
        Ok(None)
    }

    fn release_pin(&mut self) {
        if let Some(pin) = self.pin.take() {
            let _receipt = pin.abort();
        }
    }
}

impl core::fmt::Debug for WriteTxn {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WriteTxn")
            .field("basis", &self.basis)
            .field("staged_batches", &self.staged.len())
            .field("has_prepared_write", &self.prepared.is_some())
            .field("read_set_len", &self.read_set.borrow().len())
            .field(
                "match_expansion_count",
                &self.match_expansions.borrow().len(),
            )
            .field("pin_obligation", &self.pin.as_ref().map(PurposeObligation::id))
            .finish()
    }
}

impl Drop for WriteTxn {
    fn drop(&mut self) {
        self.release_pin();
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTxnError;
    use crate::{Database, DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts, VId};

    #[test]
    fn write_refuses_an_advanced_snapshot_without_preparing() {
        let ((), report) = run_async_under_lab(0x7a_10, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let baseline = txn_cx.outstanding_obligations();
            let directory = std::env::temp_dir().join(format!(
                "fgdb-write-txn-snapshot-advanced-{}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let mut database = Database::create(&commit, &directory, keys)
                .await
                .expect("database creates");
            let mut transaction = database.begin(&txn_cx).expect("transaction begins");
            let pinned = transaction.basis();

            let mut advancing = WriteBatch::new(RelationId(1));
            advancing.create_vertex(VId(1), Vec::new(), Vec::new());
            let live = database
                .write(&commit, advancing)
                .await
                .expect("autocommit advances the live frontier");

            let mut stale = WriteBatch::new(RelationId(1));
            stale.create_vertex(VId(2), Vec::new(), Vec::new());
            let error = transaction
                .write(&mut database, stale)
                .expect_err("a stale transaction cannot prepare against the live fold");
            assert!(matches!(
                error,
                WriteTxnError::SnapshotAdvanced {
                    pinned: error_pinned,
                    live: error_live,
                } if error_pinned == pinned && error_live == live
            ));
            assert!(
                transaction.staged.is_empty(),
                "snapshot refusal must not retain a staged batch"
            );
            assert!(
                transaction.prepared.is_none(),
                "snapshot refusal must not retain a prepared template"
            );
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline + 1,
                "snapshot refusal keeps the pin live until explicit abort"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), baseline);
        });

        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }
}
