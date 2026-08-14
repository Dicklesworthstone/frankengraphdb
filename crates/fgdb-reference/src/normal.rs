//! Live subset of `NetEffectNormalForm` (fgdb-w5-effects-normal-form-819).
//!
//! Finalization evaluates intents in order against a workspace. The commit
//! stream is sequence-neutral: `LogicalDeltaTemplate::build` byte-sorts rows
//! before `apply_template` replays them against the durable graph. Byte order
//! is not applicability order, so a multi-write transaction whose evaluation
//! rows share a target fails at apply (`fgdb-p6tm`).
//!
//! The plan's answer is to fold at finalization *before* canonicalize. This
//! module does that for the families that exist as `ReferenceGraph` state
//! today: create/delete/property/label/valid-time, plus counter/escrow/sketch
//! and schema/constraint when those maps differ. It is a subset of the 819
//! artifact — no `CommittedEffectSet`, no authority envelopes, no global
//! charge typing. Those wait on G0 identity registries.
//!
//! The fold is a state diff, not a row rewrite. Evaluation already applied
//! every source row to the workspace, so the workspace *is* the after-image
//! and the snapshot graph *is* the before-image. Diffing them yields
//! target-disjoint rows whose before-images match the durable graph
//! `apply_template` will see (in particular, a delete after a same-txn
//! property change CASes the *basis* version, not the workspace version).

use crate::{Edge, ReferenceGraph, Vertex};
use fgdb_delta_types::{
    DeltaRow, ElementId, EscrowDomainId, LabelId, OperationKey, PropertyKeyId, ValidTimePeriod,
};
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};
use std::collections::{BTreeMap, BTreeSet};

/// Why a source row did not survive into the committed form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoOpPolicy {
    /// Folded chain ended where it started (`before == after`).
    Identity,
    /// Create and delete of the same identity cancelled.
    InverseCancellation,
    /// Mutation folded into a surviving create or absorbed by a delete.
    Absorbed,
}

/// Fate of one evaluation-order source row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectFate {
    Survives { dest_index: usize },
    NoOp { policy: NoOpPolicy },
}

/// One source row's mapping into the normal form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMapping {
    pub source_index: usize,
    pub fate: EffectFate,
}

/// Target-disjoint committed effects plus the totality map from source rows.
#[derive(Clone, Debug, PartialEq)]
pub struct NetEffectNormalForm {
    pub rows: Vec<DeltaRow>,
    pub mapping: Vec<SourceMapping>,
}

