//! The product MATCH executors, all riding ONE kernel
//! (fgdb-gql-one-kernel-7y17): live, as-of, and the transaction overlay
//! (`write_txn.rs`) each hand `crate::execute_bound_plan_over` their own
//! source set and expansion, so the row discipline — projected vertex IDs
//! ascending, deduplicated — has exactly one owner and cannot drift between
//! faces. The parser normalizes either arrow spelling into edge source and
//! destination variables, so execution always builds source-to-destinations
//! adjacency in one pass over the admitted edge table.
//!
//! A two-hop pattern (fgdb-gql-two-hop-8pfw) composes TWO per-relation
//! adjacency maps, both filled by the same single scan of the admitted edge
//! table — never a neighbours scan, never a second table read. The expansion
//! handed to the kernel is projection-shaped: hop-2 destinations for the
//! path's far end, the intermediates that continue for the via variable, and
//! either serves the source projection since the kernel only asks "did this
//! source reach anything".

use crate::{Database, EdgeRecord, EmbeddedReadView, ReadError, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::{BoundPlan, EdgeDirection, ReturnProjection};
use fgdb_types::{CanonicalScalar, CommitSeq, VId};
use std::collections::BTreeMap;

/// The minimal immutable read contract consumed by the bounded GQL kernel.
///
/// Keeping this trait inside the composition crate makes the parser/executor
/// boundary explicit while allowing both the live database handle and an
/// already-pinned [`EmbeddedReadView`] to share one execution body. It is not
/// a public storage abstraction and carries no authorization or retention
/// semantics beyond those of the implementing read surface.
pub(crate) trait GqlSnapshotReader {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError>;
    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError>;
    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError>;
}

impl<V: Vfs + Clone> GqlSnapshotReader for Database<V> {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        Database::vertex_at(self, vid, as_of)
    }

    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        Database::vertices_at(self, as_of)
    }

    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        Database::edges_at(self, as_of)
    }
}

impl GqlSnapshotReader for EmbeddedReadView {
    fn gql_vertex_at(&self, vid: VId, as_of: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        EmbeddedReadView::vertex_at(self, vid, as_of)
    }

    fn gql_vertices_at(&self, as_of: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        EmbeddedReadView::vertices_at(self, as_of)
    }

    fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        EmbeddedReadView::edges_at(self, as_of)
    }
}

/// Both-orientation adjacency for the hop-1 relation AND the optional hop-2
/// relation, filled by ONE loop over the fetched edge table
/// (fgdb-w5-parsers-nje.2 one-hop, fgdb-gql-undir-2hop-7mrc two-hop) — the
/// undirected twin of [`relation_adjacencies`]. Each edge lists its dst
/// under its src AND its src under its dst, so a vertex's expansion is the
/// union of outgoing dests and incoming srcs, and the hop-1 key set is
/// every incident vertex (a dest-only vertex is still a `RETURN a` row).
/// The composition downstream is map-agnostic, so feeding it these maps IS
/// the undirected two-hop: vias are undirected hop-1 neighbours, and their
/// undirected hop-2 neighbours (or the vias that have any) are the rows.
fn undirected_adjacencies(
    records: &[EdgeRecord],
    hop1: RelationId,
    hop2: Option<RelationId>,
) -> (BTreeMap<VId, Vec<VId>>, BTreeMap<VId, Vec<VId>>) {
    let mut hop1_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    let mut hop2_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == hop1 {
            hop1_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
            hop1_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
            hop2_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
    }
    (hop1_adjacency, hop2_adjacency)
}

