//! Native set syntax compiles to ordinary maintained row/set nodes, not reruns.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GlaDirection, GraphValue};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const P: PropertyKeyId = PropertyKeyId(1);
const OPS: [&str; 6] = [
    "UNION ALL",
    "UNION",
    "INTERSECT ALL",
    "INTERSECT",
    "EXCEPT ALL",
    "EXCEPT",
];
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
// Both arms name their column `p`: a set operator over differently named
// columns is DifferentColumnsInUnion (openCypher; c8a7a399).
fn query(op: &str) -> String {
    format!("MATCH (n:L) RETURN n.p AS p {op} MATCH (n:R) RETURN n.p AS p")
}
fn oracle(db: &Database<MemVfs>, op: usize) -> QueryResult {
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let value = GraphValue::Scalar(
            row.props
                .iter()
                .find(|(key, _)| *key == P)
                .map_or(CanonicalScalar::Null, |(_, value)| {
                    value.as_scalar().expect("scalar fixture").clone()
                }),
        );
        for (label, bag) in [(LabelId(1), &mut left), (LabelId(2), &mut right)] {
            if row.labels.contains(&label) {
                *bag.entry(value.clone()).or_insert(0_u64) += 1;
            }
        }
    }
    let keys = left.keys().chain(right.keys()).collect::<BTreeSet<_>>();
    let mut rows = Vec::new();
    for key in keys {
        let l = left.get(key).copied().unwrap_or(0);
        let r = right.get(key).copied().unwrap_or(0);
        let count = match op {
            0 => l + r,
            1 => u64::from(l > 0 || r > 0),
            2 => l.min(r),
            3 => u64::from(l > 0 && r > 0),
            4 => l.saturating_sub(r),
            5 => u64::from(l > 0 && r == 0),
            _ => unreachable!(),
        };
        for _ in 0..count {
            rows.push(vec![QueryValue::Value(key.clone())]);
        }
    }
    QueryResult::Rows {
        columns: vec!["p".into()],
        rows,
    }
}

// Full traversal of current graph facts, independent of the incremental
// support arrangement, closure kernel and native TRAIL enumeration.
fn reachable_pairs(
    db: &Database<MemVfs>,
    selected: &[RelationId],
    direction: GlaDirection,
    swap: bool,
    reflexive: bool,
) -> ZSet<Vec<QueryValue>> {
    let mut adjacency: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
    if reflexive {
        for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
            adjacency.entry(row.vid).or_default();
        }
    }
    for row in db
        .edges()
        .unwrap()
        .iter()
        .filter(|row| selected.is_empty() || selected.contains(&row.entry.relation))
    {
        let (a, b) = if direction == GlaDirection::Reverse {
            (row.entry.dst, row.entry.src)
        } else {
            (row.entry.src, row.entry.dst)
        };
        adjacency.entry(a).or_default().insert(b);
        if direction == GlaDirection::Undirected {
            adjacency.entry(b).or_default().insert(a);
        }
    }
    let mut pairs = Vec::new();
    for (&source, neighbors) in &adjacency {
        let mut seen = BTreeSet::new();
        if reflexive {
            seen.insert(source);
        }
        let mut pending: Vec<_> = neighbors.iter().copied().collect();
        while let Some(vertex) = pending.pop() {
            if seen.insert(vertex)
                && let Some(next) = adjacency.get(&vertex)
            {
                pending.extend(next);
            }
        }
        for destination in seen {
            let (a, b) = if swap {
                (destination, source)
            } else {
                (source, destination)
            };
            pairs.push((
                vec![
                    QueryValue::Value(GraphValue::Vertex(a)),
                    QueryValue::Value(GraphValue::Vertex(b)),
                ],
                ZWeight::ONE,
            ));
        }
    }
    ZSet::from_updates(pairs, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
        Ok::<_, ()>(())
    })
    .unwrap()
}

