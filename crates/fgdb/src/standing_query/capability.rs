//! Pre-expansion capability homomorphism (`Pi_C`) for standing subscriptions (`FG-INV-20`).
//!
//! Projects both the initial graph snapshot and each committed `LogicalDeltaBatch`
//! through an attenuated capability's [`PlannerPredicates`] *before* any query
//! scan, join, or aggregate circuit node observes vertices, edges, properties,
//! or frontier sequence advances. Hidden mutations produce zero projected rows,
//! zero visible frontier transitions, and zero downstream circuit work.

use super::{Meter, StandingQueryError, StandingQueryFailure, StandingQueryStats};
use crate::Database;
use asupersync::fs::Vfs;
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, LabelId, LogicalDeltaBatch, PropertyKeyId, RelationId,
    SchemaEpoch, ZSetEvent,
};
use fgdb_gql::GqlQueryPolicy;
use fgdb_types::{CanonicalPropertyValue, CommitSeq, EId, ObjectId, VId};
use fgdb_warden::PlannerPredicates;
use std::collections::{BTreeMap, BTreeSet};

const ZERO_OBJECT: ObjectId = ObjectId([0; 32]);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TrackedVertex {
    all_labels: BTreeSet<LabelId>,
    visible_props: BTreeMap<PropertyKeyId, CanonicalPropertyValue>,
    incident_allowed_edges: BTreeSet<EId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedEdge {
    relation: RelationId,
    src: VId,
    dst: VId,
    visible_props: BTreeMap<PropertyKeyId, CanonicalPropertyValue>,
}

pub(super) struct VisibleVertex<'a> {
    pub(super) vid: VId,
    pub(super) labels: Vec<LabelId>,
    pub(super) props: &'a BTreeMap<PropertyKeyId, CanonicalPropertyValue>,
}

pub(super) struct VisibleEdge<'a> {
    pub(super) eid: EId,
    pub(super) relation: RelationId,
    pub(super) src: VId,
    pub(super) dst: VId,
    pub(super) props: &'a BTreeMap<PropertyKeyId, CanonicalPropertyValue>,
}

pub(crate) struct State {
    pub(super) scope: PlannerPredicates,
    pub(super) policy: GqlQueryPolicy,
    pub(super) physical_frontier: CommitSeq,
    pub(super) visible_frontier: CommitSeq,
    pub(super) members_end: usize,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
    pub(super) current_delta: Option<LogicalDeltaBatch>,
    vertices: BTreeMap<VId, TrackedVertex>,
    edges: BTreeMap<EId, TrackedEdge>,
}

