//! Native set syntax compiles to ordinary maintained row/set nodes, not reruns.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
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

fn selected_analytic_text(metric: &str, relation: Option<RelationId>) -> String {
    let relation = relation.map_or("", |_| "relations:['R'],");
    let field = match metric {
        "triangles" => "triangles",
        "clustering_coefficient" => "score",
        _ => unreachable!(),
    };
    format!(
        "CALL fnx.{metric}({{labels:['L'],{relation}direction:'undirected',\
         parallel:'collapse',self_loops:'drop'}}) YIELD vertex AS node,{field} AS value \
         RETURN node,value"
    )
}

// Enumerate unordered triples in the induced simple graph. The oracle uses
// neither Prism's intersection kernel nor the maintained triangle derivative.
fn selected_analytic_oracle(
    db: &Database<MemVfs>,
    metric: &str,
    relation: Option<RelationId>,
) -> QueryResult {
    let vertices: BTreeSet<_> = db
        .vertices()
        .unwrap()
        .iter()
        .filter(|vertex| vertex.labels.contains(&LabelId(1)))
        .map(|vertex| vertex.vid)
        .collect();
    let edges: BTreeSet<_> = db
        .edges()
        .unwrap()
        .iter()
        .filter(|edge| {
            let edge = &edge.entry;
            relation.is_none_or(|relation| edge.relation == relation)
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
    let adjacent = |a: VId, b: VId| edges.contains(&(a.min(b), a.max(b)));
    let mut counts: BTreeMap<_, i64> = vertices.iter().map(|&vertex| (vertex, 0)).collect();
    for &a in &vertices {
        for &b in vertices.range((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded)) {
            for &c in vertices.range((std::ops::Bound::Excluded(b), std::ops::Bound::Unbounded)) {
                if adjacent(a, b) && adjacent(a, c) && adjacent(b, c) {
                    for vertex in [a, b, c] {
                        *counts.get_mut(&vertex).unwrap() += 1;
                    }
                }
            }
        }
    }
    QueryResult::Rows {
        columns: vec!["node".into(), "value".into()],
        rows: counts
            .into_iter()
            .map(|(vertex, count)| {
                let value = match metric {
                    "triangles" => CanonicalScalar::Int(count),
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
        for metric in ["triangles", "clustering_coefficient"] {
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
        for batch in [
            seed, select, duplicates, last_edge, other, leave, rejoin, unrelated,
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
        let text = selected_analytic_text("triangles", Some(RelationId(1)));
        let limited = db
            .register_standing_native(&cx, &text, &params, symbols, small)
            .unwrap();
        let healthy = db
            .register_standing_native(&cx, &text, &params, symbols, policy())
            .unwrap();
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
        assert_eq!(
            db.standing_native_query(&cx, &limited, policy()).unwrap(),
            db.standing_native_query(&cx, &healthy, policy()).unwrap()
        );
        let mut grow = WriteBatch::new(RelationId(1));
        grow.set_vertex_label(VId(3), LabelId(1), true);
        let committed = db.write(&commit, grow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), committed);
        assert!(matches!(
            db.standing_native_query(&cx, &limited, policy()),
            Err(StandingQueryError::Unavailable { .. })
        ));
        assert!(db.rebuild_standing_query(&cx, &limited, small).is_err());
        let expected = selected_analytic_oracle(&db, "triangles", Some(RelationId(1)));
        assert_eq!(
            db.standing_native_query(&cx, &healthy, policy()).unwrap().1,
            expected
        );
        db.rebuild_standing_query(&cx, &limited, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &limited, policy()).unwrap(),
            (committed, expected)
        );
        let mut shrink = WriteBatch::new(RelationId(1));
        shrink.set_vertex_label(VId(1), LabelId(1), false);
        db.write(&commit, shrink).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &limited, policy()).unwrap(),
            db.standing_native_query(&cx, &healthy, policy()).unwrap()
        );
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
