use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
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
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
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
        (11, 0, 2),
        (12, 0, 3),
        (13, 1, 2),
        (14, 1, 3),
        (15, 2, 3),
        (16, 0, 1),
        (17, 1, 0),
        (18, 3, 4),
        (19, u128::MAX, u128::MAX),
    ] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    batch
}

fn call() -> String {
    format!("CALL fnx.core_number({INPUT}) YIELD core AS shell,vertex AS node RETURN shell,node")
}

fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("native core rows");
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

// Definition-level oracle: enumerate every induced vertex subset, compute its
// minimum degree, and retain each vertex's greatest such minimum. This shares
// neither the maintained affected-component peeling nor Prism's bucket kernel.
pub(super) fn oracle(db: &Database<MemVfs>, relation: Option<RelationId>) -> ZSet<Vec<QueryValue>> {
    let vertices: Vec<_> = db.vertices().unwrap().iter().map(|row| row.vid).collect();
    assert!(
        vertices.len() <= 12,
        "exhaustive oracle fixture must remain small"
    );
    let live: BTreeSet<_> = vertices.iter().copied().collect();
    let mut adjacent = BTreeSet::new();
    for edge in db.edges().unwrap().iter().filter(|edge| {
        relation.is_none_or(|selected| edge.entry.relation == selected)
            && edge.entry.src != edge.entry.dst
            && live.contains(&edge.entry.src)
            && live.contains(&edge.entry.dst)
    }) {
        adjacent.insert((edge.entry.src, edge.entry.dst));
        adjacent.insert((edge.entry.dst, edge.entry.src));
    }
    let mut cores: BTreeMap<_, _> = vertices.iter().copied().map(|v| (v, 0_i64)).collect();
    for subset in 1_u64..(1_u64 << vertices.len()) {
        let selected: Vec<_> = vertices
            .iter()
            .enumerate()
            .filter(|(at, _)| subset & (1_u64 << *at) != 0)
            .map(|(_, &vertex)| vertex)
            .collect();
        let minimum = selected
            .iter()
            .map(|&vertex| {
                selected
                    .iter()
                    .filter(|&&other| adjacent.contains(&(vertex, other)))
                    .count()
            })
            .min()
            .unwrap() as i64;
        for vertex in selected {
            let core = cores.get_mut(&vertex).unwrap();
            *core = (*core).max(minimum);
        }
    }
    ZSet::from_updates(
        cores.into_iter().map(|(vertex, core)| {
            (
                vec![
                    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(core))),
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

/// The oracle with its two columns swapped: a body completed by an implicit
/// `RETURN *` orders them by name, node before shell (fgdb-7jja3).
fn node_first(set: &ZSet<Vec<QueryValue>>) -> ZSet<Vec<QueryValue>> {
    ZSet::from_updates(
        set.iter().map(|(row, weight)| {
            (
                vec![row[1].clone(), row[0].clone()],
                weight.checked_clone(LIMBS).unwrap(),
            )
        }),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn core_calls_and_downstream_stages_match_independent_induced_subgraph_oracle() {
    let ((), report) = run_async_under_lab(0x6e6b_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(30), VId(4), VId(u128::MAX), vec![]);
        db.write(&commit, other).await.unwrap();

        let typed = db.register_standing_core_numbers(&cx, R, policy()).unwrap();
        let StandingQuery::CoreNumbers(typed_state) = &db.standing_queries[typed.index] else {
            panic!("typed core source");
        };
        assert!(!typed_state.has_value_rows());
        assert!(sets::rows(&db.standing_queries[typed.index]).is_none());

        let mut cases = vec![
            call(),
            format!("CALL fnx.core_number({INPUT}) YIELD core AS shell RETURN shell"),
            format!("CALL fnx.core_number({INPUT}) YIELD core AS shell RETURN DISTINCT shell"),
            format!(
                "CALL fnx.core_number({INPUT}) YIELD core AS shell \
                 RETURN shell,COUNT(*) AS members GROUP BY shell ORDER BY shell LIMIT 8"
            ),
            format!(
                "CALL fnx.core_number({INPUT}) YIELD vertex AS node,core AS shell \
                 WHERE shell >= 2 RETURN node,shell ORDER BY node DESC LIMIT 3"
            ),
            "CALL fnx.core_number({relations:['R'],parallel:'collapse',self_loops:'drop'}) \
             YIELD core AS shell,vertex AS node RETURN shell,node"
                .into(),
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
            "CALL fnx.core_number({INPUT}) YIELD core AS shell RETURN COUNT(*) AS vertices"
        );
        let total_policy = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
        let total = db
            .register_standing_native(&cx, &total_text, &params, symbols, total_policy)
            .unwrap();
        assert_eq!(
            db.standing_native_columns(&cx, &handles[0]).unwrap(),
            ["shell", "node"]
        );

        for tick in 0..=12 {
            let expected = oracle(&db, Some(R));
            for (index, (text, handle)) in cases.iter().zip(&handles).enumerate() {
                let (frontier, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, db.frontier().unwrap());
                assert_eq!(actual, eager(&db, &cx, text), "tick {tick}: {text}");
                if index == 0 || index == 5 {
                    assert_eq!(
                        actual, expected,
                        "induced-subgraph oracle, tick {tick}: {text}"
                    );
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
                    assert_eq!(actual, selected, "canonical core page, tick {tick}");
                    assert_eq!(
                        db.standing_native_query(&cx, handle, policy()).unwrap().1,
                        db.query(&cx, text, &params, symbols, policy()).unwrap(),
                    );
                }
            }
            assert_eq!(
                db.standing_native_bag(&cx, &total, total_policy).unwrap().1,
                eager(&db, &cx, &total_text)
            );
            let typed_rows = db.standing_core_numbers(&cx, &typed).unwrap();
            for row in expected.iter().map(|(row, _)| row) {
                let [
                    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(core))),
                    QueryValue::Value(GraphValue::Vertex(vertex)),
                ] = row.as_slice()
                else {
                    panic!("oracle schema");
                };
                assert_eq!(
                    typed_rows.rows().weight(&(*vertex, *core as u64)),
                    Some(&ZWeight::ONE)
                );
            }
            if tick == 12 {
                break;
            }
            let mut change = WriteBatch::new(if tick == 11 { S } else { R });
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
                }
                1 => {
                    change.delete_edge(EId(16));
                }
                2 => {
                    change.delete_edge(EId(17));
                }
                3 => {
                    change.delete_edge(EId(10));
                }
                4 => {
                    change.add_edge(EId(20), VId(0), VId(1), vec![]);
                }
                5 => {
                    change.delete_edge(EId(13));
                    change.delete_edge(EId(14));
                }
                6 => {
                    change.add_edge(EId(21), VId(4), VId(0), vec![]);
                }
                7 => {
                    change.delete_vertex(VId(2));
                }
                8 => {
                    change.create_vertex(VId(9), vec![], vec![]);
                }
                9 => {
                    change.add_edge(EId(22), VId(u128::MAX), VId(0), vec![]);
                    change.add_edge(EId(23), VId(u128::MAX), VId(3), vec![]);
                }
                10 => {
                    change.add_edge(EId(24), VId(4), VId(4), vec![]);
                }
                _ => {
                    change.add_edge(EId(31), VId(9), VId(u128::MAX), vec![]);
                }
            }
            db.write(&commit, change).await.unwrap();
            if tick == 3 || tick == 7 {
                for handle in &handles {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                }
                db.rebuild_standing_query(&cx, &total, total_policy)
                    .unwrap();
                db.rebuild_standing_query(&cx, &typed, policy()).unwrap();
                let StandingQuery::CoreNumbers(state) = &db.standing_queries[typed.index] else {
                    panic!("typed core after rebuild");
                };
                assert!(!state.has_value_rows());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn core_subscription_replays_empty_ticks_and_shell_retractions_then_rebaselines() {
    let ((), report) = run_async_under_lab(0x6e6b_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let body = format!("CALL fnx.core_number({INPUT}) YIELD core AS shell,vertex AS node");
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
                assert_eq!(columns, ["node", "shell"]);
                Ok::<_, SubscriptionError>(Arc::clone(batch))
            })
            .unwrap();
        assert!(baseline.is_snapshot());
        assert_eq!(baseline.frontier(), start);
        let mut delivered = baseline
            .rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(delivered, node_first(&oracle(&db, Some(R))));
        subscriber.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for tick in 0..4 {
            let mut change = WriteBatch::new(R);
            match tick {
                0 => {
                    change.delete_edge(EId(16));
                }
                1 => {
                    change.delete_edge(EId(17));
                }
                2 => {
                    change.delete_edge(EId(10));
                }
                _ => {
                    change.set_vertex_property(
                        VId(0),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Bool(false)),
                    );
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            expected.push((at, node_first(&oracle(&db, Some(R)))));
        }
        let mut after = start;
        for (tick, (at, expected)) in expected.iter().enumerate() {
            let frame = subscriber.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!((frame.from(), frame.frontier()), (Some(after), *at));
            if tick == 2 {
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
        assert_eq!(fresh.rows(), &node_first(&oracle(&db, Some(R))));
        subscriber.acknowledge(fresh.receipt()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn core_projection_refusals_and_quotas_never_publish_partial_circuits() {
    let ((), report) = run_async_under_lab(0x6e6b_6103, |root| async move {
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
            "CALL fnx.core_number()".to_string(),
            "CALL fnx.core_number({relations:['R'],self_loops:'drop'})".into(),
            "CALL fnx.core_number({relations:['R'],parallel:'collapse'})".into(),
            "CALL fnx.core_number({relations:['R'],parallel:'collapse',self_loops:'keep'})".into(),
            "CALL fnx.core_number({relations:['R'],parallel:'collapse',self_loops:'reject'})".into(),
            "CALL fnx.core_number({relations:['R'],labels:['L'],parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.core_number({relations:['R'],weight:'p',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.core_number({relations:['R','S'],parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.core_number({relations:['R'],direction:'directed',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.core_number({relations:['R'],direction:'reversed',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.core_number(relation => 'R',parallel => 'collapse',self_loops => 'drop')".into(),
            format!("CALL fnx.core_number({INPUT},7)"),
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
            StandingQuery::CoreNumbers(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Projection(query) => query.as_ref() as *const _ as usize,
            _ => panic!("core source and projections only"),
        })
        .collect()
}

#[test]
fn every_core_registration_and_rebuild_boundary_preserves_the_accepted_circuit() {
    let ((), report) = run_async_under_lab(0x6e6b_6104, |root| async move {
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
            panic!("native core circuit");
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
