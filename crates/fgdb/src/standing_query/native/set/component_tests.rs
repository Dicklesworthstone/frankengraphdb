use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const INPUT: &str = "{relations:['R'],parallel:'collapse',self_loops:'drop'}";

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x79; 32],
        DatabaseSecurityNamespaceId([0x7a; 32]),
        [0x7b; 32],
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
    for vertex in [0, 1, 2, 3, u128::MAX] {
        batch.create_vertex(VId(vertex), vec![], vec![]);
    }
    for (id, source, target) in [
        (10, 0, 1),
        (11, 0, 1),
        (12, 1, 2),
        (13, 2, 0),
        (14, 2, 3),
        (15, u128::MAX, u128::MAX),
    ] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    batch
}
fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("native read rows");
    };
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn copied(rows: &ZSet<Vec<QueryValue>>) -> ZSet<Vec<QueryValue>> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn eager(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> ZSet<Vec<QueryValue>> {
    result_bag(
        db.query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap_or_else(|error| panic!("{text}: {error:?}")),
    )
}
fn call(name: &str) -> String {
    format!(
        "CALL fnx.{name}({INPUT}) YIELD component AS group_id,vertex AS node \
         RETURN group_id,node"
    )
}

// Independent full-source oracle: BFS from every vertex, then mutual
// reachability for SCC. It shares neither maintained kernel nor Prism CSR.
fn oracle(db: &Database<MemVfs>, strong: bool) -> ZSet<Vec<QueryValue>> {
    let vertices: BTreeSet<_> = db.vertices().unwrap().iter().map(|row| row.vid).collect();
    let edges = db.edges().unwrap();
    let mut closure = BTreeMap::new();
    for &vertex in &vertices {
        let mut reached = BTreeSet::from([vertex]);
        let mut todo = VecDeque::from([vertex]);
        while let Some(from) = todo.pop_front() {
            for edge in edges.iter().filter(|edge| {
                edge.entry.relation == R
                    && vertices.contains(&edge.entry.src)
                    && vertices.contains(&edge.entry.dst)
            }) {
                let target = if edge.entry.src == from {
                    Some(edge.entry.dst)
                } else if !strong && edge.entry.dst == from {
                    Some(edge.entry.src)
                } else {
                    None
                };
                if let Some(target) = target
                    && reached.insert(target)
                {
                    todo.push_back(target);
                }
            }
        }
        closure.insert(vertex, reached);
    }
    ZSet::from_updates(
        vertices.iter().map(|vertex| {
            let representative = vertices
                .iter()
                .find(|other| closure[vertex].contains(*other) && closure[*other].contains(vertex))
                .unwrap();
            (
                vec![
                    QueryValue::Value(GraphValue::Vertex(*representative)),
                    QueryValue::Value(GraphValue::Vertex(*vertex)),
                ],
                ZWeight::ONE,
            )
        }),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn all_component_calls_and_downstream_operators_match_fresh_queries_after_mutations() {
    let ((), report) = run_async_under_lab(0x6e63_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(30), VId(3), VId(u128::MAX), vec![]);
        db.write(&commit, other).await.unwrap();
        let mut cases: Vec<_> = [
            "connected_components",
            "weakly_connected_components",
            "strongly_connected_components",
        ]
        .map(call)
        .into();
        cases.extend([
            format!(
                "CALL fnx.connected_components({INPUT}) YIELD component AS group_id \
                 RETURN group_id"
            ),
            format!(
                "CALL fnx.connected_components({INPUT}) YIELD component AS group_id \
                 RETURN DISTINCT group_id"
            ),
            format!(
                "CALL fnx.connected_components({INPUT}) YIELD component AS group_id \
                 RETURN group_id,COUNT(*) AS members GROUP BY group_id \
                 ORDER BY group_id LIMIT 8"
            ),
            format!(
                "CALL fnx.connected_components({INPUT}) YIELD vertex AS node,component AS group_id \
                 WHERE node <> group_id RETURN group_id,node ORDER BY node DESC LIMIT 3"
            ),
            "CALL fnx.strongly_connected_components(\
             {relations:['R'],parallel:'collapse',self_loops:'drop',direction:'reversed'}) \
             YIELD component AS group_id,vertex AS node RETURN group_id,node"
                .into(),
        ]);
        let params = GqlParameters::new();
        let mut handles = Vec::new();
        for text in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (bound, source_width) = prepared.subscription_footprint(&cx, &params).unwrap();
            assert_eq!(source_width, 2, "{text}");
            let before = db.standing_queries.len();
            handles.push(
                prepared
                    .register_standing(&mut db, &cx, &params, policy())
                    .unwrap_or_else(|error| panic!("{text}: {error:?}")),
            );
            let actual = u64::try_from(db.standing_queries.len() - before).unwrap();
            assert!(actual <= bound, "{text}: {actual} > {bound}");
        }
        assert_eq!(
            db.standing_native_columns(&cx, &handles[0]).unwrap(),
            &["group_id", "node"]
        );
        // Check the initial graph and every committed mutation, including the
        // last one. Every isolate and full-width identity remains visible.
        for tick in 0..=10 {
            for (index, (text, handle)) in cases.iter().zip(&handles).enumerate() {
                let (frontier, actual) = db.standing_native_bag(&cx, handle, policy()).unwrap();
                assert_eq!(frontier, db.frontier().unwrap());
                assert_eq!(actual, eager(&db, &cx, text), "tick {tick}: {text}");
                if index < 3 || index == 7 {
                    assert_eq!(
                        actual,
                        oracle(&db, index == 2 || index == 7),
                        "independent reachability oracle, tick {tick}: {text}"
                    );
                }
            }
            if tick == 10 {
                break;
            }
            let mut change = WriteBatch::new(R);
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
                        Some(CanonicalScalar::Bool(true)),
                    );
                }
                1 => {
                    change.add_edge(EId(16), VId(3), VId(0), vec![]);
                }
                2 => {
                    change.delete_edge(EId(10));
                }
                3 => {
                    change.delete_edge(EId(11));
                }
                4 => {
                    change.add_edge(EId(17), VId(3), VId(u128::MAX), vec![]);
                }
                5 => {
                    change.delete_edge(EId(14));
                }
                6 => {
                    change.delete_vertex(VId(1));
                }
                7 => {
                    change.create_vertex(VId(9), vec![], vec![]);
                }
                8 => {
                    change.delete_edge(EId(17));
                }
                _ => {
                    change.add_edge(EId(18), VId(u128::MAX), VId(2), vec![]);
                    change.add_edge(EId(19), VId(2), VId(u128::MAX), vec![]);
                }
            }
            db.write(&commit, change).await.unwrap();
            if tick == 4 {
                for handle in &handles {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn component_subscription_replays_exact_retractions_empty_ticks_and_rebaselines() {
    let ((), report) = run_async_under_lab(0x6e63_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let start = db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let body = format!(
            "CALL fnx.strongly_connected_components({INPUT}) \
             YIELD component AS group_id,vertex AS node"
        );
        let text = format!("{body} RETURN group_id,node");
        // The public statement path supplies its ordinary implicit RETURN *.
        let prepared = PreparedNativeRead::prepare_subscription(
            &cx,
            &format!("SUBSCRIBE TO {body}"),
            &params,
            symbols,
        )
        .unwrap();
        let (bound, width) = prepared.subscription_footprint(&cx, &params).unwrap();
        assert_eq!(width, 2);
        assert!(
            bound >= 3,
            "source, YIELD projection and terminal reservation"
        );
        let before = db.standing_queries.len();
        let setup = NativeSubscriptionSetup {
            maintenance: policy(),
            delivery: policy(),
            retention: [16, 1000, 100_000],
        };
        // Refuse only after source, schema projection and replay baseline
        // exist: the whole private suffix must disappear.
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
                assert_eq!(columns, &["group_id", "node"]);
                Ok::<_, SubscriptionError>(Arc::clone(batch))
            })
            .unwrap();
        assert!(
            u64::try_from(db.standing_queries.len() - before).unwrap() <= bound + 1,
            "the footprint covers the native circuit, plus one replay sink"
        );
        assert_eq!(baseline.frontier(), start);
        assert!(baseline.is_snapshot());
        let mut delivered = copied(baseline.rows());
        assert_eq!(delivered, eager(&db, &cx, &text));
        assert_eq!(delivered, oracle(&db, true));
        subscriber.acknowledge(baseline.receipt()).unwrap();

        let mut expected = Vec::new();
        for tick in 0..3 {
            let mut change = WriteBatch::new(R);
            if tick < 2 {
                change.delete_edge(EId(10 + tick));
            } else {
                change.set_vertex_property(VId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
            }
            let at = db.write(&commit, change).await.unwrap();
            expected.push((at, eager(&db, &cx, &text)));
        }
        // Poll only after all three commits: duplicate support deletion,
        // actual SCC split, and unrelated property edit must stay distinct.
        let mut after = start;
        for (tick, (at, expected)) in expected.iter().enumerate() {
            let frame = subscriber.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!((frame.from(), frame.frontier()), (Some(after), *at));
            if tick != 1 {
                assert!(frame.rows().is_empty());
            } else {
                assert!(!frame.rows().is_empty());
                assert!(
                    frame
                        .rows()
                        .iter()
                        .any(|(_, weight)| weight < &ZWeight::ZERO)
                );
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
        assert_eq!(fresh.frontier(), after);
        assert_eq!(fresh.rows(), &oracle(&db, true));
        subscriber.acknowledge(fresh.receipt()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_component_contracts_and_implicit_pages_refuse_before_publication() {
    let ((), report) = run_async_under_lab(0x6e63_6103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let accepted = call("connected_components");
        let prior = db
            .register_standing_native(&cx, &accepted, &params, symbols, policy())
            .unwrap();
        let before = db.standing_queries.len();
        let answer = db.standing_native_bag(&cx, &prior, policy()).unwrap();
        let mut bodies = vec![
            "CALL fnx.connected_components()".to_string(),
            "CALL fnx.connected_components({relations:['R'],self_loops:'drop'})".into(),
            "CALL fnx.connected_components({relations:['R'],parallel:'collapse'})".into(),
            "CALL fnx.connected_components({relations:['R'],parallel:'collapse',self_loops:'reject'})".into(),
            "CALL fnx.connected_components({relations:['R'],labels:['L'],parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.connected_components({relations:['R'],weight:'p',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.connected_components({relations:['R'],direction:'directed',parallel:'collapse',self_loops:'drop'})".into(),
            "CALL fnx.strongly_connected_components({relations:['R'],direction:'undirected',parallel:'collapse',self_loops:'drop'})".into(),
            format!("CALL fnx.connected_components({INPUT},7)"),
            // Named arguments are runtime values, not the zero-argument
            // literal projection profile admitted by this derivative.
            "CALL fnx.connected_components(relation => 'R',parallel => 'collapse',self_loops => 'drop')".into(),
        ];
        for name in ["pagerank", "triangles", "core_number"] {
            bodies.push(format!("CALL fnx.{name}({INPUT})"));
        }
        for body in bodies {
            // An empty, explicitly ordered page cannot hide an unsupported
            // descendant or bypass complete projection/argument validation.
            let text = format!("{body} YIELD vertex RETURN vertex ORDER BY vertex LIMIT 0");
            if let Ok(prepared) = PreparedNativeRead::prepare(&text, &params, symbols) {
                assert!(
                    prepared.subscription_footprint(&cx, &params).is_err(),
                    "footprint admitted {text}"
                );
            }
            assert!(
                db.register_standing_native(&cx, &text, &params, symbols, policy())
                    .is_err(),
                "{text}"
            );
            assert_eq!(db.standing_queries.len(), before);
        }
        for text in [
            format!("{accepted} LIMIT 1"),
            format!("{accepted} SKIP 1 LIMIT 0"),
            format!(
                "{accepted} UNION ALL MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 \
                 RETURN n AS group_id,n AS node"
            ),
        ] {
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
        for tight in [
            GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000),
            GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 0, 10_000_000),
            GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 0),
        ] {
            assert!(
                db.register_standing_native(&cx, &accepted, &params, symbols, tight)
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
            StandingQuery::Components(query) => query.as_ref() as *const _ as usize,
            StandingQuery::Projection(query) => query.as_ref() as *const _ as usize,
            _ => panic!("component source and ordinary projections only"),
        })
        .collect()
}

#[test]
fn every_component_registration_and_rebuild_stage_rolls_back_its_private_suffix() {
    let ((), report) = run_async_under_lab(0x6e63_6104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let query = PreparedGraphSetText::prepare(&call("strongly_connected_components"), symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let handle = register(&mut db, &cx, &query, policy()).unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("native component circuit");
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
        assert_eq!(db.standing_queries.len(), before);
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