/// Fold `workspace` (after-image) against `basis` (snapshot / before-image).
///
/// `source` is the evaluation-order effect list. It does not determine the
/// surviving rows — the two graphs do — but every source row is mapped to a
/// surviving row or an explicit `NoOp`, which is the 819 totality law for
/// the families this crate can currently emit.
pub fn normalize(
    basis: &ReferenceGraph,
    workspace: &ReferenceGraph,
    source: &[DeltaRow],
) -> NetEffectNormalForm {
    let mut rows = Vec::new();

    let basis_vids: BTreeSet<VId> = basis.iter_vertices().map(|(id, _)| id).collect();
    let work_vids: BTreeSet<VId> = workspace.iter_vertices().map(|(id, _)| id).collect();
    let basis_eids: BTreeSet<EId> = basis.iter_edges().map(|(id, _)| id).collect();
    let work_eids: BTreeSet<EId> = workspace.iter_edges().map(|(id, _)| id).collect();

    let deleted_vids: BTreeSet<VId> = basis_vids.difference(&work_vids).copied().collect();
    let created_vids: BTreeSet<VId> = work_vids.difference(&basis_vids).copied().collect();
    let live_vids: BTreeSet<VId> = basis_vids.intersection(&work_vids).copied().collect();

    let mut cascaded_eids: BTreeSet<EId> = BTreeSet::new();
    for vid in &deleted_vids {
        for eid in basis.incident_edges(*vid) {
            cascaded_eids.insert(eid);
        }
    }

    let deleted_eids: BTreeSet<EId> = basis_eids
        .difference(&work_eids)
        .copied()
        .filter(|eid| !cascaded_eids.contains(eid))
        .collect();
    let created_eids: BTreeSet<EId> = work_eids.difference(&basis_eids).copied().collect();
    let live_eids: BTreeSet<EId> = basis_eids.intersection(&work_eids).copied().collect();

    for vid in &created_vids {
        let Some(vertex) = workspace.vertex(*vid) else {
            continue;
        };
        rows.push(create_vertex_row(*vid, vertex));
    }
    for eid in &created_eids {
        let Some(edge) = workspace.edge(*eid) else {
            continue;
        };
        rows.push(create_edge_row(*eid, edge));
    }
    for vid in &deleted_vids {
        let Some(vertex) = basis.vertex(*vid) else {
            continue;
        };
        // Apply is byte-sorted: DeleteVertex rows run in VId order. A shared
        // cascade eid must sit only on the smallest deleted endpoint so the
        // first apply still equals the live incident set (fgdb-qgk9 / s9ja).
        let sorted_retired_incident_edges: Vec<EId> = basis
            .incident_edges(*vid)
            .into_iter()
            .filter(|eid| {
                let Some(edge) = basis.edge(*eid) else {
                    return true;
                };
                [edge.src, edge.dst]
                    .into_iter()
                    .filter(|endpoint| deleted_vids.contains(endpoint))
                    .min()
                    == Some(*vid)
            })
            .collect();
        rows.push(DeltaRow::DeleteVertex {
            vid: *vid,
            before_version: vertex.version,
            sorted_retired_incident_edges,
        });
    }
    for eid in &deleted_eids {
        let Some(edge) = basis.edge(*eid) else {
            continue;
        };
        rows.push(DeltaRow::DeleteEdge {
            eid: *eid,
            before_version: edge.version,
        });
    }

    for vid in &live_vids {
        let Some(before) = basis.vertex(*vid) else {
            continue;
        };
        let Some(after) = workspace.vertex(*vid) else {
            continue;
        };
        push_label_deltas(&mut rows, *vid, &before.labels, &after.labels);
        push_property_deltas(
            &mut rows,
            ElementId::Vertex(*vid),
            &before.props,
            &after.props,
        );
        push_valid_time_delta(
            &mut rows,
            ElementId::Vertex(*vid),
            source,
            before.valid_time,
            after.valid_time,
        );
    }
    for eid in &live_eids {
        let Some(before) = basis.edge(*eid) else {
            continue;
        };
        let Some(after) = workspace.edge(*eid) else {
            continue;
        };
        push_property_deltas(
            &mut rows,
            ElementId::Edge(*eid),
            &before.props,
            &after.props,
        );
        push_valid_time_delta(
            &mut rows,
            ElementId::Edge(*eid),
            source,
            before.valid_time,
            after.valid_time,
        );
    }

    push_counter_deltas(&mut rows, basis, workspace, source);
    push_escrow_deltas(&mut rows, basis, workspace, source);
    push_sketch_deltas(&mut rows, basis, workspace, source);
    push_schema_delta(&mut rows, basis, workspace);
    push_constraint_delta(&mut rows, basis, workspace);

    let identities = IdentityPartition {
        created_vids: &created_vids,
        deleted_vids: &deleted_vids,
        live_vids: &live_vids,
        created_eids: &created_eids,
        deleted_eids: &deleted_eids,
        live_eids: &live_eids,
        cascaded_eids: &cascaded_eids,
    };
    let mapping = source
        .iter()
        .enumerate()
        .map(|(source_index, row)| SourceMapping {
            source_index,
            fate: classify(row, &rows, identities),
        })
        .collect();

    NetEffectNormalForm { rows, mapping }
}

fn create_vertex_row(vid: VId, vertex: &Vertex) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid,
        birth_ordinal: vertex.birth_ordinal,
        labels: vertex.labels.iter().copied().collect(),
        props: vertex.props.iter().map(|(k, v)| (*k, v.clone())).collect(),
        valid_time: vertex.valid_time,
    }
}

fn create_edge_row(eid: EId, edge: &Edge) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid,
        birth_ordinal: edge.birth_ordinal,
        src: edge.src,
        relation: edge.relation,
        dst: edge.dst,
        canonical_key: edge.canonical_key.clone(),
        props: edge.props.iter().map(|(k, v)| (*k, v.clone())).collect(),
        valid_time: edge.valid_time,
    }
}

