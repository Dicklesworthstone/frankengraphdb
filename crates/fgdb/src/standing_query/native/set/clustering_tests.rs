use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const INPUT: &str =
    "{relations:['R'],direction:'undirected',parallel:'collapse',self_loops:'keep'}";

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}

fn call() -> String {
    format!(
        "CALL fnx.clustering_coefficient({INPUT}) YIELD score AS local,vertex AS node RETURN local,node"
    )
}

fn bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("native clustering rows");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn eager(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> ZSet<Vec<QueryValue>> {
    bag(db
        .query(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap())
}

// Definition-level oracle: collapse selected undirected supports and directly
// count edges between every unordered pair of a vertex's distinct neighbors.
// This shares neither the changed-side derivative nor Prism's CSR kernel.
pub(super) fn oracle(db: &Database<MemVfs>, relation: Option<RelationId>) -> ZSet<Vec<QueryValue>> {
    let mut neighbors: BTreeMap<_, BTreeSet<_>> = db
        .vertices()
        .unwrap()
        .iter()
        .map(|row| (row.vid, BTreeSet::new()))
        .collect();
    for edge in db.edges().unwrap().iter().filter(|edge| {
        relation.is_none_or(|selected| edge.entry.relation == selected)
            && edge.entry.src != edge.entry.dst
    }) {
        neighbors
            .get_mut(&edge.entry.src)
            .unwrap()
            .insert(edge.entry.dst);
        neighbors
            .get_mut(&edge.entry.dst)
            .unwrap()
            .insert(edge.entry.src);
    }
    let rows = neighbors.iter().map(|(&vertex, adjacent)| {
        let mut triangles = 0_u128;
        for &a in adjacent {
            for &b in adjacent.range((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded)) {
                triangles += u128::from(neighbors[&a].contains(&b));
            }
        }
        let degree = adjacent.len() as u128;
        let score = if degree < 2 {
            0.0
        } else {
            (2 * triangles) as f64 / (degree * (degree - 1)) as f64
        };
        (
            vec![
                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(score),
                ))),
                QueryValue::Value(GraphValue::Vertex(vertex)),
            ],
            ZWeight::ONE,
        )
    });
    ZSet::from_updates(rows, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}

fn score(rows: &ZSet<Vec<QueryValue>>, vertex: VId) -> u64 {
    rows.iter()
        .find_map(|(row, weight)| {
            assert_eq!(weight, &ZWeight::ONE);
            match row.as_slice() {
                [
                    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Float(value))),
                    QueryValue::Value(GraphValue::Vertex(id)),
                ] if *id == vertex => Some(value.to_bits()),
                _ => None,
            }
        })
        .unwrap()
}

