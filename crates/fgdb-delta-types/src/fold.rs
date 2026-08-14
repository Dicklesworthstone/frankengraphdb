//! Fold evaluation-order delta rows into a target-disjoint net.
//!
//! `LogicalDeltaTemplate::build` byte-sorts rows. Byte order is not
//! applicability order, so a multi-write batch whose rows share a target
//! cannot be committed unless this fold runs first (fgdb-p6tm /
//! fgdb-w5-effects-normal-form-819.2).
//!
//! This is the row-list form of the live NetEffectNormalForm subset. The
//! reference oracle diffs two graphs instead; both must produce the same
//! family of net rows for the same intent sequence.

use crate::{DeltaRow, ElementId, LabelId, PropertyKeyId, ValidTimePeriod};
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};
use std::collections::BTreeMap;

/// Collapse `rows` (evaluation order) so remaining rows are target-disjoint
/// and therefore safe to byte-sort before apply.
pub fn fold_target_disjoint(rows: Vec<DeltaRow>) -> Vec<DeltaRow> {
    let mut vertices: BTreeMap<VId, VertexNet> = BTreeMap::new();
    let mut edges: BTreeMap<EId, EdgeNet> = BTreeMap::new();
    let mut pass_through = Vec::new();

    for row in rows {
        match row {
            DeltaRow::CreateVertex {
                vid,
                birth_ordinal,
                labels,
                props,
                valid_time,
            } => {
                let net = vertices.entry(vid).or_default();
                // Identities never recycle (§6.2). A prior delete or a
                // same-batch create-delete already spent this VId; a later
                // create must not wipe that and emit a resurrection (fgdb-iv5z).
                if net.cancelled || net.deleted.is_some() {
                    continue;
                }
                net.created = Some(CreatedVertex {
                    birth_ordinal,
                    labels: labels.clone(),
                    props: props.clone(),
                    valid_time,
                });
                net.cancelled = false;
                net.deleted = None;
                net.props.clear();
                net.labels.clear();
                for label in labels {
                    net.labels.insert(label, (false, true));
                }
                for (key, value) in props {
                    net.props.insert(key, (None, Some(value)));
                }
                net.valid_time = valid_time.map(|after| (None, Some(after)));
            }
            DeltaRow::DeleteVertex {
                vid,
                before_version,
                sorted_retired_incident_edges,
            } => {
                for eid in &sorted_retired_incident_edges {
                    absorb_edge(&mut edges, *eid);
                }
                let durable_cascade: Vec<EId> = sorted_retired_incident_edges
                    .into_iter()
                    .filter(|eid| edges.get(eid).is_none_or(|edge| !edge.cancelled))
                    .collect();
                let net = vertices.entry(vid).or_default();
                if net.created.is_some() {
                    *net = VertexNet::default();
                    net.cancelled = true;
                } else {
                    net.deleted = Some(DeletedVertex {
                        before_version,
                        sorted_retired_incident_edges: durable_cascade,
                    });
                    net.props.clear();
                    net.labels.clear();
                    net.valid_time = None;
                }
            }
            DeltaRow::CreateEdge {
                eid,
                birth_ordinal,
                src,
                relation,
                dst,
                canonical_key,
                props,
                valid_time,
            } => {
                let net = edges.entry(eid).or_default();
                if net.cancelled || net.deleted.is_some() || net.cascade_owned {
                    continue;
                }
                net.created = Some(CreatedEdge {
                    birth_ordinal,
                    src,
                    relation,
                    dst,
                    canonical_key,
                    props: props.clone(),
                    valid_time,
                });
                net.cancelled = false;
                net.deleted = None;
                net.props.clear();
                for (key, value) in props {
                    net.props.insert(key, (None, Some(value)));
                }
            }
            DeltaRow::DeleteEdge {
                eid,
                before_version,
            } => {
                absorb_edge_with_delete(&mut edges, eid, before_version);
            }
            DeltaRow::Property {
                elem,
                property,
                before,
                after,
            } => match elem {
                ElementId::Vertex(vid) => {
                    let net = vertices.entry(vid).or_default();
                    if net.cancelled || net.deleted.is_some() {
                        continue;
                    }
                    if let Some(created) = &mut net.created {
                        apply_prop_map(&mut created.props, property, after);
                        continue;
                    }
                    fold_prop(&mut net.props, property, before, after);
                }
                ElementId::Edge(eid) => {
                    let net = edges.entry(eid).or_default();
                    if net.cancelled || net.deleted.is_some() || net.cascade_owned {
                        continue;
                    }
                    if let Some(created) = &mut net.created {
                        apply_prop_map(&mut created.props, property, after);
                        continue;
                    }
                    fold_prop(&mut net.props, property, before, after);
                }
            },
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } => {
                let net = vertices.entry(vid).or_default();
                if net.cancelled || net.deleted.is_some() {
                    continue;
                }
                if let Some(created) = &mut net.created {
                    apply_label_list(&mut created.labels, label, after);
                    continue;
                }
                let entry = net.labels.entry(label).or_insert((before, after));
                entry.1 = after;
            }
            DeltaRow::ValidTime {
                elem,
                contract_id,
                before,
                after,
            } => match elem {
                ElementId::Vertex(vid) => {
                    let net = vertices.entry(vid).or_default();
                    if net.cancelled || net.deleted.is_some() {
                        continue;
                    }
                    if let Some(created) = &mut net.created {
                        created.valid_time = after;
                        continue;
                    }
                    net.valid_time = Some(match net.valid_time {
                        None => (before, after),
                        Some((first, _)) => (first, after),
                    });
                    net.valid_time_contract = Some(contract_id);
                }
                ElementId::Edge(eid) => {
                    let net = edges.entry(eid).or_default();
                    if net.cancelled || net.deleted.is_some() || net.cascade_owned {
                        continue;
                    }
                    if let Some(created) = &mut net.created {
                        created.valid_time = after;
                        continue;
                    }
                    net.valid_time = Some(match net.valid_time {
                        None => (before, after),
                        Some((first, _)) => (first, after),
                    });
                    net.valid_time_contract = Some(contract_id);
                }
            },
            other => pass_through.push(other),
        }
    }

    // NENF emits DeleteVertex in VId order. A shared cascade eid must sit
    // only on the smallest deleted VId so the first-applied cascade still
    // equals the live incident set (fgdb-s9ja / fgdb-cczg).
    let mut claimed: BTreeMap<EId, VId> = BTreeMap::new();
    for (vid, net) in &vertices {
        if let Some(deleted) = &net.deleted {
            for eid in &deleted.sorted_retired_incident_edges {
                claimed
                    .entry(*eid)
                    .and_modify(|owner| {
                        if *vid < *owner {
                            *owner = *vid;
                        }
                    })
                    .or_insert(*vid);
            }
        }
    }
    if !claimed.is_empty() {
        for (vid, net) in &mut vertices {
            if let Some(deleted) = &mut net.deleted {
                deleted
                    .sorted_retired_incident_edges
                    .retain(|eid| claimed.get(eid) == Some(vid));
            }
        }
    }

    let mut out = Vec::new();
    for (vid, net) in vertices {
        if net.cancelled {
            continue;
        }
        if let Some(created) = net.created {
            out.push(DeltaRow::CreateVertex {
                vid,
                birth_ordinal: created.birth_ordinal,
                labels: created.labels,
                props: created.props,
                valid_time: created.valid_time,
            });
            continue;
        }
        if let Some(deleted) = net.deleted {
            out.push(DeltaRow::DeleteVertex {
                vid,
                before_version: deleted.before_version,
                sorted_retired_incident_edges: deleted.sorted_retired_incident_edges,
            });
            continue;
        }
        for (label, (before, after)) in net.labels {
            if before != after {
                out.push(DeltaRow::LabelMembership {
                    vid,
                    label,
                    before,
                    after,
                });
            }
        }
        emit_props(&mut out, ElementId::Vertex(vid), net.props);
        emit_valid_time(
            &mut out,
            ElementId::Vertex(vid),
            net.valid_time_contract,
            net.valid_time,
        );
    }
    for (eid, net) in edges {
        if net.cancelled {
            continue;
        }
        if let Some(created) = net.created {
            out.push(DeltaRow::CreateEdge {
                eid,
                birth_ordinal: created.birth_ordinal,
                src: created.src,
                relation: created.relation,
                dst: created.dst,
                canonical_key: created.canonical_key,
                props: created.props,
                valid_time: created.valid_time,
            });
            continue;
        }
        if let Some(before_version) = net.deleted {
            out.push(DeltaRow::DeleteEdge {
                eid,
                before_version,
            });
            continue;
        }
        emit_props(&mut out, ElementId::Edge(eid), net.props);
        emit_valid_time(
            &mut out,
            ElementId::Edge(eid),
            net.valid_time_contract,
            net.valid_time,
        );
    }
    out.extend(pass_through);
    out
}