fn push_label_deltas(
    rows: &mut Vec<DeltaRow>,
    vid: VId,
    before: &BTreeSet<LabelId>,
    after: &BTreeSet<LabelId>,
) {
    for label in before.union(after) {
        let had = before.contains(label);
        let has = after.contains(label);
        if had != has {
            rows.push(DeltaRow::LabelMembership {
                vid,
                label: *label,
                before: had,
                after: has,
            });
        }
    }
}

fn push_property_deltas(
    rows: &mut Vec<DeltaRow>,
    elem: ElementId,
    before: &BTreeMap<PropertyKeyId, CanonicalScalar>,
    after: &BTreeMap<PropertyKeyId, CanonicalScalar>,
) {
    let keys: BTreeSet<PropertyKeyId> = before.keys().chain(after.keys()).copied().collect();
    for key in keys {
        let left = before.get(&key);
        let right = after.get(&key);
        if left != right {
            rows.push(DeltaRow::Property {
                elem,
                property: key,
                before: left.cloned(),
                after: right.cloned(),
            });
        }
    }
}

fn push_valid_time_delta(
    rows: &mut Vec<DeltaRow>,
    elem: ElementId,
    source: &[DeltaRow],
    before: Option<ValidTimePeriod>,
    after: Option<ValidTimePeriod>,
) {
    if before == after {
        return;
    }
    let contract_id = source
        .iter()
        .find_map(|row| match row {
            DeltaRow::ValidTime {
                elem: row_elem,
                contract_id,
                ..
            } if *row_elem == elem => Some(*contract_id),
            _ => None,
        })
        .unwrap_or(ObjectId([0u8; 32]));
    rows.push(DeltaRow::ValidTime {
        elem,
        contract_id,
        before,
        after,
    });
}

fn first_counter_meta(
    source: &[DeltaRow],
    elem: ElementId,
    property: PropertyKeyId,
) -> Option<(OperationKey, ObjectId)> {
    source.iter().find_map(|row| match row {
        DeltaRow::Counter {
            operation_key,
            elem: row_elem,
            property: row_prop,
            algebra_profile,
            ..
        } if *row_elem == elem && *row_prop == property => Some((*operation_key, *algebra_profile)),
        _ => None,
    })
}

fn push_counter_deltas(
    rows: &mut Vec<DeltaRow>,
    basis: &ReferenceGraph,
    workspace: &ReferenceGraph,
    source: &[DeltaRow],
) {
    let keys: BTreeSet<(ElementId, PropertyKeyId)> = basis
        .iter_counters()
        .map(|(k, _)| k)
        .chain(workspace.iter_counters().map(|(k, _)| k))
        .collect();
    for (elem, property) in keys {
        let before = basis.counter(elem, property).unwrap_or(0);
        let after = workspace.counter(elem, property).unwrap_or(0);
        if before == after {
            continue;
        }
        let Some((operation_key, algebra_profile)) = first_counter_meta(source, elem, property)
        else {
            continue;
        };
        rows.push(DeltaRow::Counter {
            operation_key,
            elem,
            property,
            algebra_profile,
            delta: after.saturating_sub(before),
            before,
            after,
        });
    }
}

fn first_escrow_meta(source: &[DeltaRow], domain: EscrowDomainId) -> Option<(OperationKey, u64)> {
    source.iter().find_map(|row| match row {
        DeltaRow::Escrow {
            domain_id,
            epoch,
            operation_key,
            ..
        } if *domain_id == domain => Some((*operation_key, *epoch)),
        _ => None,
    })
}

fn push_escrow_deltas(
    rows: &mut Vec<DeltaRow>,
    basis: &ReferenceGraph,
    workspace: &ReferenceGraph,
    source: &[DeltaRow],
) {
    let domains: BTreeSet<EscrowDomainId> = basis
        .iter_escrow()
        .map(|(d, _)| d)
        .chain(workspace.iter_escrow().map(|(d, _)| d))
        .collect();
    for domain in domains {
        let before_value = basis.escrow_balance(domain);
        let after_value = workspace.escrow_balance(domain);
        if before_value == after_value {
            continue;
        }
        let Some((operation_key, epoch)) = first_escrow_meta(source, domain) else {
            continue;
        };
        let subject = source.iter().find_map(|row| match row {
            DeltaRow::Escrow {
                domain_id, subject, ..
            } if *domain_id == domain => Some(*subject),
            _ => None,
        });
        let Some(subject) = subject else {
            continue;
        };
        rows.push(DeltaRow::Escrow {
            domain_id: domain,
            epoch,
            operation_key,
            subject,
            subject_property: None,
            delta: after_value.saturating_sub(before_value),
            before_value,
            after_value,
        });
    }
}