/// dst → srcs adjacency for the hop-1 relation AND the optional hop-2
/// relation, filled by ONE loop over the fetched edge table
/// (fgdb-w5-parsers-nje.4) — the inverted twin of [`relation_adjacencies`]
/// for the incoming TWO-hop chain `(a)<-[:R]-(b)<-[:S]-(c)`: walking from
/// the anchor means walking every edge against its flow, so both hops
/// invert and the existing composition then IS the reverse composition.
/// Incoming ONE-hop never comes here — the parser normalizes its variable
/// roles, so it executes on the uninverted maps.
fn inverted_adjacencies(
    records: &[EdgeRecord],
    hop1: RelationId,
    hop2: Option<RelationId>,
) -> (BTreeMap<VId, Vec<VId>>, BTreeMap<VId, Vec<VId>>) {
    let mut hop1_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    let mut hop2_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == hop1 {
            hop1_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.dst)
                .or_default()
                .push(record.entry.src);
        }
    }
    (hop1_adjacency, hop2_adjacency)
}

/// src → dsts adjacency for one relation, and the same for the optional
/// hop-2 relation, filled by ONE scan of the fetched edge table.
fn relation_adjacencies(
    records: Vec<EdgeRecord>,
    hop1: RelationId,
    hop2: Option<RelationId>,
) -> (BTreeMap<VId, Vec<VId>>, BTreeMap<VId, Vec<VId>>) {
    let mut hop1_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    let mut hop2_adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for record in records {
        if record.entry.relation == hop1 {
            hop1_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
        if hop2 == Some(record.entry.relation) {
            hop2_adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
    }
    (hop1_adjacency, hop2_adjacency)
}

/// Run the shared kernel over the prepared adjacency maps — the one body
/// behind the live and as-of faces.
fn execute_over_adjacencies(
    plan: &BoundPlan,
    hop1: BTreeMap<VId, Vec<VId>>,
    hop2: BTreeMap<VId, Vec<VId>>,
) -> Result<Vec<VId>, ReadError> {
    let sources: Vec<_> = hop1.keys().copied().collect();
    // WHERE a <> b (fgdb-gql-where-neq-v476) and WHERE a = b
    // (fgdb-w5-parsers-nje.6): both predicates bind the two hop-1 pattern
    // variables, so both filter exactly the hop-1 step — before any
    // projection or hop-2 composition. Inequality drops the self-loop
    // edges; equality keeps ONLY them (src == dst). Filtering the KERNEL's
    // composed expansion instead would express a-vs-c predicates on a
    // two-hop plan, which are different (unrequested) predicates.
    let hop1_kept = |src: VId, via: &VId| {
        if plan.neq.is_some() && *via == src {
            return false;
        }
        if plan.eq.is_some() && *via != src {
            return false;
        }
        true
    };
    if plan.hop2_relation.is_none() {
        return crate::execute_bound_plan_over(plan, sources, |src, _| {
            let mut dests = hop1.get(&src).cloned().unwrap_or_default();
            dests.retain(|dst| hop1_kept(src, dst));
            Ok(dests)
        });
    }
    crate::execute_bound_plan_over(plan, sources, |src, _| {
        let vias: Vec<VId> = hop1
            .get(&src)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|via| hop1_kept(src, via))
            .copied()
            .collect();
        let vias = vias.as_slice();
        Ok(match plan.projection {
            // RETURN of the via variable: the intermediates that actually
            // continue — a hop-1 destination with no hop-2 edge is not on
            // any two-hop path.
            ReturnProjection::Destination => vias
                .iter()
                .filter(|via| hop2.contains_key(via))
                .copied()
                .collect(),
            // The path's far end — and the source projection, whose kernel
            // arm only asks whether this source reached anything, which for
            // a two-hop pattern means reaching a composed destination.
            ReturnProjection::Source | ReturnProjection::Hop2Destination => vias
                .iter()
                .flat_map(|via| hop2.get(via).cloned().unwrap_or_default())
                .collect(),
        })
    })
}

/// Apply the plan's node-label predicates to the hop-1 adjacency BEFORE the
/// kernel runs (fgdb-w5-parsers-nje.5, corrected law): labels constrain the
/// MATCH itself, independent of projection. `src_label` drops anchors — map
/// keys — that lack it, so their whole expansions vanish (RETURN b of
/// `(a:Person)-[:R]->(b)` answers only Person sources' dests); `dst_label`
/// drops hop-1 destinations inside each expansion (RETURN a of
/// `(a)-[:R]->(b:L)` keeps only sources still reaching an L dest, because
/// the kernel's Source arm asks for a non-empty expansion). The parser
/// already assigned each label to its edge-flow role — the incoming swap
/// included — so no direction special-casing happens here. An unlabeled
/// plan consults no vertex row at all.
fn filter_hop1_by_labels<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
    hop1: &mut BTreeMap<VId, Vec<VId>>,
) -> Result<(), ReadError> {
    if plan.src_label.is_none() && plan.dst_label.is_none() {
        return Ok(());
    }
    let has_label = |vid: VId, label: LabelId| -> Result<bool, ReadError> {
        let row = reader.gql_vertex_at(vid, as_of)?;
        Ok(row.is_some_and(|row| row.labels.contains(&label)))
    };
    if let Some(label) = plan.src_label {
        let keys: Vec<VId> = hop1.keys().copied().collect();
        let mut labeled = std::collections::BTreeSet::new();
        for vid in keys {
            if has_label(vid, label)? {
                labeled.insert(vid);
            }
        }
        hop1.retain(|anchor, _| labeled.contains(anchor));
    }
    if let Some(label) = plan.dst_label {
        let dests: std::collections::BTreeSet<VId> = hop1.values().flatten().copied().collect();
        let mut labeled = std::collections::BTreeSet::new();
        for vid in dests {
            if has_label(vid, label)? {
                labeled.insert(vid);
            }
        }
        for expansion in hop1.values_mut() {
            expansion.retain(|dst| labeled.contains(dst));
        }
    }
    Ok(())
}

