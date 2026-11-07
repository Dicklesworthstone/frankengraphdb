//! Source-ordered relation composition using the existing mutation evaluator.
//!
//! Vertex instructions form a shared ordered backbone. Each immutable edge
//! identity belongs to one relation slice; that slice retains every instruction
//! for the edge and every vertex instruction, at their original positions.
//! The ordinary evaluator therefore sees the same endpoint lifetimes, guards,
//! property prefixes and cascades as the full program. Nothing is hoisted.
//!
//! This is a decomposition law, not a second interpreter or a shadow database.
//! Every slice runs build_write_template via prepare_write_checked. Normalized
//! vertex effects must agree exactly across slices and are published once in
//! the least coordinate. Birth ordinals are restored to original raw visits.

use super::{PreparedDependencies, RawOrderedSummary};
use crate::gql_exec::source::IndexMap;
use crate::{Database, PendingRow, PreparedWrite, WriteBatch, WriteError, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{CoordinateEntry, DeltaRow, ElementId, LogicalDeltaTemplate, RelationId};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::{CommitCx, CommitSeq, EId, MergeEvalCx};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Ordinary preparation builds declarations once; retained ordered prefixes
/// share immutable search paths instead of copying the entire declaration map.
pub(super) enum EdgeOwners {
    Flat(BTreeMap<EId, RelationId>),
    Shared(IndexMap<EId, RelationId>),
}

impl EdgeOwners {
    pub(super) fn get(&self, eid: &EId) -> Option<&RelationId> {
        match self {
            Self::Flat(owners) => owners.get(eid),
            Self::Shared(owners) => owners.get(eid),
        }
    }

    pub(super) fn into_shared(self) -> Option<IndexMap<EId, RelationId>> {
        match self {
            Self::Shared(owners) => Some(owners),
            Self::Flat(owners) if owners.len() <= u32::MAX as usize => {
                Some(IndexMap::from_sorted(owners.into_iter().collect(), &mut 0))
            }
            Self::Flat(_) => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct DeclarationConflict<K, R> {
    identity: K,
    first: R,
    second: R,
}

/// Admit only incoming declarations while sharing every unchanged search path.
/// The supplied basis lookup is the same one used by complete preparation.
fn extend_shared_declarations<K: Ord + Clone, R: Copy + Eq>(
    previous: &IndexMap<K, R>,
    declarations: impl IntoIterator<Item = (K, R)>,
    mut live_owner: impl FnMut(&K) -> Option<R>,
) -> Result<Option<IndexMap<K, R>>, DeclarationConflict<K, R>> {
    let mut owners = previous.clone();
    for (identity, relation) in declarations {
        let known = live_owner(&identity);
        let retained = owners.get(&identity).copied();
        let owner = retained.or(known).unwrap_or(relation);
        if owner != relation {
            return Err(DeclarationConflict {
                identity,
                first: owner,
                second: relation,
            });
        }
        if retained.is_none() {
            // The AVL's compact length is a representation bound, not a new
            // transaction limit. Request the unchanged complete preparation.
            if owners.len() == u32::MAX as usize {
                return Ok(None);
            }
            owners = owners.insert(identity, owner, &mut 0);
        }
    }
    Ok(Some(owners))
}

impl RawOrderedSummary {
    /// Admit declarations before routing any suffix row, exactly as the full
    /// ordered path does. Earlier raw targets were resolved at this same basis;
    /// only new declarations can introduce an ownership conflict.
    pub(super) fn extend_declarations(
        &self,
        writer: &BlockWriter,
        batches: &[WriteBatch],
    ) -> Result<Option<EdgeOwners>, WriteTxnError> {
        let declarations = batches.iter().flat_map(|batch| {
            batch.rows.iter().filter_map(move |pending| match pending {
                PendingRow::Edge { eid, .. } => Some((*eid, batch.relation)),
                _ => None,
            })
        });
        extend_shared_declarations(&self.declarations, declarations, |eid| {
            writer.live_edge(*eid).map(|(_, relation, _, _)| relation)
        })
        .map(|owners| owners.map(EdgeOwners::Shared))
        .map_err(|conflict| WriteTxnError::AtomicRelationConflict {
            first: conflict.first,
            second: conflict.second,
            element: ElementId::Edge(conflict.identity),
        })
    }
}

/// Fixed edge ownership, including declarations whose effects later cancel.
/// A request identity cannot be declared under two relation types in one
/// ordered program. Reject even ambiguous ENSURE declarations: their requested
/// ID may be unused, but must never authorize a cross-type identity collision.
pub(super) fn edge_owners<'a>(
    writer: &BlockWriter,
    batches: impl IntoIterator<Item = &'a WriteBatch>,
) -> Result<EdgeOwners, WriteTxnError> {
    let mut owners = BTreeMap::new();
    for batch in batches {
        for pending in &batch.rows {
            if let PendingRow::Edge { eid, .. } = pending {
                let known = writer.live_edge(*eid).map(|(_, relation, _, _)| relation);
                let owner = owners
                    .entry(*eid)
                    .or_insert(known.unwrap_or(batch.relation));
                if *owner != batch.relation {
                    return Err(WriteTxnError::AtomicRelationConflict {
                        first: *owner,
                        second: batch.relation,
                        element: ElementId::Edge(*eid),
                    });
                }
            }
        }
    }
    Ok(EdgeOwners::Flat(owners))
}

/// Only edge creation takes its type from the enclosing batch. Existing edge
/// mutations are identity-addressed; putting one in another batch must not
/// move it to that batch's relation or lose its earlier same-program writes.
pub(super) fn route(
    writer: &BlockWriter,
    declarations: &EdgeOwners,
    relation: RelationId,
    pending: &PendingRow,
) -> Option<RelationId> {
    route_resolved(writer, declarations, relation, pending).0
}

/// The boolean distinguishes a stable identity owner from the enclosing
/// relation fallback. A later edge declaration can reroute the latter, even
/// when its original instruction was a conditional no-op.
pub(super) fn route_resolved(
    writer: &BlockWriter,
    declarations: &EdgeOwners,
    relation: RelationId,
    pending: &PendingRow,
) -> (Option<RelationId>, bool) {
    let eid = match pending {
        PendingRow::Edge { .. } => return (Some(relation), true),
        PendingRow::DeleteEdge { eid, .. } | PendingRow::SetEdgeProperty { eid, .. } => *eid,
        PendingRow::CompareAndSet {
            elem: ElementId::Edge(eid),
            ..
        } => *eid,
        PendingRow::Vertex { .. }
        | PendingRow::DeleteVertex { .. }
        | PendingRow::SetLabel { .. }
        | PendingRow::SetProperty { .. }
        | PendingRow::CompareAndSet {
            elem: ElementId::Vertex(_),
            ..
        } => return (None, true),
    };
    let owner = declarations
        .get(&eid)
        .copied()
        .or_else(|| writer.live_edge(eid).map(|(_, relation, _, _)| relation));
    // Missing targets stay in a real slice, where the existing evaluator
    // distinguishes an optional no-op from UnknownEdge or a failed guard.
    (Some(owner.unwrap_or(relation)), owner.is_some())
}

pub(super) fn vertex_effect(row: &DeltaRow) -> Result<bool, WriteTxnError> {
    match row {
        DeltaRow::CreateVertex { .. }
        | DeltaRow::DeleteVertex { .. }
        | DeltaRow::LabelMembership { .. }
        | DeltaRow::Property {
            elem: ElementId::Vertex(_),
            ..
        } => Ok(true),
        DeltaRow::CreateEdge { .. }
        | DeltaRow::DeleteEdge { .. }
        | DeltaRow::Property {
            elem: ElementId::Edge(_),
            ..
        } => Ok(false),
        // A new mutation family needs a decomposition law before admission.
        _ => Err(WriteTxnError::UnsupportedAtomicMutation),
    }
}

pub(super) fn restore_ordinal(row: &mut DeltaRow, visits: &[u64]) -> Result<(), WriteTxnError> {
    if let DeltaRow::CreateVertex { birth_ordinal, .. }
    | DeltaRow::CreateEdge { birth_ordinal, .. } = row
    {
        let index = birth_ordinal
            .checked_sub(1)
            .and_then(|ordinal| usize::try_from(ordinal).ok())
            .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
        *birth_ordinal = *visits
            .get(index)
            .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
    }
    Ok(())
}

fn admit_expanded_rows(limit: Option<u64>, required: u128) -> Result<(), WriteTxnError> {
    if let Some(limit) = limit
        && required > u128::from(limit)
    {
        return Err(WriteTxnError::OrderedWriteBudgetExceeded { limit, required });
    }
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare dependent, source-ordered writes spanning edge relations.
    ///
    /// Unlike prepare_atomic_writes' independent groups, later instructions
    /// observe earlier vertex and edge mutations across batch boundaries.
    /// Create/update/CAS/delete/ensure use the same evaluator as a single batch.
    /// No intermediate capsule, sequence, snapshot or durable object is made.
    /// The returned PreparedWrite retains all original-basis dependencies and
    /// commits through the ordinary FCW and Chronicle publication path.
    ///
    /// Successful effects obey input order, not relation-ID order. Error
    /// selection across independent slices is deterministic but does not claim
    /// to identify the earliest failing source instruction. An EId requested by
    /// a creation/ensure must have one declared relation throughout the program,
    /// including cancelled declarations and otherwise-unused ensure IDs.
    /// Existing edge updates/deletes/CAS route by the edge's real identity.
    ///
    /// This bounded decomposition copies the vertex instruction backbone per
    /// touched relation. It is not spill, arbitrary future intent semantics,
    /// cross-graph transactions or SSI. The independent-group API is unchanged.
    pub fn prepare_ordered_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.ensure_writable()?;
        if batches.is_empty() || batches.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        let first = batches[0].relation;
        if batches.iter().all(|batch| batch.relation == first) {
            // Preserve the original one-relation template byte-for-byte,
            // including existing identity-addressed foreign-edge mutations.
            let mut combined = WriteBatch::new(first);
            for batch in batches {
                combined.extend(batch)?;
            }
            return self.prepare_write_checked(combined).map_err(Into::into);
        }
        let declarations = edge_owners(&self.writer, &batches)?;
        let mut relations = BTreeSet::new();
        let mut batch_relations = BTreeSet::new();
        let mut edge_relations = BTreeSet::new();
        let mut routed = Vec::new();
        let mut ordinal = 0_u64;
        let mut vertices = 0_u64;
        let mut stable_routing = true;
        for batch in &batches {
            relations.insert(batch.relation);
            batch_relations.insert(batch.relation);
            for pending in &batch.rows {
                ordinal = ordinal
                    .checked_add(1)
                    .ok_or(WriteTxnError::AtomicOrdinalOverflow)?;
                let (target, resolved) =
                    route_resolved(&self.writer, &declarations, batch.relation, pending);
                stable_routing &= resolved && super::incremental::supported(pending);
                if let Some(relation) = target {
                    relations.insert(relation);
                    edge_relations.insert(relation);
                } else {
                    vertices += 1;
                }
                routed.push((ordinal, target, pending));
            }
        }
        let mut coordinates: BTreeMap<RelationId, CoordinateEntry> = BTreeMap::new();
        let mut shared_vertices: Option<Vec<DeltaRow>> = None;
        let mut dependencies = PreparedDependencies::default();
        let mut replay_row_allowance = Some(0_u64);
        for relation in relations {
            let mut slice = WriteBatch::new(relation);
            let mut visits = Vec::new();
            for &(ordinal, target, pending) in &routed {
                if target.is_none_or(|target| target == relation) {
                    slice.rows.push(pending.clone());
                    visits.push(ordinal);
                }
            }
            if slice.is_empty() {
                continue;
            }
            let prepared = self.prepare_write_checked(slice)?;
            // Count the actual native evaluator inputs, including the vertex
            // backbone repeated in each nonempty relation slice. Never infer
            // a replay allowance from the smaller normalized effect template.
            replay_row_allowance = replay_row_allowance
                .and_then(|total| total.checked_add(prepared.replay_row_allowance?));
            let [coordinate] = prepared.template.coordinate_entries() else {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            };
            if coordinate.relation != relation {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            }
            if let Some(previous) = coordinates.values().next()
                && (previous.graph != coordinate.graph
                    || previous.branch != coordinate.branch
                    || previous.schema_epoch != coordinate.schema_epoch
                    || previous.schema_transition != coordinate.schema_transition)
            {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            }
            let mut coordinate = coordinate.clone();
            let mut vertices = Vec::new();
            let mut edges = Vec::new();
            for mut row in coordinate.rows {
                restore_ordinal(&mut row, &visits)?;
                if vertex_effect(&row)? {
                    vertices.push(row);
                } else {
                    edges.push(row);
                }
            }
            match &shared_vertices {
                Some(expected) if *expected != vertices => {
                    // In particular the same normalized cascade must name the
                    // same BASIS edges in every slice. Same-program incident
                    // creates are cancelled by the existing NENF before here.
                    return Err(WriteTxnError::UnsupportedAtomicMutation);
                }
                Some(_) => {}
                None => shared_vertices = Some(vertices),
            }
            coordinate.rows = edges;
            coordinates.insert(relation, coordinate);
            dependencies.extend(prepared.dependencies);
        }
        // Every new endpoint is born before any later coordinate's edges.
        // Vertex deletes also precede later coordinates; each slice's NENF has
        // already absorbed ALL effects on cascaded edges, so no update can
        // resurrect an edge retired by this single shared delete payload.
        let Some((_, owner)) = coordinates.first_key_value() else {
            return Err(WriteTxnError::UnsupportedAtomicMutation);
        };
        let owner_relation = owner.relation;
        coordinates
            .get_mut(&owner_relation)
            .expect("nonempty coordinate map")
            .rows
            .extend(shared_vertices.unwrap_or_default());
        let template = LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(),
            [0_u8; 32],
            coordinates.into_values().collect(),
        )
        .map_err(WriteError::Canonical)?;
        let raw_ordered = if stable_routing {
            declarations
                .into_shared()
                .map(|declarations| RawOrderedSummary {
                    batches: batches.len(),
                    visits: ordinal,
                    vertices,
                    batch_relations,
                    edge_relations,
                    declarations,
                })
        } else {
            None
        };
        Ok(PreparedWrite {
            template,
            basis: self.snapshot.frontier,
            handle_owner: Arc::clone(&self.handle_owner),
            dependencies,
            replay_row_allowance,
            raw_input: None,
            raw_ordered,
            authorization_decision_digest: None,
        })
    }

    /// Prepare ordered writes with a limit on decomposed evaluator-input rows.
    ///
    /// A single-relation program charges each input row once. A mixed program
    /// charges each edge instruction once and each vertex instruction once per
    /// relation slice, including slices introduced by identity-addressed writes
    /// to existing edges. Conditional no-ops count before normalization.
    /// Equality with the limit succeeds. Refusal precedes cloning any relation
    /// slice or evaluating its mutations, and never publishes a partial write.
    ///
    /// Input-count admission precedes routing allocations and storage lookups;
    /// exact expansion admission follows routing, before mutation evaluation.
    /// Ownership conflicts can therefore precede the expansion-budget error.
    /// Both admitted paths use the same evaluator, dependencies and template as
    /// `prepare_ordered_writes`; this method does not truncate the program.
    ///
    /// This governs row replication, not variable-size payload bytes, storage
    /// scans (including cascades/ensure), total CPU work, cancellation or spill.
    /// The caller already owns the input allocation. It is not a memory quota.
    pub fn prepare_ordered_writes_bounded(
        &mut self,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.ensure_writable()?;
        self.admit_ordered_write_rows(batches.iter(), max_expanded_rows)?;
        self.prepare_ordered_writes(batches)
    }

    /// Deterministic intent replay for the rebase finalizers (FG-INV-17,
    /// doctrine #3). This is the same evaluator and admission as
    /// [`Self::prepare_ordered_writes_bounded`]. It runs only under the
    /// capability-empty [`MergeEvalCx`], never a commit, transaction or query
    /// context, so a rebase cannot re-evaluate with clock, entropy, I/O or
    /// spawn authority. The runtime mask blocks mask-aware ambient effects
    /// while it runs. See [`MergeEvalCx::with_restriction`] for what the pinned
    /// runtime's mask does not cover.
    ///
    /// ```compile_fail,E0308
    /// # use fgdb::{Database, MemVfs, WriteBatch};
    /// # use fgdb_types::CommitCx;
    /// fn replay_under_a_commit_context(db: &mut Database<MemVfs>, commit: &CommitCx) {
    ///     let _ = db.prepare_ordered_writes_replay(commit, Vec::<WriteBatch>::new(), 1);
    /// }
    /// ```
    pub fn prepare_ordered_writes_replay(
        &mut self,
        merge: &MergeEvalCx,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
    ) -> Result<PreparedWrite, WriteTxnError> {
        merge.with_restriction(|| self.prepare_ordered_writes_bounded(batches, max_expanded_rows))
    }

    /// Borrow the complete program so transaction admission does not first
    /// clone an arbitrarily large staged prefix. Callers admit owner/health
    /// before entering. Routing shares the evaluator's ownership law; this
    /// preflight never interprets a mutation or clones its scalar payload.
    pub(crate) fn admit_ordered_write_rows<'a>(
        &self,
        batches: impl Iterator<Item = &'a WriteBatch> + Clone,
        max_expanded_rows: u64,
    ) -> Result<(), WriteTxnError> {
        let Some(first) = batches.clone().next() else {
            return Err(WriteError::EmptyBatch.into());
        };
        if batches.clone().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        let input_rows: u128 = batches.clone().map(|batch| batch.rows.len() as u128).sum();
        admit_expanded_rows(Some(max_expanded_rows), input_rows)?;
        if batches
            .clone()
            .all(|batch| batch.relation == first.relation)
        {
            return Ok(());
        }
        let declarations = edge_owners(&self.writer, batches.clone())?;
        let mut relations = BTreeSet::new();
        let mut vertex_rows = 0_u128;
        for batch in batches {
            relations.insert(batch.relation);
            for pending in &batch.rows {
                match route(&self.writer, &declarations, batch.relation, pending) {
                    Some(relation) => {
                        relations.insert(relation);
                    }
                    None => vertex_rows += 1,
                }
            }
        }
        let expanded_rows = input_rows - vertex_rows + vertex_rows * relations.len() as u128;
        admit_expanded_rows(Some(max_expanded_rows), expanded_rows)
    }

    /// Reuse exact raw-prefix counts and stable identity routing for bounded
    /// transaction admission. False requests the unchanged complete preflight.
    /// The raw-row limit still precedes ownership lookups and expansion checks.
    pub(crate) fn admit_cached_ordered_write_suffix(
        &self,
        previous: &PreparedWrite,
        staged_batches: usize,
        suffix: &[WriteBatch],
        max_expanded_rows: u64,
    ) -> Result<bool, WriteTxnError> {
        let Some(summary) = previous
            .raw_ordered
            .as_ref()
            .filter(|summary| summary.batches == staged_batches)
        else {
            return Ok(false);
        };
        if !Arc::ptr_eq(&self.handle_owner, &previous.handle_owner)
            || previous.basis != self.snapshot.frontier
        {
            return Ok(false);
        }
        if suffix.is_empty() || suffix.iter().any(WriteBatch::is_empty) {
            return Err(WriteError::EmptyBatch.into());
        }
        let input_rows = u128::from(summary.visits)
            + suffix
                .iter()
                .map(|batch| batch.rows.len() as u128)
                .sum::<u128>();
        admit_expanded_rows(Some(max_expanded_rows), input_rows)?;
        let Some(declarations) = summary.extend_declarations(&self.writer, suffix)? else {
            return Ok(false);
        };
        let mut relations = summary.batch_relations.clone();
        relations.extend(&summary.edge_relations);
        let mut vertices = u128::from(summary.vertices);
        for batch in suffix {
            relations.insert(batch.relation);
            for pending in &batch.rows {
                match route(&self.writer, &declarations, batch.relation, pending) {
                    Some(relation) => {
                        relations.insert(relation);
                    }
                    None => vertices += 1,
                }
            }
        }
        let expanded_rows = input_rows - vertices + vertices * relations.len() as u128;
        admit_expanded_rows(Some(max_expanded_rows), expanded_rows)?;
        Ok(true)
    }

    /// Publish a dependent ordered program as ONE commit. Preparation never
    /// publishes a prefix, and commit retains the normal unknown-outcome fence.
    pub async fn write_ordered(
        &mut self,
        cx: &CommitCx,
        batches: Vec<WriteBatch>,
    ) -> Result<CommitSeq, WriteTxnError> {
        let prepared = self.prepare_ordered_writes(batches)?;
        self.commit_prepared(cx, prepared).await.map_err(Into::into)
    }

    /// Admit all expanded input rows before publishing one ordered commit.
    /// The limit has the same scope as `prepare_ordered_writes_bounded`.
    pub async fn write_ordered_bounded(
        &mut self,
        cx: &CommitCx,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        let prepared = self.prepare_ordered_writes_bounded(batches, max_expanded_rows)?;
        self.commit_prepared(cx, prepared).await.map_err(Into::into)
    }
}

