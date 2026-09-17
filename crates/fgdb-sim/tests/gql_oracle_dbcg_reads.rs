//! Minimal independent differential for an anonymous intermediate node.
//! Expected rows come only from public storage scans, not GQL evaluation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        _ => None,
    }
}

#[test]
fn anonymous_intermediate_nodes_match_independent_reference() {
    let ((), report) = run_async_under_lab(0xD001, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let dir =
            std::env::temp_dir().join(format!("fgdb-oracle-dbcg-minimal-{}", std::process::id()));
        let keys = DatabaseKeys::new(
            [0x6b; 32],
            DatabaseSecurityNamespaceId([0x78; 32]),
            [0x3d; 32],
        );
        let mut db = Database::create(&commit, &dir, keys)
            .await
            .expect("oracle database");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![PERSON], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.create_vertex(VId(4), vec![], vec![]);
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        batch.add_edge(EId(3), VId(4), VId(2), vec![]);
        let as_of = db.write(&commit, batch).await.expect("fixture commit");

        // Each edge pair contributes a row; no endpoint or row deduplication.
        // The scans select visible versions. Only the source has a label test.
        let vertices = db.vertices_at(as_of).expect("vertex scan");
        let edges = db.edges_at(as_of).expect("edge scan");
        let mut pairs = Vec::new();
        for first in edges.iter().filter(|edge| edge.entry.relation == R) {
            let Some(source) = vertices.iter().find(|v| v.vid == first.entry.src) else {
                continue;
            };
            if !source.labels.contains(&PERSON)
                || !vertices.iter().any(|v| v.vid == first.entry.dst)
            {
                continue;
            }
            for second in edges
                .iter()
                .filter(|edge| edge.entry.relation == R && edge.entry.src == first.entry.dst)
            {
                if vertices.iter().any(|v| v.vid == second.entry.dst) {
                    pairs.push((first.entry.src, second.entry.dst));
                }
            }
        }
        pairs.sort_unstable();
        assert_eq!(
            pairs,
            vec![(VId(1), VId(3))],
            "fixture has a two-hop match with an unlabeled intermediate, not an unlabeled source"
        );
        let expected: Vec<Vec<GraphValue>> = pairs
            .into_iter()
            .map(|(a, c)| vec![GraphValue::Vertex(a), GraphValue::Vertex(c)])
            .collect();

        let text = "MATCH (a:Person)-[:KNOWS]->()-[:KNOWS]->(c) RETURN a,c ORDER BY a,c";
        let result = db
            .query(
                &cx,
                text,
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
            )
            .unwrap_or_else(|error| panic!("query={text}: {error:?}"));
        let QueryResult::Rows { rows, .. } = result else {
            panic!("expected rows for {text}, got {result:?}");
        };
        let actual: Vec<Vec<GraphValue>> = rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| match cell {
                        GraphAggregateValue::Value(value) => value,
                        other => panic!("expected vertex value cell, got {other:?}"),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(actual, expected, "query={text}");
    });
    assert!(report.lab_test_passed(), "report={report:?}");
}

