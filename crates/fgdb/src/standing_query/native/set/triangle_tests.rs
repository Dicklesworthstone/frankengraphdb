use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::zset::triangles::TriangleQuantifier;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const INPUT: &str =
    "{relations:['R'],direction:'undirected',parallel:'collapse',self_loops:'drop'}";

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
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

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for vertex in [0, 1, 2, 3, 4, u128::MAX] {
        batch.create_vertex(VId(vertex), vec![], vec![]);
    }
    for (id, source, target) in [
        (10, 0, 1),
        (11, 1, 2),
        (12, 2, 0),
        (13, 0, 1),
        (14, 1, 0),
        (15, u128::MAX, u128::MAX),
        (16, 3, 4),
    ] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    batch
}

fn call() -> String {
    format!(
        "CALL fnx.triangles({INPUT}) YIELD triangles AS motifs,vertex AS node RETURN motifs,node"
    )
}

fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("native triangle rows");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn eager(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> ZSet<Vec<QueryValue>> {
    result_bag(
        db.query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap_or_else(|error| panic!("{text}: {error:?}")),
    )
}

// Definition-level simple undirected oracle. Enumerate sorted unique triples
// after collapsing physical edge support; initialize every live vertex to zero.
// This shares neither the derivative kernel nor Prism's oriented intersection.
fn oracle(db: &Database<MemVfs>) -> ZSet<Vec<QueryValue>> {
    let live: BTreeSet<_> = db.vertices().unwrap().iter().map(|row| row.vid).collect();
    let mut edges = BTreeSet::new();
    for edge in db.edges().unwrap().iter().filter(|edge| {
        edge.entry.relation == R
            && edge.entry.src != edge.entry.dst
            && live.contains(&edge.entry.src)
            && live.contains(&edge.entry.dst)
    }) {
        edges.insert((
            edge.entry.src.min(edge.entry.dst),
            edge.entry.src.max(edge.entry.dst),
        ));
    }
    let vertices: Vec<_> = live.into_iter().collect();
    let mut counts: BTreeMap<_, _> = vertices.iter().copied().map(|v| (v, 0_i64)).collect();
    for (i, &a) in vertices.iter().enumerate() {
        for (j, &b) in vertices.iter().enumerate().skip(i + 1) {
            for &c in vertices.iter().skip(j + 1) {
                if edges.contains(&(a, b)) && edges.contains(&(a, c)) && edges.contains(&(b, c)) {
                    for vertex in [a, b, c] {
                        *counts.get_mut(&vertex).unwrap() += 1;
                    }
                }
            }
        }
    }
    ZSet::from_updates(
        counts.into_iter().map(|(vertex, triangles)| {
            (
                vec![
                    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(triangles))),
                    QueryValue::Value(GraphValue::Vertex(vertex)),
                ],
                ZWeight::ONE,
            )
        }),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn total_occurrences(rows: &ZSet<Vec<QueryValue>>) -> i64 {
    rows.iter()
        .map(|(row, weight)| {
            assert_eq!(weight, &ZWeight::ONE);
            let QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(count))) = &row[0] else {
                panic!("per-vertex triangle schema");
            };
            *count
        })
        .sum()
}

