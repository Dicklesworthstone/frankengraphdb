//! Borrow the pinned generation and own only canonical changed rows. This is a
//! read adapter over the existing prepared net, not another intent evaluator.

use super::{Database, EdgeRecord, PendingRow, VertexRow, Vfs, WriteTxn, WriteTxnError};
use crate::gql_exec::source::{self, SourceEvent};
use fgdb_delta_types::{DeltaRow, ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::VertexScanDomain;
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

/// Private statement-lifetime owner. Neither the transaction nor database can
/// mutate while borrowed rows or the sparse replacements are in use. A missing
/// map entry borrows the basis; Some(None) is a tombstone, NEVER a basis fallback.
/// Payload ownership is proportional to changed identities, not graph size.
/// Historical source cursors and conservative conflict witnesses remain resident;
/// this does not promise a whole-process memory cap or physical I/O isolation.
pub(crate) struct OverlayRows<'a, V: Vfs + Clone> {
    transaction: &'a WriteTxn,
    database: &'a Database<V>,
    vertices: BTreeMap<VId, Option<VertexRow>>,
    edges: Option<BTreeMap<EId, Option<EdgeRecord>>>,
}

impl<'a, V: Vfs + Clone> OverlayRows<'a, V> {
    pub(crate) fn new(
        transaction: &'a WriteTxn,
        database: &'a Database<V>,
        read_edges: bool,
        poll: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Self, WriteTxnError> {
        transaction.ensure_database(database)?;
        poll()?;
        database.ensure_readable()?;
        database.snapshot.check_frontier(transaction.basis)?;
        let mut owner = Self {
            transaction,
            database,
            vertices: BTreeMap::new(),
            edges: read_edges.then(BTreeMap::new),
        };
        // Preserve negative dependencies erased by normalization, including
        // unused ensure aliases. Pending rows supply witnesses ONLY; their
        // mutation semantics are never replayed by this adapter.
        for batch in &transaction.staged {
            poll()?;
            for pending in &batch.rows {
                poll()?;
                let observed = match pending {
                    PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                        Some(ElementId::Vertex(*vid))
                    }
                    PendingRow::Edge { eid, .. }
                    | PendingRow::DeleteEdge { eid, .. }
                    | PendingRow::SetEdgeProperty { eid, .. }
                    | PendingRow::CompareAndSet {
                        elem: ElementId::Edge(eid),
                        ..
                    } if read_edges => Some(ElementId::Edge(*eid)),
                    _ => None,
                };
                if let Some(observed) = observed {
                    transaction.read_set.borrow_mut().insert(observed);
                }
            }
        }
        if let Some(prepared) = &transaction.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                poll()?;
                for effect in &coordinate.rows {
                    poll()?;
                    match effect {
                        DeltaRow::CreateVertex { vid, .. }
                        | DeltaRow::DeleteVertex { vid, .. }
                        | DeltaRow::LabelMembership { vid, .. }
                        | DeltaRow::Property {
                            elem: ElementId::Vertex(vid),
                            ..
                        } => {
                            transaction
                                .read_set
                                .borrow_mut()
                                .insert(ElementId::Vertex(*vid));
                            let row = match owner.vertices.entry(*vid) {
                                Entry::Occupied(entry) => entry.into_mut(),
                                Entry::Vacant(entry) => {
                                    poll()?;
                                    // Birth and retirement replace the whole image;
                                    // do not copy a payload that will be discarded.
                                    let basis = if matches!(
                                        effect,
                                        DeltaRow::CreateVertex { .. }
                                            | DeltaRow::DeleteVertex { .. }
                                    ) {
                                        None
                                    } else {
                                        database.vertex_at(*vid, transaction.basis)?
                                    };
                                    entry.insert(basis)
                                }
                            };
                            transaction.apply_vertex_effect(*vid, row, effect);
                        }
                        _ => {}
                    }
                    let Some(edges) = owner.edges.as_mut() else {
                        continue;
                    };
                    match effect {
                        DeltaRow::CreateEdge { eid, .. }
                        | DeltaRow::DeleteEdge { eid, .. }
                        | DeltaRow::Property {
                            elem: ElementId::Edge(eid),
                            ..
                        } => {
                            transaction
                                .read_set
                                .borrow_mut()
                                .insert(ElementId::Edge(*eid));
                            let row = match edges.entry(*eid) {
                                Entry::Occupied(entry) => entry.into_mut(),
                                Entry::Vacant(entry) => {
                                    poll()?;
                                    let basis = if matches!(
                                        effect,
                                        DeltaRow::CreateEdge { .. } | DeltaRow::DeleteEdge { .. }
                                    ) {
                                        None
                                    } else {
                                        database.edge_at(*eid, transaction.basis)?
                                    };
                                    if let Some(record) = &basis {
                                        transaction
                                            .read_set
                                            .borrow_mut()
                                            .insert(ElementId::Vertex(record.entry.src));
                                    }
                                    entry.insert(basis)
                                }
                            };
                            if let Some(source) = transaction.apply_edge_effect(*eid, row, effect) {
                                transaction
                                    .read_set
                                    .borrow_mut()
                                    .insert(ElementId::Vertex(source));
                            }
                        }
                        DeltaRow::DeleteVertex {
                            vid,
                            sorted_retired_incident_edges,
                            ..
                        } => {
                            for eid in sorted_retired_incident_edges {
                                poll()?;
                                // The canonical cascade image is already exact.
                                // Retain its source witness even after an earlier
                                // explicit edge delete. No incidence rescan occurs.
                                let mut reads = transaction.read_set.borrow_mut();
                                reads.insert(ElementId::Edge(*eid));
                                reads.insert(ElementId::Vertex(*vid));
                                drop(reads);
                                let row = edges.entry(*eid).or_insert(None);
                                transaction.apply_edge_effect(*eid, row, effect);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        poll()?;
        Ok(owner)
    }

    /// Merge replacements and borrowed historical winners in identity order.
    /// A refusal stops before the next output and retains the scan witness.
    /// The caller decides which events are polls versus charged admissions;
    /// authorized callers must NOT bill physical traversal of hidden records.
    ///
    /// `domain` is the plan's vertex scan domain. A basis vertex enters the
    /// read set only if the domain admits its basis or its staged image, and
    /// the phantom witness matches: the label for a labelled root, nothing
    /// when no vertex is scanned (the edge reads witness every bound vertex),
    /// the whole vertex table for `All` (fgdb-h1d6l). Every vertex is still
    /// visited and offered. `All` is the incumbent's witness exactly.
    pub(crate) fn visit_vertices<'s, E, C>(
        &'s self,
        domain: VertexScanDomain,
        control: &mut C,
        mut visit: impl FnMut(&'s VertexRow, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        match domain {
            VertexScanDomain::All => self.transaction.scanned_vertices.set(true),
            VertexScanDomain::Label(label) => {
                self.transaction
                    .scanned_vertex_labels
                    .borrow_mut()
                    .insert(label);
            }
            VertexScanDomain::Unscanned => {}
        }
        let mut replacements = self.vertices.iter().peekable();
        source::visit_vertices(
            &self.database.snapshot.patches,
            self.transaction.basis,
            control,
            |basis, control| {
                control(SourceEvent::Work)?;
                if domain.admits(&basis.labels)
                    || self
                        .vertices
                        .get(&basis.vid)
                        .and_then(Option::as_ref)
                        .is_some_and(|staged| domain.admits(&staged.labels))
                {
                    self.transaction
                        .read_set
                        .borrow_mut()
                        .insert(ElementId::Vertex(basis.vid));
                }
                while replacements
                    .peek()
                    .is_some_and(|(vid, _)| **vid < basis.vid)
                {
                    let (_, row) = replacements.next().expect("peeked sparse vertex");
                    if let Some(row) = row {
                        control(SourceEvent::Work)?;
                        visit(row, control)?;
                    }
                }
                let row = if replacements
                    .peek()
                    .is_some_and(|(vid, _)| **vid == basis.vid)
                {
                    replacements
                        .next()
                        .expect("matching sparse vertex")
                        .1
                        .as_ref()
                } else {
                    Some(basis)
                };
                if let Some(row) = row {
                    visit(row, control)?;
                }
                Ok(())
            },
        )?;
        for (_, row) in replacements {
            control(SourceEvent::Work)?;
            if let Some(row) = row {
                visit(row, control)?;
            }
        }
        Ok(())
    }

    /// None means this owner was deliberately prepared for vertex-only reads.
    /// Callers requiring edges must reject that case rather than using the basis
    /// without its staged changes. Returned properties remain borrowed.
    ///
    /// `relations` names every relation the caller's plan reads. Only their
    /// edges enter the read set, with BOTH endpoints, because a narrowed vertex
    /// scan no longer records every vertex (fgdb-h1d6l). The phantom witness
    /// is those relations, not the whole edge table
    /// (fgdb-whole-edge-read-flag-4qe1z). Every edge is still visited and
    /// offered. `None` keeps the whole-table witness, edge and source only.
    pub(crate) fn visit_edges<'s, E, C>(
        &'s self,
        relations: Option<&BTreeSet<RelationId>>,
        control: &mut C,
        mut visit: impl FnMut(
            &'s AdjacencyEntry,
            &'s [(PropertyKeyId, CanonicalScalar)],
            &mut C,
        ) -> Result<(), E>,
    ) -> Option<Result<(), E>>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        let edges = self.edges.as_ref()?;
        // Records `entry` as read when the plan reads its relation.
        let observe = |entry: &AdjacencyEntry| {
            let mut reads = self.transaction.read_set.borrow_mut();
            if relations.is_some_and(|read| !read.contains(&entry.relation)) {
                return false;
            }
            reads.insert(ElementId::Edge(entry.eid));
            reads.insert(ElementId::Vertex(entry.src));
            reads.insert(ElementId::Vertex(entry.dst));
            true
        };
        Some((|| {
            match relations {
                None => self.transaction.scanned_edges.set(true),
                Some(read) => self
                    .transaction
                    .scanned_edge_relations
                    .borrow_mut()
                    .extend(read.iter().copied()),
            }
            let mut replacements = edges.iter().peekable();
            let mut emit = |entry: &'s AdjacencyEntry,
                            properties: &'s [(PropertyKeyId, CanonicalScalar)],
                            control: &mut C| {
                if observe(entry) {
                    self.transaction
                        .match_expansions
                        .borrow_mut()
                        .insert((entry.src, entry.relation));
                }
                visit(entry, properties, control)
            };
            self.database.snapshot.visit_indexed_edges(
                self.transaction.basis,
                control,
                |basis, properties, control| {
                    control(SourceEvent::Work)?;
                    // Observe the original source even when the winner is a
                    // tombstone or is replaced before being offered downstream.
                    observe(basis);
                    while replacements
                        .peek()
                        .is_some_and(|(eid, _)| **eid < basis.eid)
                    {
                        let (_, row) = replacements.next().expect("peeked sparse edge");
                        if let Some(row) = row {
                            control(SourceEvent::Work)?;
                            emit(&row.entry, &row.props, control)?;
                        }
                    }
                    if replacements
                        .peek()
                        .is_some_and(|(eid, _)| **eid == basis.eid)
                    {
                        if let Some(row) = replacements.next().expect("matching sparse edge").1 {
                            emit(&row.entry, &row.props, control)?;
                        }
                    } else {
                        emit(basis, properties, control)?;
                    }
                    Ok(())
                },
            )?;
            for (_, row) in replacements {
                control(SourceEvent::Work)?;
                if let Some(row) = row {
                    emit(&row.entry, &row.props, control)?;
                }
            }
            Ok(())
        })())
    }
}

#[cfg(test)]
#[path = "overlay_scan_tests.rs"]
mod tests;