impl core::fmt::Debug for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CapabilityScope")
            .field("physical_frontier", &self.physical_frontier)
            .field("visible_frontier", &self.visible_frontier)
            .field("members_end", &self.members_end)
            .field("failure", &self.failure)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl State {
    pub(super) fn build<V: Vfs + Clone>(
        database: &Database<V>,
        scope: PlannerPredicates,
        policy: GqlQueryPolicy,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<Self, StandingQueryError> {
        checkpoint().map_err(StandingQueryError::Maintenance)?;
        let raw_vertices = database.vertices().map_err(StandingQueryError::Read)?;
        let raw_edges = database.edges().map_err(StandingQueryError::Read)?;

        let mut vertices = BTreeMap::new();
        for row in raw_vertices {
            checkpoint().map_err(StandingQueryError::Maintenance)?;
            let all_labels: BTreeSet<LabelId> = row.labels.into_iter().collect();
            let visible_props = row
                .props
                .into_iter()
                .filter(|(key, _)| scope.allows_property(*key))
                .collect();
            vertices.insert(
                row.vid,
                TrackedVertex {
                    all_labels,
                    visible_props,
                    incident_allowed_edges: BTreeSet::new(),
                },
            );
        }

        let mut edges = BTreeMap::new();
        for record in raw_edges {
            checkpoint().map_err(StandingQueryError::Maintenance)?;
            if !scope.allows_relation(record.entry.relation) {
                continue;
            }
            let visible_props = record
                .props
                .iter()
                .filter(|(key, _)| scope.allows_property(*key))
                .cloned()
                .collect();
            let eid = record.entry.eid;
            let src = record.entry.src;
            let dst = record.entry.dst;
            if let Some(vertex) = vertices.get_mut(&src) {
                vertex.incident_allowed_edges.insert(eid);
            }
            if dst != src
                && let Some(vertex) = vertices.get_mut(&dst)
            {
                vertex.incident_allowed_edges.insert(eid);
            }
            edges.insert(
                eid,
                TrackedEdge {
                    relation: record.entry.relation,
                    src,
                    dst,
                    visible_props,
                },
            );
        }

        let has_visible = vertices
            .values()
            .any(|vertex| scope.allows_vertex_set(&vertex.all_labels));
        let visible_frontier = CommitSeq(u64::from(has_visible));

        Ok(Self {
            scope,
            policy,
            physical_frontier: database.snapshot.frontier,
            visible_frontier,
            members_end: usize::MAX,
            stats: StandingQueryStats::default(),
            failure: None,
            current_delta: None,
            vertices,
            edges,
        })
    }

    fn vertex_visible_in(scope: &PlannerPredicates, vertex: Option<&TrackedVertex>) -> bool {
        vertex.is_some_and(|v| scope.allows_vertex_set(&v.all_labels))
    }

    pub(super) fn visible_vertices(&self) -> impl Iterator<Item = VisibleVertex<'_>> + '_ {
        self.vertices.iter().filter_map(|(&vid, vertex)| {
            if !self.scope.allows_vertex_set(&vertex.all_labels) {
                return None;
            }
            let labels = vertex
                .all_labels
                .iter()
                .copied()
                .filter(|label| self.scope.allows_label(*label))
                .collect();
            Some(VisibleVertex {
                vid,
                labels,
                props: &vertex.visible_props,
            })
        })
    }

    pub(super) fn visible_edges(&self) -> impl Iterator<Item = VisibleEdge<'_>> + '_ {
        self.edges.iter().filter_map(|(&eid, edge)| {
            if !Self::vertex_visible_in(&self.scope, self.vertices.get(&edge.src))
                || !Self::vertex_visible_in(&self.scope, self.vertices.get(&edge.dst))
            {
                return None;
            }
            Some(VisibleEdge {
                eid,
                relation: edge.relation,
                src: edge.src,
                dst: edge.dst,
                props: &edge.visible_props,
            })
        })
    }

    pub(super) fn admit_vertex_snapshot(
        &self,
        records: &mut u64,
        meter: &mut Meter<'_>,
        stored_units: impl Fn(&CanonicalPropertyValue) -> usize,
    ) -> Result<(), StandingQueryFailure> {
        for vertex in self.visible_vertices() {
            *records = records
                .checked_add(1)
                .ok_or(StandingQueryFailure::SnapshotBudget)?;
            if meter
                .policy
                .rows
                .max_snapshot_records()
                .is_some_and(|limit| *records > limit)
            {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 1 + vertex.labels.len())?;
            for value in vertex.props.values() {
                meter.units(ZSetEvent::ScratchEntry, stored_units(value))?;
            }
        }
        Ok(())
    }

    pub(super) fn admit_edge_snapshot(
        &self,
        records: &mut u64,
        meter: &mut Meter<'_>,
        stored_units: impl Fn(&CanonicalPropertyValue) -> usize,
    ) -> Result<(), StandingQueryFailure> {
        for edge in self.visible_edges() {
            *records = records
                .checked_add(1)
                .ok_or(StandingQueryFailure::SnapshotBudget)?;
            if meter
                .policy
                .rows
                .max_snapshot_records()
                .is_some_and(|limit| *records > limit)
            {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 4)?;
            if !edge.props.is_empty() {
                meter.charge(ZSetEvent::ScratchEntry)?;
                for value in edge.props.values() {
                    meter.charge(ZSetEvent::Work)?;
                    meter.units(ZSetEvent::ScratchEntry, stored_units(value))?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<(), StandingQueryFailure> {
        checkpoint()?;
        let at = batch.commit_seq();
        if self
            .physical_frontier
            .checked_successor()
            .map_err(|_| StandingQueryFailure::InvalidDelta)?
            != at
            || batch.frontier() != at
            || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }

        let mut touched_vertices = BTreeSet::new();
        let mut touched_edges = BTreeSet::new();
        let mut schema_epoch = SchemaEpoch(0);
        let mut vertex_coord_relation = RelationId(0);
        let mut saw_coord = false;

        for entry in batch.coordinate_entries() {
            checkpoint()?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            if entry.schema_transition.is_some() {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            if !saw_coord {
                schema_epoch = entry.schema_epoch;
                vertex_coord_relation = entry.relation;
                saw_coord = true;
            }
            for row in &entry.rows {
                checkpoint()?;
                match row {
                    DeltaRow::CreateVertex { vid, .. } | DeltaRow::LabelMembership { vid, .. } => {
                        touched_vertices.insert(*vid);
                    }
                    DeltaRow::DeleteVertex {
                        vid,
                        sorted_retired_incident_edges,
                        ..
                    } => {
                        touched_vertices.insert(*vid);
                        for eid in sorted_retired_incident_edges {
                            if self.edges.contains_key(eid) {
                                touched_edges.insert(*eid);
                            }
                        }
                    }
                    DeltaRow::Property {
                        elem: ElementId::Vertex(vid),
                        property,
                        ..
                    } => {
                        if self.scope.allows_property(*property) {
                            touched_vertices.insert(*vid);
                        }
                    }
                    DeltaRow::CreateEdge {
                        eid,
                        src,
                        relation,
                        dst,
                        ..
                    } => {
                        if self.scope.allows_relation(*relation) {
                            touched_edges.insert(*eid);
                            touched_vertices.insert(*src);
                            touched_vertices.insert(*dst);
                        }
                    }
                    DeltaRow::DeleteEdge { eid, .. } => {
                        if let Some(existing) = self.edges.get(eid) {
                            touched_edges.insert(*eid);
                            touched_vertices.insert(existing.src);
                            touched_vertices.insert(existing.dst);
                        }
                    }
                    DeltaRow::Property {
                        elem: ElementId::Edge(eid),
                        property,
                        ..
                    } => {
                        if self.scope.allows_property(*property) && self.edges.contains_key(eid) {
                            touched_edges.insert(*eid);
                        }
                    }
                    DeltaRow::Schema { .. } | DeltaRow::Constraint { .. } => {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    DeltaRow::Counter { elem, property, .. } => {
                        if self.scope.allows_property(*property) {
                            match elem {
                                ElementId::Vertex(vid) if self.vertices.contains_key(vid) => {
                                    return Err(StandingQueryFailure::InvalidDelta);
                                }
                                ElementId::Edge(eid) if self.edges.contains_key(eid) => {
                                    return Err(StandingQueryFailure::InvalidDelta);
                                }
                                _ => {}
                            }
                        }
                    }
                    DeltaRow::Escrow {
                        subject,
                        subject_property,
                        ..
                    } => {
                        let prop_visible =
                            subject_property.is_none_or(|key| self.scope.allows_property(key));
                        if prop_visible {
                            match subject {
                                ElementId::Vertex(vid) if self.vertices.contains_key(vid) => {
                                    return Err(StandingQueryFailure::InvalidDelta);
                                }
                                ElementId::Edge(eid) if self.edges.contains_key(eid) => {
                                    return Err(StandingQueryFailure::InvalidDelta);
                                }
                                _ => {}
                            }
                        }
                    }
                    DeltaRow::ValidTime { .. } | DeltaRow::Sketch { .. } => {}
                }
            }
        }

        if touched_vertices.is_empty() && touched_edges.is_empty() {
            self.current_delta = None;
            self.stats = StandingQueryStats::default();
            return Ok(());
        }

        let mut staged_vertices: BTreeMap<VId, Option<TrackedVertex>> = touched_vertices
            .iter()
            .map(|&vid| (vid, self.vertices.get(&vid).cloned()))
            .collect();
        let mut staged_edges: BTreeMap<EId, Option<TrackedEdge>> = touched_edges
            .iter()
            .map(|&eid| (eid, self.edges.get(&eid).cloned()))
            .collect();

        // First apply edge births so vertex cascades in another coordinate see them.
        for entry in batch.coordinate_entries() {
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            for row in &entry.rows {
                checkpoint()?;
                match row {
                    DeltaRow::CreateVertex {
                        vid, labels, props, ..
                    } => {
                        let slot = staged_vertices
                            .get_mut(vid)
                            .ok_or(StandingQueryFailure::InvalidDelta)?;
                        if slot.is_some() {
                            return Err(StandingQueryFailure::InvalidDelta);
                        }
                        *slot = Some(TrackedVertex {
                            all_labels: labels.iter().copied().collect(),
                            visible_props: props
                                .iter()
                                .filter(|(key, _)| self.scope.allows_property(*key))
                                .cloned()
                                .collect(),
                            incident_allowed_edges: BTreeSet::new(),
                        });
                    }
                    DeltaRow::LabelMembership {
                        vid,
                        label,
                        before,
                        after,
                    } => {
                        let vertex = staged_vertices
                            .get_mut(vid)
                            .and_then(Option::as_mut)
                            .ok_or(StandingQueryFailure::InvalidDelta)?;
                        if vertex.all_labels.contains(label) != *before {
                            return Err(StandingQueryFailure::InvalidDelta);
                        }
                        if *after {
                            vertex.all_labels.insert(*label);
                        } else {
                            vertex.all_labels.remove(label);
                        }
                    }
                    DeltaRow::Property {
                        elem: ElementId::Vertex(vid),
                        property,
                        before,
                        after,
                    } if self.scope.allows_property(*property) => {
                        let vertex = staged_vertices
                            .get_mut(vid)
                            .and_then(Option::as_mut)
                            .ok_or(StandingQueryFailure::InvalidDelta)?;
                        if vertex.visible_props.get(property) != before.as_ref() {
                            return Err(StandingQueryFailure::InvalidDelta);
                        }
                        match after {
                            Some(value) => {
                                vertex.visible_props.insert(*property, value.clone());
                            }
                            None => {
                                vertex.visible_props.remove(property);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        for entry in batch.coordinate_entries() {
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            for row in &entry.rows {
                checkpoint()?;
                if let DeltaRow::CreateEdge {
                    eid,
                    src,
                    relation,
                    dst,
                    props,
                    ..
                } = row
                    && self.scope.allows_relation(*relation)
                {
                    let slot = staged_edges
                        .get_mut(eid)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                    if slot.is_some() {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    let visible_props = props
                        .iter()
                        .filter(|(key, _)| self.scope.allows_property(*key))
                        .cloned()
                        .collect();
                    *slot = Some(TrackedEdge {
                        relation: *relation,
                        src: *src,
                        dst: *dst,
                        visible_props,
                    });
                    if let Some(Some(v)) = staged_vertices.get_mut(src) {
                        v.incident_allowed_edges.insert(*eid);
                    }
                    if dst != src
                        && let Some(Some(v)) = staged_vertices.get_mut(dst)
                    {
                        v.incident_allowed_edges.insert(*eid);
                    }
                }
            }
        }

        for entry in batch.coordinate_entries() {
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            for row in &entry.rows {
                checkpoint()?;
                match row {
                    DeltaRow::Property {
                        elem: ElementId::Edge(eid),
                        property,
                        before,
                        after,
                    } if self.scope.allows_property(*property) => {
                        if let Some(Some(edge)) = staged_edges.get_mut(eid) {
                            if edge.visible_props.get(property) != before.as_ref() {
                                return Err(StandingQueryFailure::InvalidDelta);
                            }
                            match after {
                                Some(value) => {
                                    edge.visible_props.insert(*property, value.clone());
                                }
                                None => {
                                    edge.visible_props.remove(property);
                                }
                            }
                        }
                    }
                    DeltaRow::DeleteEdge { eid, .. } => {
                        if let Some(slot) = staged_edges.get_mut(eid)
                            && let Some(removed) = slot.take()
                        {
                            if let Some(Some(v)) = staged_vertices.get_mut(&removed.src) {
                                v.incident_allowed_edges.remove(eid);
                            }
                            if removed.dst != removed.src
                                && let Some(Some(v)) = staged_vertices.get_mut(&removed.dst)
                            {
                                v.incident_allowed_edges.remove(eid);
                            }
                        }
                    }
                    DeltaRow::DeleteVertex {
                        vid,
                        sorted_retired_incident_edges,
                        ..
                    } => {
                        for eid in sorted_retired_incident_edges {
                            let edge_entry = staged_edges
                                .entry(*eid)
                                .or_insert_with(|| self.edges.get(eid).cloned());
                            if let Some(removed) = edge_entry.take() {
                                for endpoint in [removed.src, removed.dst] {
                                    let v_entry = staged_vertices
                                        .entry(endpoint)
                                        .or_insert_with(|| self.vertices.get(&endpoint).cloned());
                                    if let Some(v) = v_entry.as_mut() {
                                        v.incident_allowed_edges.remove(eid);
                                    }
                                }
                            }
                        }
                        staged_vertices
                            .get_mut(vid)
                            .and_then(Option::take)
                            .ok_or(StandingQueryFailure::InvalidDelta)?;
                    }
                    _ => {}
                }
            }
        }

        // Any vertex whose visibility changed also affects all its incident allowed edges.
        let mut candidate_edges: BTreeSet<EId> = staged_edges.keys().copied().collect();
        for (&vid, after_opt) in &staged_vertices {
            let before_opt = self.vertices.get(&vid);
            let before_vis = Self::vertex_visible_in(&self.scope, before_opt);
            let after_vis = Self::vertex_visible_in(&self.scope, after_opt.as_ref());
            if before_vis != after_vis {
                if let Some(before) = before_opt {
                    candidate_edges.extend(before.incident_allowed_edges.iter().copied());
                }
                if let Some(after) = after_opt {
                    candidate_edges.extend(after.incident_allowed_edges.iter().copied());
                }
            }
        }

        let vertex_after_visible = |vid: VId| -> bool {
            match staged_vertices.get(&vid) {
                Some(after_opt) => Self::vertex_visible_in(&self.scope, after_opt.as_ref()),
                None => Self::vertex_visible_in(&self.scope, self.vertices.get(&vid)),
            }
        };
        let vertex_before_visible =
            |vid: VId| -> bool { Self::vertex_visible_in(&self.scope, self.vertices.get(&vid)) };
        let edge_before = |eid: EId| -> Option<&TrackedEdge> { self.edges.get(&eid) };
        let edge_after = |eid: EId| -> Option<&TrackedEdge> {
            match staged_edges.get(&eid) {
                Some(after_opt) => after_opt.as_ref(),
                None => self.edges.get(&eid),
            }
        };
        let edge_before_visible = |eid: EId| -> bool {
            edge_before(eid)
                .is_some_and(|e| vertex_before_visible(e.src) && vertex_before_visible(e.dst))
        };
        let edge_after_visible = |eid: EId| -> bool {
            edge_after(eid)
                .is_some_and(|e| vertex_after_visible(e.src) && vertex_after_visible(e.dst))
        };

        let mut by_relation: BTreeMap<RelationId, Vec<DeltaRow>> = BTreeMap::new();
        let mut cascade_retired_edges = BTreeSet::new();

        for (&vid, after_opt) in &staged_vertices {
            checkpoint()?;
            let before_opt = self.vertices.get(&vid);
            let before_vis = Self::vertex_visible_in(&self.scope, before_opt);
            let after_vis = Self::vertex_visible_in(&self.scope, after_opt.as_ref());
            match (before_vis, after_vis) {
                (false, false) => {}
                (false, true) => {
                    let after = after_opt.as_ref().expect("visible vertex exists");
                    let labels = after
                        .all_labels
                        .iter()
                        .copied()
                        .filter(|label| self.scope.allows_label(*label))
                        .collect();
                    let props = after
                        .visible_props
                        .iter()
                        .map(|(&k, v)| (k, v.clone()))
                        .collect();
                    by_relation.entry(vertex_coord_relation).or_default().push(
                        DeltaRow::CreateVertex {
                            vid,
                            birth_ordinal: 0,
                            labels,
                            props,
                            valid_time: None,
                        },
                    );
                }
                (true, false) => {
                    let before = before_opt.expect("previously visible vertex exists");
                    let mut sorted_retired_incident_edges = Vec::new();
                    for &eid in &before.incident_allowed_edges {
                        if edge_before_visible(eid) {
                            sorted_retired_incident_edges.push(eid);
                            cascade_retired_edges.insert(eid);
                        }
                    }
                    by_relation.entry(vertex_coord_relation).or_default().push(
                        DeltaRow::DeleteVertex {
                            vid,
                            before_version: ZERO_OBJECT,
                            sorted_retired_incident_edges,
                        },
                    );
                }
                (true, true) => {
                    let before = before_opt.expect("before visible");
                    let after = after_opt.as_ref().expect("after visible");
                    let all_label_keys: BTreeSet<LabelId> = before
                        .all_labels
                        .union(&after.all_labels)
                        .copied()
                        .filter(|label| self.scope.allows_label(*label))
                        .collect();
                    for label in all_label_keys {
                        let b = before.all_labels.contains(&label);
                        let a = after.all_labels.contains(&label);
                        if b != a {
                            by_relation.entry(vertex_coord_relation).or_default().push(
                                DeltaRow::LabelMembership {
                                    vid,
                                    label,
                                    before: b,
                                    after: a,
                                },
                            );
                        }
                    }
                    let all_prop_keys: BTreeSet<PropertyKeyId> = before
                        .visible_props
                        .keys()
                        .chain(after.visible_props.keys())
                        .copied()
                        .collect();
                    for property in all_prop_keys {
                        let b = before.visible_props.get(&property);
                        let a = after.visible_props.get(&property);
                        if b != a {
                            by_relation.entry(vertex_coord_relation).or_default().push(
                                DeltaRow::Property {
                                    elem: ElementId::Vertex(vid),
                                    property,
                                    before: b.cloned(),
                                    after: a.cloned(),
                                },
                            );
                        }
                    }
                }
            }
        }

        for eid in candidate_edges {
            checkpoint()?;
            let before_vis = edge_before_visible(eid);
            let after_vis = edge_after_visible(eid);
            match (before_vis, after_vis) {
                (false, false) => {}
                (false, true) => {
                    let after = edge_after(eid).expect("visible edge exists");
                    let props = after
                        .visible_props
                        .iter()
                        .map(|(&k, v)| (k, v.clone()))
                        .collect();
                    by_relation
                        .entry(after.relation)
                        .or_default()
                        .push(DeltaRow::CreateEdge {
                            eid,
                            birth_ordinal: 0,
                            src: after.src,
                            relation: after.relation,
                            dst: after.dst,
                            canonical_key: None,
                            props,
                            valid_time: None,
                        });
                }
                (true, false) => {
                    if !cascade_retired_edges.contains(&eid) {
                        let before = edge_before(eid).expect("previously visible edge exists");
                        by_relation.entry(before.relation).or_default().push(
                            DeltaRow::DeleteEdge {
                                eid,
                                before_version: ZERO_OBJECT,
                            },
                        );
                    }
                }
                (true, true) => {
                    let before = edge_before(eid).expect("before visible");
                    let after = edge_after(eid).expect("after visible");
                    if before.relation != after.relation
                        || before.src != after.src
                        || before.dst != after.dst
                    {
                        by_relation.entry(before.relation).or_default().push(
                            DeltaRow::DeleteEdge {
                                eid,
                                before_version: ZERO_OBJECT,
                            },
                        );
                        let props = after
                            .visible_props
                            .iter()
                            .map(|(&k, v)| (k, v.clone()))
                            .collect();
                        by_relation
                            .entry(after.relation)
                            .or_default()
                            .push(DeltaRow::CreateEdge {
                                eid,
                                birth_ordinal: 0,
                                src: after.src,
                                relation: after.relation,
                                dst: after.dst,
                                canonical_key: None,
                                props,
                                valid_time: None,
                            });
                    } else {
                        let all_prop_keys: BTreeSet<PropertyKeyId> = before
                            .visible_props
                            .keys()
                            .chain(after.visible_props.keys())
                            .copied()
                            .collect();
                        for property in all_prop_keys {
                            let b = before.visible_props.get(&property);
                            let a = after.visible_props.get(&property);
                            if b != a {
                                by_relation.entry(after.relation).or_default().push(
                                    DeltaRow::Property {
                                        elem: ElementId::Edge(eid),
                                        property,
                                        before: b.cloned(),
                                        after: a.cloned(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        let coordinate_entries: Vec<CoordinateEntry> = by_relation
            .into_iter()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(relation, rows)| CoordinateEntry {
                graph: crate::GRAPH,
                branch: crate::BRANCH,
                relation,
                schema_epoch,
                schema_transition: None,
                rows,
            })
            .collect();

        let next_visible_frontier = if coordinate_entries.is_empty() {
            None
        } else {
            Some(
                self.visible_frontier
                    .checked_successor()
                    .map_err(|_| StandingQueryFailure::Arithmetic)?,
            )
        };

        // Commit staged state atomically after all fallible checks succeeded.
        for (vid, after_opt) in staged_vertices {
            match after_opt {
                Some(vertex) => {
                    self.vertices.insert(vid, vertex);
                }
                None => {
                    self.vertices.remove(&vid);
                }
            }
        }
        for (eid, after_opt) in staged_edges {
            match after_opt {
                Some(edge) => {
                    self.edges.insert(eid, edge);
                }
                None => {
                    self.edges.remove(&eid);
                }
            }
        }

        let delta_rows: u64 = coordinate_entries
            .iter()
            .map(|entry| entry.rows.len() as u64)
            .sum();
        self.stats = StandingQueryStats {
            delta_rows,
            ..StandingQueryStats::default()
        };
        if let Some(projected_seq) = next_visible_frontier {
            self.visible_frontier = projected_seq;
            self.current_delta = Some(batch.project_committed(coordinate_entries, projected_seq));
        } else {
            self.current_delta = None;
        }
        Ok(())
    }
}