fn first_sketch_key(source: &[DeltaRow], profile: ObjectId) -> Option<OperationKey> {
    source.iter().find_map(|row| match row {
        DeltaRow::Sketch {
            operation_key,
            sketch_profile_oid,
            ..
        } if *sketch_profile_oid == profile => Some(*operation_key),
        _ => None,
    })
}

fn push_sketch_deltas(
    rows: &mut Vec<DeltaRow>,
    basis: &ReferenceGraph,
    workspace: &ReferenceGraph,
    source: &[DeltaRow],
) {
    let profiles: BTreeSet<ObjectId> = basis
        .iter_sketches()
        .map(|(p, _)| p)
        .chain(workspace.iter_sketches().map(|(p, _)| p))
        .collect();
    for profile in profiles {
        let before = basis.sketch_digest(profile).unwrap_or([0u8; 32]);
        let after = workspace.sketch_digest(profile).unwrap_or([0u8; 32]);
        if before == after {
            continue;
        }
        let Some(operation_key) = first_sketch_key(source, profile) else {
            continue;
        };
        rows.push(DeltaRow::Sketch {
            operation_key,
            sketch_profile_oid: profile,
            before_state_digest: before,
            after_state_oid: ObjectId(after),
        });
    }
}

fn push_schema_delta(rows: &mut Vec<DeltaRow>, basis: &ReferenceGraph, workspace: &ReferenceGraph) {
    if basis.schema_epoch() == workspace.schema_epoch() {
        return;
    }
    rows.push(DeltaRow::Schema {
        transition_oid: workspace.schema_root(),
        before_epoch: basis.schema_epoch(),
        after_epoch: workspace.schema_epoch(),
    });
}

fn push_constraint_delta(
    rows: &mut Vec<DeltaRow>,
    basis: &ReferenceGraph,
    workspace: &ReferenceGraph,
) {
    if basis.schema_root() == workspace.schema_root()
        && basis.constraint_root() == workspace.constraint_root()
    {
        return;
    }
    rows.push(DeltaRow::Constraint {
        before_schema_root: basis.schema_root(),
        after_schema_root: workspace.schema_root(),
        before_constraint_root: basis.constraint_root(),
        after_constraint_root: workspace.constraint_root(),
    });
}

#[derive(Clone, Copy)]
struct IdentityPartition<'a> {
    created_vids: &'a BTreeSet<VId>,
    deleted_vids: &'a BTreeSet<VId>,
    live_vids: &'a BTreeSet<VId>,
    created_eids: &'a BTreeSet<EId>,
    deleted_eids: &'a BTreeSet<EId>,
    live_eids: &'a BTreeSet<EId>,
    cascaded_eids: &'a BTreeSet<EId>,
}