/// Plain bounded MATCH is a WALK: parallel edges, revisited vertices and
/// reused edges each contribute their full occurrence count. The reference
/// expands only public storage scans, without invoking any GQL evaluator.
#[test]
fn bounded_plain_match_walks_match_storage_scan_oracle() {
    for seed in [0xD011_u64, 0xD012, 0xD013] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-oracle-dbcg-bounded-{}-{seed}",
                std::process::id()
            ));
            let keys = DatabaseKeys::new(
                [0x6b; 32],
                DatabaseSecurityNamespaceId([0x78; 32]),
                [0x3d; 32],
            );
            let mut db = Database::create(&commit, &dir, keys)
                .await
                .expect("bounded oracle database");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![PERSON], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.create_vertex(VId(3), vec![PERSON], vec![]);
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            batch.add_edge(EId(2), VId(1), VId(2), vec![]);
            batch.add_edge(EId(3), VId(2), VId(1), vec![]);
            batch.add_edge(EId(4), VId(1), VId(1), vec![]);
            // VId(3) stays isolated; seeded extra parallel occurrences vary
            // the graph itself, not merely the lab scheduler.
            for extra in 0..seed % 3 {
                batch.add_edge(EId(u128::from(10 + extra)), VId(1), VId(2), vec![]);
            }
            let historical = db.write(&commit, batch).await.expect("initial graph");
            let mut deletion = WriteBatch::new(R);
            deletion.delete_edge(EId(2));
            let frontier = db
                .write(&commit, deletion)
                .await
                .expect("delete occurrence");
            let mut snapshot_outputs = Vec::new();
            for (as_of, temporal) in [
                (
                    historical,
                    format!(" FOR SYSTEM_TIME AS OF SEQ {}", historical.0),
                ),
                (frontier, String::new()),
            ] {
                let vertices = db.vertices_at(as_of).expect("visible vertices");
                let edges = db.edges_at(as_of).expect("visible edges");
                for (min, max) in [(0, 0), (0, 4), (2, 4)] {
                    let mut mode_outputs = Vec::new();
                    for mode in ["", " WALK", " TRAIL"] {
                        let mut pairs = Vec::new();
                        for source in vertices.iter().filter(|v| v.labels.contains(&PERSON)) {
                            let mut layer = vec![(source.vid, Vec::<EId>::new())];
                            for depth in 0..=max {
                                if depth >= min {
                                    pairs.extend(
                                        layer.iter().map(|(target, _)| (source.vid, *target)),
                                    );
                                }
                                if depth == max {
                                    break;
                                }
                                let mut next = Vec::new();
                                for (at, used) in layer {
                                    for edge in edges.iter().filter(|edge| {
                                        edge.entry.relation == R && edge.entry.src == at
                                    }) {
                                        if vertices.iter().any(|v| v.vid == edge.entry.dst)
                                            && (mode != " TRAIL" || !used.contains(&edge.entry.eid))
                                        {
                                            let mut path = used.clone();
                                            path.push(edge.entry.eid);
                                            next.push((edge.entry.dst, path));
                                        }
                                    }
                                }
                                layer = next;
                            }
                        }
                        pairs.sort_unstable();
                        if max == 0 {
                            assert_eq!(pairs, vec![(VId(1), VId(1)), (VId(3), VId(3))]);
                        } else {
                            assert!(
                                pairs
                                    .iter()
                                    .filter(|pair| **pair == (VId(1), VId(1)))
                                    .count()
                                    > 1,
                                "anti-vacuity: returning cycles retain repeated occurrences"
                            );
                            if min == 0 {
                                assert!(
                                    pairs.contains(&(VId(3), VId(3))),
                                    "zero-hop isolated root"
                                );
                                if mode.is_empty() {
                                    snapshot_outputs.push(pairs.clone());
                                }
                            } else {
                                assert!(!pairs.contains(&(VId(3), VId(3))), "positive lower bound");
                            }
                        }
                        mode_outputs.push(pairs.clone());
                        let expected: Vec<Vec<GraphValue>> = pairs
                            .into_iter()
                            .map(|(a, b)| vec![GraphValue::Vertex(a), GraphValue::Vertex(b)])
                            .collect();
                        let text = format!(
                            "MATCH{mode} (a:Person)-[:KNOWS*{min}..{max}]->(b){temporal} RETURN a,b ORDER BY a,b"
                        );
                        let result = db
                            .query(
                                &cx,
                                &text,
                                &GqlParameters::new(),
                                symbols,
                                GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
                            )
                            .unwrap_or_else(|error| panic!("seed={seed} query={text}: {error:?}"));
                        let QueryResult::Rows { rows, .. } = result else {
                            panic!("expected rows for {text}, got {result:?}");
                        };
                        let actual: Vec<Vec<GraphValue>> = rows
                            .into_iter()
                            .map(|row| {
                                row.into_iter()
                                    .map(|cell| match cell {
                                        GraphAggregateValue::Value(value) => value,
                                        other => panic!("expected vertex value cell, got {other:?}"),
                                    })
                                    .collect()
                            })
                            .collect();
                        assert_eq!(actual, expected, "seed={seed} query={text}");
                    }
                    assert_eq!(mode_outputs[0], mode_outputs[1], "plain MATCH is WALK");
                    if max > 0 {
                        assert_ne!(mode_outputs[0], mode_outputs[2], "TRAIL forbids edge reuse");
                    }
                }
            }
            assert_ne!(
                snapshot_outputs[0], snapshot_outputs[1],
                "delete changes walk multiplicity"
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
