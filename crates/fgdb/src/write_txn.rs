use crate::{
    Database, EdgeRecord, GqlError, PendingRow, PreparedWrite, ReadError, RelationBind, VertexRow,
    WriteBatch, WriteError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{
    Acquired, CommitCx, CommitSeq, EId, ObligationAcquireError, ObligationId, PurposeObligation,
    TxnCx, VId,
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
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Edge(eid));
        read_set.extend(observed_sources.into_iter().map(ElementId::Vertex));
        Ok(overlay)
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
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::DeleteVertex { .. }
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
        Ok(destinations.into_iter().collect())
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

        let basis_vertices = database.vertices_at(self.basis)?;
        let mut observed = std::collections::BTreeSet::new();
        let mut vertices: std::collections::BTreeSet<VId> = basis_vertices
            .into_iter()
            .map(|row| {
                observed.insert(ElementId::Vertex(row.vid));
                row.vid
            })
            .collect();
        let mut edges: std::collections::BTreeMap<
            fgdb_types::EId,
            (VId, RelationId, VId),
        > = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|row| {
                observed.insert(ElementId::Edge(row.entry.eid));
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

        let destinations = crate::execute_bound_plan_over(
            &plan,
            vertices.iter().copied(),
            |src, relation| {
                Ok(edges
                    .values()
                    .filter_map(|(edge_src, edge_relation, dst)| {
                        (*edge_src == src
                            && *edge_relation == relation
                            && vertices.contains(dst))
                        .then_some(*dst)
                    })
                    .collect())
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
            .extend(vertices.iter().copied().map(|src| (src, plan.relation)));
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
                    crate::touched_elements(row, &mut touched);
                }
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