fn classify(source: &DeltaRow, net: &[DeltaRow], ids: IdentityPartition<'_>) -> EffectFate {
    match source {
        DeltaRow::CreateVertex { vid, .. } => {
            if ids.created_vids.contains(vid) {
                survive_create_vertex(net, *vid)
            } else {
                EffectFate::NoOp {
                    policy: NoOpPolicy::InverseCancellation,
                }
            }
        }
        DeltaRow::CreateEdge { eid, .. } => {
            if ids.created_eids.contains(eid) {
                survive_create_edge(net, *eid)
            } else {
                EffectFate::NoOp {
                    policy: NoOpPolicy::InverseCancellation,
                }
            }
        }
        DeltaRow::DeleteVertex { vid, .. } => {
            if ids.deleted_vids.contains(vid) {
                survive_delete_vertex(net, *vid)
            } else {
                EffectFate::NoOp {
                    policy: NoOpPolicy::InverseCancellation,
                }
            }
        }
        DeltaRow::DeleteEdge { eid, .. } => {
            if ids.deleted_eids.contains(eid) {
                survive_delete_edge(net, *eid)
            } else {
                EffectFate::NoOp {
                    policy: NoOpPolicy::InverseCancellation,
                }
            }
        }
        DeltaRow::Property { elem, property, .. } => classify_property(net, *elem, *property, ids),
        DeltaRow::LabelMembership { vid, label, .. } => classify_label(net, *vid, *label, ids),
        DeltaRow::ValidTime { elem, .. } => classify_valid_time(net, *elem, ids),
        DeltaRow::Counter {
            elem,
            property,
            operation_key,
            ..
        } => survive_or_identity(
            net,
            |row| matches!(row, DeltaRow::Counter { elem: e, property: p, operation_key: k, .. } if *e == *elem && *p == *property && *k == *operation_key),
        ),
        DeltaRow::Escrow {
            domain_id,
            operation_key,
            ..
        } => survive_or_identity(
            net,
            |row| matches!(row, DeltaRow::Escrow { domain_id: d, operation_key: k, .. } if *d == *domain_id && *k == *operation_key),
        ),
        DeltaRow::Sketch {
            sketch_profile_oid,
            operation_key,
            ..
        } => survive_or_identity(
            net,
            |row| matches!(row, DeltaRow::Sketch { sketch_profile_oid: p, operation_key: k, .. } if *p == *sketch_profile_oid && *k == *operation_key),
        ),
        DeltaRow::Schema { .. } => {
            survive_or_identity(net, |row| matches!(row, DeltaRow::Schema { .. }))
        }
        DeltaRow::Constraint { .. } => {
            survive_or_identity(net, |row| matches!(row, DeltaRow::Constraint { .. }))
        }
    }
}

fn survive_or_identity(net: &[DeltaRow], pred: impl Fn(&DeltaRow) -> bool) -> EffectFate {
    match net.iter().position(pred) {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::Identity,
        },
    }
}

fn survive_create_vertex(net: &[DeltaRow], vid: VId) -> EffectFate {
    match net
        .iter()
        .position(|row| matches!(row, DeltaRow::CreateVertex { vid: id, .. } if *id == vid))
    {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        },
    }
}

fn survive_create_edge(net: &[DeltaRow], eid: EId) -> EffectFate {
    match net
        .iter()
        .position(|row| matches!(row, DeltaRow::CreateEdge { eid: id, .. } if *id == eid))
    {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        },
    }
}

fn survive_delete_vertex(net: &[DeltaRow], vid: VId) -> EffectFate {
    match net
        .iter()
        .position(|row| matches!(row, DeltaRow::DeleteVertex { vid: id, .. } if *id == vid))
    {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        },
    }
}

fn survive_delete_edge(net: &[DeltaRow], eid: EId) -> EffectFate {
    match net
        .iter()
        .position(|row| matches!(row, DeltaRow::DeleteEdge { eid: id, .. } if *id == eid))
    {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        },
    }
}

fn element_created(elem: ElementId, ids: IdentityPartition<'_>) -> bool {
    match elem {
        ElementId::Vertex(vid) => ids.created_vids.contains(&vid),
        ElementId::Edge(eid) => ids.created_eids.contains(&eid),
    }
}

fn element_deleted(elem: ElementId, ids: IdentityPartition<'_>) -> bool {
    match elem {
        ElementId::Vertex(vid) => ids.deleted_vids.contains(&vid),
        ElementId::Edge(eid) => ids.deleted_eids.contains(&eid) || ids.cascaded_eids.contains(&eid),
    }
}

fn element_is_live(elem: ElementId, ids: IdentityPartition<'_>) -> bool {
    match elem {
        ElementId::Vertex(vid) => ids.live_vids.contains(&vid),
        ElementId::Edge(eid) => ids.live_eids.contains(&eid),
    }
}

fn classify_property(
    net: &[DeltaRow],
    elem: ElementId,
    property: PropertyKeyId,
    ids: IdentityPartition<'_>,
) -> EffectFate {
    if element_created(elem, ids) || element_deleted(elem, ids) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::Absorbed,
        };
    }
    if !element_is_live(elem, ids) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        };
    }
    match net.iter().position(|row| {
        matches!(row, DeltaRow::Property { elem: e, property: p, .. } if *e == elem && *p == property)
    }) {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::Identity,
        },
    }
}

