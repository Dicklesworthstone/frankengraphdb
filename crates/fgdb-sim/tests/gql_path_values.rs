//! Captured path identities survive the real snapshot and transaction adapters.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
    PreparedTemporalGraphText,
};
use fgdb_reference::ReferenceGraph;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const R: RelationId = RelationId(1);
const KEY: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn routes(rows: &[GraphValueRow]) -> Vec<Vec<EId>> {
    rows.iter()
        .map(|row| {
            row.values()[0]
                .as_path()
                .expect("typed path column")
                .edges()
                .collect()
        })
        .collect()
}

#[test]
fn temporal_edge_delete_preserves_real_path_ids_and_governed_growth() {
    let ((), report) = run_async_under_lab(0x4_7474, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let keys = DatabaseKeys::new(
            [0x71; 32],
            DatabaseSecurityNamespaceId([0x72; 32]),
            [0x73; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=4 {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(KEY, CanonicalScalar::Int(id as i64))],
            );
        }
        for (eid, src, dst) in [(91, 1, 2), (92, 2, 4), (93, 1, 3), (94, 3, 4), (95, 1, 4)] {
            batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        let before = db.write(&commit, batch).await.unwrap();
        let template = PreparedTemporalGraphText::prepare(
            "MATCH p = ALL SHORTEST WALK (a)-[:R*1..4]->(b) FOR SYSTEM_TIME AS OF SEQ $at WHERE a.key=1 AND b.key=4 RETURN p", symbols).unwrap();
        let at = |seq| {
            template
                .bind_parameters(&GqlParameters::new().with_uint64("at", seq).unwrap())
                .unwrap()
        };
        let old = db
            .execute_temporal_graph_text_governed(&query, &at(before.0), policy())
            .unwrap();
        assert_eq!(routes(&old.value), vec![vec![EId(95)]]);
        let mut deletion = WriteBatch::new(R);
        deletion.delete_edge(EId(95));
        let after = db.write(&commit, deletion).await.unwrap();
        let current = db
            .execute_temporal_graph_text_governed(&query, &at(after.0), policy())
            .unwrap();
        assert_eq!(
            routes(&current.value),
            vec![vec![EId(91), EId(92)], vec![EId(93), EId(94)]]
        );
        assert_eq!(
            routes(
                &db.execute_temporal_graph_text_governed(&query, &at(before.0), policy())
                    .unwrap()
                    .value
            ),
            vec![vec![EId(95)]]
        );
        let caps = [
            current.rows.snapshot_records,
            current.rows.result_rows,
            current.evaluator.work_units,
            current.evaluator.scratch_entries,
        ];
        for dimension in 0..4 {
            let mut limited = caps;
            limited[dimension] -= 1;
            assert!(
                db.execute_temporal_graph_text_governed(
                    &query,
                    &at(after.0),
                    GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3])
                )
                .is_err(),
                "dimension {dimension}"
            );
        }
        let pattern = PreparedGraphText::prepare(
            "MATCH p = ANY SHORTEST WALK (a)-[:R*1..4]->(b) WHERE a.key=1 AND b.key=4 RETURN p",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut shortcut = WriteBatch::new(R);
        shortcut.add_edge(EId(99), VId(1), VId(4), vec![]);
        txn.write(&mut db, shortcut).unwrap();
        assert_eq!(
            routes(
                &txn.execute_graph_pattern_governed(&db, &query, &pattern, policy())
                    .unwrap()
                    .value
            ),
            vec![vec![EId(99)]]
        );
        assert_eq!(
            routes(
                &db.execute_graph_pattern_governed(&query, &pattern, policy())
                    .unwrap()
                    .value
            ),
            vec![vec![EId(91), EId(92)]]
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn reference_bfs(graph: &ReferenceGraph, start: VId) -> BTreeMap<VId, i64> {
    let mut distances = BTreeMap::from([(start, 0)]);
    let mut pending = VecDeque::from([start]);
    while let Some(source) = pending.pop_front() {
        let next_distance = distances[&source] + 1;
        for (_, edge) in graph.iter_edges() {
            if edge.relation != R || edge.src != source {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(entry) = distances.entry(edge.dst) {
                entry.insert(next_distance);
                pending.push_back(edge.dst);
            }
        }
    }
    distances
}

#[test]
fn seeded_database_shortest_walk_lengths_match_reference_bfs() {
    let mut graph_shapes = BTreeSet::new();
    for seed in [0x51_0747_u64, 0x92_1849, 0xd3_2951] {
        let (shape, report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let keys = DatabaseKeys::new(
                [0x81; 32],
                DatabaseSecurityNamespaceId([0x82; 32]),
                [0x83; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=8 {
                batch.create_vertex(
                    VId(id),
                    vec![],
                    vec![(KEY, CanonicalScalar::Int(id as i64))],
                );
            }

            // A six-vertex directed cycle reaches a sink; vertex eight is isolated.
            // Parallel edges and a self-loop exercise WALK without changing BFS distance.
            let mut endpoints = vec![
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 1),
                (6, 7),
                (1, 2),
                (3, 3),
            ];
            let mut state = seed;
            for _ in 0..12 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let source = 1 + (state >> 32) % 5;
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let target = 1 + (state >> 32) % 5;
                endpoints.push((source, target));
            }
            for (ordinal, &(source, target)) in endpoints.iter().enumerate() {
                batch.add_edge(
                    EId(ordinal as u128 + 1),
                    VId(source as u128),
                    VId(target as u128),
                    vec![],
                );
            }
            db.write(&commit, batch).await.unwrap();

            // Materialize the independent oracle from actual committed delta rows,
            // never from an engine graph adapter or an engine/reference path search.
            let mut reference = ReferenceGraph::new();
            for committed in db.delta_since(fgdb_types::CommitSeq::ORIGIN).unwrap() {
                for entry in committed.coordinate_entries() {
                    for row in &entry.rows {
                        reference.apply_row(row).unwrap();
                    }
                }
            }
            assert_eq!(reference.vertex_count(), 8);
            assert_eq!(reference.edge_count(), 21);
            let shape: Vec<_> = reference
                .iter_edges()
                .map(|(_, edge)| (edge.src, edge.dst))
                .collect();
            let template = PreparedGraphText::prepare(
                "MATCH p = ANY SHORTEST WALK (a)-[:R*0..7]->(b) WHERE a.key=$source AND b.key=$target RETURN path_length(p)",
                symbols,
            ).unwrap();
            let mut reachable = 0;
            let mut unreachable = 0;
            let mut zero_hop = 0;
            let mut multi_hop = 0;
            let mut three_or_more_hops = 0;
            for (source, _) in reference.iter_vertices() {
                let distances = reference_bfs(&reference, source);
                for (target, _) in reference.iter_vertices() {
                    let parameters = GqlParameters::new()
                        .with_int64("source", source.0 as i64)
                        .unwrap()
                        .with_int64("target", target.0 as i64)
                        .unwrap();
                    let pattern = template.bind_parameters(&parameters).unwrap();
                    let actual = db
                        .execute_graph_pattern_governed(&query, &pattern, policy())
                        .unwrap()
                        .value;
                    match distances.get(&target) {
                        Some(&distance) => {
                            assert_eq!(actual.len(), 1, "seed {seed:#x}, {source:?}->{target:?}");
                            assert_eq!(actual[0].values().len(), 1);
                            assert_eq!(
                                actual[0].values()[0].as_scalar(),
                                Some(&CanonicalScalar::Int(distance)),
                                "seed {seed:#x}, {source:?}->{target:?}",
                            );
                            reachable += 1;
                            zero_hop += usize::from(distance == 0);
                            multi_hop += usize::from(distance >= 2);
                            three_or_more_hops += usize::from(distance >= 3);
                        }
                        None => {
                            assert!(actual.is_empty(), "seed {seed:#x}, {source:?}->{target:?}");
                            unreachable += 1;
                        }
                    }
                }
            }
            assert_eq!(
                (reachable, zero_hop, unreachable),
                (44, 8, 20),
                "seed {seed:#x}"
            );
            assert!(
                multi_hop >= 5,
                "seed {seed:#x}: {multi_hop} multi-hop pairs"
            );
            assert!(
                three_or_more_hops >= 4,
                "seed {seed:#x}: {three_or_more_hops} long pairs"
            );
            shape
        });
        assert!(report.lab_test_passed(), "seed {seed:#x}: {report:?}");
        assert!(
            graph_shapes.insert(shape),
            "seed {seed:#x} repeated an earlier graph"
        );
    }
    assert_eq!(graph_shapes.len(), 3);
}
