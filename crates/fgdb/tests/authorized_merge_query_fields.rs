//! Scalar MERGE action/RETURN reads must not bill hidden storage comparisons.
//! Exercise both public vertex query APIs over real native transactions. These
//! are logical quota checks, not timing or physical-memory isolation claims.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaLimitDimension, GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphVertexMergePolicy, GraphVertexUpsertError,
    GraphVertexUpsertPolicy, GraphVertexUpsertStats, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use fgdb_warden::{
    Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights,
    Scope,
};
use std::sync::atomic::{AtomicU64, Ordering};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x67; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const ABSENT: PropertyKeyId = PropertyKeyId(3);
const SECRET: PropertyKeyId = PropertyKeyId(10);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;
const STATEMENT: &str = "MERGE (n:Visible {p:7}) ON CREATE SET n.q=n.p+1 ON MATCH SET n.q=n.p+1 \
     SET n.q=n.q+1 RETURN n AS node,n.p AS p,n.q AS q, \
     n.secret AS private_value,n.missing AS missing_value ORDER BY node";

type VertexFault =
    GqlQueryError<GraphVertexUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type VertexReceipt = (
    GraphVertexUpsertStats,
    GqlQueryExecution<GraphValueRow>,
    EmbeddedTxnCompletion,
);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "missing") => Some(GraphSymbol::Property(ABSENT)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn vertex_query(text: &str) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn authority() -> (Authority, CapabilityToken) {
    let authority =
        Authority::new(AuthKey::from_seed(8267), NS, "graph", SchemaEpoch(1), 1).unwrap();
    let token = authority
        .issue_at(
            &Grant {
                branch: "main".into(),
                labels: Scope::only([L]),
                relations: Scope::only([R]),
                properties: Scope::only([P, Q, ABSENT]),
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 10_000,
                    max_work: 1_000_000,
                    max_rows: 100,
                },
                expires_at_ms: EXPIRES,
            },
            NOW,
        )
        .unwrap();
    (authority, token)
}

fn vertex_policy() -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 100, 1_000_000, 100_000)),
        100,
    )
}

async fn vertex_database(cx: &CommitCx, count: u128, hidden: u64) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x66; 32], NS, [0x68; 32]))
        .await
        .unwrap();
    if count != 0 {
        let mut batch = WriteBatch::new(R);
        for id in 1..=count {
            let mut properties = vec![(P, CanonicalScalar::Int(7))];
            // Hidden values stay small, so even 255 of them fit one row's 16
            // KiB storage bound: only how many keys there are moves a
            // physical lookup.
            for offset in 0..hidden {
                properties.push((
                    PropertyKeyId(SECRET.0 + offset),
                    CanonicalScalar::bytes(vec![0x39; 16]).unwrap(),
                ));
            }
            batch.create_vertex(VId(id), vec![L], properties);
        }
        db.write(cx, batch).await.unwrap();
    }
    db
}