#[derive(Default)]
struct VertexNet {
    created: Option<CreatedVertex>,
    deleted: Option<DeletedVertex>,
    cancelled: bool,
    props: BTreeMap<PropertyKeyId, (Option<CanonicalScalar>, Option<CanonicalScalar>)>,
    labels: BTreeMap<LabelId, (bool, bool)>,
    valid_time: Option<(Option<ValidTimePeriod>, Option<ValidTimePeriod>)>,
    valid_time_contract: Option<ObjectId>,
}

struct CreatedVertex {
    birth_ordinal: u64,
    labels: Vec<LabelId>,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
    valid_time: Option<ValidTimePeriod>,
}

struct DeletedVertex {
    before_version: ObjectId,
    sorted_retired_incident_edges: Vec<EId>,
}

#[derive(Default)]
struct EdgeNet {
    created: Option<CreatedEdge>,
    deleted: Option<ObjectId>,
    cancelled: bool,
    /// A DeleteVertex cascade already owns this durable identity. Later
    /// DeleteEdge / Property / ValidTime rows must not re-emit: apply is
    /// tag-ordered (`DeleteVertex` before `DeleteEdge` / `Property`), so a
    /// leftover row would hit a retired edge (fgdb-qgk9).
    cascade_owned: bool,
    props: BTreeMap<PropertyKeyId, (Option<CanonicalScalar>, Option<CanonicalScalar>)>,
    valid_time: Option<(Option<ValidTimePeriod>, Option<ValidTimePeriod>)>,
    valid_time_contract: Option<ObjectId>,
}