/// Source-property integer predicates drop hop-1 SOURCE keys whose vertex
/// props do not satisfy the bound comparison. Equality requires
/// `(key, Int(n))`; inequality requires the key to be present as an integer
/// other than `n` (fgdb-w5-parsers-nje.15); strict comparisons require an
/// integer above or below `n` (fgdb-w5-parsers-nje.22/23). No-WHERE plans
/// consult no property row. Node-only labeled WHERE applies the same tests
/// inside [`node_scan`] (fgdb-w5-parsers-nje.11).
fn filter_hop1_by_src_prop<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
    hop1: &mut BTreeMap<VId, Vec<VId>>,
) -> Result<(), ReadError> {
    if plan.src_prop.is_none()
        && plan.src_prop_ne.is_none()
        && plan.src_prop_gt.is_none()
        && plan.src_prop_lt.is_none()
        && plan.src_prop_ge.is_none()
        && plan.src_prop_le.is_none()
    {
        return Ok(());
    }
    let carries = |vid: VId| -> Result<bool, ReadError> {
        let row = reader.gql_vertex_at(vid, as_of)?;
        Ok(row.is_some_and(|row| {
            let equal = plan.src_prop.is_none_or(|(key, value)| {
                let wanted = CanonicalScalar::Int(value);
                row.props
                    .iter()
                    .any(|(property, scalar)| *property == key && *scalar == wanted)
            });
            let not_equal = plan.src_prop_ne.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual != value)
                })
            });
            let greater = plan.src_prop_gt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual > value)
                })
            });
            let less = plan.src_prop_lt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual < value)
                })
            });
            let greater_or_equal = plan.src_prop_ge.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual >= value)
                })
            });
            let less_or_equal = plan.src_prop_le.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual <= value)
                })
            });
            equal && not_equal && greater && less && greater_or_equal && less_or_equal
        }))
    };
    let keys: Vec<VId> = hop1.keys().copied().collect();
    let mut kept = std::collections::BTreeSet::new();
    for vid in keys {
        if carries(vid)? {
            kept.insert(vid);
        }
    }
    hop1.retain(|anchor, _| kept.contains(anchor));
    Ok(())
}