fn selected_reachable_rows(
    db: &Database<MemVfs>,
    relations: &[RelationId],
    reflexive: bool,
    endpoint_labels: [&[LabelId]; 2],
    columns: &[usize],
) -> ZSet<Vec<QueryValue>> {
    let vertices: BTreeMap<_, _> = db
        .vertices_at(db.frontier().unwrap())
        .unwrap()
        .into_iter()
        .map(|vertex| (vertex.vid, vertex.labels))
        .collect();
    let pairs = reachable_pairs(db, relations, GlaDirection::Forward, false, reflexive);
    let rows: BTreeSet<_> = pairs
        .iter()
        .filter(|(row, _)| {
            row.iter().zip(endpoint_labels).all(|(value, labels)| {
                let QueryValue::Value(GraphValue::Vertex(vertex)) = value else {
                    unreachable!("endpoint fixture")
                };
                labels.iter().all(|label| vertices[vertex].contains(label))
            })
        })
        .map(|(row, _)| columns.iter().map(|&column| row[column].clone()).collect())
        .collect();
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        fgdb_delta_types::LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

// Enumerate concrete finite walks directly from live source facts. Unlike
// the maintained circuit this retains every pending path occurrence, including
// repeated edges and vertices, and only deduplicates the final selected rows.
fn bounded_walk_rows(
    db: &Database<MemVfs>,
    relations: &[RelationId],
    direction: GlaDirection,
    bounds: [u32; 2],
    endpoint_labels: [&[LabelId]; 2],
    columns: &[usize],
) -> ZSet<Vec<QueryValue>> {
    let vertices: BTreeMap<_, _> = db
        .vertices()
        .unwrap()
        .into_iter()
        .map(|vertex| (vertex.vid, vertex.labels))
        .collect();
    let mut adjacency: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for edge in db.edges().unwrap() {
        if !relations.is_empty() && !relations.contains(&edge.entry.relation) {
            continue;
        }
        let (source, destination) = if direction == GlaDirection::Reverse {
            (edge.entry.dst, edge.entry.src)
        } else {
            (edge.entry.src, edge.entry.dst)
        };
        adjacency.entry(source).or_default().push(destination);
        if direction == GlaDirection::Undirected {
            adjacency.entry(destination).or_default().push(source);
        }
    }
    let mut rows = BTreeSet::new();
    for (&source, labels) in &vertices {
        if !endpoint_labels[0]
            .iter()
            .all(|label| labels.contains(label))
        {
            continue;
        }
        let mut pending = vec![(source, 0)];
        while let Some((destination, length)) = pending.pop() {
            if length >= bounds[0]
                && endpoint_labels[1]
                    .iter()
                    .all(|label| vertices[&destination].contains(label))
            {
                let endpoints = [source, destination];
                rows.insert(
                    columns
                        .iter()
                        .map(|&column| QueryValue::Value(GraphValue::Vertex(endpoints[column])))
                        .collect::<Vec<_>>(),
                );
            }
            if length < bounds[1]
                && let Some(next) = adjacency.get(&destination)
            {
                pending.extend(next.iter().map(|&vertex| (vertex, length + 1)));
            }
        }
    }
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        fgdb_delta_types::LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn native_bounded_walks_maintain_intervals_cycles_parallel_support_and_endpoint_membership() {
    struct Case {
        text: &'static str,
        relations: &'static [RelationId],
        direction: GlaDirection,
        bounds: [u32; 2],
        labels: [&'static [LabelId]; 2],
        columns: &'static [usize],
        nodes: usize,
    }
    let ((), report) = run_async_under_lab(0x6e73_7c21, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let cases = [
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..3]->(b) RETURN DISTINCT a,b",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [0, 3],
                labels: [&[], &[]],
                columns: &[0, 1],
                nodes: 11,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a)<-[:R|S*2..3]-(b) RETURN DISTINCT b AS target,a AS origin",
                relations: &[RelationId(1), RelationId(2)],
                direction: GlaDirection::Reverse,
                bounds: [2, 3],
                labels: [&[], &[]],
                columns: &[1, 0],
                nodes: 8,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a)-[*2]-(b) RETURN DISTINCT a,b",
                relations: &[],
                direction: GlaDirection::Undirected,
                bounds: [2, 2],
                labels: [&[], &[]],
                columns: &[0, 1],
                nodes: 5,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*3]->(b) RETURN DISTINCT a,b",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [3, 3],
                labels: [&[], &[]],
                columns: &[0, 1],
                nodes: 7,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0]->(b) RETURN DISTINCT a,b",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [0, 0],
                labels: [&[], &[]],
                columns: &[0, 1],
                nodes: 2,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a:L)-[:R*0..3]->(b:R) RETURN DISTINCT b AS node",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [0, 3],
                labels: [&[LabelId(1)], &[LabelId(2)]],
                columns: &[1],
                nodes: 15,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a:L:R)-[:R*1..3]->(b) RETURN DISTINCT a,b,a AS again",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [1, 3],
                labels: [&[LabelId(1), LabelId(2)], &[]],
                columns: &[0, 1, 0],
                nodes: 11,
            },
            Case {
                text: "MATCH REPEATABLE ELEMENTS WALK (a:L)-[:R*2]->(b:R) RETURN DISTINCT b AS first,b AS second",
                relations: &[RelationId(1)],
                direction: GlaDirection::Forward,
                bounds: [2, 2],
                labels: [&[LabelId(1)], &[LabelId(2)]],
                columns: &[1, 1],
                nodes: 9,
            },
        ];
        let params = GqlParameters::new();
        let one_row = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let mut handles = Vec::new();
        for (index, case) in cases.iter().enumerate() {
            let prepared = PreparedNativeRead::prepare(case.text, &params, symbols).unwrap();
            assert_eq!(
                prepared.subscription_footprint(&cx, &params).unwrap().0,
                case.nodes as u64,
                "{}",
                case.text
            );
            let handle = prepared
                .register_standing(
                    &mut db,
                    &cx,
                    &params,
                    if index == 7 { one_row } else { policy() },
                )
                .unwrap();
            assert!(
                db.standing_native_bag(&cx, &handle, policy())
                    .unwrap()
                    .1
                    .is_empty()
            );
            handles.push(handle);
        }
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels) in [
            (1, vec![LabelId(1)]),
            (2, vec![LabelId(1)]),
            (3, vec![]),
            (4, vec![LabelId(2)]),
            (5, vec![LabelId(1), LabelId(2)]),
            (6, vec![]),
            (u128::MAX, vec![]),
        ] {
            seed.create_vertex(VId(id), labels, vec![]);
        }
        for (id, a, b) in [
            (10, 1, 3),
            (11, 1, 3),
            (12, 2, 3),
            (13, 3, 4),
            (14, 3, 3),
            (15, 4, 1),
        ] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let mut second_relation = WriteBatch::new(RelationId(2));
        second_relation.add_edge(EId(20), VId(4), VId(6), vec![]);
        let mut parallel = WriteBatch::new(RelationId(1));
        parallel.delete_edge(EId(10));
        let mut alternate = WriteBatch::new(RelationId(1));
        alternate.delete_edge(EId(11));
        let mut shortcut = WriteBatch::new(RelationId(1));
        shortcut.add_edge(EId(16), VId(1), VId(4), vec![]);
        let mut last_support = WriteBatch::new(RelationId(1));
        last_support.delete_edge(EId(13));
        let mut restore = WriteBatch::new(RelationId(1));
        restore.add_edge(EId(17), VId(3), VId(4), vec![]);
        restore.create_vertex(VId(8), vec![], vec![]);
        let mut leave = WriteBatch::new(RelationId(1));
        leave.set_vertex_label(VId(4), LabelId(2), false);
        let mut rejoin = WriteBatch::new(RelationId(1));
        rejoin.set_vertex_label(VId(4), LabelId(2), true);
        let mut cascade = WriteBatch::new(RelationId(1));
        cascade.delete_vertex(VId(3));
        let mut isolate = WriteBatch::new(RelationId(1));
        isolate.delete_vertex(VId(u128::MAX));
        let changes = [
            seed,
            second_relation,
            parallel,
            alternate,
            shortcut,
            last_support,
            restore,
            leave,
            rejoin,
            cascade,
            isolate,
        ];
        let mut previous: Vec<_> = handles
            .iter()
            .map(|handle| db.standing_native_bag(&cx, handle, policy()).unwrap().1)
            .collect();
        for (tick, change) in changes.into_iter().enumerate() {
            let from = db.frontier().unwrap();
            db.write(&commit, change).await.unwrap();
            for (index, (case, handle)) in cases.iter().zip(&handles).enumerate() {
                let expected = bounded_walk_rows(
                    &db,
                    case.relations,
                    case.direction,
                    case.bounds,
                    case.labels,
                    case.columns,
                );
                let (at, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(
                    actual, expected,
                    "source-walk oracle, tick {tick}: {}",
                    case.text
                );
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    db.query(&cx, case.text, &params, symbols, policy())
                        .unwrap(),
                    "ordinary execution, tick {tick}: {}",
                    case.text
                );
                let (through, delta) = db
                    .standing_native_delta(&cx, handle, from, policy())
                    .unwrap();
                assert_eq!(through, at);
                previous[index]
                    .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(
                    previous[index], expected,
                    "derivative, tick {tick}: {}",
                    case.text
                );
                if index == 7 && matches!(tick, 2 | 3) {
                    assert!(
                        delta.is_empty(),
                        "alternative parallel/source support survives"
                    );
                }
                if matches!(tick, 3 | 8) {
                    db.rebuild_standing_query(
                        &cx,
                        handle,
                        if index == 7 { one_row } else { policy() },
                    )
                    .unwrap();
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap(),
                        (at, expected)
                    );
                }
                assert_eq!(
                    db.standing_query_node_stats(&cx, handle).unwrap().count(),
                    case.nodes
                );
            }
            if matches!(tick, 5 | 7 | 9) {
                assert!(
                    previous[7].is_empty(),
                    "the final selected support has left"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_selected_endpoint_reachability_tracks_labels_witnesses_and_isolates() {
    let ((), report) = run_async_under_lab(0x6e73_7c13, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels) in [
            (1, vec![LabelId(1)]),
            (2, vec![LabelId(1)]),
            (3, vec![]),
            (4, vec![LabelId(2)]),
            (5, vec![LabelId(1), LabelId(2)]),
            (6, vec![]),
        ] {
            seed.create_vertex(VId(id), labels, vec![]);
        }
        for (id, a, b) in [(10, 1, 3), (11, 2, 3), (12, 3, 4)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let cases: [(&str, bool, [&[LabelId]; 2], &[usize], usize); 7] = [
            (
                "MATCH (a:L)-[:R*]->(b:R) RETURN DISTINCT b AS node",
                false,
                [&[LabelId(1)], &[LabelId(2)]],
                &[1],
                7,
            ),
            (
                "MATCH (a:L)-[:R*]->(b:R) RETURN DISTINCT a,b",
                false,
                [&[LabelId(1)], &[LabelId(2)]],
                &[0, 1],
                7,
            ),
            (
                "MATCH (a:L)-[:R*0..]->(b:R) RETURN DISTINCT a,b",
                true,
                [&[LabelId(1)], &[LabelId(2)]],
                &[0, 1],
                9,
            ),
            (
                "MATCH (a:L)-[:R*]->(b) RETURN DISTINCT b AS first,b AS second",
                false,
                [&[LabelId(1)], &[]],
                &[1, 1],
                5,
            ),
            (
                "MATCH (a)-[:R*]->(b:R) RETURN DISTINCT a",
                false,
                [&[], &[LabelId(2)]],
                &[0],
                5,
            ),
            (
                "MATCH (a:L:R)-[:R*0..]->(b:R) RETURN DISTINCT a",
                true,
                [&[LabelId(1), LabelId(2)], &[LabelId(2)]],
                &[0],
                9,
            ),
            (
                "MATCH (a)-[:R*]->(b) RETURN DISTINCT b AS first,b AS second",
                false,
                [&[], &[]],
                &[1, 1],
                3,
            ),
        ];
        let small = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let params = GqlParameters::new();
        let mut handles = Vec::new();
        for (index, (text, _, _, _, nodes)) in cases.iter().enumerate() {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (reserved, _) = prepared.subscription_footprint(&cx, &params).unwrap();
            let allowance = if index == 0 { small } else { policy() };
            let handle = prepared
                .register_standing(&mut db, &cx, &params, allowance)
                .unwrap();
            assert_eq!(reserved, *nodes as u64, "{text}");
            assert_eq!(
                db.standing_query_node_stats(&cx, &handle).unwrap().count(),
                *nodes,
                "{text}"
            );
            handles.push(handle);
        }
        let literal = |rows: &[&[u128]]| {
            ZSet::from_updates(
                rows.iter().map(|row| {
                    (
                        row.iter()
                            .map(|&vertex| QueryValue::Value(GraphValue::Vertex(VId(vertex))))
                            .collect::<Vec<_>>(),
                        ZWeight::ONE,
                    )
                }),
                fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap()
        };
        // Vertex 3 has neither endpoint label, yet the two-hop paths exist.
        // The complete private closure has five pairs, while this delivered
        // destination set fits a one-row allowance.
        for (index, expected) in [
            (0, literal(&[&[4]])),
            (1, literal(&[&[1, 4], &[2, 4]])),
            (2, literal(&[&[1, 4], &[2, 4], &[5, 5]])),
            (5, literal(&[&[5]])),
        ] {
            assert_eq!(
                db.standing_native_bag(&cx, &handles[index], policy())
                    .unwrap()
                    .1,
                expected
            );
        }
        let mut first_leaves = WriteBatch::new(RelationId(1));
        first_leaves.set_vertex_label(VId(1), LabelId(1), false);
        let mut last_leaves = WriteBatch::new(RelationId(1));
        last_leaves.set_vertex_label(VId(2), LabelId(1), false);
        let mut rejoin = WriteBatch::new(RelationId(1));
        rejoin.set_vertex_label(VId(1), LabelId(1), true);
        rejoin.set_vertex_label(VId(2), LabelId(1), true);
        let mut destination_leaves = WriteBatch::new(RelationId(1));
        destination_leaves.set_vertex_label(VId(4), LabelId(2), false);
        let mut destination_rejoins = WriteBatch::new(RelationId(1));
        destination_rejoins.set_vertex_label(VId(4), LabelId(2), true);
        destination_rejoins.set_vertex_label(VId(3), LabelId(1), true);
        let mut first_edge = WriteBatch::new(RelationId(1));
        first_edge.set_vertex_label(VId(3), LabelId(1), false);
        first_edge.delete_edge(EId(10));
        let mut last_edge = WriteBatch::new(RelationId(1));
        last_edge.delete_edge(EId(11));
        let mut select_isolate = WriteBatch::new(RelationId(1));
        select_isolate.set_vertex_label(VId(6), LabelId(1), true);
        select_isolate.set_vertex_label(VId(6), LabelId(2), true);
        let mut retire_isolate = WriteBatch::new(RelationId(1));
        retire_isolate.delete_vertex(VId(5));
        let mut restore_edges = WriteBatch::new(RelationId(1));
        restore_edges.add_edge(EId(20), VId(1), VId(3), vec![]);
        restore_edges.add_edge(EId(21), VId(2), VId(3), vec![]);
        restore_edges.set_vertex_label(VId(1), LabelId(2), true);
        let changes = [
            first_leaves,
            last_leaves,
            rejoin,
            destination_leaves,
            destination_rejoins,
            first_edge,
            last_edge,
            select_isolate,
            retire_isolate,
            restore_edges,
        ];
        let mut previous: Vec<_> = handles
            .iter()
            .map(|handle| db.standing_native_bag(&cx, handle, policy()).unwrap().1)
            .collect();
        for (tick, change) in changes.into_iter().enumerate() {
            let from = db.frontier().unwrap();
            db.write(&commit, change).await.unwrap();
            for (index, ((text, reflexive, labels, columns, nodes), handle)) in
                cases.iter().zip(&handles).enumerate()
            {
                let expected =
                    selected_reachable_rows(&db, &[RelationId(1)], *reflexive, *labels, columns);
                let (at, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(actual, expected, "full graph oracle, tick {tick}: {text}");
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    "ordinary execution, tick {tick}: {text}"
                );
                let (through, delta) = db
                    .standing_native_delta(&cx, handle, from, policy())
                    .unwrap();
                assert_eq!(through, at);
                if index == 0 && matches!(tick, 0 | 5) {
                    assert!(
                        delta.is_empty(),
                        "another selected source still reaches vertex 4"
                    );
                }
                previous[index]
                    .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(previous[index], expected, "delivered delta, tick {tick}");
                if matches!(tick, 0 | 7 | 9) {
                    db.rebuild_standing_query(
                        &cx,
                        handle,
                        if index == 0 { small } else { policy() },
                    )
                    .unwrap();
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap(),
                        (at, expected)
                    );
                    assert_eq!(
                        db.standing_query_node_stats(&cx, handle).unwrap().count(),
                        *nodes
                    );
                }
            }
            if matches!(tick, 1 | 6) {
                assert!(previous[0].is_empty(), "the last selected source has left");
            }
            if tick == 7 {
                assert_eq!(previous[2], literal(&[&[5, 5], &[6, 6]]));
            }
            if tick == 9 {
                assert_eq!(previous[2], literal(&[&[1, 1], &[1, 4], &[2, 4], &[6, 6]]));
            }
        }
        // An endpoint label change can still exceed the FINAL row allowance.
        // Private closure/joins must not disable the public projection's cap.
        let mut overflow = WriteBatch::new(RelationId(1));
        overflow.set_vertex_label(VId(3), LabelId(2), true);
        db.write(&commit, overflow).await.unwrap();
        assert!(matches!(
            db.standing_native_bag(&cx, &handles[0], policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert!(db.rebuild_standing_query(&cx, &handles[0], small).is_err());
        db.rebuild_standing_query(&cx, &handles[0], policy())
            .unwrap();
        assert_eq!(
            db.standing_native_bag(&cx, &handles[0], policy())
                .unwrap()
                .1,
            literal(&[&[3], &[4]])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_source_projection_waits_for_the_last_selected_destination() {
    let ((), report) = run_async_under_lab(0x6e73_7c14, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels) in [
            (1, vec![LabelId(1)]),
            (2, vec![LabelId(2)]),
            (3, vec![LabelId(2)]),
            (4, vec![]),
            (5, vec![LabelId(1)]),
        ] {
            seed.create_vertex(VId(id), labels, vec![]);
        }
        for (id, a, b) in [(10, 1, 4), (11, 4, 2), (12, 4, 3)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let texts = [
            "MATCH (a:L)-[:R*]->(b:R) RETURN DISTINCT a AS node",
            "MATCH (a:L)-[:R*0..]->(b:R) RETURN DISTINCT a AS node",
        ];
        let small = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let params = GqlParameters::new();
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                db.register_standing_native(&cx, text, &params, symbols, small)
                    .unwrap()
            })
            .collect();
        let result = |present: bool| QueryResult::Rows {
            columns: vec!["node".into()],
            rows: if present {
                vec![vec![QueryValue::Value(GraphValue::Vertex(VId(1)))]]
            } else {
                vec![]
            },
        };
        for handle in &handles {
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap().1,
                result(true)
            );
        }
        for (tick, (vertex, label, present)) in [
            (VId(2), LabelId(2), false),
            (VId(3), LabelId(2), false),
            (VId(1), LabelId(2), true),
            (VId(1), LabelId(1), false),
            (VId(1), LabelId(1), true),
        ]
        .into_iter()
        .enumerate()
        {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_label(vertex, label, present);
            db.write(&commit, change).await.unwrap();
            for (index, (text, handle)) in texts.iter().zip(&handles).enumerate() {
                let present = tick == 0 || (index == 1 && matches!(tick, 2 | 4));
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    result(present),
                    "target label support, tick {tick}: {text}"
                );
                assert_eq!(
                    db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    result(present)
                );
                if tick == 0 {
                    assert!(
                        db.standing_native_delta(&cx, handle, from, policy())
                            .unwrap()
                            .1
                            .is_empty(),
                        "the second selected destination still supports origin 1"
                    );
                }
                db.rebuild_standing_query(&cx, handle, small).unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    result(present)
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn reachability_property_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => symbols(kind, name),
    }
}

fn property_output_bag(values: &[GraphValue]) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(
        values
            .iter()
            .map(|value| (vec![QueryValue::Value(value.clone())], ZWeight::ONE)),
        fgdb_delta_types::LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

// Read current graph facts and traverse them independently of the maintained
// circuit. Endpoint selection never removes an intermediate traversal vertex.
fn reachable_property_rows(
    db: &Database<MemVfs>,
    undirected: bool,
    reflexive: bool,
    detailed: bool,
    bounds: Option<[u32; 2]>,
) -> ZSet<Vec<QueryValue>> {
    let vertices: BTreeMap<_, _> = db
        .vertices_at(db.frontier().unwrap())
        .unwrap()
        .into_iter()
        .map(|vertex| (vertex.vid, vertex))
        .collect();
    let direction = if undirected {
        GlaDirection::Undirected
    } else {
        GlaDirection::Forward
    };
    let pairs = if let Some(bounds) = bounds {
        // Enumerate only the requested finite walk lengths. Never use the
        // unbounded closure oracle as an approximation for this branch.
        bounded_walk_rows(db, &[RelationId(1)], direction, bounds, [&[], &[]], &[0, 1])
    } else {
        reachable_pairs(db, &[RelationId(1)], direction, false, reflexive)
    };
    let mut rows = BTreeSet::new();
    for (pair, _) in pairs.iter() {
        let (QueryValue::Value(GraphValue::Vertex(a)), QueryValue::Value(GraphValue::Vertex(b))) =
            (&pair[0], &pair[1])
        else {
            unreachable!("vertex-pair oracle");
        };
        let origin = &vertices[a];
        let destination = &vertices[b];
        if !origin.labels.contains(&LabelId(1))
            || !origin.props.iter().any(|(key, value)| {
                *key == P && value.as_scalar() == Some(&CanonicalScalar::Int(1))
            })
            || !destination.labels.contains(&LabelId(2))
        {
            continue;
        }
        let value = destination
            .props
            .iter()
            .find(|(key, _)| *key == PropertyKeyId(2))
            .map_or(GraphValue::Scalar(CanonicalScalar::Null), |(_, value)| {
                GraphValue::from_property_value(value).unwrap()
            });
        let row = if detailed {
            vec![
                GraphValue::Vertex(*a),
                GraphValue::Vertex(*b),
                value.clone(),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                value,
            ]
        } else {
            vec![value]
        };
        rows.insert(row.into_iter().map(QueryValue::Value).collect::<Vec<_>>());
    }
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        fgdb_delta_types::LimbLimit::new(4),
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn native_reachability_properties_follow_membership_payloads_and_last_value_witnesses() {
    use fgdb_types::{
        CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalPropertyValue, CanonicalText,
    };

    let ((), report) = run_async_under_lab(0x6e73_7c15, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let q = PropertyKeyId(2);
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels, properties) in [
            (1, vec![LabelId(1)], vec![(P, CanonicalScalar::Int(1))]),
            (2, vec![LabelId(1)], vec![(P, CanonicalScalar::Int(0))]),
            (3, vec![], vec![]),
            (4, vec![LabelId(2)], vec![(q, CanonicalScalar::Int(7))]),
            (5, vec![LabelId(2)], vec![(q, CanonicalScalar::Int(7))]),
            (6, vec![LabelId(2)], vec![]),
        ] {
            seed.create_vertex(VId(id), labels, properties);
        }
        for (id, a, b) in [(10, 1, 3), (11, 2, 3), (12, 3, 4), (13, 3, 5), (14, 3, 6)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let cases = [
            (
                "MATCH (a:L {p:1})-[:R*]->(b:R) RETURN DISTINCT b.q AS value",
                false,
                false,
                false,
                7,
                None,
            ),
            (
                "MATCH (b:R)<-[:R*]-(a:L {p:1}) RETURN DISTINCT b.q AS value",
                false,
                false,
                false,
                7,
                None,
            ),
            (
                "MATCH (a:L {p:1})-[:R*0..]-(b:R) RETURN DISTINCT b.q AS value",
                true,
                true,
                false,
                9,
                None,
            ),
            (
                "MATCH (a:L {p:1})-[:R*0..]->(b:R) RETURN DISTINCT a AS origin,b AS target,b.q AS first,a.p AS selected,b.q AS again",
                false,
                true,
                true,
                9,
                None,
            ),
            (
                "MATCH REPEATABLE ELEMENTS WALK (a:L {p:1})-[:R*3]->(b:R) RETURN DISTINCT b.q AS value",
                false,
                false,
                false,
                11,
                Some([3, 3]),
            ),
        ];
        let params = GqlParameters::new();
        let mut handles = Vec::new();
        for (text, _, _, _, nodes, _) in &cases {
            let prepared =
                PreparedNativeRead::prepare(text, &params, reachability_property_symbols).unwrap();
            assert_eq!(
                prepared.subscription_footprint(&cx, &params).unwrap().0,
                *nodes as u64
            );
            let handle = prepared
                .register_standing(&mut db, &cx, &params, policy())
                .unwrap();
            assert_eq!(
                db.standing_query_node_stats(&cx, &handle).unwrap().count(),
                *nodes
            );
            handles.push(handle);
        }
        // Many private reachability pairs and endpoint rows collapse to two
        // delivered values. Only the final DISTINCT output spends this cap.
        let small = GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000);
        let capped = db
            .register_standing_native(
                &cx,
                cases[0].0,
                &params,
                reachability_property_symbols,
                small,
            )
            .unwrap();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let number = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let list = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::Int(2).into(),
                CanonicalScalar::Int(1).into(),
            ])
            .unwrap(),
        );
        let map = CanonicalPropertyValue::Map(
            CanonicalMap::try_new(vec![CanonicalMapEntry::new(
                CanonicalText::new_ucs_basic("k").unwrap(),
                CanonicalScalar::Int(3).into(),
            )])
            .unwrap(),
        );
        let list_value = GraphValue::List(vec![number(2), number(1)].into_boxed_slice());
        let map_value = GraphValue::map(vec![(Box::<str>::from("k"), number(3))]).unwrap();
        let mut previous: Vec<_> = handles
            .iter()
            .map(|handle| db.standing_native_bag(&cx, handle, policy()).unwrap().1)
            .collect();
        for answer in &previous[..3] {
            assert_eq!(*answer, property_output_bag(&[null.clone(), number(7)]));
        }
        assert!(
            previous[4].is_empty(),
            "two-hop targets do not satisfy an exact three-hop WALK"
        );
        for tick in 0..17 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(1)));
                }
                1 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                }
                2 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(0)));
                }
                3 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                    change.set_vertex_property(VId(4), q, Some(CanonicalScalar::Int(8)));
                }
                4 => {
                    change.set_vertex_property(VId(5), q, Some(CanonicalScalar::Int(8)));
                }
                5 => {
                    change.set_vertex_property(VId(4), q, None);
                }
                6 => {
                    change.set_vertex_property(VId(5), q, Some(CanonicalScalar::Null));
                }
                7 => {
                    change.set_vertex_property_value(VId(6), q, Some(list.clone()));
                }
                8 => {
                    change.set_vertex_property_value(VId(4), q, Some(map.clone()));
                }
                9 => {
                    change.set_vertex_label(VId(5), LabelId(2), false);
                }
                10 => {
                    change.set_vertex_label(VId(6), LabelId(2), false);
                }
                11 => {
                    change.delete_edge(EId(12));
                }
                12 => {
                    change.create_vertex(
                        VId(7),
                        vec![LabelId(1), LabelId(2)],
                        vec![(P, CanonicalScalar::Int(1)), (q, CanonicalScalar::Int(9))],
                    );
                }
                13 => {
                    change.set_vertex_property(VId(7), P, Some(CanonicalScalar::Int(0)));
                }
                14 => {
                    change.set_vertex_property(VId(7), P, Some(CanonicalScalar::Int(1)));
                    change.set_vertex_label(VId(5), LabelId(2), true);
                    // This supplies the first exact three-hop route, while
                    // leaving all unbounded endpoint/value sets unchanged.
                    change.add_edge(EId(15), VId(3), VId(3), vec![]);
                }
                15 => {
                    change.delete_vertex(VId(7));
                }
                16 => {
                    change.set_vertex_property(VId(5), q, Some(CanonicalScalar::Int(3)));
                }
                _ => unreachable!(),
            }
            let from = db.frontier().unwrap();
            db.write(&commit, change).await.unwrap();
            let literal = match tick {
                0 | 1 => vec![null.clone(), number(7)],
                2 | 11 | 12 | 13 => vec![],
                3 => vec![null.clone(), number(7), number(8)],
                4 | 5 => vec![null.clone(), number(8)],
                6 | 14 | 15 => vec![null.clone()],
                7 => vec![null.clone(), list_value.clone()],
                8 => vec![null.clone(), list_value.clone(), map_value.clone()],
                9 => vec![list_value.clone(), map_value.clone()],
                10 => vec![map_value.clone()],
                16 => vec![number(3)],
                _ => unreachable!(),
            };
            for (index, ((text, undirected, reflexive, detailed, nodes, bounds), handle)) in
                cases.iter().zip(&handles).enumerate()
            {
                let expected =
                    reachable_property_rows(&db, *undirected, *reflexive, *detailed, *bounds);
                let (at, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(actual, expected, "current graph facts, tick {tick}: {text}");
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    db.query(&cx, text, &params, reachability_property_symbols, policy())
                        .unwrap(),
                    "ordinary path execution, tick {tick}: {text}"
                );
                let (through, delta) = db
                    .standing_native_delta(&cx, handle, from, policy())
                    .unwrap();
                assert_eq!(through, at);
                if index < 3 && matches!(tick, 0 | 1 | 5) {
                    assert!(
                        delta.is_empty(),
                        "remaining endpoint/value support, tick {tick}: {text}"
                    );
                }
                previous[index]
                    .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                assert_eq!(
                    previous[index], expected,
                    "signed derivative, tick {tick}: {text}"
                );
                if index < 3 {
                    let mut values = literal.clone();
                    if index == 2 && matches!(tick, 12 | 14) {
                        values.push(number(9));
                    }
                    assert_eq!(
                        actual,
                        property_output_bag(&values),
                        "literal values, tick {tick}: {text}"
                    );
                }
                if index == 4 {
                    let values = match tick {
                        14 | 15 => vec![null.clone()],
                        16 => vec![number(3)],
                        _ => vec![],
                    };
                    assert_eq!(
                        actual,
                        property_output_bag(&values),
                        "exact three-hop values, tick {tick}"
                    );
                }
                if matches!(tick, 3 | 8 | 12 | 16) {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap(),
                        (at, expected)
                    );
                    assert_eq!(
                        db.standing_query_node_stats(&cx, handle).unwrap().count(),
                        *nodes
                    );
                }
            }
            if tick == 3 {
                assert!(matches!(
                    db.standing_native_bag(&cx, &capped, policy()),
                    Err(StandingQueryError::Unavailable {
                        reason: StandingQueryFailure::ResultBudget,
                        ..
                    })
                ));
                assert!(db.rebuild_standing_query(&cx, &capped, small).is_err());
                db.rebuild_standing_query(&cx, &capped, policy()).unwrap();
                assert_eq!(
                    db.standing_native_bag(&cx, &capped, policy()).unwrap().1,
                    property_output_bag(&literal)
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_reachability_property_subscriptions_mask_payloads_and_bridges_before_maintenance() {
    use asupersync::security::key::AuthKey;
    use fgdb::{NativeSubscriptionSetup, SubscriptionError};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0x6e73_7c16, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let q = PropertyKeyId(2);
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels, properties) in [
            (1, vec![LabelId(1)], vec![(P, CanonicalScalar::Int(1))]),
            (2, vec![LabelId(1)], vec![(q, CanonicalScalar::Int(7))]),
            (3, vec![LabelId(1)], vec![(q, CanonicalScalar::Int(99))]),
            (10, vec![LabelId(2)], vec![]),
        ] {
            seed.create_vertex(VId(id), labels, properties);
        }
        for (id, a, b) in [(1, 1, 2), (10, 2, 10), (11, 10, 3)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let text = "MATCH (a:L {p:1})-[:R*]->(b:L) RETURN DISTINCT b.q AS value";
        let params = GqlParameters::new();
        let query =
            PreparedNativeRead::prepare(text, &params, reachability_property_symbols).unwrap();
        let ordinary = db
            .query(&cx, text, &params, reachability_property_symbols, policy())
            .unwrap();
        assert_eq!(
            ordinary,
            QueryResult::Rows {
                columns: vec!["value".into()],
                rows: vec![
                    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                        7
                    )))],
                    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                        99
                    )))],
                ],
            },
            "the unscoped route through hidden vertex 10 really reaches vertex 3"
        );
        let issuer = Authority::new(
            AuthKey::from_seed(0x6e73_7c16),
            DatabaseSecurityNamespaceId([0x52; 32]),
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut subscriptions = Vec::new();
        let mut delivered = Vec::new();
        let limbs = fgdb_delta_types::LimbLimit::new(4);
        for expose_q in [true, false] {
            let mut grant = Grant::read_only(
                "main",
                1000,
                QueryLimits {
                    max_nodes: 100_000,
                    max_work: 10_000_000,
                    max_rows: 100_000,
                },
            );
            grant.labels = Scope::only([LabelId(1)]);
            grant.relations = Scope::only([RelationId(1)]);
            grant.properties = if expose_q {
                Scope::only([P, q])
            } else {
                Scope::only([P])
            };
            let token = issuer.issue_at(&grant, 0).unwrap();
            let authority = issuer.verify_at(&token, "main", 1).unwrap();
            assert_eq!(
                query
                    .subscription_footprint_authorized(&cx, &params, authority.predicates())
                    .unwrap()
                    .0,
                8
            );
            let (mut subscription, initial) = query
                .subscribe_authorized(
                    &mut db,
                    &cx,
                    &params,
                    authority.predicates(),
                    NativeSubscriptionSetup {
                        maintenance: policy(),
                        delivery: policy(),
                        retention: [16, 1000, 100_000],
                    },
                    |columns, batch| {
                        assert_eq!(columns, ["value"]);
                        Ok::<_, SubscriptionError>(Arc::clone(batch))
                    },
                )
                .unwrap();
            let rows = initial
                .rows()
                .checked_clone(limbs, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(
                rows,
                property_output_bag(&[GraphValue::Scalar(if expose_q {
                    CanonicalScalar::Int(7)
                } else {
                    CanonicalScalar::Null
                })])
            );
            subscription.acknowledge(initial.receipt()).unwrap();
            subscriptions.push(subscription);
            delivered.push(rows);
        }
        for tick in 0..8 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(3), q, Some(CanonicalScalar::Int(100)));
                }
                1 => {
                    change.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(1)));
                }
                2 => {
                    change.add_edge(EId(2), VId(2), VId(3), vec![]);
                }
                3 => {
                    change.set_vertex_property(VId(2), q, Some(CanonicalScalar::Int(100)));
                }
                4 => {
                    change.delete_edge(EId(1));
                }
                5 => {
                    change.add_edge(EId(3), VId(1), VId(3), vec![]);
                }
                6 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                }
                7 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                    change.set_vertex_property(VId(3), q, None);
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let values = match tick {
                0 | 1 => vec![GraphValue::Scalar(CanonicalScalar::Int(7))],
                2 => vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    GraphValue::Scalar(CanonicalScalar::Int(100)),
                ],
                3 | 5 => vec![GraphValue::Scalar(CanonicalScalar::Int(100))],
                4 | 6 => vec![],
                7 => vec![GraphValue::Scalar(CanonicalScalar::Null)],
                _ => unreachable!(),
            };
            for (index, subscription) in subscriptions.iter_mut().enumerate() {
                let batch = subscription.poll(&db, &cx, policy()).unwrap();
                if tick == 1 || (index == 1 && matches!(tick, 0 | 3)) {
                    assert!(
                        batch.is_none(),
                        "hidden property/bridge update has no public frontier, tick {tick}"
                    );
                }
                if let Some(batch) = batch {
                    let repeated = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
                    assert!(
                        Arc::ptr_eq(&repeated, &batch),
                        "retry keeps the exact delivery and receipt"
                    );
                    assert_eq!(repeated.rows(), batch.rows());
                    assert!(!batch.is_snapshot());
                    delivered[index]
                        .integrate(batch.rows(), limbs, &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                    subscription.acknowledge(batch.receipt()).unwrap();
                }
                let expected = if index == 0 || values.is_empty() {
                    property_output_bag(&values)
                } else {
                    property_output_bag(&[GraphValue::Scalar(CanonicalScalar::Null)])
                };
                assert_eq!(
                    delivered[index], expected,
                    "authorized values, tick {tick}, scope {index}"
                );
            }
        }
        for subscription in &mut subscriptions {
            assert_eq!(
                subscription
                    .close_and_release(&mut db, &cx, policy())
                    .unwrap(),
                9,
                "capability mask, property-aware reachability circuit and replay owner"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_reachability_keeps_exact_support_deltas_and_rebuilds_the_owned_circuit() {
    let ((), report) = run_async_under_lab(0x6e73_7c10, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [1, 2, 3, 4, 5, 6, u128::MAX] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        for (id, a, b) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 3, 1), (5, 4, 5)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        seed.add_edge(EId(6), VId(u128::MAX), VId(u128::MAX), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(20), VId(3), VId(4), vec![]);
        db.write(&commit, other).await.unwrap();
        let cases = [
            (
                "MATCH (a)-[:R*]->(b) RETURN DISTINCT a,b",
                vec![RelationId(1)],
                GlaDirection::Forward,
                false,
                false,
            ),
            (
                "MATCH TRAIL (a)<-[:R*]-(b) RETURN DISTINCT b AS target,a AS origin",
                vec![RelationId(1)],
                GlaDirection::Reverse,
                true,
                false,
            ),
            (
                "MATCH (a)-[:R|S*1..]->(b) RETURN DISTINCT a,b",
                vec![RelationId(1), RelationId(2)],
                GlaDirection::Forward,
                false,
                false,
            ),
            (
                "MATCH (a)-[*]->(b) RETURN DISTINCT a,b",
                vec![],
                GlaDirection::Forward,
                false,
                false,
            ),
            (
                "MATCH (a)-[:R*0..]->(b) RETURN DISTINCT a,b",
                vec![RelationId(1)],
                GlaDirection::Forward,
                false,
                true,
            ),
            (
                "MATCH TRAIL (a)<-[:R*0..]-(b) RETURN DISTINCT b AS target,a AS origin",
                vec![RelationId(1)],
                GlaDirection::Reverse,
                true,
                true,
            ),
            (
                "MATCH (a)-[:R|S*0..]->(b) RETURN DISTINCT a,b",
                vec![RelationId(1), RelationId(2)],
                GlaDirection::Forward,
                false,
                true,
            ),
            (
                "MATCH (a)-[*0..]->(b) RETURN DISTINCT a,b",
                vec![],
                GlaDirection::Forward,
                false,
                true,
            ),
            (
                "MATCH (a)-[:R|S*0..]-(b) RETURN DISTINCT a,b",
                vec![RelationId(1), RelationId(2)],
                GlaDirection::Undirected,
                false,
                true,
            ),
        ];
        let params = GqlParameters::new();
        let mut handles = Vec::new();
        for (text, _, _, _, reflexive) in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (reserved, _) = prepared.subscription_footprint(&cx, &params).unwrap();
            let handle = prepared
                .register_standing(&mut db, &cx, &params, policy())
                .unwrap();
            assert_eq!(reserved, if *reflexive { 5 } else { 3 });
            assert_eq!(
                db.standing_query_node_stats(&cx, &handle).unwrap().count(),
                if *reflexive { 5 } else { 3 }
            );
            handles.push(handle);
        }
        assert_eq!(
            db.standing_native_columns(&cx, &handles[1]).unwrap(),
            ["target", "origin"]
        );
        let mut previous: Vec<_> = cases
            .iter()
            .map(|(_, selected, direction, swap, reflexive)| {
                reachable_pairs(&db, selected, *direction, *swap, *reflexive)
            })
            .collect();
        let mut from = db.frontier().unwrap();
        for tick in 0..=12 {
            for (index, ((text, selected, direction, swap, reflexive), handle)) in
                cases.iter().zip(&handles).enumerate()
            {
                let expected = reachable_pairs(&db, selected, *direction, *swap, *reflexive);
                let (at, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(
                    actual, expected,
                    "independent graph oracle, tick {tick}: {text}"
                );
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    "ordinary TRAIL semantics, tick {tick}: {text}"
                );
                if tick != 0 {
                    let (through, delta) = db
                        .standing_native_delta(&cx, handle, from, policy())
                        .unwrap();
                    assert_eq!(through, at);
                    if *reflexive && matches!(tick, 1 | 5 | 6) {
                        assert!(delta.is_empty(), "counted self/parallel support: {text}");
                    }
                    previous[index]
                        .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                    assert_eq!(
                        previous[index], expected,
                        "delivered retractions, tick {tick}"
                    );
                }
                if tick == 5 {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert_eq!(
                        db.standing_query_node_stats(&cx, handle).unwrap().count(),
                        if *reflexive { 5 } else { 3 }
                    );
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap().1,
                        expected
                    );
                }
            }
            if tick == 12 {
                break;
            }
            from = db.frontier().unwrap();
            let mut change = WriteBatch::new(if tick == 6 || tick == 8 {
                RelationId(3)
            } else {
                RelationId(1)
            });
            match tick {
                0 => {
                    change.delete_edge(EId(1));
                }
                1 => {
                    change.delete_edge(EId(2));
                }
                2 => {
                    change.add_edge(EId(7), VId(1), VId(2), vec![]);
                }
                3 => {
                    change.delete_edge(EId(4));
                }
                4 => {
                    change.delete_edge(EId(6));
                }
                5 => {
                    change.add_edge(EId(8), VId(3), VId(3), vec![]);
                }
                6 => {
                    change.add_edge(EId(30), VId(5), VId(u128::MAX), vec![]);
                }
                7 => {
                    change.create_vertex(VId(99), vec![], vec![]);
                }
                8 => {
                    change.delete_edge(EId(30));
                }
                9 => {
                    change.delete_vertex(VId(2));
                }
                10 => {
                    change.add_edge(EId(9), VId(1), VId(3), vec![]);
                }
                11 => {
                    change.delete_vertex(VId(99));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_reflexive_undirected_reachability_tracks_empty_graph_and_single_edge_lifetimes() {
    let ((), report) = run_async_under_lab(0x6e73_7c13, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (a)-[:R*0..]-(b) RETURN DISTINCT a,b";
        let handle = db
            .register_standing_native(&cx, text, &params, symbols, policy())
            .unwrap();
        let (mut from, mut delivered) = db.standing_native_bag(&cx, &handle, policy()).unwrap();
        assert!(delivered.is_empty());
        assert!(
            db.register_standing_native(
                &cx,
                "MATCH (a)-[:R*1..]-(b) RETURN DISTINCT a,b",
                &params,
                symbols,
                policy(),
            )
            .is_err()
        );
        for tick in 0..5 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    for id in 1..=3 {
                        change.create_vertex(VId(id), vec![], vec![]);
                    }
                }
                1 => {
                    change.add_edge(EId(1), VId(1), VId(2), vec![]);
                }
                2 => {
                    change.delete_edge(EId(1));
                }
                3 => {
                    change.delete_vertex(VId(2));
                }
                4 => {
                    change.delete_vertex(VId(1));
                    change.delete_vertex(VId(3));
                }
                _ => unreachable!(),
            }
            let at = db.write(&commit, change).await.unwrap();
            let (through, delta) = db
                .standing_native_delta(&cx, &handle, from, policy())
                .unwrap();
            assert_eq!(through, at);
            delivered
                .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap();
            let expected: &[(u128, u128)] = match tick {
                0 | 2 => &[(1, 1), (2, 2), (3, 3)],
                1 => &[(1, 1), (1, 2), (2, 1), (2, 2), (3, 3)],
                3 => &[(1, 1), (3, 3)],
                _ => &[],
            };
            let expected = ZSet::from_updates(
                expected.iter().map(|&(a, b)| {
                    (
                        vec![
                            QueryValue::Value(GraphValue::Vertex(VId(a))),
                            QueryValue::Value(GraphValue::Vertex(VId(b))),
                        ],
                        ZWeight::ONE,
                    )
                }),
                fgdb_delta_types::LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
            assert_eq!(
                delivered, expected,
                "literal empty/single-edge law, tick {tick}"
            );
            assert_eq!(
                db.standing_native_bag(&cx, &handle, policy()).unwrap(),
                (at, expected)
            );
            assert_eq!(
                db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                db.query(&cx, text, &params, symbols, policy()).unwrap()
            );
            from = at;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_reachability_masks_hidden_bridges_before_recursive_subscription_state() {
    native_reachability_authorized_updates(false, false, false);
}

#[test]
fn native_reflexive_reachability_masks_hidden_vertices_and_maintains_visible_isolates() {
    native_reachability_authorized_updates(true, false, false);
}

#[test]
fn native_selected_reachability_masks_hidden_bridges_before_endpoint_filtering() {
    native_reachability_authorized_updates(true, true, false);
}

#[test]
fn native_bounded_walk_subscription_masks_hidden_bridges_before_every_hop() {
    native_reachability_authorized_updates(false, false, true);
}

#[test]
fn native_bounded_walk_subscription_masks_isolates_before_endpoint_selection() {
    native_reachability_authorized_updates(true, true, true);
}

fn native_reachability_authorized_updates(reflexive: bool, selected: bool, bounded: bool) {
    use asupersync::security::key::AuthKey;
    use fgdb::{NativeSubscriptionSetup, SubscriptionError};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0x6e73_7c12, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut visible = Database::open_memory(&commit, keys()).await.unwrap();
        for graph in [&mut db, &mut visible] {
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=3 {
                seed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
            }
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            graph.write(&commit, seed).await.unwrap();
        }
        let mut bridge = WriteBatch::new(RelationId(1));
        bridge.create_vertex(VId(10), vec![LabelId(2)], vec![]);
        bridge.add_edge(EId(10), VId(2), VId(10), vec![]);
        bridge.add_edge(EId(11), VId(10), VId(3), vec![]);
        db.write(&commit, bridge).await.unwrap();
        let mut hidden_relation = WriteBatch::new(RelationId(2));
        hidden_relation.add_edge(EId(20), VId(3), VId(1), vec![]);
        db.write(&commit, hidden_relation).await.unwrap();

        let issuer = Authority::new(
            AuthKey::from_seed(0x6e73_7c12),
            DatabaseSecurityNamespaceId([0x52; 32]),
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 100_000,
                max_work: 10_000_000,
                max_rows: 100_000,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([RelationId(1)]);
        let token = issuer.issue_at(&grant, 0).unwrap();
        let authority = issuer.verify_at(&token, "main", 1).unwrap();
        let params = GqlParameters::new();
        let text = if bounded && selected {
            "MATCH REPEATABLE ELEMENTS WALK (a:L)-[*0..3]->(b:L) RETURN DISTINCT a,b"
        } else if bounded && reflexive {
            "MATCH REPEATABLE ELEMENTS WALK (a)-[*0..3]->(b) RETURN DISTINCT a,b"
        } else if bounded {
            "MATCH REPEATABLE ELEMENTS WALK (a)-[*1..3]->(b) RETURN DISTINCT a,b"
        } else if selected {
            "MATCH (a:L)-[*0..]->(b:L) RETURN DISTINCT a,b"
        } else if reflexive {
            "MATCH (a)-[*0..]->(b) RETURN DISTINCT a,b"
        } else {
            "MATCH (a)-[*]->(b) RETURN DISTINCT a,b"
        };
        let query = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let circuit_nodes = match (bounded, reflexive) {
            (true, true) => 11,
            (true, false) => 9,
            (false, true) => 5,
            (false, false) => 3,
        } + if selected { 4 } else { 0 };
        assert_eq!(
            query
                .subscription_footprint_authorized(&cx, &params, authority.predicates())
                .unwrap()
                .0,
            (circuit_nodes + 1) as u64,
            "the masked source owner and all shared hop owners are reserved"
        );
        let expected = |graph: &Database<MemVfs>| {
            let labels: [&[LabelId]; 2] = if selected {
                [&[LabelId(1)], &[LabelId(1)]]
            } else {
                [&[], &[]]
            };
            if bounded {
                bounded_walk_rows(
                    graph,
                    &[],
                    GlaDirection::Forward,
                    [u32::from(!reflexive), 3],
                    labels,
                    &[0, 1],
                )
            } else {
                selected_reachable_rows(graph, &[], reflexive, labels, &[0, 1])
            }
        };
        let control = query
            .register_standing(&mut db, &cx, &params, policy())
            .unwrap();
        let (mut subscription, initial) = query
            .subscribe_authorized(
                &mut db,
                &cx,
                &params,
                authority.predicates(),
                NativeSubscriptionSetup {
                    maintenance: policy(),
                    delivery: policy(),
                    retention: [16, 1000, 100_000],
                },
                |columns, batch| {
                    assert_eq!(columns, ["a", "b"]);
                    Ok::<_, SubscriptionError>(Arc::clone(batch))
                },
            )
            .unwrap();
        let limb_limit = fgdb_delta_types::LimbLimit::new(4);
        let mut delivered = initial
            .rows()
            .checked_clone(limb_limit, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        subscription.acknowledge(initial.receipt()).unwrap();
        assert_eq!(delivered, expected(&visible));
        let hidden_bridge_pair = vec![
            QueryValue::Value(GraphValue::Vertex(VId(1))),
            QueryValue::Value(GraphValue::Vertex(VId(3))),
        ];
        assert!(!delivered.iter().any(|(row, _)| row == &hidden_bridge_pair));
        assert!(
            db.standing_native_bag(&cx, &control, policy())
                .unwrap()
                .1
                .iter()
                .any(|(row, _)| row == &hidden_bridge_pair)
        );
        for tick in 0..9 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.add_edge(EId(12), VId(10), VId(3), vec![]);
                }
                2 => {
                    change.add_edge(EId(2), VId(2), VId(3), vec![]);
                }
                3 => {
                    change.delete_edge(EId(1));
                }
                4 => {
                    change.delete_vertex(VId(10));
                }
                5 => {
                    change.delete_edge(EId(2));
                }
                6 => {
                    change.create_vertex(VId(4), vec![LabelId(1)], vec![]);
                }
                7 => {
                    change.create_vertex(VId(11), vec![LabelId(2)], vec![]);
                }
                8 => {
                    change.delete_vertex(VId(4));
                }
                _ => unreachable!(),
            }
            if matches!(tick, 2 | 3 | 5 | 6 | 8) {
                visible.write(&commit, change.clone()).await.unwrap();
            }
            db.write(&commit, change).await.unwrap();
            if tick <= 1 {
                assert_eq!(
                    db.standing_native_bag(&cx, &control, policy())
                        .unwrap()
                        .1
                        .iter()
                        .any(|(row, _)| row == &hidden_bridge_pair),
                    tick == 1,
                    "removing and restoring the hidden bridge changes unscoped reachability"
                );
            }
            if let Some(batch) = subscription.poll(&db, &cx, policy()).unwrap() {
                if batch.is_snapshot() {
                    delivered = batch
                        .rows()
                        .checked_clone(limb_limit, &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                } else {
                    delivered
                        .integrate(batch.rows(), limb_limit, &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                }
                subscription.acknowledge(batch.receipt()).unwrap();
            }
            assert_eq!(
                delivered,
                expected(&visible),
                "visible-only independent oracle, tick {tick}"
            );
            assert_eq!(
                db.standing_native_bag(&cx, &control, policy()).unwrap().1,
                expected(&db),
                "unscoped live control, tick {tick}"
            );
        }
        assert_eq!(
            subscription
                .close_and_release(&mut db, &cx, policy())
                .unwrap(),
            circuit_nodes + 2,
            "mask, complete recursive circuit, replay"
        );
        assert_eq!(
            db.standing_query_node_stats(&cx, &control).unwrap().count(),
            circuit_nodes
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn analytic_field(metric: &str) -> &'static str {
    match metric {
        "triangles" => "triangles",
        "clustering_coefficient" => "score",
        "core_number" => "core",
        _ => unreachable!(),
    }
}

fn selected_analytic_text(metric: &str, relation: Option<RelationId>) -> String {
    let relation = relation.map_or("", |_| "relations:['R'],");
    let field = analytic_field(metric);
    format!(
        "CALL fnx.{metric}({{labels:['L'],{relation}direction:'undirected',\
         parallel:'collapse',self_loops:'drop'}}) YIELD vertex AS node,{field} AS value \
         RETURN node,value"
    )
}

// Exhaust every induced subset and its minimum degree. A vertex's core is
// the largest such minimum among subsets containing it; neither Prism nor the
// maintained peeling kernel supplies this oracle. Fixtures have at most six
// vertices, so the complete subset space contains at most 64 entries.
fn selected_core_oracle(
    vertices: &BTreeSet<VId>,
    edges: &BTreeSet<(VId, VId)>,
) -> BTreeMap<VId, i64> {
    assert!(vertices.len() <= 6);
    let ordered: Vec<_> = vertices.iter().copied().collect();
    let mut cores: BTreeMap<_, _> = vertices.iter().map(|&vertex| (vertex, 0)).collect();
    for mask in 1..(1_u64 << ordered.len()) {
        let selected: Vec<_> = ordered
            .iter()
            .enumerate()
            .filter_map(|(bit, &vertex)| (mask & (1 << bit) != 0).then_some(vertex))
            .collect();
        let degree = selected
            .iter()
            .map(|&vertex| {
                selected
                    .iter()
                    .filter(|&&other| {
                        vertex != other && edges.contains(&(vertex.min(other), vertex.max(other)))
                    })
                    .count()
            })
            .min()
            .unwrap();
        let degree = i64::try_from(degree).unwrap();
        for vertex in selected {
            let core = cores.get_mut(&vertex).unwrap();
            *core = (*core).max(degree);
        }
    }
    cores
}

// Count unordered triples for triangles/clustering, or induced subsets for
// core numbers, over the independently reconstructed selected simple graph.
fn selected_analytic_oracle(
    db: &Database<MemVfs>,
    metric: &str,
    relation: Option<RelationId>,
) -> QueryResult {
    union_analytic_oracle(db, metric, &[LabelId(1)], relation.as_slice())
}

fn union_analytic_oracle(
    db: &Database<MemVfs>,
    metric: &str,
    labels: &[LabelId],
    relations: &[RelationId],
) -> QueryResult {
    let vertices: BTreeSet<_> = db
        .vertices()
        .unwrap()
        .iter()
        .filter(|vertex| {
            labels.is_empty() || labels.iter().any(|label| vertex.labels.contains(label))
        })
        .map(|vertex| vertex.vid)
        .collect();
    let edges: BTreeSet<_> = db
        .edges()
        .unwrap()
        .iter()
        .filter(|edge| {
            let edge = &edge.entry;
            (relations.is_empty() || relations.contains(&edge.relation))
                && edge.src != edge.dst
                && vertices.contains(&edge.src)
                && vertices.contains(&edge.dst)
        })
        .map(|edge| {
            (
                edge.entry.src.min(edge.entry.dst),
                edge.entry.src.max(edge.entry.dst),
            )
        })
        .collect();
    simple_analytic_oracle(&vertices, &edges, metric, ["node", "value"])
}

fn simple_analytic_oracle(
    vertices: &BTreeSet<VId>,
    edges: &BTreeSet<(VId, VId)>,
    metric: &str,
    columns: [&str; 2],
) -> QueryResult {
    let adjacent = |a: VId, b: VId| edges.contains(&(a.min(b), a.max(b)));
    let counts = if metric == "core_number" {
        selected_core_oracle(vertices, edges)
    } else {
        let mut counts: BTreeMap<_, i64> = vertices.iter().map(|&vertex| (vertex, 0)).collect();
        for &a in vertices {
            for &b in vertices.range((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded)) {
                for &c in vertices.range((std::ops::Bound::Excluded(b), std::ops::Bound::Unbounded))
                {
                    if adjacent(a, b) && adjacent(a, c) && adjacent(b, c) {
                        for vertex in [a, b, c] {
                            *counts.get_mut(&vertex).unwrap() += 1;
                        }
                    }
                }
            }
        }
        counts
    };
    QueryResult::Rows {
        columns: columns.into_iter().map(String::from).collect(),
        rows: counts
            .into_iter()
            .map(|(vertex, count)| {
                let value = match metric {
                    "triangles" | "core_number" => CanonicalScalar::Int(count),
                    "clustering_coefficient" => {
                        let degree = vertices
                            .iter()
                            .filter(|&&other| adjacent(vertex, other))
                            .count();
                        let score = if degree < 2 {
                            0.0
                        } else {
                            (2 * count) as f64 / (degree * (degree - 1)) as f64
                        };
                        CanonicalScalar::Float(CanonicalF64::new(score))
                    }
                    _ => unreachable!(),
                };
                vec![
                    QueryValue::Value(GraphValue::Vertex(vertex)),
                    QueryValue::Value(GraphValue::Scalar(value)),
                ]
            })
            .collect(),
    }
}

#[test]
fn selected_motif_analytics_follow_label_membership_parallel_support_and_rebuilds() {
    let ((), report) = run_async_under_lab(0x6e73_7301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        // Six public vertices fit even when the private selected topology has
        // more than six rows. Its self-pairs retain isolates, not result rows.
        let allowance = GqlQueryPolicy::new(100_000, 6, 10_000_000, 10_000_000);
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            for relation in [Some(RelationId(1)), None] {
                let text = selected_analytic_text(metric, relation);
                let handle = db
                    .register_standing_native(&cx, &text, &params, symbols, allowance)
                    .unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                    selected_analytic_oracle(&db, metric, relation)
                );
                handles.push((metric, relation, text, handle));
            }
        }
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [0, 1, 2, 3, 4, u128::MAX] {
            let labels = if [0, 1, 3, u128::MAX].contains(&id) {
                vec![LabelId(1)]
            } else {
                vec![]
            };
            seed.create_vertex(VId(id), labels, vec![]);
        }
        for (id, a, b) in [
            (10, 0, 1),
            (11, 1, 2),
            (12, 2, 0),
            (13, 0, 1),
            (14, 1, 0),
            (15, u128::MAX, u128::MAX),
            (16, 0, 3),
        ] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let mut select = WriteBatch::new(RelationId(1));
        select.set_vertex_label(VId(2), LabelId(1), true);
        let mut duplicates = WriteBatch::new(RelationId(1));
        duplicates.delete_edge(EId(10));
        duplicates.delete_edge(EId(13));
        let mut last_edge = WriteBatch::new(RelationId(1));
        last_edge.delete_edge(EId(14));
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(30), VId(0), VId(1), vec![]);
        let mut leave = WriteBatch::new(RelationId(1));
        leave.set_vertex_label(VId(1), LabelId(1), false);
        leave.set_vertex_label(VId(u128::MAX), LabelId(1), false);
        let mut rejoin = WriteBatch::new(RelationId(1));
        rejoin.set_vertex_label(VId(1), LabelId(1), true);
        rejoin.set_vertex_label(VId(u128::MAX), LabelId(1), true);
        let mut unrelated = WriteBatch::new(RelationId(1));
        unrelated.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(99)));
        let mut select_isolate = WriteBatch::new(RelationId(1));
        select_isolate.set_vertex_label(VId(4), LabelId(1), true);
        let mut grow_core = WriteBatch::new(RelationId(1));
        for (id, a, b) in [(40, 0, 1), (41, 1, 3), (42, 2, 3)] {
            grow_core.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        // The complete four-vertex graph has core 3. Cascading its last
        // vertex's edges lowers the remaining triangle to core 2; both the
        // self-loop-only vertex and the newly selected isolate stay at 0.
        let mut retire = WriteBatch::new(RelationId(1));
        retire.delete_vertex(VId(3));
        for batch in [
            seed,
            select,
            duplicates,
            last_edge,
            other,
            leave,
            rejoin,
            unrelated,
            select_isolate,
            grow_core,
            retire,
        ] {
            db.write(&commit, batch).await.unwrap();
            for (metric, relation, text, handle) in &handles {
                let expected = selected_analytic_oracle(&db, metric, *relation);
                let (at, actual) = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(actual, expected, "{text}");
                assert_eq!(
                    actual,
                    db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    "fresh execution: {text}"
                );
                db.rebuild_standing_query(&cx, handle, allowance).unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap(),
                    (at, expected),
                    "rebuild: {text}"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_motif_result_refusal_preserves_commits_and_rebuild_requires_all_inputs() {
    let ((), report) = run_async_under_lab(0x6e73_7302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let small = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            let text = selected_analytic_text(metric, Some(RelationId(1)));
            let limited = db
                .register_standing_native(&cx, &text, &params, symbols, small)
                .unwrap();
            let healthy = db
                .register_standing_native(&cx, &text, &params, symbols, policy())
                .unwrap();
            handles.push((metric, limited, healthy));
        }
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 0..4 {
            seed.create_vertex(
                VId(id),
                if id < 3 { vec![LabelId(1)] } else { vec![] },
                vec![],
            );
        }
        for (id, a, b) in [(10, 0, 1), (11, 1, 2), (12, 2, 0), (13, 2, 3)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        for (_, limited, healthy) in &handles {
            assert_eq!(
                db.standing_native_query(&cx, limited, policy()).unwrap(),
                db.standing_native_query(&cx, healthy, policy()).unwrap()
            );
        }
        let mut grow = WriteBatch::new(RelationId(1));
        grow.set_vertex_label(VId(3), LabelId(1), true);
        let committed = db.write(&commit, grow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), committed);
        for (metric, limited, healthy) in &handles {
            assert!(matches!(
                db.standing_native_query(&cx, limited, policy()),
                Err(StandingQueryError::Unavailable { .. })
            ));
            assert!(db.rebuild_standing_query(&cx, limited, small).is_err());
            assert!(matches!(
                db.standing_native_query(&cx, limited, policy()),
                Err(StandingQueryError::Unavailable { .. })
            ));
            let expected = selected_analytic_oracle(&db, metric, Some(RelationId(1)));
            assert_eq!(
                db.standing_native_query(&cx, healthy, policy()).unwrap().1,
                expected,
                "healthy sibling after failed rebuild: {metric}"
            );
            db.rebuild_standing_query(&cx, limited, policy()).unwrap();
            assert_eq!(
                db.standing_native_query(&cx, limited, policy()).unwrap(),
                (committed, expected),
                "repaired selected analytics: {metric}"
            );
        }
        let mut shrink = WriteBatch::new(RelationId(1));
        shrink.set_vertex_label(VId(1), LabelId(1), false);
        db.write(&commit, shrink).await.unwrap();
        for (metric, limited, healthy) in &handles {
            let expected = selected_analytic_oracle(&db, metric, Some(RelationId(1)));
            assert_eq!(
                db.standing_native_query(&cx, limited, policy()).unwrap().1,
                expected,
                "selected source after repaired view advances: {metric}"
            );
            assert_eq!(
                db.standing_native_query(&cx, limited, policy()).unwrap(),
                db.standing_native_query(&cx, healthy, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const ROW_ANALYTIC_SOURCE: &str = "MATCH (a)-[e:R]->(b) WHERE e.p = 1 \
     RETURN 9 AS marker,a AS source,'ignored' AS note,b AS target";

fn register_row_analytic(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    source: &StandingQueryHandle,
    metric: &str,
    endpoints: [usize; 2],
    allowance: GqlQueryPolicy,
) -> Result<StandingQueryHandle, StandingQueryError> {
    match metric {
        "triangles" => {
            db.register_standing_triangle_counts_from_rows(cx, source, endpoints, allowance)
        }
        "clustering_coefficient" => {
            db.register_standing_clustering_coefficients_from_rows(cx, source, endpoints, allowance)
        }
        "core_number" => {
            db.register_standing_core_numbers_from_rows(cx, source, endpoints, allowance)
        }
        _ => unreachable!(),
    }
}

fn row_analytic_seed(tail_enabled: bool) -> WriteBatch {
    let mut seed = WriteBatch::new(RelationId(1));
    for vertex in [0, 1, 2, 3, u128::MAX] {
        seed.create_vertex(VId(vertex), vec![], vec![]);
    }
    for (id, a, b) in [(10, 0, 1), (11, 1, 2), (12, 2, 0), (13, 0, 3)] {
        seed.add_edge(
            EId(id),
            VId(a),
            VId(b),
            vec![(P, CanonicalScalar::Int(i64::from(id != 13 || tail_enabled)))],
        );
    }
    seed
}

// Select the input graph independently from stored edge properties, not from
// the maintained parent or any of the analytics being checked. Vertices enter
// this selected universe only through a row's endpoints.
fn row_analytic_oracle(db: &Database<MemVfs>, metric: &str) -> QueryResult {
    let mut vertices = BTreeSet::new();
    let mut edges = BTreeSet::new();
    for edge in db.edges().unwrap() {
        if edge.entry.relation != RelationId(1)
            || !edge.props.iter().any(|(key, value)| {
                *key == P && matches!(value.as_scalar(), Some(CanonicalScalar::Int(1)))
            })
        {
            continue;
        }
        let (a, b) = (edge.entry.src, edge.entry.dst);
        vertices.extend([a, b]);
        if a != b {
            edges.insert((a.min(b), a.max(b)));
        }
    }
    simple_analytic_oracle(
        &vertices,
        &edges,
        metric,
        ["vertex", analytic_field(metric)],
    )
}

#[test]
fn analytics_from_rows_follow_property_filters_and_integrate_native_signed_deltas() {
    let ((), report) = run_async_under_lab(0x6e73_7303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut after = db.write(&commit, row_analytic_seed(true)).await.unwrap();
        let source = db
            .register_standing_native(
                &cx,
                ROW_ANALYTIC_SOURCE,
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        assert_eq!(
            db.standing_native_columns(&cx, &source).unwrap(),
            ["marker", "source", "note", "target"]
        );
        let allowance = GqlQueryPolicy::new(100_000, 4, 10_000_000, 10_000_000);
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            let handle =
                register_row_analytic(&mut db, &cx, &source, metric, [1, 3], allowance).unwrap();
            assert_eq!(
                db.standing_native_columns(&cx, &handle).unwrap(),
                ["vertex", analytic_field(metric)]
            );
            let expected = row_analytic_oracle(&db, metric);
            assert_eq!(
                db.standing_native_query(&cx, &handle, policy()).unwrap(),
                (after, expected)
            );
            if metric == "core_number" {
                assert_eq!(
                    db.standing_core_number(&cx, &handle, VId(0)).unwrap(),
                    Some(2)
                );
                assert_eq!(
                    db.standing_core_number(&cx, &handle, VId(3)).unwrap(),
                    Some(1)
                );
                assert_eq!(
                    db.standing_core_number(&cx, &handle, VId(u128::MAX))
                        .unwrap(),
                    None
                );
                assert_eq!(
                    db.standing_core_numbers(&cx, &handle).unwrap().rows().len(),
                    4
                );
            }
            let (at, delivered) = db.standing_native_bag(&cx, &handle, policy()).unwrap();
            assert_eq!(at, after);
            handles.push((metric, handle, delivered));
        }
        // Initially the triangle plus tail has coefficient 1/3 at vertex 0,
        // coefficient 1 at 1/2, and core numbers 2,2,2,1. Removing only the
        // 1->2 row turns it into a star while the physical edge remains live.
        for tick in 0..8 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 | 1 | 5 => {
                    change.set_edge_property(
                        EId(11),
                        P,
                        Some(CanonicalScalar::Int(i64::from(tick != 0))),
                    );
                }
                2 => {
                    change.add_edge(EId(14), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
                    change.add_edge(EId(15), VId(2), VId(1), vec![(P, CanonicalScalar::Int(1))]);
                }
                3 => {
                    for id in [11, 14] {
                        change.set_edge_property(EId(id), P, Some(CanonicalScalar::Int(0)));
                    }
                }
                4 => {
                    change.set_edge_property(EId(15), P, Some(CanonicalScalar::Int(0)));
                }
                6 => {
                    change.set_edge_property(EId(13), P, Some(CanonicalScalar::Int(0)));
                }
                _ => {
                    for id in [10, 11, 12] {
                        change.set_edge_property(EId(id), P, Some(CanonicalScalar::Int(0)));
                    }
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            assert!(
                db.edge(EId(11)).unwrap().is_some(),
                "selection must not delete the base edge"
            );
            assert!(db.vertex(VId(3)).unwrap().is_some());
            for (metric, handle, delivered) in &mut handles {
                let expected = row_analytic_oracle(&db, metric);
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap(),
                    (at, expected),
                    "{metric}, tick {tick}"
                );
                let (frontier, delta) = db
                    .standing_native_delta(&cx, handle, after, policy())
                    .unwrap();
                assert_eq!(frontier, at);
                if matches!(tick, 2 | 3) {
                    assert!(
                        delta.is_empty(),
                        "parallel/opposite support does not change {metric}"
                    );
                }
                if matches!(tick, 0 | 4 | 6 | 7) {
                    assert!(
                        delta
                            .iter()
                            .any(|(_, weight)| weight.to_i128().is_some_and(|weight| weight < 0)),
                        "{metric} must retract old rows at tick {tick}"
                    );
                }
                delivered
                    .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                let (frontier, current) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, at);
                assert_eq!(&*delivered, &current);
                if tick == 2 {
                    db.rebuild_standing_query(&cx, handle, allowance).unwrap();
                    assert!(matches!(
                        db.standing_native_delta(&cx, handle, after, policy()),
                        Err(StandingQueryError::DeltaUnavailable { .. })
                    ));
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap().1,
                        current
                    );
                }
                if *metric == "core_number" && tick >= 6 {
                    assert_eq!(db.standing_core_number(&cx, handle, VId(3)).unwrap(), None);
                }
            }
            after = at;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn analytics_from_rows_require_valid_schemas_and_repair_parents_before_children() {
    let ((), report) = run_async_under_lab(0x6e73_7304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let small = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let parent = db
            .register_standing_native(&cx, ROW_ANALYTIC_SOURCE, &params, symbols, small)
            .unwrap();
        let healthy_parent = db
            .register_standing_native(&cx, ROW_ANALYTIC_SOURCE, &params, symbols, policy())
            .unwrap();
        let scalar = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.p AS value",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            // Empty inputs still validate declared endpoint types and bounds;
            // owner admission precedes even an invalid endpoint selection.
            for endpoints in [[0, 3], [1, 4]] {
                assert!(matches!(
                    register_row_analytic(&mut db, &cx, &parent, metric, endpoints, policy()),
                    Err(StandingQueryError::Unsupported)
                ));
            }
            assert!(matches!(
                register_row_analytic(&mut db, &cx, &scalar, metric, [0, 0], policy()),
                Err(StandingQueryError::Unsupported)
            ));
            assert!(matches!(
                register_row_analytic(
                    &mut foreign,
                    &cx,
                    &parent,
                    metric,
                    [usize::MAX, usize::MAX],
                    policy()
                ),
                Err(StandingQueryError::ForeignHandle)
            ));
            let dependent =
                register_row_analytic(&mut db, &cx, &parent, metric, [1, 3], policy()).unwrap();
            let healthy =
                register_row_analytic(&mut db, &cx, &healthy_parent, metric, [1, 3], policy())
                    .unwrap();
            assert!(
                db.standing_native_bag(&cx, &dependent, policy())
                    .unwrap()
                    .1
                    .is_empty()
            );
            handles.push((metric, dependent, healthy));
        }
        db.write(&commit, row_analytic_seed(false)).await.unwrap();
        for (metric, dependent, healthy) in &handles {
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy())
                    .unwrap()
                    .1,
                row_analytic_oracle(&db, metric)
            );
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy()).unwrap(),
                db.standing_native_query(&cx, healthy, policy()).unwrap()
            );
        }
        let mut grow = WriteBatch::new(RelationId(1));
        grow.set_edge_property(EId(13), P, Some(CanonicalScalar::Int(1)));
        let committed = db.write(&commit, grow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), committed);
        assert!(matches!(
            db.standing_native_query(&cx, &parent, policy()),
            Err(StandingQueryError::Unavailable { .. })
        ));
        for (metric, dependent, healthy) in &handles {
            assert!(matches!(
                db.standing_native_query(&cx, dependent, policy()),
                Err(StandingQueryError::Unavailable {
                    reason: StandingQueryFailure::DependencyUnavailable,
                    ..
                })
            ));
            assert!(matches!(
                db.rebuild_standing_query(&cx, dependent, policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::DependencyUnavailable
                ))
            ));
            assert_eq!(
                db.standing_native_query(&cx, healthy, policy()).unwrap().1,
                row_analytic_oracle(&db, metric)
            );
        }
        assert!(db.rebuild_standing_query(&cx, &parent, small).is_err());
        db.rebuild_standing_query(&cx, &parent, policy()).unwrap();
        for (metric, dependent, healthy) in &handles {
            assert!(matches!(
                db.standing_native_query(&cx, dependent, policy()),
                Err(StandingQueryError::Unavailable { .. })
            ));
            db.rebuild_standing_query(&cx, dependent, policy()).unwrap();
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy()).unwrap(),
                (committed, row_analytic_oracle(&db, metric))
            );
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy()).unwrap(),
                db.standing_native_query(&cx, healthy, policy()).unwrap()
            );
        }
        let mut selected_change = WriteBatch::new(RelationId(1));
        selected_change.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(0)));
        let at = db.write(&commit, selected_change).await.unwrap();
        assert!(db.edge(EId(11)).unwrap().is_some());
        for (metric, dependent, healthy) in &handles {
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy()).unwrap(),
                (at, row_analytic_oracle(&db, metric))
            );
            assert_eq!(
                db.standing_native_query(&cx, dependent, policy()).unwrap(),
                db.standing_native_query(&cx, healthy, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_six_native_set_laws_and_nested_grouping_follow_whole_commits() {
    let ((), report) = run_async_under_lab(0x6e73_7201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let handles: Vec<_> = OPS
            .iter()
            .map(|op| {
                db.register_standing_native(&cx, &query(op), &params, symbols, policy())
                    .unwrap()
            })
            .collect();
        let nested = format!("({}) EXCEPT ALL ({})", query(OPS[0]), query(OPS[2]));
        let nested_handle = db
            .register_standing_native(&cx, &nested, &params, symbols, policy())
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, value) in [
            (1, 1, Some(4)),
            (2, 1, Some(4)),
            (3, 2, Some(4)),
            (4, 2, Some(7)),
            (5, 1, None),
            (6, 2, None),
        ] {
            seed.create_vertex(
                VId(id),
                vec![LabelId(label)],
                value
                    .map(|v| (P, CanonicalScalar::Int(v)))
                    .into_iter()
                    .collect(),
            );
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(1));
        edit.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(4)));
        let mut labels = WriteBatch::new(RelationId(1));
        labels.set_vertex_label(VId(2), LabelId(1), false);
        labels.set_vertex_label(VId(2), LabelId(2), true);
        for batch in [seed, edit, labels] {
            db.write(&commit, batch).await.unwrap();
            for (at, handle) in handles.iter().enumerate() {
                let (frontier, actual) = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, db.frontier().unwrap());
                assert_eq!(actual, oracle(&db, at), "{}", OPS[at]);
                assert_eq!(
                    actual,
                    db.query(&cx, &query(OPS[at]), &params, symbols, policy())
                        .unwrap()
                );
                assert_eq!(db.standing_native_columns(&cx, handle).unwrap(), &["p"]);
            }
            assert_eq!(
                db.standing_native_query(&cx, &nested_handle, policy())
                    .unwrap()
                    .1,
                db.query(&cx, &nested, &params, symbols, policy()).unwrap()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_root_handle_repairs_failed_hidden_operands_and_preserves_independent_circuits() {
    let ((), report) = run_async_under_lab(0x6e73_7202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let text = query("INTERSECT ALL");
        let small = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let bounded = db
            .register_standing_native(&cx, &text, &params, symbols, small)
            .unwrap();
        let healthy = db
            .register_standing_native(&cx, &text, &params, symbols, policy())
            .unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        first.create_vertex(
            VId(1),
            vec![LabelId(1), LabelId(2)],
            vec![(P, CanonicalScalar::Int(4))],
        );
        db.write(&commit, first).await.unwrap();
        let mut second = WriteBatch::new(RelationId(1));
        second.create_vertex(VId(2), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, second).await.unwrap();
        assert!(matches!(
            db.standing_native_query(&cx, &bounded, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        let expected = db.standing_native_query(&cx, &healthy, policy()).unwrap();
        assert!(db.rebuild_standing_query(&cx, &bounded, small).is_err());
        assert!(matches!(
            db.standing_native_query(&cx, &bounded, policy()),
            Err(StandingQueryError::Unavailable { .. })
        ));
        assert_eq!(
            db.standing_native_query(&cx, &healthy, policy()).unwrap(),
            expected
        );
        db.rebuild_standing_query(&cx, &bounded, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &bounded, policy()).unwrap(),
            expected
        );
        let mut tick = WriteBatch::new(RelationId(1));
        tick.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, tick).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &bounded, policy()).unwrap(),
            db.standing_native_query(&cx, &healthy, policy()).unwrap()
        );
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            other.standing_native_query(&cx, &bounded, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintained_motif_unions_follow_membership_and_parallel_support() {
    let ((), report) = run_async_under_lab(0x6e73_7204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let allowance = GqlQueryPolicy::new(100_000, 6, 10_000_000, 10_000_000);
        let profiles: [(&str, &[LabelId], &[RelationId]); 3] = [
            ("labels:['R','L','R'],", &[LabelId(1), LabelId(2)], &[]),
            (
                "relations:['S','R','S'],",
                &[],
                &[RelationId(1), RelationId(2)],
            ),
            (
                "labels:['R','L','R'],relations:['S','R','S'],",
                &[LabelId(1), LabelId(2)],
                &[RelationId(1), RelationId(2)],
            ),
        ];
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            for &(projection, labels, relations) in &profiles {
                let field = analytic_field(metric);
                let text = format!(
                    "CALL fnx.{metric}({{{projection}direction:'undirected',\
                     parallel:'collapse',self_loops:'drop'}}) \
                     YIELD vertex AS node,{field} AS value RETURN node,value"
                );
                let handle = db
                    .register_standing_native(&cx, &text, &params, symbols, allowance)
                    .unwrap();
                let (_, delivered) = db.standing_native_bag(&cx, &handle, policy()).unwrap();
                handles.push((metric, labels, relations, text, handle, delivered));
            }
        }
        let mut after = db.frontier().unwrap();
        for tick in 0..13 {
            let relation = match tick {
                1 | 6 => RelationId(2),
                2 => RelationId(3),
                _ => RelationId(1),
            };
            let mut change = WriteBatch::new(relation);
            match tick {
                0 => {
                    for (id, labels) in [
                        (0, vec![LabelId(1)]),
                        (1, vec![LabelId(2)]),
                        (2, vec![LabelId(1), LabelId(2)]),
                        (3, vec![]),
                        (4, vec![LabelId(3)]),
                        (u128::MAX, vec![LabelId(2)]),
                    ] {
                        change.create_vertex(VId(id), labels, vec![]);
                    }
                    for (id, a, b) in [
                        (10, 0, 1),
                        (11, 1, 2),
                        (12, 3, 0),
                        (13, u128::MAX, u128::MAX),
                        (14, 0, 1),
                    ] {
                        change.add_edge(EId(id), VId(a), VId(b), vec![]);
                    }
                }
                1 => {
                    for (id, a, b) in [(20, 2, 0), (21, 0, 1), (22, 2, 3)] {
                        change.add_edge(EId(id), VId(a), VId(b), vec![]);
                    }
                }
                2 => {
                    // An excluded third type would change the motif counts
                    // if a union were widened to the whole graph.
                    change.add_edge(EId(30), VId(0), VId(u128::MAX), vec![]);
                    change.add_edge(EId(31), VId(u128::MAX), VId(1), vec![]);
                }
                3 => {
                    change.set_vertex_label(VId(2), LabelId(1), false);
                }
                4 => {
                    // Both membership inputs change in one atomic tick.
                    change.set_vertex_label(VId(0), LabelId(1), false);
                    change.set_vertex_label(VId(0), LabelId(2), true);
                }
                5 => {
                    change.delete_edge(EId(10));
                    change.delete_edge(EId(14));
                }
                6 => {
                    change.delete_edge(EId(21));
                }
                7 => {
                    change.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(99)));
                }
                8 => {
                    change.set_vertex_label(VId(3), LabelId(2), true);
                }
                9 => {
                    // Endpoint selection and edge removal share a successor;
                    // no transient half-selected topology may be published.
                    change.set_vertex_label(VId(2), LabelId(2), false);
                    change.delete_edge(EId(11));
                }
                10 => {
                    change.set_vertex_label(VId(u128::MAX), LabelId(2), false);
                }
                11 => {
                    change.set_vertex_label(VId(u128::MAX), LabelId(1), true);
                }
                _ => {
                    change.delete_vertex(VId(3));
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            for (metric, labels, relations, text, handle, delivered) in &mut handles {
                let expected = union_analytic_oracle(&db, metric, labels, relations);
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap(),
                    (at, expected),
                    "{text}, tick {tick}"
                );
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    "fresh projection: {text}, tick {tick}"
                );
                let (frontier, delta) = db
                    .standing_native_delta(&cx, handle, after, policy())
                    .unwrap();
                assert_eq!(frontier, at);
                if matches!(tick, 3 | 4 | 5 | 7) {
                    assert!(delta.is_empty(), "unchanged support: {text}, tick {tick}");
                }
                delivered
                    .integrate(&delta, fgdb_delta_types::LimbLimit::new(4), &mut |_| {
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                let (_, current) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(&*delivered, &current);
                if tick == 6 {
                    // Rebuild must remap the shared vertex dependency once.
                    db.rebuild_standing_query(&cx, handle, allowance).unwrap();
                    assert_eq!(
                        db.standing_native_bag(&cx, handle, policy()).unwrap().1,
                        current
                    );
                }
            }
            after = at;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintained_motif_unions_accept_all_sixteen_selected_ids_without_widening() {
    let ((), report) = run_async_under_lab(0x6e73_7205, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let resolve = |kind, name: &str| match kind {
            GraphSymbolKind::Label => name
                .strip_prefix('L')
                .and_then(|id| id.parse::<u64>().ok())
                .map(|id| GraphSymbol::Label(LabelId(id))),
            GraphSymbolKind::Relation => name
                .strip_prefix('R')
                .and_then(|id| id.parse::<u64>().ok())
                .map(|id| GraphSymbol::Relation(RelationId(id))),
            _ => None,
        };
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, labels) in [
            (0, vec![LabelId(1)]),
            (1, vec![LabelId(9)]),
            (2, vec![LabelId(1), LabelId(16)]),
            (3, vec![LabelId(17)]),
            (u128::MAX, vec![LabelId(16)]),
        ] {
            seed.create_vertex(VId(id), labels, vec![]);
        }
        seed.add_edge(EId(10), VId(0), VId(1), vec![]);
        db.write(&commit, seed).await.unwrap();
        let mut ninth = WriteBatch::new(RelationId(9));
        ninth.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(&commit, ninth).await.unwrap();
        let mut sixteenth = WriteBatch::new(RelationId(16));
        for (id, a, b) in [(12, 2, 0), (13, 0, 3), (14, 3, 1)] {
            sixteenth.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, sixteenth).await.unwrap();
        let mut excluded = WriteBatch::new(RelationId(17));
        excluded.add_edge(EId(15), VId(0), VId(u128::MAX), vec![]);
        excluded.add_edge(EId(16), VId(u128::MAX), VId(1), vec![]);
        db.write(&commit, excluded).await.unwrap();
        let labels: Vec<_> = (1..=16).map(LabelId).collect();
        let relations: Vec<_> = (1..=16).map(RelationId).collect();
        let names = |prefix: char, count: u64| {
            (1..=count)
                .rev()
                .map(|id| format!("'{prefix}{id}'"))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut handles = Vec::new();
        for metric in ["triangles", "clustering_coefficient", "core_number"] {
            let field = analytic_field(metric);
            let text = format!(
                "CALL fnx.{metric}({{labels:[{},'L16'],relations:[{},'R9'],\
                 direction:'undirected',parallel:'collapse',self_loops:'drop'}}) \
                 YIELD vertex AS node,{field} AS value RETURN node,value",
                names('L', 16),
                names('R', 16),
            );
            let handle = db
                .register_standing_native(&cx, &text, &params, resolve, policy())
                .unwrap();
            assert_eq!(
                db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                union_analytic_oracle(&db, metric, &labels, &relations)
            );
            assert_eq!(
                db.standing_native_query(&cx, &handle, policy()).unwrap().1,
                db.query(&cx, &text, &params, resolve, policy()).unwrap()
            );
            handles.push((metric, text, handle));
        }
        for tick in 0..3 {
            let mut change = WriteBatch::new(RelationId(9));
            match tick {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.set_vertex_label(VId(1), LabelId(9), false);
                }
                _ => {
                    change.set_vertex_label(VId(1), LabelId(16), true);
                    change.add_edge(EId(17), VId(1), VId(2), vec![]);
                }
            }
            db.write(&commit, change).await.unwrap();
            for (metric, text, handle) in &handles {
                let expected = union_analytic_oracle(&db, metric, &labels, &relations);
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    expected,
                    "{text}, tick {tick}"
                );
                db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    expected
                );
            }
        }
        // Seventeen DISTINCT labels or types still refuse during preparation.
        for selection in [
            format!("labels:[{}]", names('L', 17)),
            format!("relations:[{}]", names('R', 17)),
        ] {
            let text = format!(
                "CALL fnx.triangles({{{selection},parallel:'collapse',self_loops:'drop'}}) \
                 YIELD vertex,triangles RETURN vertex,triangles"
            );
            assert!(
                db.register_standing_native(&cx, &text, &params, resolve, policy())
                    .is_err()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameters_freeze_per_circuit_and_unsupported_final_pages_refuse_without_fallback() {
    let ((), report) = run_async_under_lab(0x6e73_7203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text =
            "MATCH (n:L) WHERE n.p > $floor RETURN n.p AS p UNION ALL MATCH (n:R) RETURN n.p AS p";
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let template = PreparedNativeRead::prepare(text, &low, symbols).unwrap();
        let a = template
            .register_standing(&mut db, &cx, &low, policy())
            .unwrap();
        let b = template
            .register_standing(&mut db, &cx, &high, policy())
            .unwrap();
        assert!(matches!(
            template.register_standing(&mut db, &cx, &GqlParameters::new(), policy()),
            Err(StandingQueryError::NativePrepare(_))
        ));
        drop(template);
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(4))]);
        db.write(&commit, seed).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &a, policy()).unwrap().1,
            db.query(&cx, text, &low, symbols, policy()).unwrap()
        );
        assert_eq!(
            db.standing_native_query(&cx, &b, policy()).unwrap().1,
            db.query(&cx, text, &high, symbols, policy()).unwrap()
        );
        assert_ne!(
            db.standing_native_query(&cx, &a, policy()).unwrap().1,
            db.standing_native_query(&cx, &b, policy()).unwrap().1
        );
        let paged = format!("({}) ORDER BY p DESC LIMIT 0", query("UNION ALL"));
        let expected = db
            .query(&cx, &paged, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let window = db
            .register_standing_native(&cx, &paged, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &window, policy()).unwrap().1,
            expected
        );
        // This product-free set has canonical order before its bare page.
        let paged = format!("({}) LIMIT 0", query("UNION ALL"));
        let expected = db
            .query(&cx, &paged, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let window = db
            .register_standing_native(&cx, &paged, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &window, policy()).unwrap().1,
            expected
        );
        let unbounded = format!("({}) ORDER BY p DESC", query("UNION ALL"));
        assert!(matches!(
            db.register_standing_native(&cx, &unbounded, &GqlParameters::new(), symbols, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(db.standing_native_query(&cx, &a, policy()).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