#[allow(clippy::too_many_arguments)]
async fn vertex_execute(
    db: &mut Database<MemVfs>,
    contexts: &PurposeContexts,
    authority: &Authority,
    token: &CapabilityToken,
    plural: bool,
    query: &PreparedGraphVertexUpsertQuery,
    policy: GraphVertexUpsertPolicy,
    clock: impl FnMut() -> u64,
) -> Result<VertexReceipt, VertexFault> {
    let txn = contexts.txn();
    let query_cx = contexts.query();
    let commit = contexts.commit();
    let result = if plural {
        db.execute_graph_vertex_upserts_query_authorized(
            &txn, &query_cx, &commit, authority, token, "main", query, policy, clock,
        )
        .await
    } else {
        db.execute_graph_vertex_upsert_query_authorized(
            &txn, &query_cx, &commit, authority, token, "main", query, policy, clock,
        )
        .await
        .map(|(stats, _, rows, completion)| (stats, rows, completion))
    };
    assert_eq!(txn.outstanding_obligations(), 0);
    result
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn vertex_work_refusal(error: &VertexFault, signed: bool) {
    if signed {
        assert_eq!(
            authorization(error),
            Some(Error::LimitExceeded(LimitDimension::Work))
        );
    } else {
        assert!(
            matches!(
                error,
                GqlQueryError::Evaluator(limit) if limit.dimension == GlaLimitDimension::WorkUnits
            ),
            "unexpected refusal: {error:?}"
        );
    }
}

#[test]
fn vertex_scalar_actions_and_return_have_scope_stable_admission_boundaries() {
    let (_, report) = run_async_under_lab(0xb181, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (authority, token) = authority();
        let query = vertex_query(STATEMENT);
        for (plural, count) in [(false, 1), (true, 2)] {
            for signed in [true, false] {
                let mut expected = None;
                let mut attempt = |receipt: VertexReceipt| {
                    let (stats, rows, _) = receipt;
                    assert_eq!(rows.value.len() as u128, count);
                    for (index, row) in rows.value.iter().enumerate() {
                        assert_eq!(
                            row.values(),
                            &[
                                GraphValue::Vertex(VId(index as u128 + 1)),
                                GraphValue::Scalar(CanonicalScalar::Int(7)),
                                GraphValue::Scalar(CanonicalScalar::Int(9)),
                                GraphValue::Scalar(CanonicalScalar::Null),
                                GraphValue::Scalar(CanonicalScalar::Null),
                            ]
                        );
                    }
                    let observation = (stats, rows.rows, rows.evaluator, rows.value);
                    if let Some(expected) = &expected {
                        assert_eq!(&observation, expected);
                    } else {
                        expected = Some(observation);
                    }
                };
                // Discover a real clean-store boundary, then require the same
                // boundary and typed error with 31/255 hidden keys per target.
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let allowance = low + (high - low) / 2;
                    let limited = if signed {
                        token.attenuate(Restriction::MaxWork(allowance)).unwrap()
                    } else {
                        token.clone()
                    };
                    let mut policy = vertex_policy();
                    if !signed {
                        policy.merge.query.evaluator.max_work_units = allowance;
                    }
                    let mut db = vertex_database(&commit, count, 0).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap());
                    match vertex_execute(
                        &mut db,
                        &contexts,
                        &authority,
                        &limited,
                        plural,
                        &query,
                        policy,
                        || NOW,
                    )
                    .await
                    {
                        Ok(receipt) => {
                            attempt(receipt);
                            high = allowance;
                        }
                        Err(error) => {
                            vertex_work_refusal(&error, signed);
                            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                            low = allowance + 1;
                        }
                    }
                }
                assert!(low > 0 && low < 8192);
                for hidden in [0, 31, 255] {
                    for allowance in [low - 1, low, low + 1] {
                        let limited = if signed {
                            token.attenuate(Restriction::MaxWork(allowance)).unwrap()
                        } else {
                            token.clone()
                        };
                        let mut policy = vertex_policy();
                        if !signed {
                            policy.merge.query.evaluator.max_work_units = allowance;
                        }
                        let mut db = vertex_database(&commit, count, hidden).await;
                        let before = (db.frontier().unwrap(), db.vertices().unwrap());
                        let result = vertex_execute(
                            &mut db,
                            &contexts,
                            &authority,
                            &limited,
                            plural,
                            &query,
                            policy,
                            || NOW,
                        )
                        .await;
                        if allowance < low {
                            vertex_work_refusal(&result.unwrap_err(), signed);
                            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
                        } else {
                            attempt(result.unwrap());
                            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
                            for original in &before.1 {
                                let current = db.vertex(original.vid).unwrap().unwrap();
                                assert_eq!(current.props[0], (P, CanonicalScalar::Int(7).into()));
                                assert_eq!(current.props[1], (Q, CanonicalScalar::Int(9).into()));
                                assert_eq!(&current.props[2..], &original.props[1..]);
                            }
                        }
                    }
                }
                assert!(expected.is_some());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_query_late_refusals_rollback_and_limit_zero_does_not_skip_actions() {
    let (_, report) = run_async_under_lab(0xb182, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (authority, token) = authority();
        for (plural, count) in [(false, 1), (true, 2)] {
            let mut db = vertex_database(&commit, count, 31).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let forbidden = vertex_query(
                "MERGE (n:Visible {p:7}) ON MATCH SET n.q=8 SET n.secret=99 RETURN n.q",
            );
            let error = vertex_execute(
                &mut db,
                &contexts,
                &authority,
                &token,
                plural,
                &forbidden,
                vertex_policy(),
                || NOW,
            )
            .await
            .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::ScopeDenied));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);

            let bad_return = vertex_query(
                "MERGE (n:Visible {p:7}) ON MATCH SET n.q=n.p+1 SET n.q=n.q+1 \
                 RETURN 1/(n.q-9) AS bad LIMIT 0",
            );
            let error = vertex_execute(
                &mut db,
                &contexts,
                &authority,
                &token,
                plural,
                &bad_return,
                vertex_policy(),
                || NOW,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                GqlQueryError::Source(GraphVertexUpsertError::Returning(_))
            ));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);

            let query = vertex_query(STATEMENT);
            let no_rows = token.attenuate(Restriction::MaxRows(0)).unwrap();
            let error = vertex_execute(
                &mut db,
                &contexts,
                &authority,
                &no_rows,
                plural,
                &query,
                vertex_policy(),
                || NOW,
            )
            .await
            .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::LimitExceeded(LimitDimension::Rows))
            );
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);

            // Sample the actual live-check trace, then expire immediately at
            // its last check. No result or staged update may be published.
            let calls = AtomicU64::new(0);
            let mut probe = vertex_database(&commit, count, 31).await;
            vertex_execute(
                &mut probe,
                &contexts,
                &authority,
                &token,
                plural,
                &query,
                vertex_policy(),
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    NOW
                },
            )
            .await
            .unwrap();
            let cutoff = calls.load(Ordering::Relaxed) - 1;
            assert!(cutoff > 1);
            calls.store(0, Ordering::Relaxed);
            let error = vertex_execute(
                &mut db,
                &contexts,
                &authority,
                &token,
                plural,
                &query,
                vertex_policy(),
                || {
                    if calls.fetch_add(1, Ordering::Relaxed) < cutoff {
                        NOW
                    } else {
                        EXPIRES
                    }
                },
            )
            .await
            .unwrap_err();
            assert_eq!(authorization(&error), Some(Error::Expired));
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);

            let empty = vertex_query(&format!("{STATEMENT} LIMIT 0"));
            let (_, rows, completion) = vertex_execute(
                &mut db,
                &contexts,
                &authority,
                &no_rows,
                plural,
                &empty,
                vertex_policy(),
                || NOW,
            )
            .await
            .unwrap();
            assert!(rows.value.is_empty());
            assert!(matches!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted { .. }
            ));
            assert_eq!(db.frontier().unwrap().0, before.0.0 + 1);
            for original in &before.1 {
                let current = db.vertex(original.vid).unwrap().unwrap();
                assert_eq!(current.props[1], (Q, CanonicalScalar::Int(9).into()));
                assert_eq!(&current.props[2..], &original.props[1..]);
            }
            // The creation branch reads its own new property through the same
            // projector; no historical target is necessary for that lookup.
            let mut empty_db = vertex_database(&commit, 0, 0).await;
            let (_, rows, _) = vertex_execute(
                &mut empty_db,
                &contexts,
                &authority,
                &token,
                plural,
                &query,
                vertex_policy(),
                || NOW,
            )
            .await
            .unwrap();
            assert_eq!(rows.value.len(), 1);
            assert_eq!(
                rows.value[0].values()[2],
                GraphValue::Scalar(CanonicalScalar::Int(9))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_vertex_merge_outputs_use_post_action_values_and_mask_hidden_keys() {
    let (_, report) = run_async_under_lab(0xb183, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (authority, token) = authority();
        for (plural, count) in [(false, 0), (false, 1), (true, 0), (true, 2)] {
            for (tail, grouped, aggregate) in [
                (
                    "RETURN [v IN [n] | v.q] AS values, [v IN [n] | v.secret] AS hidden",
                    false,
                    false,
                ),
                (
                    "RETURN sum([v IN [n] | v.q][0]) AS total, \
                     [v IN collect(n) | v.secret] AS hidden",
                    true,
                    true,
                ),
                (
                    "WITH collect(n) AS ns \
                     RETURN [v IN ns | v.q] AS values, [v IN ns | v.secret] AS hidden",
                    true,
                    false,
                ),
            ] {
                let mut db = vertex_database(&commit, count, 31).await;
                let query = vertex_query(&format!(
                    "MERGE (n:Visible {{p:7}}) ON CREATE SET n.q=n.p+1 \
                     ON MATCH SET n.q=n.p+1 SET n.q=n.q+1 {tail}"
                ));
                let (_, rows, _) = vertex_execute(
                    &mut db,
                    &contexts,
                    &authority,
                    &token,
                    plural,
                    &query,
                    vertex_policy(),
                    || NOW,
                )
                .await
                .unwrap();
                let occurrences = count.max(1) as usize;
                let width = if grouped { occurrences } else { 1 };
                let values = if aggregate {
                    GraphValue::Scalar(CanonicalScalar::Int(9 * occurrences as i64))
                } else {
                    GraphValue::List(
                        vec![GraphValue::Scalar(CanonicalScalar::Int(9)); width].into_boxed_slice(),
                    )
                };
                let hidden = GraphValue::List(
                    vec![GraphValue::Scalar(CanonicalScalar::Null); width].into_boxed_slice(),
                );
                assert_eq!(rows.value.len(), if grouped { 1 } else { occurrences });
                for row in rows.value {
                    assert_eq!(row.values(), &[values.clone(), hidden.clone()], "{tail}");
                }
                for vertex in db.vertices().unwrap() {
                    assert!(vertex.props.contains(&(Q, CanonicalScalar::Int(9).into())));
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_merge_output_masks_parameter_ids_relations_and_both_edge_endpoints() {
    let (_, report) = run_async_under_lab(0xb184, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (authority, token) = authority();
        let mut db = vertex_database(&commit, 1, 1).await;
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(9)));
        batch.create_vertex(
            VId(2),
            vec![L],
            vec![(P, CanonicalScalar::Int(8)), (Q, CanonicalScalar::Int(22))],
        );
        batch.create_vertex(
            VId(3),
            vec![LabelId(2)],
            vec![(P, CanonicalScalar::Int(8)), (Q, CanonicalScalar::Int(43))],
        );
        for (id, source, target, value) in [(11, 1, 2, 7), (12, 1, 3, 44), (13, 3, 1, 45)] {
            batch.add_edge(
                EId(id),
                VId(source),
                VId(target),
                vec![(Q, CanonicalScalar::Int(value))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let mut hidden_relation = WriteBatch::new(RelationId(2));
        hidden_relation.add_edge(EId(14), VId(1), VId(2), vec![(Q, CanonicalScalar::Int(46))]);
        db.write(&commit, hidden_relation).await.unwrap();
        let template = PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
            "MERGE (n:Visible {p:7}) ON MATCH SET n.q=9 \
             RETURN [v IN $refs | v.q] AS values, [v IN $refs | v.secret] AS hidden",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap();
        for plural in [false, true] {
            let mut hidden_stats = [None, None];
            for (identity, value) in [
                (GraphValue::Vertex(VId(2)), Some(22)),
                (GraphValue::Edge(EId(11)), Some(7)),
                (GraphValue::Vertex(VId(3)), None),
                (GraphValue::Edge(EId(12)), None),
                (GraphValue::Edge(EId(13)), None),
                (GraphValue::Edge(EId(14)), None),
                (GraphValue::Vertex(VId(999)), None),
                (GraphValue::Edge(EId(999)), None),
            ] {
                let parameters = GqlParameters::new()
                    .with_list("refs", vec![identity.clone()])
                    .unwrap();
                let query = template.bind_parameters(&parameters).unwrap();
                let (stats, rows, _) = vertex_execute(
                    &mut db,
                    &contexts,
                    &authority,
                    &token,
                    plural,
                    &query,
                    vertex_policy(),
                    || NOW,
                )
                .await
                .unwrap();
                let expected =
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int));
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[
                        GraphValue::List(vec![expected].into_boxed_slice()),
                        GraphValue::List(
                            vec![GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
                        ),
                    ],
                    "plural={plural}, identity={identity:?}"
                );
                if value.is_none() {
                    let kind = usize::from(matches!(&identity, GraphValue::Edge(_)));
                    let observed = (stats, rows.rows, rows.evaluator);
                    match hidden_stats[kind] {
                        Some(previous) => assert_eq!(observed, previous, "{identity:?}"),
                        None => hidden_stats[kind] = Some(observed),
                    }
                }
                let write_only = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                let error = vertex_execute(
                    &mut db,
                    &contexts,
                    &authority,
                    &write_only,
                    plural,
                    &query,
                    vertex_policy(),
                    || NOW,
                )
                .await
                .unwrap_err();
                assert_eq!(authorization(&error), Some(Error::PermissionDenied));
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_merge_tail_refusals_never_publish_partial_actions() {
    let (_, report) = run_async_under_lab(0xb185, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let (authority, token) = authority();
        for (plural, count) in [(false, 1), (true, 2)] {
            let prefix = "MERGE (n:Visible {p:7}) ON MATCH SET n.q=8 SET n.q=n.q+1 \
                          WITH collect(n) AS ns";
            let query = vertex_query(&format!("{prefix} RETURN [v IN ns | v.q] AS values"));
            let bad = vertex_query(&format!(
                "{prefix} RETURN [v IN ns | 1/(v.q-9)] AS bad LIMIT 0"
            ));
            let mut probe = vertex_database(&commit, count, 1).await;
            let calls = AtomicU64::new(0);
            let (_, baseline, _) = vertex_execute(
                &mut probe,
                &contexts,
                &authority,
                &token,
                plural,
                &query,
                vertex_policy(),
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    NOW
                },
            )
            .await
            .unwrap();
            let last_check = calls.load(Ordering::Relaxed) - 1;
            assert!(last_check > 1);
            let no_rows = token.attenuate(Restriction::MaxRows(0)).unwrap();
            for case in 0..4 {
                let mut db = vertex_database(&commit, count, 1).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap());
                let mut policy = vertex_policy();
                if case == 2 {
                    policy.merge.query.evaluator.max_work_units = baseline.evaluator.work_units - 1;
                }
                calls.store(0, Ordering::Relaxed);
                let error = vertex_execute(
                    &mut db,
                    &contexts,
                    &authority,
                    if case == 1 { &no_rows } else { &token },
                    plural,
                    if case == 0 { &bad } else { &query },
                    policy,
                    || {
                        if case == 3 && calls.fetch_add(1, Ordering::Relaxed) >= last_check {
                            EXPIRES
                        } else {
                            NOW
                        }
                    },
                )
                .await
                .unwrap_err();
                match case {
                    0 => assert!(matches!(
                        error,
                        GqlQueryError::Source(GraphVertexUpsertError::Returning(_))
                    )),
                    1 => assert_eq!(
                        authorization(&error),
                        Some(Error::LimitExceeded(LimitDimension::Rows))
                    ),
                    2 => vertex_work_refusal(&error, false),
                    3 => assert_eq!(authorization(&error), Some(Error::Expired)),
                    _ => unreachable!(),
                }
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_authorized_edge_hosts_read_computed_final_values_and_mask_hidden_keys() {
    let (_, report) = run_async_under_lab(0xb186, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let (authority, token) = authority();
        let query = fgdb_gql::PreparedGraphEdgeUpsertQueryText::prepare(
            "MATCH (a:Visible {p:7}),(b:Visible {p:8}) MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=5 ON MATCH SET r.p=r.p+1 SET r.p=r.p*2 \
             WITH collect(r) AS edges \
             RETURN [e IN edges | e.p] AS values, [e IN edges | e.secret] AS hidden",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let policy = fgdb_gql::GraphEdgeUpsertPolicy::new(
            fgdb_gql::GraphEdgeMergePolicy::new(vertex_policy().merge.query),
            100,
        );
        for plural in [false, true] {
            for existing in [false, true] {
                let mut db = vertex_database(&commit, 1, 1).await;
                let mut seed = WriteBatch::new(R);
                seed.create_vertex(VId(2), vec![L], vec![(P, CanonicalScalar::Int(8))]);
                if existing {
                    for offset in 0..if plural { 2 } else { 1 } {
                        seed.add_edge(
                            EId(11 + offset),
                            VId(1),
                            VId(2),
                            vec![
                                (P, CanonicalScalar::Int(5 + offset as i64)),
                                (SECRET, CanonicalScalar::Int(99)),
                            ],
                        );
                    }
                }
                db.write(&commit, seed).await.unwrap();
                let rows = if plural {
                    db.execute_graph_edge_upserts_query_authorized(
                        &txcx,
                        &cx,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &query,
                        policy,
                        || NOW,
                    )
                    .await
                    .unwrap()
                    .1
                } else {
                    db.execute_graph_edge_upsert_query_authorized(
                        &txcx,
                        &cx,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &query,
                        policy,
                        || NOW,
                    )
                    .await
                    .unwrap()
                    .2
                };
                let values = if !existing {
                    vec![10]
                } else if plural {
                    vec![12, 14]
                } else {
                    vec![12]
                };
                assert_eq!(rows.value.len(), 1);
                assert_eq!(
                    rows.value[0].values(),
                    &[
                        GraphValue::List(
                            values
                                .iter()
                                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
                                .collect(),
                        ),
                        GraphValue::List(
                            vec![GraphValue::Scalar(CanonicalScalar::Null); values.len()]
                                .into_boxed_slice(),
                        ),
                    ],
                    "plural={plural}, existing={existing}"
                );
                let stored: Vec<_> = db
                    .edges()
                    .unwrap()
                    .into_iter()
                    .map(|edge| edge.props.into_iter().find(|(key, _)| *key == P).unwrap().1)
                    .collect();
                assert_eq!(
                    stored,
                    values
                        .into_iter()
                        .map(|value| CanonicalScalar::Int(value).into())
                        .collect::<Vec<fgdb_types::CanonicalPropertyValue>>()
                );
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
