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
                // A second Create of a still-born identity is AlreadyLive at
                // apply. Last-wins used to replace the first birth and drop
                // any Property/Label that had already folded into it.
                if net.cancelled || net.deleted.is_some() || net.created.is_some() {
                    continue;
                }
                // Apply is tag-order: CreateVertex (0x01) before Label (0x05)
                // and Property (0x06). A hostile update-before-create must
                // bake into the birth the same way create-then-update does.
                // Clearing the pending maps here used to drop that state, so
                // fold(update, create) omitted rows apply of the byte-sorted
                // pair would write.
                let pending_labels = std::mem::take(&mut net.labels);
                let pending_props = std::mem::take(&mut net.props);
                let pending_valid_time = net.valid_time.take();
                net.created = Some(CreatedVertex {
                    birth_ordinal,
                    labels: labels.clone(),
                    props: props.clone(),
                    valid_time,
                });
                net.cancelled = false;
                net.deleted = None;
                if let Some(created) = net.created.as_mut() {
                    for (label, (_, after)) in pending_labels {
                        apply_label_list(&mut created.labels, label, after);
                    }
                    for (key, (_, after)) in pending_props {
                        apply_prop_map(&mut created.props, key, after);
                    }
                    if let Some((_, after)) = pending_valid_time {
                        created.valid_time = after;
                    }
                }
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
                // Same-batch incident creates must cancel even if the cascade
                // list omitted them. Apply births CreateEdge before
                // DeleteVertex; a leftover create is CascadeImageMismatch
                // on a basis delete (fgdb-aaz7) and DanglingEndpoint when
                // the vertex itself is cancelled (fgdb-chx6).
                cancel_incident_created_edges(&mut edges, vid);
                let created_here = vertices.get(&vid).is_some_and(|net| net.created.is_some());
                if created_here {
                    // A never-born vertex cannot cascade-own a basis eid.
                    let net = vertices.entry(vid).or_default();
                    *net = VertexNet::default();
                    net.cancelled = true;
                } else {
                    for eid in &sorted_retired_incident_edges {
                        // A cascade that names a same-batch create which is
                        // not incident to this vid must not assassinate it.
                        // Absorb would cancel the create and strip it from
                        // the durable image, so apply would succeed minus
                        // an unrelated edge (fgdb-kfta). Leave it; apply
                        // refuses CascadeImageMismatch.
                        if created_edge_is_non_incident(&edges, *eid, vid) {
                            continue;
                        }
                        absorb_edge(&mut edges, *eid);
                    }
                    let durable_cascade: Vec<EId> = sorted_retired_incident_edges
                        .into_iter()
                        .filter(|eid| edges.get(eid).is_none_or(|edge| !edge.cancelled))
                        .collect();
                    let net = vertices.entry(vid).or_default();
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
                if net.cancelled
                    || net.deleted.is_some()
                    || net.cascade_owned
                    || net.created.is_some()
                {
                    continue;
                }
                // Apply is tag-order: CreateEdge (0x02) before DeleteVertex
                // (0x03). A create against an endpoint this batch already
                // deleted or cancelled cannot become durable — emitting it
                // either dangles (vertex rows cancelled) or is born before
                // the cascade image is checked (fgdb-c3ru).
                if vertex_endpoint_gone(&vertices, src) || vertex_endpoint_gone(&vertices, dst) {
                    *net = EdgeNet::default();
                    net.cancelled = true;
                    continue;
                }
                let pending_props = std::mem::take(&mut net.props);
                let pending_valid_time = net.valid_time.take();
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
                if let Some(created) = net.created.as_mut() {
                    for (key, (_, after)) in pending_props {
                        apply_prop_map(&mut created.props, key, after);
                    }
                    if let Some((_, after)) = pending_valid_time {
                        created.valid_time = after;
                    }
                }
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

fn vertex_endpoint_gone(vertices: &BTreeMap<VId, VertexNet>, vid: VId) -> bool {
    vertices
        .get(&vid)
        .is_some_and(|net| net.cancelled || net.deleted.is_some())
}

fn created_edge_is_non_incident(edges: &BTreeMap<EId, EdgeNet>, eid: EId, vid: VId) -> bool {
    edges
        .get(&eid)
        .and_then(|net| net.created.as_ref())
        .is_some_and(|created| created.src != vid && created.dst != vid)
}

fn cancel_incident_created_edges(edges: &mut BTreeMap<EId, EdgeNet>, vid: VId) {
    for net in edges.values_mut() {
        let incident = net
            .created
            .as_ref()
            .is_some_and(|created| created.src == vid || created.dst == vid);
        if incident {
            *net = EdgeNet::default();
            net.cancelled = true;
        }
    }
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
    use crate::{DeltaRow, ElementId, LabelId, PropertyKeyId};
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

    /// A basis delete that omits a same-batch incident create must still
    /// drop the create: apply would birth the edge, then refuse the cascade
    /// image (fgdb-aaz7).
    #[test]
    fn same_batch_edge_create_is_stripped_even_when_omitted_from_a_basis_delete_cascade() {
        let version = ObjectId([0x33; 32]);
        let out = fold_target_disjoint(vec![
            create_edge(1000, 1, 2),
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

    /// Claimed-map min VId is apply order only because `u128` vids encode
    /// big-endian. Little-endian would apply VId(256) before VId(1) and the
    /// stripped cascade would CascadeImageMismatch.
    #[test]
    fn a_shared_cascade_on_vid_256_still_belongs_to_vid_1_in_byte_order() {
        let version = ObjectId([0x44; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(256),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10)],
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
                    vid: VId(256),
                    before_version: version,
                    sorted_retired_incident_edges: vec![],
                },
            ]
        );
        let first = out[0].canonical_bytes().expect("encodes");
        let second = out[1].canonical_bytes().expect("encodes");
        assert!(
            first < second,
            "encoded DeleteVertex VId(1) must sort before VId(256)"
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

    fn create_edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
        DeltaRow::CreateEdge {
            eid: EId(eid),
            birth_ordinal: eid as u64,
            src: VId(src),
            relation: crate::RelationId(1),
            dst: VId(dst),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        }
    }

    /// DeleteVertex then CreateEdge to that vertex must not emit the create:
    /// apply would birth the edge before the cascade runs (fgdb-c3ru).
    #[test]
    fn create_edge_after_endpoint_delete_is_dropped() {
        let version = ObjectId([0xaa; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            },
            create_edge(10, 1, 2),
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
    fn create_edge_after_cancelled_endpoint_is_dropped() {
        let out = fold_target_disjoint(vec![
            create_vertex(1, 1),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0xbb; 32]),
                sorted_retired_incident_edges: vec![],
            },
            create_edge(10, 1, 2),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn property_after_dropped_create_against_dead_endpoint_does_not_reemit() {
        let version = ObjectId([0xcc; 32]);
        let out = fold_target_disjoint(vec![
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![],
            },
            create_edge(10, 1, 2),
            DeltaRow::Property {
                elem: ElementId::Edge(EId(10)),
                property: PropertyKeyId(1),
                before: None,
                after: Some(CanonicalScalar::Int(9)),
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

    /// CreateVertex + CreateEdge + DeleteVertex with an empty cascade must
    /// still drop the edge: apply never sees the cancelled vertex (fgdb-chx6).
    #[test]
    fn cancelled_vertex_drops_incident_creates_even_without_a_cascade() {
        let out = fold_target_disjoint(vec![
            create_vertex(1, 1),
            create_edge(10, 1, 2),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0xdd; 32]),
                sorted_retired_incident_edges: vec![],
            },
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn cancelled_vertex_does_not_swallow_a_basis_eid_from_a_hostile_cascade() {
        let version = ObjectId([0xee; 32]);
        let out = fold_target_disjoint(vec![
            create_vertex(1, 1),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(10)],
            },
            DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: version,
            },
        ]);
        assert_eq!(
            out,
            vec![DeltaRow::DeleteEdge {
                eid: EId(10),
                before_version: version,
            }],
            "a never-born vertex must not cascade-own a basis eid"
        );
    }

    /// A basis DeleteVertex cascade that names a same-batch create which
    /// does not touch this vid must not cancel that create (fgdb-kfta).
    #[test]
    fn a_hostile_cascade_does_not_assassinate_a_non_incident_create() {
        let version = ObjectId([0xff; 32]);
        let out = fold_target_disjoint(vec![
            create_edge(99, 2, 3),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: version,
                sorted_retired_incident_edges: vec![EId(99)],
            },
        ]);
        assert_eq!(
            out,
            vec![
                DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: version,
                    sorted_retired_incident_edges: vec![EId(99)],
                },
                create_edge(99, 2, 3),
            ],
            "the unrelated create must survive so apply can refuse the cascade"
        );
    }

    #[test]
    fn a_self_loop_create_is_incident_and_cancels_with_its_vertex() {
        let out = fold_target_disjoint(vec![
            create_vertex(1, 1),
            create_edge(10, 1, 1),
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0x01; 32]),
                sorted_retired_incident_edges: vec![EId(10)],
            },
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    /// Apply is Create (0x01) then Property (0x06). Hostile update-before-create
    /// must match create-then-update so the birth carries the net after.
    #[test]
    fn property_before_create_folds_the_same_as_create_then_property() {
        let create = DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        };
        let property = DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(1),
            before: None,
            after: Some(CanonicalScalar::Int(5)),
        };
        let expected = vec![DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
            valid_time: None,
        }];
        assert_eq!(
            fold_target_disjoint(vec![create.clone(), property.clone()]),
            expected
        );
        assert_eq!(
            fold_target_disjoint(vec![property, create]),
            expected,
            "Property-before-Create must bake into the create"
        );
    }

    #[test]
    fn label_before_create_folds_the_same_as_create_then_label() {
        let create = DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        };
        let label = DeltaRow::LabelMembership {
            vid: VId(1),
            label: LabelId(3),
            before: false,
            after: true,
        };
        let expected = vec![DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![LabelId(3)],
            props: vec![],
            valid_time: None,
        }];
        assert_eq!(
            fold_target_disjoint(vec![create.clone(), label.clone()]),
            expected
        );
        assert_eq!(
            fold_target_disjoint(vec![label, create]),
            expected,
            "Label-before-Create must bake into the create"
        );
    }

    #[test]
    fn property_before_create_edge_folds_the_same_as_create_then_property() {
        let create = create_edge(10, 1, 2);
        let property = DeltaRow::Property {
            elem: ElementId::Edge(EId(10)),
            property: PropertyKeyId(1),
            before: None,
            after: Some(CanonicalScalar::Int(7)),
        };
        let expected = vec![DeltaRow::CreateEdge {
            eid: EId(10),
            birth_ordinal: 10,
            src: VId(1),
            relation: crate::RelationId(1),
            dst: VId(2),
            canonical_key: None,
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
            valid_time: None,
        }];
        assert_eq!(
            fold_target_disjoint(vec![create.clone(), property.clone()]),
            expected
        );
        assert_eq!(
            fold_target_disjoint(vec![property, create]),
            expected,
            "Property-before-CreateEdge must bake into the create"
        );
    }

    /// A second Create of a still-born identity is AlreadyLive at apply.
    /// Last-wins replaced the first birth and dropped updates already baked
    /// into it (the Property lives on `created`, not the shadow maps).
    #[test]
    fn a_second_create_keeps_the_first_birth_and_its_folded_updates() {
        let first = DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        };
        let property = DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(1),
            before: None,
            after: Some(CanonicalScalar::Int(5)),
        };
        let second = DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 2,
            labels: vec![LabelId(9)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
            valid_time: None,
        };
        let expected = vec![DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
            valid_time: None,
        }];
        assert_eq!(
            fold_target_disjoint(vec![first.clone(), property.clone(), second.clone()]),
            expected,
            "Property between two Creates must stay on the first birth"
        );
        assert_eq!(
            fold_target_disjoint(vec![first, second]),
            vec![create_vertex(1, 1)],
            "the second Create must not replace the first birth"
        );
    }

    #[test]
    fn a_second_create_edge_keeps_the_first_birth_and_its_folded_updates() {
        let first = create_edge(10, 1, 2);
        let property = DeltaRow::Property {
            elem: ElementId::Edge(EId(10)),
            property: PropertyKeyId(1),
            before: None,
            after: Some(CanonicalScalar::Int(5)),
        };
        let second = DeltaRow::CreateEdge {
            eid: EId(10),
            birth_ordinal: 99,
            src: VId(3),
            relation: crate::RelationId(1),
            dst: VId(4),
            canonical_key: None,
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(99))],
            valid_time: None,
        };
        let expected = vec![DeltaRow::CreateEdge {
            eid: EId(10),
            birth_ordinal: 10,
            src: VId(1),
            relation: crate::RelationId(1),
            dst: VId(2),
            canonical_key: None,
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
            valid_time: None,
        }];
        assert_eq!(
            fold_target_disjoint(vec![first.clone(), property, second.clone()]),
            expected,
            "Property between two CreateEdges must stay on the first birth"
        );
        assert_eq!(
            fold_target_disjoint(vec![first, second]),
            vec![create_edge(10, 1, 2)],
            "the second CreateEdge must not replace endpoints"
        );
    }
}