#[test]
fn clustering_calls_preserve_score_bits_for_degree_only_support_and_lifetime_changes() {
    let ((), report) = run_async_under_lab(0x6e63_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in [0, 1, 2, 3, 4, u128::MAX] {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        for (id, a, b) in [
            (10, 0, 1),
            (11, 1, 2),
            (12, 2, 0),
            (13, 0, 1),
            (14, 1, 0),
            (15, u128::MAX, u128::MAX),
            (16, 3, 4),
        ] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let params = GqlParameters::new();
        let cases = [
            call(),
            call().replace("'keep'", "'drop'"),
            format!("CALL fnx.clustering_coefficient({INPUT}) YIELD score AS local RETURN local"),
            format!("CALL fnx.clustering_coefficient({INPUT}) YIELD score AS local RETURN DISTINCT local"),
            format!("CALL fnx.clustering_coefficient({INPUT}) YIELD score AS local RETURN local,COUNT(*) AS members GROUP BY local ORDER BY local"),
            format!("CALL fnx.clustering_coefficient({INPUT}) YIELD vertex AS node,score AS local WHERE local > 0.0 RETURN node,local ORDER BY node DESC LIMIT 3"),
            format!("{} SKIP 1 LIMIT 3", call()),
            "CALL fnx.clustering_coefficient({relations:['R'],parallel:'collapse'}) YIELD score AS local,vertex AS node RETURN local,node".into(),
        ];
        let mut handles = Vec::new();
        for text in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (bound, width) = prepared.subscription_footprint(&cx, &params).unwrap();
            assert_eq!(width, 2);
            let before = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &params, policy())
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert!(u64::try_from(db.standing_queries.len() - before).unwrap() <= bound);
            handles.push(handle);
        }
        assert_eq!(
            db.standing_native_columns(&cx, &handles[0]).unwrap(),
            ["local", "node"]
        );
        for tick in 0..=12 {
            let expected = oracle(&db, Some(R));
            for (index, (text, handle)) in cases.iter().zip(&handles).enumerate() {
                let (at, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(at, db.frontier().unwrap());
                assert_eq!(
                    actual,
                    eager(&db, &cx, text),
                    "Prism score bits, tick {tick}: {text}"
                );
                if [0, 1, 7].contains(&index) {
                    assert_eq!(
                        actual, expected,
                        "independent neighborhood oracle, tick {tick}"
                    );
                }
                if index == 6 {
                    let page = ZSet::from_updates(
                        expected
                            .iter()
                            .skip(1)
                            .take(3)
                            .map(|(row, _)| (row.clone(), ZWeight::ONE)),
                        LIMBS,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .unwrap();
                    assert_eq!(actual, page, "canonical score-first CALL page, tick {tick}");
                    assert_eq!(
                        db.standing_native_query(&cx, handle, policy()).unwrap().1,
                        db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    );
                }
            }
            if tick == 12 {
                break;
            }
            let mut change = WriteBatch::new(if tick == 10 { S } else { R });
            match tick {
                0 => {
                    change.add_edge(EId(20), VId(0), VId(3), vec![]);
                }
                1 => {
                    change.add_edge(EId(21), VId(3), VId(0), vec![]);
                }
                2 => {
                    change.delete_edge(EId(20));
                }
                3 => {
                    change.delete_edge(EId(21));
                }
                4 => {
                    change.add_edge(EId(22), VId(0), VId(3), vec![]);
                    change.add_edge(EId(23), VId(1), VId(3), vec![]);
                }
                5 => {
                    change.delete_edge(EId(11));
                }
                6 => {
                    change.delete_vertex(VId(1));
                    change.create_vertex(VId(9), vec![], vec![]);
                    change.add_edge(EId(24), VId(9), VId(9), vec![]);
                }
                7 => {
                    change.add_edge(EId(25), VId(2), VId(3), vec![]);
                }
                8 => {
                    change.delete_edge(EId(15));
                    change.delete_vertex(VId(u128::MAX));
                }
                9 => {
                    change.create_vertex(VId(1_u128 << 100), vec![], vec![]);
                }
                10 => {
                    change.add_edge(EId(30), VId(2), VId(1_u128 << 100), vec![]);
                }
                _ => {
                    change.set_vertex_property(
                        VId(0),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(7)),
                    );
                    change.set_vertex_label(VId(0), LabelId(1), true);
                }
            }
            db.write(&commit, change).await.unwrap();
            let next = oracle(&db, Some(R));
            if tick == 0 {
                assert_eq!(score(&expected, VId(0)), 1.0_f64.to_bits());
                assert_eq!(score(&next, VId(0)), (1.0_f64 / 3.0).to_bits());
                let Layout::Circuit { first, .. } = handles[0].native.as_deref().unwrap() else {
                    panic!("clustering circuit");
                };
                let StandingQuery::TriangleCounts(source) = &db.standing_queries[*first] else {
                    panic!("shared triangle derivative owner");
                };
                assert_eq!(
                    source.delta().unwrap().len(),
                    2,
                    "one exact score replacement from a degree-only change"
                );
            }
            if [1, 2, 10, 11].contains(&tick) {
                assert_eq!(
                    next, expected,
                    "multiplicity, unselected, or property-only tick {tick}"
                );
            }
            if tick == 4 || tick == 8 {
                for handle in &handles {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_four_vertex_simple_graph_matches_real_prism_and_independent_scores() {
    let ((), report) = run_async_under_lab(0x6e63_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let ids = [0, 1, 1_u128 << 100, u128::MAX];
        let mut seed = WriteBatch::new(R);
        for id in ids {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let handle = db
            .register_standing_native(&cx, &call(), &GqlParameters::new(), symbols, policy())
            .unwrap();
        let pairs = [(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)];
        let mut live = [None; 6];
        let mut previous = 0_u32;
        for tick in 0_u32..64 {
            let mask = tick ^ (tick >> 1);
            if tick != 0 {
                let changed = (mask ^ previous).trailing_zeros() as usize;
                let mut change = WriteBatch::new(R);
                if mask & (1 << changed) == 0 {
                    change.delete_edge(live[changed].take().unwrap());
                } else {
                    let edge = EId(100 + u128::from(tick));
                    let (a, b) = pairs[changed];
                    change.add_edge(edge, VId(ids[a]), VId(ids[b]), vec![]);
                    live[changed] = Some(edge);
                }
                db.write(&commit, change).await.unwrap();
            }
            let actual = db.standing_native_bag(&cx, &handle, policy()).unwrap().1;
            assert_eq!(actual, oracle(&db, Some(R)), "simple graph {mask:#08b}");
            assert_eq!(
                actual,
                eager(&db, &cx, &call()),
                "Prism bits, graph {mask:#08b}"
            );
            previous = mask;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn clustering_projection_refusals_and_final_row_budgets_preserve_the_circuit() {
    let ((), report) = run_async_under_lab(0x6e63_6303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..3 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        seed.add_edge(EId(1), VId(0), VId(1), vec![]);
        seed.add_edge(EId(2), VId(1), VId(2), vec![]);
        seed.add_edge(EId(3), VId(2), VId(0), vec![]);
        db.write(&commit, seed).await.unwrap();
        let params = GqlParameters::new();
        let three = GqlQueryPolicy::new(100_000, 3, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_native(&cx, &call(), &params, symbols, three)
            .unwrap();
        let before = db.standing_queries.len();
        let accepted = db.standing_native_bag(&cx, &handle, three).unwrap();
        for input in [
            "{}",
            "{relations:['R'],self_loops:'drop'}",
            "{relations:['R'],parallel:'collapse',self_loops:'reject'}",
            "{relations:['R'],labels:['L'],parallel:'collapse',self_loops:'drop'}",
            "{relations:['R'],weight:'p',parallel:'collapse',self_loops:'drop'}",
            "{relations:['R','S'],parallel:'collapse',self_loops:'drop'}",
            "{relations:['R'],direction:'directed',parallel:'collapse',self_loops:'drop'}",
            "{relations:['R'],direction:'reversed',parallel:'collapse',self_loops:'drop'}",
        ] {
            for page in ["", " ORDER BY vertex LIMIT 0", " SKIP 1 LIMIT 0"] {
                let text = format!(
                    "CALL fnx.clustering_coefficient({input}) YIELD vertex RETURN vertex{page}"
                );
                if let Ok(prepared) = PreparedNativeRead::prepare(&text, &params, symbols) {
                    assert!(
                        prepared.subscription_footprint(&cx, &params).is_err(),
                        "{text}"
                    );
                }
                assert!(
                    db.register_standing_native(&cx, &text, &params, symbols, policy())
                        .is_err(),
                    "{text}"
                );
                assert_eq!(db.standing_queries.len(), before);
            }
        }
        for tight in [
            GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000),
            GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 0, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 0),
        ] {
            assert!(
                db.register_standing_native(&cx, &call(), &params, symbols, tight)
                    .is_err()
            );
            assert_eq!(db.standing_queries.len(), before);
        }
        assert_eq!(
            db.standing_native_bag(&cx, &handle, three).unwrap(),
            accepted
        );
        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(3), vec![], vec![]);
        change.add_edge(EId(4), VId(0), VId(3), vec![]);
        let at = db.write(&commit, change).await.unwrap();
        assert!(db.standing_native_bag(&cx, &handle, policy()).is_err());
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("clustering circuit");
        };
        let StandingQuery::TriangleCounts(source) = &db.standing_queries[*first] else {
            panic!("clustering owner");
        };
        assert_eq!(source.frontier, accepted.0);
        assert_eq!(source.failure, Some(StandingQueryFailure::ResultBudget));
        let four = GqlQueryPolicy::new(100_000, 4, 10_000_000, 10_000_000);
        db.rebuild_standing_query(&cx, &handle, four).unwrap();
        assert_eq!(
            db.standing_native_bag(&cx, &handle, four).unwrap(),
            (at, oracle(&db, Some(R)))
        );
        let StandingQuery::TriangleCounts(source) = &db.standing_queries[*first] else {
            panic!("rebuilt clustering owner");
        };
        assert_eq!(source.output(), triangle_counts::Output::Clustering);
        assert!(source.delta().is_none() && source.failure.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