/// Dest-property integer predicates drop hop-1 DESTINATIONS whose vertex
/// props do not satisfy the bound comparison. Incoming two-hop adjacency is
/// inverted, so its pattern destination is the map key rather than a value.
/// Equality requires
/// `(key, Int(n))`; inequality and strict greater-than require the key to be
/// present as an integer satisfying the comparison (fgdb-w5-parsers-nje.16,
/// fgdb-w5-parsers-nje.24). No-WHERE plans consult no property row.
fn filter_hop1_by_dst_prop<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
    hop1: &mut BTreeMap<VId, Vec<VId>>,
) -> Result<(), ReadError> {
    if plan.dst_prop.is_none()
        && plan.dst_prop_ne.is_none()
        && plan.dst_prop_gt.is_none()
        && plan.dst_prop_lt.is_none()
        && plan.dst_prop_ge.is_none()
        && plan.dst_prop_le.is_none()
    {
        return Ok(());
    }
    let filter_keys = plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some();
    let dests: std::collections::BTreeSet<VId> = if filter_keys {
        hop1.keys().copied().collect()
    } else {
        hop1.values().flatten().copied().collect()
    };
    let mut kept = std::collections::BTreeSet::new();
    for vid in dests {
        let row = reader.gql_vertex_at(vid, as_of)?;
        if row.is_some_and(|row| {
            let equal = plan.dst_prop.is_none_or(|(key, value)| {
                let wanted = CanonicalScalar::Int(value);
                row.props
                    .iter()
                    .any(|(property, scalar)| *property == key && *scalar == wanted)
            });
            let not_equal = plan.dst_prop_ne.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual != value)
                })
            });
            let greater = plan.dst_prop_gt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual > value)
                })
            });
            let less = plan.dst_prop_lt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual < value)
                })
            });
            let greater_or_equal = plan.dst_prop_ge.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual >= value)
                })
            });
            let less_or_equal = plan.dst_prop_le.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual <= value)
                })
            });
            equal && not_equal && greater && less && greater_or_equal && less_or_equal
        }) {
            kept.insert(vid);
        }
    }
    if filter_keys {
        hop1.retain(|dst, _| kept.contains(dst));
    } else {
        for expansion in hop1.values_mut() {
            expansion.retain(|dst| kept.contains(dst));
        }
    }
    Ok(())
}

/// A two-hop far-end predicate filters hop-2 adjacency VALUES, never the
/// hop-1 via vertices governed by `dst_prop`.
fn filter_hop2_by_dst_prop<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
    hop2: &mut BTreeMap<VId, Vec<VId>>,
) -> Result<(), ReadError> {
    if plan.hop2_dst_prop.is_none()
        && plan.hop2_dst_prop_ne.is_none()
        && plan.hop2_dst_prop_gt.is_none()
        && plan.hop2_dst_prop_lt.is_none()
        && plan.hop2_dst_prop_ge.is_none()
        && plan.hop2_dst_prop_le.is_none()
    {
        return Ok(());
    }
    let far_ends: std::collections::BTreeSet<VId> = hop2.values().flatten().copied().collect();
    let mut kept = std::collections::BTreeSet::new();
    for vid in far_ends {
        let row = reader.gql_vertex_at(vid, as_of)?;
        if row.is_some_and(|row| {
            let equal = plan.hop2_dst_prop.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key && *scalar == CanonicalScalar::Int(value)
                })
            });
            let not_equal = plan.hop2_dst_prop_ne.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual != value)
                })
            });
            let greater = plan.hop2_dst_prop_gt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual > value)
                })
            });
            let less = plan.hop2_dst_prop_lt.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual < value)
                })
            });
            let greater_or_equal = plan.hop2_dst_prop_ge.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual >= value)
                })
            });
            let less_or_equal = plan.hop2_dst_prop_le.is_none_or(|(key, value)| {
                row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual <= value)
                })
            });
            equal && not_equal && greater && less && greater_or_equal && less_or_equal
        }) {
            kept.insert(vid);
        }
    }
    hop2.retain(|_, expansion| {
        expansion.retain(|far_end| kept.contains(far_end));
        !expansion.is_empty()
    });
    Ok(())
}