struct CreatedEdge {
    birth_ordinal: u64,
    src: VId,
    relation: crate::RelationId,
    dst: VId,
    canonical_key: Option<CanonicalScalar>,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
    valid_time: Option<ValidTimePeriod>,
}

fn absorb_edge(edges: &mut BTreeMap<EId, EdgeNet>, eid: EId) {
    let net = edges.entry(eid).or_default();
    if net.created.is_some() {
        // Same-batch create: the identity never becomes durable. Drop it
        // from the cascade image too (`cancelled`).
        *net = EdgeNet::default();
        net.cancelled = true;
    } else {
        // Basis edge: the vertex delete's cascade image owns the retirement.
        // Do not emit a standalone DeleteEdge, but keep the eid in the
        // cascade so apply can check the durable incident set.
        net.props.clear();
        net.valid_time = None;
        net.deleted = None;
        net.cascade_owned = true;
    }
}

fn absorb_edge_with_delete(edges: &mut BTreeMap<EId, EdgeNet>, eid: EId, before_version: ObjectId) {
    let net = edges.entry(eid).or_default();
    if net.created.is_some() {
        *net = EdgeNet::default();
        net.cancelled = true;
    } else if net.cascade_owned {
        // Already claimed by a DeleteVertex cascade. A later DeleteEdge
        // must not resurrect a standalone row (fgdb-qgk9).
    } else if !net.cancelled {
        net.deleted = Some(before_version);
        net.props.clear();
        net.valid_time = None;
    }
}

fn apply_prop_map(
    props: &mut Vec<(PropertyKeyId, CanonicalScalar)>,
    key: PropertyKeyId,
    after: Option<CanonicalScalar>,
) {
    if let Some(at) = props.iter().position(|(k, _)| *k == key) {
        match after {
            Some(value) => props[at].1 = value,
            None => {
                props.remove(at);
            }
        }
    } else if let Some(value) = after {
        props.push((key, value));
        props.sort_by_key(|(k, _)| k.0);
    }
}

fn apply_label_list(labels: &mut Vec<LabelId>, label: LabelId, after: bool) {
    match (labels.iter().position(|l| *l == label), after) {
        (Some(at), false) => {
            labels.remove(at);
        }
        (None, true) => {
            labels.push(label);
            labels.sort_by_key(|l| l.0);
        }
        _ => {}
    }
}

fn fold_prop(
    map: &mut BTreeMap<PropertyKeyId, (Option<CanonicalScalar>, Option<CanonicalScalar>)>,
    key: PropertyKeyId,
    before: Option<CanonicalScalar>,
    after: Option<CanonicalScalar>,
) {
    let entry = map.entry(key).or_insert((before, after.clone()));
    entry.1 = after;
}

fn emit_props(
    out: &mut Vec<DeltaRow>,
    elem: ElementId,
    props: BTreeMap<PropertyKeyId, (Option<CanonicalScalar>, Option<CanonicalScalar>)>,
) {
    for (property, (before, after)) in props {
        if before != after {
            out.push(DeltaRow::Property {
                elem,
                property,
                before,
                after,
            });
        }
    }
}

fn emit_valid_time(
    out: &mut Vec<DeltaRow>,
    elem: ElementId,
    contract_id: Option<ObjectId>,
    pair: Option<(Option<ValidTimePeriod>, Option<ValidTimePeriod>)>,
) {
    let Some((before, after)) = pair else {
        return;
    };
    if before == after {
        return;
    }
    out.push(DeltaRow::ValidTime {
        elem,
        contract_id: contract_id.unwrap_or(ObjectId([0u8; 32])),
        before,
        after,
    });
}

