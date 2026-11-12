//! Native set syntax compiles to ordinary maintained row/set nodes, not reruns.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
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