impl PreparedWrite {
    /// Keep every birth ordinal the previously staged prefix already exposed
    /// when an ordered suffix re-prepares it.
    ///
    /// Ordered preparation assigns births in source order, which can permute
    /// births a prefix staged as independent groups (relation order) already
    /// exposed. Observed ordinals are part of the transaction's visible state,
    /// so under ordered composition a created element keeps the ordinal it was
    /// first seen with. The whole prefix precedes the suffix in source order,
    /// so it re-occupies the same ordinal set and new births are unaffected;
    /// a collision would mean that premise broke, and it refuses rather than
    /// publishing ambiguous births.
    ///
    /// Only ordered composition calls this. Atomic groups are canonical by
    /// design: relation order assigns births on every call, so a new group
    /// that sorts first moves the births of later relations (atomic_txn's
    /// point_bulk_and_committed_vertices_use_the_same_canonical_births_and_values).
    ///
    /// Restores the law ebabc3ae introduced, at the call site it used, and
    /// that d8ce9e90 silently reverted (fgdb-write-ordered-silent-revert-2d80i).
    pub(crate) fn retain_birth_ordinals(
        mut self,
        previous: Option<&Self>,
    ) -> Result<Self, WriteTxnError> {
        let Some(previous) = previous else {
            return Ok(self);
        };
        if !Arc::ptr_eq(&self.handle_owner, &previous.handle_owner) {
            return Err(WriteTxnError::WrongDatabase);
        }
        if self.basis != previous.basis {
            return Err(WriteTxnError::SnapshotAdvanced {
                pinned: previous.basis,
                live: self.basis,
            });
        }
        let births: BTreeMap<ElementId, u64> = previous
            .template
            .coordinate_entries()
            .iter()
            .flat_map(|coordinate| &coordinate.rows)
            .filter_map(birth)
            .collect();
        if births.is_empty() {
            return Ok(self);
        }
        let mut coordinates = self.template.coordinate_entries().to_vec();
        let mut changed = false;
        for row in coordinates
            .iter_mut()
            .flat_map(|coordinate| &mut coordinate.rows)
        {
            let (identity, ordinal) = match row {
                DeltaRow::CreateVertex {
                    vid, birth_ordinal, ..
                } => (ElementId::Vertex(*vid), birth_ordinal),
                DeltaRow::CreateEdge {
                    eid, birth_ordinal, ..
                } => (ElementId::Edge(*eid), birth_ordinal),
                _ => continue,
            };
            if let Some(retained) = births.get(&identity) {
                changed |= *ordinal != *retained;
                *ordinal = *retained;
            }
        }
        if !changed {
            return Ok(self);
        }
        let mut seen = BTreeSet::new();
        let unique = coordinates
            .iter()
            .flat_map(|coordinate| &coordinate.rows)
            .filter_map(birth)
            .all(|(_, ordinal)| seen.insert(ordinal));
        if !unique {
            return Err(WriteTxnError::BirthOrdinalCollision);
        }
        let intent = self.template.intent_semantics_oid();
        let source = *self.template.source_intent_root_digest();
        self.template = LogicalDeltaTemplate::build(intent, source, coordinates)
            .map_err(WriteError::Canonical)?;
        Ok(self)
    }
}