fn classify_label(
    net: &[DeltaRow],
    vid: VId,
    label: LabelId,
    ids: IdentityPartition<'_>,
) -> EffectFate {
    if ids.created_vids.contains(&vid) || ids.deleted_vids.contains(&vid) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::Absorbed,
        };
    }
    if !ids.live_vids.contains(&vid) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        };
    }
    match net.iter().position(|row| {
        matches!(row, DeltaRow::LabelMembership { vid: id, label: l, .. } if *id == vid && *l == label)
    }) {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::Identity,
        },
    }
}

fn classify_valid_time(
    net: &[DeltaRow],
    elem: ElementId,
    ids: IdentityPartition<'_>,
) -> EffectFate {
    if element_created(elem, ids) || element_deleted(elem, ids) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::Absorbed,
        };
    }
    if !element_is_live(elem, ids) {
        return EffectFate::NoOp {
            policy: NoOpPolicy::InverseCancellation,
        };
    }
    match net
        .iter()
        .position(|row| matches!(row, DeltaRow::ValidTime { elem: e, .. } if *e == elem))
    {
        Some(dest_index) => EffectFate::Survives { dest_index },
        None => EffectFate::NoOp {
            policy: NoOpPolicy::Identity,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{EffectFate, NoOpPolicy, normalize};
    use crate::ReferenceGraph;
    use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, EId, VId};

    const PROP: PropertyKeyId = PropertyKeyId(100);
    const LABEL: LabelId = LabelId(10);

    fn int(value: i64) -> CanonicalScalar {
        CanonicalScalar::Int(value)
    }

    fn create(vid: u128, value: i64) -> DeltaRow {
        DeltaRow::CreateVertex {
            vid: VId(vid),
            birth_ordinal: vid as u64,
            labels: vec![LABEL],
            props: vec![(PROP, int(value))],
            valid_time: None,
        }
    }

    fn property(vid: u128, before: i64, after: i64) -> DeltaRow {
        DeltaRow::Property {
            elem: ElementId::Vertex(VId(vid)),
            property: PROP,
            before: Some(int(before)),
            after: Some(int(after)),
        }
    }

    fn apply(graph: &mut ReferenceGraph, rows: &[DeltaRow]) {
        for row in rows {
            graph.apply_row(row).expect("row applies");
        }
    }

    #[test]
    fn chained_property_writes_collapse_to_first_before_last_after() {
        let mut basis = ReferenceGraph::new();
        apply(&mut basis, &[create(1, 5)]);
        let mut workspace = basis.clone();
        let source = [property(1, 5, 3), property(1, 3, 7)];
        apply(&mut workspace, &source);

        let form = normalize(&basis, &workspace, &source);
        assert_eq!(
            form.rows,
            vec![DeltaRow::Property {
                elem: ElementId::Vertex(VId(1)),
                property: PROP,
                before: Some(int(5)),
                after: Some(int(7)),
            }]
        );
        assert_eq!(
            form.mapping.iter().map(|m| m.fate).collect::<Vec<_>>(),
            vec![
                EffectFate::Survives { dest_index: 0 },
                EffectFate::Survives { dest_index: 0 },
            ]
        );
    }

    #[test]
    fn identity_property_chain_emits_nothing() {
        let mut basis = ReferenceGraph::new();
        apply(&mut basis, &[create(1, 5)]);
        let mut workspace = basis.clone();
        let source = [property(1, 5, 3), property(1, 3, 5)];
        apply(&mut workspace, &source);

        let form = normalize(&basis, &workspace, &source);
        assert!(form.rows.is_empty());
        assert!(form.mapping.iter().all(|m| m.fate
            == EffectFate::NoOp {
                policy: NoOpPolicy::Identity
            }));
    }

    #[test]
    fn create_then_delete_cancels() {
        let basis = ReferenceGraph::new();
        let mut workspace = basis.clone();
        // Apply only the live prefix: delete needs the post-create version.
        apply(&mut workspace, &[create(1, 5), property(1, 5, 9)]);
        let version = workspace.vertex(VId(1)).expect("created").version;
        let delete = DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: version,
            sorted_retired_incident_edges: vec![],
        };
        apply(&mut workspace, std::slice::from_ref(&delete));
        let source = [create(1, 5), property(1, 5, 9), delete];

        let form = normalize(&basis, &workspace, &source);
        assert!(form.rows.is_empty(), "net rows: {:?}", form.rows);
        assert!(form.mapping.iter().all(|m| matches!(
            m.fate,
            EffectFate::NoOp {
                policy: NoOpPolicy::InverseCancellation | NoOpPolicy::Absorbed
            }
        )));
    }

    #[test]
    fn property_then_delete_emits_basis_version_delete() {
        let mut basis = ReferenceGraph::new();
        apply(&mut basis, &[create(1, 5)]);
        let basis_version = basis.vertex(VId(1)).expect("seeded").version;
        let mut workspace = basis.clone();
        apply(&mut workspace, &[property(1, 5, 3)]);
        let delete = DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: workspace.vertex(VId(1)).expect("still live").version,
            sorted_retired_incident_edges: vec![],
        };
        apply(&mut workspace, std::slice::from_ref(&delete));
        let source = [property(1, 5, 3), delete];

        let form = normalize(&basis, &workspace, &source);
        assert_eq!(
            form.rows,
            vec![DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: basis_version,
                sorted_retired_incident_edges: vec![],
            }]
        );
        assert_eq!(
            form.mapping[0].fate,
            EffectFate::NoOp {
                policy: NoOpPolicy::Absorbed
            }
        );
        assert_eq!(form.mapping[1].fate, EffectFate::Survives { dest_index: 0 });
    }

    #[test]
    fn create_plus_property_folds_into_create() {
        let basis = ReferenceGraph::new();
        let mut workspace = basis.clone();
        apply(&mut workspace, &[create(1, 5), property(1, 5, 9)]);
        let source = [create(1, 5), property(1, 5, 9)];

        let form = normalize(&basis, &workspace, &source);
        assert_eq!(
            form.rows,
            vec![DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 1,
                labels: vec![LABEL],
                props: vec![(PROP, int(9))],
                valid_time: None,
            }]
        );
        assert_eq!(form.mapping[0].fate, EffectFate::Survives { dest_index: 0 });
        assert_eq!(
            form.mapping[1].fate,
            EffectFate::NoOp {
                policy: NoOpPolicy::Absorbed
            }
        );
    }

    fn create_edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
        DeltaRow::CreateEdge {
            eid: EId(eid),
            birth_ordinal: eid as u64,
            src: VId(src),
            relation: RelationId(1),
            dst: VId(dst),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        }
    }

    #[test]
    fn dual_endpoint_delete_keeps_a_shared_eid_on_the_smallest_vid() {
        let mut basis = ReferenceGraph::new();
        apply(
            &mut basis,
            &[
                create(1, 0),
                create(2, 0),
                create(3, 0),
                create_edge(10, 1, 2),
                create_edge(11, 2, 3),
            ],
        );
        let v1 = basis.vertex(VId(1)).expect("seeded").version;
        let v2 = basis.vertex(VId(2)).expect("seeded").version;

        // After-image: both endpoints gone. Apply the net-safe partition so
        // apply_row's cascade law can build that graph at all.
        let mut workspace = basis.clone();
        apply(
            &mut workspace,
            &[
                DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: v1,
                    sorted_retired_incident_edges: vec![EId(10)],
                },
                DeltaRow::DeleteVertex {
                    vid: VId(2),
                    before_version: v2,
                    sorted_retired_incident_edges: vec![EId(11)],
                },
            ],
        );

        // Evaluation order deleted the larger endpoint first with the full
        // incident set — the shape that is not apply-safe until normalize
        // reassigns the shared eid.
        let source = [
            DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: v2,
                sorted_retired_incident_edges: vec![EId(10), EId(11)],
            },
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: v1,
                sorted_retired_incident_edges: vec![EId(10)],
            },
        ];
        let form = normalize(&basis, &workspace, &source);
        assert_eq!(
            form.rows,
            vec![
                DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: v1,
                    sorted_retired_incident_edges: vec![EId(10)],
                },
                DeltaRow::DeleteVertex {
                    vid: VId(2),
                    before_version: v2,
                    sorted_retired_incident_edges: vec![EId(11)],
                },
            ]
        );

        let mut replay = basis.clone();
        apply(&mut replay, &form.rows);
        assert!(replay.vertex(VId(1)).is_none());
        assert!(replay.vertex(VId(2)).is_none());
        assert!(replay.edge(EId(10)).is_none());
        assert!(replay.edge(EId(11)).is_none());
        assert!(replay.vertex(VId(3)).is_some());
    }
}