/// The node-only scan face (fgdb-w5-parsers-nje.7): a plan with no edge
/// relation never touches the edge table — its rows are the vids whose
/// labels carry the pattern's label, under the same CGSE row contract
/// (ascending, deduplicated). The binder makes an unlabeled node-only plan
/// unrepresentable (it is a Parse refusal), so the missing-label arm fails
/// closed to no rows instead of inventing an all-vertices scan.
///
/// When a source-property predicate is present, the same vertex row must also
/// satisfy it; no-WHERE node-only plans still consult no property field.
fn node_scan(plan: &BoundPlan, rows: Vec<VertexRow>) -> Vec<VId> {
    let Some(label) = plan.src_label else {
        return Vec::new();
    };
    let mut vids: Vec<VId> = rows
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
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual != value)
                }),
            };
            let greater = match plan.src_prop_gt {
                None => true,
                Some((key, value)) => row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual > value)
                }),
            };
            let less = match plan.src_prop_lt {
                None => true,
                Some((key, value)) => row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual < value)
                }),
            };
            let greater_or_equal = match plan.src_prop_ge {
                None => true,
                Some((key, value)) => row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual >= value)
                }),
            };
            let less_or_equal = match plan.src_prop_le {
                None => true,
                Some((key, value)) => row.props.iter().any(|(property, scalar)| {
                    *property == key
                        && matches!(scalar, CanonicalScalar::Int(actual) if *actual <= value)
                }),
            };
            equal && not_equal && greater && less && greater_or_equal && less_or_equal
        })
        .map(|row| row.vid)
        .collect();
    vids.sort_unstable();
    vids.dedup();
    crate::apply_limit(plan, vids)
}

/// Execute the pinned bound MATCH expansion over the database's live Strata
/// view. The frontier is read once and then passed into the same snapshot
/// kernel used by historical and pinned-session execution.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let as_of = db.frontier()?;
    execute_at(plan, db, as_of)
}

/// Execute the pinned bound MATCH expansion at one exact snapshot frontier.
///
/// `reader` may be the live database or an immutable [`EmbeddedReadView`];
/// every path uses this one body, so labels, properties, directions, SKIP,
/// LIMIT, and deterministic row ordering cannot drift between surfaces.
pub(crate) fn execute_at<R: GqlSnapshotReader + ?Sized>(
    plan: &BoundPlan,
    reader: &R,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let Some(relation) = plan.relation else {
        return Ok(node_scan(plan, reader.gql_vertices_at(as_of)?));
    };
    let records = reader.gql_edges_at(as_of)?;
    let (mut hop1, mut hop2) = if plan.direction == EdgeDirection::Undirected {
        undirected_adjacencies(&records, relation, plan.hop2_relation)
    } else if plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some() {
        inverted_adjacencies(&records, relation, plan.hop2_relation)
    } else {
        relation_adjacencies(records, relation, plan.hop2_relation)
    };
    filter_hop1_by_labels(plan, reader, as_of, &mut hop1)?;
    filter_hop1_by_src_prop(plan, reader, as_of, &mut hop1)?;
    // nje.17 AND is parser-only: dual `Some` slots retain this same hop-1 map in sequence.
    filter_hop1_by_dst_prop(plan, reader, as_of, &mut hop1)?;
    filter_hop2_by_dst_prop(plan, reader, as_of, &mut hop2)?;
    execute_over_adjacencies(plan, hop1, hop2)
}