/// The element a create row births, with its ordinal.
fn birth(row: &DeltaRow) -> Option<(ElementId, u64)> {
    match row {
        DeltaRow::CreateVertex {
            vid, birth_ordinal, ..
        } => Some((ElementId::Vertex(*vid), *birth_ordinal)),
        DeltaRow::CreateEdge {
            eid, birth_ordinal, ..
        } => Some((ElementId::Edge(*eid), *birth_ordinal)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

    const P: PropertyKeyId = PropertyKeyId(1);

    async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(0))]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.unwrap();
        db
    }

    fn program() -> Vec<WriteBatch> {
        let mut first = WriteBatch::new(RelationId(9));
        first.create_vertex(VId(5), vec![], vec![]);
        first.add_edge(EId(50), VId(1), VId(5), vec![]);
        let mut second = WriteBatch::new(RelationId(2));
        second.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(7)));
        second.add_edge(EId(60), VId(5), VId(2), vec![]);
        vec![first, second]
    }

    #[test]
    fn exact_expansion_boundary_preserves_template_and_publishes_once() {
        let ((), report) = run_async_under_lab(0x6f62_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let expected = db.prepare_ordered_writes(program()).unwrap();
            for (limit, required) in [(0, 4), (3, 4), (4, 6), (5, 6)] {
                assert!(matches!(
                    db.prepare_ordered_writes_bounded(program(), limit),
                    Err(WriteTxnError::OrderedWriteBudgetExceeded {
                        limit: actual_limit,
                        required: actual_required,
                    }) if actual_limit == limit && actual_required == required
                ));
                assert_eq!(db.frontier().unwrap(), basis);
                assert!(db.vertex(VId(5)).unwrap().is_none());
            }
            for limit in [6, 7, u64::MAX] {
                let admitted = db.prepare_ordered_writes_bounded(program(), limit).unwrap();
                assert_eq!(admitted.template, expected.template);
                assert_eq!(admitted.basis(), expected.basis());
            }
            let seq = db.write_ordered_bounded(&cx, program(), 6).await.unwrap();
            assert_eq!(seq, CommitSeq(basis.0 + 1));
            assert_eq!(db.delta_since(basis).unwrap().count(), 1);
            assert_eq!(
                db.vertex(VId(5)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(7).into())]
            );
            assert_eq!(
                db.edge(EId(50)).unwrap().unwrap().entry.relation,
                RelationId(9)
            );
            assert_eq!(
                db.edge(EId(60)).unwrap().unwrap().entry.relation,
                RelationId(2)
            );
            assert!(pinned.vertex(VId(5)).unwrap().is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn routed_existing_edge_relation_and_noops_are_charged() {
        let ((), report) = run_async_under_lab(0x6f62_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let mut vertex = WriteBatch::new(RelationId(9));
            vertex.ensure_vertex(VId(1), vec![], vec![]);
            let mut edge = WriteBatch::new(RelationId(2));
            edge.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(4)));
            let batches = vec![vertex, edge];
            // Relation 1 is not a declared batch type but needs its own slice.
            // The no-op vertex instruction is repeated in all three slices.
            assert!(matches!(
                db.prepare_ordered_writes_bounded(batches.clone(), 3),
                Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 3,
                    required: 4,
                })
            ));
            let expected = db.prepare_ordered_writes(batches.clone()).unwrap();
            let admitted = db.prepare_ordered_writes_bounded(batches, 4).unwrap();
            assert_eq!(admitted.template, expected.template);

            let mut first = WriteBatch::new(RelationId(9));
            first.ensure_vertex(VId(1), vec![], vec![]);
            let mut second = WriteBatch::new(RelationId(2));
            second.ensure_vertex(VId(2), vec![], vec![]);
            assert!(matches!(
                db.prepare_ordered_writes_bounded(vec![first.clone(), second.clone()], 3),
                Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 3,
                    required: 4,
                })
            ));
            let admitted = db
                .prepare_ordered_writes_bounded(vec![first, second], 4)
                .unwrap();
            assert!(
                admitted
                    .template
                    .coordinate_entries()
                    .iter()
                    .all(|entry| entry.rows.is_empty())
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn budget_refusal_precedes_guard_evaluation_and_preserves_live_state() {
        let ((), report) = run_async_under_lab(0x6f62_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.compare_and_set_vertex_property(
                VId(1),
                P,
                Some(CanonicalScalar::Int(999)),
                CanonicalScalar::Int(8),
                WriteMismatchPolicy::AbortWrite,
            );
            let mut second = WriteBatch::new(RelationId(2));
            second.add_edge(EId(60), VId(1), VId(2), vec![]);
            let batches = vec![first, second];
            assert!(matches!(
                db.write_ordered_bounded(&cx, batches.clone(), 2).await,
                Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 2,
                    required: 3,
                })
            ));
            assert!(matches!(
                db.write_ordered_bounded(&cx, batches, 3).await,
                Err(WriteTxnError::Write(_))
            ));
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.edge(EId(60)).unwrap().is_none());
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(0).into())]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn single_relation_fast_path_charges_raw_input_only() {
        let ((), report) = run_async_under_lab(0x6f62_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let mut db = seeded(&cx).await;
            let mut first = WriteBatch::new(RelationId(9));
            first.ensure_vertex(VId(1), vec![], vec![]);
            let mut second = WriteBatch::new(RelationId(9));
            second.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(4)));
            let batches = vec![first, second];
            assert!(matches!(
                db.prepare_ordered_writes_bounded(batches.clone(), 1),
                Err(WriteTxnError::OrderedWriteBudgetExceeded {
                    limit: 1,
                    required: 2,
                })
            ));
            let expected = db.prepare_ordered_writes(batches.clone()).unwrap();
            let admitted = db.prepare_ordered_writes_bounded(batches, 2).unwrap();
            assert_eq!(admitted.template, expected.template);
            assert!(matches!(
                db.prepare_ordered_writes_bounded(vec![], 0),
                Err(WriteTxnError::Write(WriteError::EmptyBatch))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn budget_arithmetic_never_truncates_a_requirement_above_u64() {
        let required = u128::from(u64::MAX) + 1;
        assert!(matches!(
            admit_expanded_rows(Some(u64::MAX), required),
            Err(WriteTxnError::OrderedWriteBudgetExceeded {
                limit: u64::MAX,
                required: actual,
            })
                if actual == required
        ));
        assert!(admit_expanded_rows(None, required).is_ok());
        assert!(admit_expanded_rows(Some(0), 0).is_ok());
    }
}

#[cfg(test)]
mod routing_declaration_tests {
    use super::{DeclarationConflict, IndexMap, extend_shared_declarations};
    use std::collections::BTreeMap;

    #[test]
    fn persistent_declarations_match_complete_source_order_at_every_cut() {
        let input = [
            (10, 7),
            (50, 9),
            (60, 2),
            (50, 9),
            (70, 4),
            (10, 7),
            (80, 2),
        ];
        for width in [1, 2, 3, 5] {
            let mut retained = IndexMap::from_sorted(Vec::new(), &mut 0);
            let mut complete = BTreeMap::new();
            for suffix in input.chunks(width) {
                let next = extend_shared_declarations(&retained, suffix.iter().copied(), |id| {
                    (*id == 10).then_some(7)
                })
                .unwrap()
                .unwrap();
                for &(identity, relation) in suffix {
                    let owner = complete.entry(identity).or_insert(if identity == 10 {
                        7
                    } else {
                        relation
                    });
                    assert_eq!(*owner, relation);
                }
                assert_eq!(
                    next.iter()
                        .map(|(&id, &owner)| (id, owner))
                        .collect::<Vec<_>>(),
                    complete
                        .iter()
                        .map(|(&id, &owner)| (id, owner))
                        .collect::<Vec<_>>()
                );
                for (identity, owner) in retained.iter() {
                    assert_eq!(next.get(identity), Some(owner));
                }
                retained = next;
            }
        }
    }

    #[test]
    fn declaration_conflicts_preserve_first_source_error_and_pinned_prefix() {
        let retained = IndexMap::from_sorted(vec![(10, 7), (50, 9)], &mut 0);
        for (suffix, expected) in [
            (
                vec![(70, 4), (50, 2), (10, 3)],
                DeclarationConflict {
                    identity: 50,
                    first: 9,
                    second: 2,
                },
            ),
            (
                vec![(40, 2), (50, 2)],
                DeclarationConflict {
                    identity: 40,
                    first: 1,
                    second: 2,
                },
            ),
            (
                vec![(80, 5), (80, 6), (50, 2)],
                DeclarationConflict {
                    identity: 80,
                    first: 5,
                    second: 6,
                },
            ),
        ] {
            let error = extend_shared_declarations(&retained, suffix, |identity| {
                (*identity == 40).then_some(1)
            })
            .unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(
                retained
                    .iter()
                    .map(|(&id, &owner)| (id, owner))
                    .collect::<Vec<_>>(),
                vec![(10, 7), (50, 9)]
            );
        }
    }

    #[test]
    fn basis_lookups_visit_only_suffix_declarations() {
        let retained = IndexMap::from_sorted((0..10_000).map(|id| (id, id % 5)).collect(), &mut 0);
        let mut visited = Vec::new();
        let updated =
            extend_shared_declarations(&retained, [(20_000, 8), (2, 2), (20_000, 8)], |id| {
                visited.push(*id);
                None
            })
            .unwrap()
            .unwrap();
        assert_eq!(visited, vec![20_000, 2, 20_000]);
        assert_eq!(updated.len(), 10_001);
        assert_eq!(updated.get(&20_000), Some(&8));
        assert_eq!(updated.get(&2), Some(&2));
        assert_eq!(retained.len(), 10_000);
        assert_eq!(retained.get(&20_000), None);
    }

    #[test]
    fn empty_declaration_suffix_does_not_look_up_or_replace_existing_owners() {
        let retained = IndexMap::from_sorted(vec![(10, 7), (50, 9)], &mut 0);
        let unchanged = extend_shared_declarations(&retained, [], |_| {
            panic!("an empty suffix has no live-owner lookups")
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            unchanged.iter().collect::<Vec<_>>(),
            retained.iter().collect::<Vec<_>>()
        );
    }
}