#[cfg(test)]
mod tests {
    use super::fold_target_disjoint;
    use crate::{DeltaRow, ElementId, PropertyKeyId};
    use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};

    fn prop(vid: u128, before: i64, after: i64) -> DeltaRow {
        DeltaRow::Property {
            elem: ElementId::Vertex(VId(vid)),
            property: PropertyKeyId(1),
            before: Some(CanonicalScalar::Int(before)),
            after: Some(CanonicalScalar::Int(after)),
        }
    }

    #[test]
    fn two_sets_collapse_to_first_before_last_after() {
        let out = fold_target_disjoint(vec![prop(1, 5, 3), prop(1, 3, 7)]);
        assert_eq!(
            out,
            vec![DeltaRow::Property {
                elem: ElementId::Vertex(VId(1)),
                property: PropertyKeyId(1),
                before: Some(CanonicalScalar::Int(5)),
                after: Some(CanonicalScalar::Int(7)),
            }]
        );
    }

    #[test]
    fn set_then_delete_keeps_only_the_delete() {
        let version = ObjectId([0x11; 32]);
        let out = fold_target_disjoint(vec![
            prop(1, 5, 3),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            },
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            }]
        );
    }

    #[test]
    fn same_batch_edge_create_is_stripped_from_a_basis_vertex_delete_cascade() {
        let version = ObjectId([0x33; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::CreateEdge {
                eid: fgdb_types::EId(1000),
                birth_ordinal: 2,
                src: VId(1),
                relation: crate::RelationId(1),
                dst: VId(2),
                canonical_key: None,
                props: vec![],
                valid_time: None,
            },
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![fgdb_types::EId(1000)],
            },
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            }]
        );
    }

    #[test]
    fn create_set_delete_cancels() {
        let out = fold_target_disjoint(vec![
            DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 1,
                labels: vec![],
                props: vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
                valid_time: None,
            },
            prop(1, 1, 4),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0x22; 32]),
                sorted_retired_incident_edges: vec![],
            },
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn shared_cascade_eids_stay_on_the_smallest_deleted_vid() {
        let version = ObjectId([0x44; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10), EId(11)],
            },
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10)],
            },
        ]);
        assert_eq!(
            out,
            vec![
                DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: version,
                    sorted_retired_incident_edges: vec![EId(10)],
                },
                DeltaRow::DeleteVertex {
                    vid: VId(2),
                    before_version: version,
                    sorted_retired_incident_edges: vec![EId(11)],
                },
            ]
        );
    }

    #[test]
    fn delete_vertex_then_delete_edge_stays_inside_the_cascade() {
        let version = ObjectId([0x55; 32]);
        let delete_vertex = DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: version,
            sorted_retired_incident_edges: vec![EId(10)],
        };
        let delete_edge = DeltaRow::DeleteEdge {
            eid: EId(10),
            before_version: version,
        };
        let expected = vec![delete_vertex.clone()];
        assert_eq!(
            fold_target_disjoint(vec![delete_vertex.clone(), delete_edge.clone()]),
            expected
        );
        assert_eq!(
            fold_target_disjoint(vec![delete_edge, delete_vertex]),
            expected
        );
    }

    #[test]
    fn delete_vertex_then_edge_property_does_not_reemit() {
        let version = ObjectId([0x66; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10)],
            },
            DeltaRow::Property {
                elem: ElementId::Edge(EId(10)),
                property: PropertyKeyId(1),
                before: Some(CanonicalScalar::Int(1)),
                after: Some(CanonicalScalar::Int(2)),
            },
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10)],
            }]
        );
    }

    fn create_vertex(vid: u128, ordinal: u64) -> DeltaRow {
        DeltaRow::CreateVertex {
            vid: VId(vid),
            birth_ordinal: ordinal,
            labels: vec![],
            props: vec![],
            valid_time: None,
        }
    }

    #[test]
    fn delete_then_create_keeps_the_basis_delete() {
        let version = ObjectId([0x77; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            },
            create_vertex(1, 9),
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            }]
        );
    }

    #[test]
    fn create_delete_create_stays_cancelled() {
        let out = fold_target_disjoint(vec![
            create_vertex(1, 1),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0x88; 32]),
                sorted_retired_incident_edges: vec![],
            },
            create_vertex(1, 2),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn delete_edge_then_create_keeps_the_delete() {
        let version = ObjectId([0x99; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: version,
            },
            DeltaRow::CreateEdge {
                eid: EId(10),
                birth_ordinal: 3,
                src: VId(1),
                relation: crate::RelationId(1),
                dst: VId(2),
                canonical_key: None,
                props: vec![],
                valid_time: None,
            },
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: version,
            }]
        );
    }
}