#[test]
fn triangle_calls_and_downstream_stages_match_independent_triple_oracle() {
    let ((), report) = run_async_under_lab(0x6e74_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(30), VId(1), VId(u128::MAX), vec![]);
        other.add_edge(EId(31), VId(4), VId(u128::MAX), vec![]);
        db.write(&commit, other).await.unwrap();

        let typed = db
            .register_standing_triangles(&cx, R, TriangleQuantifier::All, policy())
            .unwrap();
        assert!(matches!(
            &db.standing_queries[typed.index],
            StandingQuery::Triangles(_)
        ));
        assert!(sets::rows(&db.standing_queries[typed.index]).is_none());
        // The unchanged typed ALL API retains physical multiplicity: three
        // supports for side 0--1 produce three occurrences of triangle 0,1,2.
        assert_eq!(
            db.standing_triangle_total(&cx, &typed).unwrap(),
            &ZWeight::from_i128(3)
        );

        let mut cases = vec![
            call(),
            format!("CALL fnx.triangles({INPUT}) YIELD triangles AS motifs RETURN motifs"),
            format!("CALL fnx.triangles({INPUT}) YIELD triangles AS motifs RETURN DISTINCT motifs"),
            format!("CALL fnx.triangles({INPUT}) YIELD triangles AS motifs RETURN motifs,COUNT(*) AS members GROUP BY motifs ORDER BY motifs LIMIT 8"),
            format!("CALL fnx.triangles({INPUT}) YIELD vertex AS node,triangles AS motifs WHERE motifs > 0 RETURN node,motifs ORDER BY node DESC LIMIT 3"),
            "CALL fnx.triangles({relations:['R'],parallel:'collapse',self_loops:'drop'}) YIELD triangles AS motifs,vertex AS node RETURN motifs,node".into(),
        ];
        let page_first = cases.len();
        let pages = [(0, 1), (1, 2), (1, 0)];
        for (skip, count) in pages {
            cases.push(format!("{} SKIP {skip} LIMIT {count}", call()));
        }
        let params = GqlParameters::new();
        let mut handles = Vec::new();
        for text in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (bound, width) = prepared.subscription_footprint(&cx, &params).unwrap();
            assert_eq!(width, 2, "{text}");
            let before = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &params, policy())
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert!(u64::try_from(db.standing_queries.len() - before).unwrap() <= bound);
            handles.push(handle);
        }
        let total_text = format!(
            "CALL fnx.triangles({INPUT}) YIELD triangles AS motifs RETURN COUNT(*) AS vertices"
        );
        let total_policy = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let total = db
            .register_standing_native(&cx, &total_text, &params, symbols, total_policy)
            .unwrap();
        assert_eq!(
            db.standing_native_columns(&cx, &handles[0]).unwrap(),
            ["motifs", "node"]
        );

        for tick in 0..=14 {
            let expected = oracle(&db);
            for (index, (text, handle)) in cases.iter().zip(&handles).enumerate() {
                let (frontier, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, db.frontier().unwrap());
                assert_eq!(actual, eager(&db, &cx, text), "tick {tick}: {text}");
                if index == 0 || index == 5 {
                    assert_eq!(actual, expected, "triple oracle, tick {tick}: {text}");
                }
                if index >= page_first {
                    let (skip, count) = pages[index - page_first];
                    let selected = ZSet::from_updates(
                        expected
                            .iter()
                            .skip(skip)
                            .take(count)
                            .map(|(row, _)| (row.clone(), ZWeight::ONE)),
                        LIMBS,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .unwrap();
                    assert_eq!(actual, selected, "canonical triangle page, tick {tick}");
                    assert_eq!(
                        db.standing_native_query(&cx, handle, policy()).unwrap().1,
                        db.query(&cx, text, &params, symbols, policy()).unwrap()
                    );
                }
            }
            assert_eq!(
                db.standing_native_bag(&cx, &total, total_policy).unwrap().1,
                eager(&db, &cx, &total_text)
            );
            if tick == 14 {
                break;
            }
            let mut change = WriteBatch::new(if tick == 13 { S } else { R });
            match tick {
                0 => {
                    change.set_vertex_property(
                        VId(0),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Bool(true)),
                    );
                    change.set_edge_property(
                        EId(10),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(9)),
                    );
                    change.set_vertex_label(VId(0), LabelId(1), true);
                }
                1 => {
                    change.delete_edge(EId(13));
                }
                2 => {
                    change.delete_edge(EId(10));
                }
                3 => {
                    change.delete_edge(EId(14));
                }
                4 => {
                    change.add_edge(EId(17), VId(0), VId(1), vec![]);
                    change.add_edge(EId(18), VId(0), VId(3), vec![]);
                    change.add_edge(EId(19), VId(1), VId(3), vec![]);
                }
                5 => {
                    change.delete_edge(EId(17));
                    change.add_edge(EId(20), VId(1), VId(0), vec![]);
                }
                6 => {
                    change.delete_edge(EId(11));
                    change.add_edge(EId(21), VId(2), VId(3), vec![]);
                }
                7 => {
                    change.delete_edge(EId(20));
                    change.delete_vertex(VId(0));
                    change.delete_vertex(VId(1));
                }
                8 => {
                    change.create_vertex(VId(9), vec![], vec![]);
                    change.add_edge(EId(22), VId(9), VId(9), vec![]);
                }
                9 => {
                    change.add_edge(EId(23), VId(2), VId(4), vec![]);
                    change.add_edge(EId(24), VId(2), VId(u128::MAX), vec![]);
                    change.add_edge(EId(25), VId(4), VId(u128::MAX), vec![]);
                }
                10 => {
                    change.delete_edge(EId(15));
                    change.delete_edge(EId(25));
                    change.delete_vertex(VId(u128::MAX));
                }
                11 => {
                    change.delete_edge(EId(23));
                    change.delete_vertex(VId(9));
                }
                12 => {
                    change.create_vertex(VId(1_u128 << 100), vec![], vec![]);
                }
                _ => {
                    change.add_edge(EId(32), VId(2), VId(1_u128 << 100), vec![]);
                }
            }
            db.write(&commit, change).await.unwrap();
            let successor = oracle(&db);
            if tick == 6 {
                assert_eq!(total_occurrences(&expected), 6);
                assert_eq!(total_occurrences(&successor), 6);
                assert_ne!(
                    successor, expected,
                    "a triangle swap changes local counts despite an unchanged global total"
                );
            }
            if [0, 1, 2, 5, 13].contains(&tick) {
                assert_eq!(successor, expected);
                let Layout::Circuit { first, .. } = handles[0].native.as_deref().unwrap() else {
                    panic!("triangle circuit");
                };
                let StandingQuery::TriangleCounts(source) = &db.standing_queries[*first] else {
                    panic!("triangle count source");
                };
                assert!(
                    source.delta().unwrap().is_empty(),
                    "accepted empty tick {tick}"
                );
                assert_eq!(source.frontier, db.frontier().unwrap());
            }
            if tick == 6 || tick == 10 {
                for handle in &handles {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                }
                db.rebuild_standing_query(&cx, &total, total_policy)
                    .unwrap();
                db.rebuild_standing_query(&cx, &typed, policy()).unwrap();
                assert!(matches!(
                    &db.standing_queries[typed.index],
                    StandingQuery::Triangles(_)
                ));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn triangle_subscription_replays_incidence_only_ticks_and_retractions_then_rebaselines() {
    let ((), report) = run_async_under_lab(0x6e74_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let body = format!("CALL fnx.triangles({INPUT}) YIELD triangles AS motifs,vertex AS node");
        let prepared = PreparedNativeRead::prepare_subscription(
            &cx,
            &format!("SUBSCRIBE TO {body}"),
            &params,
            symbols,
        )
        .unwrap();
        let before = db.standing_queries.len();
        let setup = NativeSubscriptionSetup {
            maintenance: policy(),
            delivery: policy(),
            retention: [16, 1000, 100_000],
        };
        let refused: Result<(NativeSubscription, ()), SubscriptionError> = prepared
            .subscribe_replaying(&mut db, &cx, &params, setup, |_, _| {
                Err(SubscriptionError::from(StandingQueryError::Delivery(
                    StandingQueryFailure::ResultBudget,
                )))
            });
        assert!(refused.is_err());
        assert_eq!(db.standing_queries.len(), before);
        let (mut subscriber, baseline) = prepared
            .subscribe_replaying(&mut db, &cx, &params, setup, |columns, batch| {
                assert_eq!(columns, ["motifs", "node"]);
                Ok::<_, SubscriptionError>(Arc::clone(batch))
            })
            .unwrap();
        assert!(baseline.is_snapshot());
        assert_eq!(baseline.frontier(), start);
        let mut delivered = baseline
            .rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(delivered, oracle(&db));
        subscriber.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for tick in 0..5 {
            let mut change = WriteBatch::new(R);
            match tick {
                0 => {
                    change.delete_edge(EId(13));
                }
                1 => {
                    change.delete_edge(EId(10));
                }
                2 => {
                    change.delete_edge(EId(14));
                }
                3 => {
                    change.set_vertex_label(VId(0), LabelId(1), true);
                }
                _ => {
                    change.delete_vertex(VId(u128::MAX));
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            expected.push((at, oracle(&db)));
        }
        let mut after = start;
        for (tick, (at, expected)) in expected.iter().enumerate() {
            let frame = subscriber.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!((frame.from(), frame.frontier()), (Some(after), *at));
            if tick == 2 || tick == 4 {
                assert!(!frame.rows().is_empty());
                assert!(
                    frame
                        .rows()
                        .iter()
                        .any(|(_, weight)| weight < &ZWeight::ZERO)
                );
            } else {
                assert!(frame.rows().is_empty());
            }
            delivered
                .integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(&delivered, expected);
            subscriber.acknowledge(frame.receipt()).unwrap();
            after = *at;
        }
        assert!(subscriber.poll(&db, &cx, policy()).unwrap().is_none());
        db.rebuild_standing_query(&cx, subscriber.handle(), policy())
            .unwrap();
        subscriber.restart_from_current().unwrap();
        let fresh = subscriber.poll(&db, &cx, policy()).unwrap().unwrap();
        assert!(fresh.is_snapshot());
        assert_eq!(fresh.rows(), &oracle(&db));
        subscriber.acknowledge(fresh.receipt()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dense_triangles_and_vertex_replacements_use_the_final_vertex_result_bound() {
    let ((), report) = run_async_under_lab(0x6e74_6303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut clique = WriteBatch::new(R);
        for vertex in 1..=6 {
            clique.create_vertex(VId(vertex), vec![], vec![]);
        }
        let mut id = 100;
        for a in 1..=6 {
            for b in (a + 1)..=6 {
                clique.add_edge(EId(id), VId(a), VId(b), vec![]);
                id += 1;
            }
        }
        db.write(&commit, clique).await.unwrap();
        let params = GqlParameters::new();
        let six = GqlQueryPolicy::new(100_000, 6, 10_000_000, 10_000_000);
        let handle = db
            .register_standing_native(&cx, &call(), &params, symbols, six)
            .unwrap();
        let expected = oracle(&db);
        assert_eq!(expected.len(), 6);
        assert_eq!(total_occurrences(&expected), 60); // K6 has 20 triangles, ten at each vertex.
        assert_eq!(
            db.standing_native_bag(&cx, &handle, six).unwrap().1,
            expected
        );
        let before = db.standing_queries.len();
        let five = GqlQueryPolicy::new(100_000, 5, 10_000_000, 10_000_000);
        assert!(
            db.register_standing_native(&cx, &call(), &params, symbols, five)
                .is_err()
        );
        assert_eq!(db.standing_queries.len(), before);

        // New vertex 0 sorts before retired vertex 6: even the row/key prefix
        // temporarily containing seven vertices must not defeat a final cap of six.
        let mut replace = WriteBatch::new(R);
        replace.create_vertex(VId(0), vec![], vec![]);
        replace.delete_vertex(VId(6));
        db.write(&commit, replace).await.unwrap();
        let expected = oracle(&db);
        assert_eq!(expected.len(), 6);
        assert_eq!(total_occurrences(&expected), 30);
        assert_eq!(
            db.standing_native_bag(&cx, &handle, six).unwrap().1,
            expected
        );
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("triangle circuit");
        };
        let first = *first;
        let StandingQuery::TriangleCounts(source) = &db.standing_queries[first] else {
            panic!("triangle count source");
        };
        let accepted = source
            .rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let frontier = source.frontier;

        let mut too_many = WriteBatch::new(R);
        too_many.create_vertex(VId(7), vec![], vec![]);
        let committed = db.write(&commit, too_many).await.unwrap();
        assert!(committed > frontier);
        assert!(db.standing_native_bag(&cx, &handle, policy()).is_err());
        let StandingQuery::TriangleCounts(source) = &db.standing_queries[first] else {
            panic!("triangle count source");
        };
        assert_eq!(source.rows(), &accepted);
        assert_eq!(source.frontier, frontier);
        assert_eq!(source.failure, Some(StandingQueryFailure::ResultBudget));
        let seven = GqlQueryPolicy::new(100_000, 7, 10_000_000, 10_000_000);
        db.rebuild_standing_query(&cx, &handle, seven).unwrap();
        assert_eq!(
            db.standing_native_bag(&cx, &handle, seven).unwrap(),
            (committed, oracle(&db))
        );
        let StandingQuery::TriangleCounts(source) = &db.standing_queries[first] else {
            panic!("rebuilt triangle source");
        };
        assert!(source.delta().is_none());
        assert!(source.failure.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn triangle_projection_refusals_and_source_quotas_never_publish_partial_circuits() {
    let ((), report) = run_async_under_lab(0x6e74_6304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let prior = db
            .register_standing_native(&cx, &call(), &params, symbols, policy())
            .unwrap();
        let before = db.standing_queries.len();
        let answer = db.standing_native_bag(&cx, &prior, policy()).unwrap();
        let bodies = [
            "CALL fnx.triangles()".to_string(),
            "CALL fnx.triangles({relations:['R'],self_loops:'drop'})".into(),
            "CALL fnx.triangles({relations:['R'],parallel:'collapse'})".into(),
            "CALL fnx.triangles({relations:['R'],parallel:'collapse',self_loops:'keep'})".into(),
            "CALL fnx.triangles({relations:['R'],parallel:'collapse',self_loops:'reject'})".into(),
            "CALL fnx.triangles({relations:['R'],labels:['L'],parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.triangles({relations:['R'],weight:'p',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.triangles({relations:['R','S'],parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.triangles({relations:['R'],direction:'directed',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.triangles({relations:['R'],direction:'reversed',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.triangles(relation => 'R',parallel => 'collapse',self_loops => 'drop')".into(),
            format!("CALL fnx.triangles({INPUT},7)"),
        ];
        for body in bodies {
            for page in ["", " ORDER BY vertex LIMIT 0", " SKIP 1 LIMIT 0"] {
                let text = format!("{body} YIELD vertex RETURN vertex{page}");
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
            GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
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
            db.standing_native_bag(&cx, &prior, policy()).unwrap(),
            answer
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn addresses(db: &Database<MemVfs>, first: usize, root: usize) -> Vec<usize> {
    db.standing_queries[first..=root]
        .iter()
        .map(|query| match query {
            StandingQuery::TriangleCounts(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Projection(query) => query.as_ref() as *const _ as usize,
            _ => panic!("triangle source and projections only"),
        })
        .collect()
}

#[test]
fn every_triangle_registration_and_rebuild_boundary_preserves_the_accepted_circuit() {
    let ((), report) = run_async_under_lab(0x6e74_6305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let query = PreparedGraphSetText::prepare(&call(), symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let handle = register(&mut db, &cx, &query, policy()).unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("native triangle circuit");
        };
        let (first, root) = (*first, handle.index);
        let before = db.standing_queries.len();
        let answer = db.standing_native_bag(&cx, &handle, policy()).unwrap();
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile_root(&cx, &query, policy(), &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
        }
        assert!(calls > 0);
        for stop in 1..=calls {
            let mut seen = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(
                    staged
                        .compile_root(&cx, &query, policy(), &mut || {
                            seen += 1;
                            if seen == stop {
                                Err(StandingQueryError::Maintenance(
                                    StandingQueryFailure::Interrupted,
                                ))
                            } else {
                                Ok(())
                            }
                        })
                        .is_err()
                );
            }
            assert_eq!(seen, stop);
            assert_eq!(db.standing_queries.len(), before);
            assert_eq!(
                db.standing_native_bag(&cx, &handle, policy()).unwrap(),
                answer
            );
        }
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, root, policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        for stop in 1..=calls {
            let identity = addresses(&db, first, root);
            let mut seen = 0;
            assert!(
                rebuild_checked(&mut db, &cx, first, root, policy(), &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryError::Maintenance(
                            StandingQueryFailure::Interrupted,
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(addresses(&db, first, root), identity);
            assert_eq!(db.standing_queries.len(), before);
            assert_eq!(
                db.standing_native_bag(&cx, &handle, policy()).unwrap(),
                answer
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
