//! Independent bounded-walk differential for native path restrictors.
//! Plan §8.2 and merged changes 492ee302 / 072451a6 / 72c9b9dc:
//! WALK permits repeated vertices and edges; TRAIL forbids repeated edge IDs;
//! ACYCLIC forbids every repeated vertex; SIMPLE permits only first == last.
//! A SIMPLE closure (including a self-loop) is terminal. Zero hops is valid.
//! TRAIL is refused by the native compiler: refusal is NOT execution coverage.
//! Accepted spelling: MATCH route = SIMPLE (a)-[:R*0..3]->(b).
//! RETURN ALL endpoints sort lexicographically with multiplicity. Captures sort
//! by (start, [(edge ID, next vertex)]), not by hop count or node sequence.
//! Incoming/undirected paths obey the same restrictions. An undirected loop
//! occurs once; SIMPLE permits one edge out/back, unlike edge-unique TRAIL.
//! The oracle generates unrestricted walks from replayed ReferenceGraph edges,
//! then filters complete paths. No engine evaluator/comparator constructs the
//! expected answers. Actual result order is never normalized before comparison.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
    PreparedTemporalGraphText,
};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::{
    BranchId, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Walk,
    Trail,
    Acyclic,
    Simple,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Walk => "WALK",
            Self::Trail => "TRAIL",
            Self::Acyclic => "ACYCLIC",
            Self::Simple => "SIMPLE",
        }
    }
}
#[derive(Clone, Copy, Debug)]
enum Direction {
    Out,
    In,
    Both,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Path {
    vertices: Vec<VId>,
    edges: Vec<EId>,
}
type Route = (VId, Vec<(EId, VId)>);

fn admits(path: &Path, mode: Mode) -> bool {
    match mode {
        Mode::Walk => true,
        Mode::Trail => path.edges.iter().collect::<BTreeSet<_>>().len() == path.edges.len(),
        Mode::Acyclic => path.vertices.iter().collect::<BTreeSet<_>>().len() == path.vertices.len(),
        Mode::Simple => {
            let vertices =
                if path.vertices.len() > 1 && path.vertices.first() == path.vertices.last() {
                    &path.vertices[..path.vertices.len() - 1]
                } else {
                    &path.vertices
                };
            vertices.iter().collect::<BTreeSet<_>>().len() == vertices.len()
        }
    }
}

fn enumerate(
    graph: &ReferenceGraph,
    minimum: usize,
    maximum: usize,
    direction: Direction,
) -> Vec<Path> {
    let mut layer: Vec<_> = graph
        .iter_vertices()
        .map(|(vertex, _)| Path {
            vertices: vec![vertex],
            edges: Vec::new(),
        })
        .collect();
    let mut paths = Vec::new();
    for depth in 0..=maximum {
        if depth >= minimum {
            paths.extend(layer.iter().cloned());
        }
        if depth == maximum {
            break;
        }
        let mut next = Vec::new();
        for path in layer {
            let tip = *path.vertices.last().expect("path source");
            for (id, edge) in graph.iter_edges() {
                if edge.relation != R {
                    continue;
                }
                let destination = match direction {
                    Direction::Out if edge.src == tip => Some(edge.dst),
                    Direction::In if edge.dst == tip => Some(edge.src),
                    Direction::Both if edge.src == tip => Some(edge.dst),
                    Direction::Both if edge.dst == tip => Some(edge.src),
                    _ => None,
                };
                if let Some(destination) = destination {
                    let mut extended = path.clone();
                    extended.vertices.push(destination);
                    extended.edges.push(id);
                    next.push(extended);
                }
            }
        }
        layer = next;
    }
    paths
}

#[derive(Clone, Copy, Debug)]
struct Case {
    mode: Mode,
    direction: Direction,
    minimum: usize,
    maximum: usize,
    capture: bool,
}
impl Case {
    fn text(self, at: Option<CommitSeq>) -> String {
        let head = if self.capture { "route = " } else { "" };
        let body = match self.direction {
            Direction::Out => format!("(a)-[:R*{}..{}]->(b)", self.minimum, self.maximum),
            Direction::In => format!("(a)<-[:R*{}..{}]-(b)", self.minimum, self.maximum),
            Direction::Both => format!("(a)-[:R*{}..{}]-(b)", self.minimum, self.maximum),
        };
        let temporal = at.map_or_else(String::new, |at| {
            format!(" FOR SYSTEM_TIME AS OF SEQ {}", at.0)
        });
        let columns = if self.capture { "route" } else { "a, b" };
        format!(
            "MATCH {head}{} {body}{temporal} RETURN ALL {columns}",
            self.mode.name()
        )
    }
}
#[derive(Debug, PartialEq, Eq)]
enum Rows {
    Endpoints(Vec<(VId, VId)>),
    Captures(Vec<Route>),
}

fn expected(paths: &[Path], case: Case) -> Rows {
    let admitted = paths.iter().filter(|path| admits(path, case.mode));
    if case.capture {
        let mut rows: Vec<_> = admitted
            .map(|p| {
                (
                    p.vertices[0],
                    p.edges
                        .iter()
                        .copied()
                        .zip(p.vertices[1..].iter().copied())
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        rows.sort();
        Rows::Captures(rows)
    } else {
        let mut rows: Vec<_> = admitted
            .map(|p| (p.vertices[0], *p.vertices.last().expect("source")))
            .collect();
        rows.sort();
        Rows::Endpoints(rows)
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn execute(db: &Database, cx: &QueryCx, case: Case, at: Option<CommitSeq>) -> Rows {
    let text = case.text(at);
    let policy = GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000);
    let rows = if at.is_some() {
        let query = PreparedTemporalGraphText::prepare(&text, symbols)
            .expect("temporal restrictor prepares")
            .bind_parameters(&GqlParameters::new())
            .expect("temporal binds");
        db.execute_temporal_graph_text_governed(cx, &query, policy)
            .expect("temporal executes")
            .value
    } else {
        let query = PreparedGraphText::prepare(&text, symbols)
            .expect("restrictor prepares")
            .bind_parameters(&GqlParameters::new())
            .expect("binds");
        db.execute_graph_pattern_governed(cx, &query, policy)
            .expect("executes")
            .value
    };
    if case.capture {
        Rows::Captures(
            rows.iter()
                .map(|row| {
                    let path = row.values()[0].as_path().expect("path column");
                    (path.start(), path.steps().to_vec())
                })
                .collect(),
        )
    } else {
        Rows::Endpoints(
            rows.iter()
                .map(|row| {
                    (
                        row.values()[0].as_vertex().expect("source column"),
                        row.values()[1].as_vertex().expect("target column"),
                    )
                })
                .collect(),
        )
    }
}
fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for mode in [Mode::Walk, Mode::Acyclic, Mode::Simple] {
        for direction in [Direction::Out, Direction::In, Direction::Both] {
            for (minimum, maximum) in [(0, 0), (0, 3), (2, 4), (4, 4)] {
                for capture in [false, true] {
                    cases.push(Case {
                        mode,
                        direction,
                        minimum,
                        maximum,
                        capture,
                    });
                }
            }
        }
    }
    cases
}
fn topology_coverage(graph: &ReferenceGraph) {
    let walks = enumerate(graph, 0, 4, Direction::Out);
    for mode in [Mode::Trail, Mode::Acyclic, Mode::Simple] {
        assert!(
            walks.iter().any(|p| !admits(p, mode)),
            "WALK strictly contains {mode:?}"
        );
    }
    assert!(
        walks
            .iter()
            .any(|p| p.edges.len() == 1 && p.vertices[0] == p.vertices[1]),
        "self-loop hit"
    );
    assert!(
        walks.iter().any(|p| p.edges.len() >= 2
            && p.vertices.first() == p.vertices.last()
            && admits(p, Mode::Simple)),
        "closed SIMPLE cycle hit"
    );
    assert!(
        walks.iter().any(|a| a.edges.len() == 1
            && walks
                .iter()
                .any(|b| a.vertices == b.vertices && a.edges != b.edges)),
        "parallel edge IDs hit"
    );
    assert!(
        walks.iter().any(|a| a.edges.len() == 2
            && a.vertices[0] == VId(1)
            && a.vertices[2] == VId(4)
            && walks.iter().any(|b| b.edges.len() == 2
                && b.vertices[0] == VId(1)
                && b.vertices[2] == VId(4)
                && b.vertices[1] != a.vertices[1])),
        "diamond routes hit"
    );
    assert!(
        walks
            .iter()
            .any(|p| admits(p, Mode::Trail) && !admits(p, Mode::Acyclic)),
        "comparison mutation must distinguish TRAIL from ACYCLIC"
    );
    let undirected = enumerate(graph, 2, 2, Direction::Both);
    assert!(
        undirected
            .iter()
            .any(|p| admits(p, Mode::Simple) && !admits(p, Mode::Trail)),
        "SIMPLE is not edge unique"
    );
}
fn verify(graph: &ReferenceGraph, results: &[(Case, Rows)], seed: u64, at: Option<CommitSeq>) {
    topology_coverage(graph);
    for (case, actual) in results {
        let paths = enumerate(graph, case.minimum, case.maximum, case.direction);
        let wanted = expected(&paths, *case);
        assert_eq!(actual, &wanted, "seed={seed} query={}", case.text(at));
    }
}

#[test]
fn seeded_path_restrictors_match_independent_reference() {
    for seed in [0xdb01_u64, 0xdb02, 0xdb03, 0xdb04] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let dir = std::env::temp_dir().join(format!(
                "fgdb-path-restrictor-oracle-{}-{seed}",
                std::process::id()
            ));
            let namespace = DatabaseSecurityNamespaceId([0x77; 32]);
            let mut db = Database::create(
                &commit,
                &dir,
                DatabaseKeys::new([0x5a; 32], namespace, [0x3c; 32]),
            )
            .await
            .expect("create database");
            let mut batch = WriteBatch::new(R);
            // Vertex 6 stays isolated, exercising zero-hop identity paths.
            for id in 1..=6 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            // A diamond, closing cycle, loop, and three parallel occurrences.
            for (id, src, dst) in [
                (10, 1, 2),
                (11, 1, 2),
                (12, 1, 2),
                (13, 1, 3),
                (14, 2, 4),
                (15, 3, 4),
                (16, 4, 1),
                (17, 2, 2),
                (18, 4, 5),
            ] {
                batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
            }
            let mut random = seed;
            for id in 30..34 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let src = u128::from((random >> 32) % 5 + 1);
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                let dst = u128::from((random >> 32) % 5 + 1);
                batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
            }
            let before = db.write(&commit, batch).await.expect("seed commits");
            let mut deletion = WriteBatch::new(R);
            deletion.delete_edge(EId(12));
            let after = db.write(&commit, deletion).await.expect("delete commits");
            // Refused TRAIL is recorded, never silently omitted or called coverage.
            for capture in [false, true] {
                let refused = Case {
                    mode: Mode::Trail,
                    direction: Direction::Out,
                    minimum: 0,
                    maximum: 3,
                    capture,
                };
                assert!(
                    PreparedGraphText::prepare(&refused.text(None), symbols).is_err(),
                    "TRAIL now accepted: extend execution differential"
                );
            }
            let results: Vec<_> = [None, Some(before), Some(after)]
                .into_iter()
                .map(|at| {
                    let rows = cases()
                        .into_iter()
                        .map(|case| (case, execute(&db, &query, case, at)))
                        .collect::<Vec<_>>();
                    (at, rows)
                })
                .collect();
            drop(db);
            let keys = CapsuleKeys::new(
                [0x5a; 32],
                namespace,
                [0x3c; 32],
                CAPSULE_OBJECT_KIND,
                CapsuleProfile::balanced(),
            );
            let coordinator = CommitCoordinator::open(&commit, &dir, keys)
                .await
                .expect("independent replay opens after writer drops");
            let full = replay(&commit, &coordinator)
                .await
                .expect("full replay")
                .database;
            let prefix = replay_through(&commit, &coordinator, before)
                .await
                .expect("prefix replay")
                .database;
            let current = full.graph(GraphId(1), BranchId(1)).expect("current graph");
            let old = prefix.graph(GraphId(1), BranchId(1)).expect("old graph");
            assert!(old.iter_edges().any(|(id, _)| id == EId(12)));
            assert!(!current.iter_edges().any(|(id, _)| id == EId(12)));
            for (at, rows) in &results {
                verify(
                    if *at == Some(before) { old } else { current },
                    rows,
                    seed,
                    *at,
                );
            }
            // The comparison itself must observe history, not merely replay counts.
            assert_ne!(
                results[0].1[2].1, results[1].1[2].1,
                "deleted edge changes walk multiplicity"
            );
            assert_eq!(
                results[0].1[2].1, results[2].1[2].1,
                "live equals explicit after-delete snapshot"
            );
        });
        assert!(report.lab_test_passed(), "seed={seed} report={report:?}");
    }
}
