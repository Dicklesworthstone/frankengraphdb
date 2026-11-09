//! Correlated OPTIONAL predicates keep the native statement's source boundary.
//! Expected rows below are literal fixtures, including rejected candidates and
//! parallel outer occurrences; no second query computes the expected answer.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSymbol, GraphSymbolKind,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    SimulationCheckpointProbe, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xb4; 32]);
const OWNER: LabelId = LabelId(1);
const VISIBLE: LabelId = LabelId(2);
const HIDDEN: LabelId = LabelId(99);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const H: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId((1_u128 << 100) + 3);
const BRANCH: &str = "main";

type Row = (VId, Option<VId>, Option<EId>);

const BEFORE: &[Row] = &[
    (VId(1), Some(VId(10)), Some(EId(101))),
    (VId(1), Some(VId(10)), Some(EId(102))),
    (VId(2), Some(VId(12)), Some(EId(104))),
    (HIGH, None, None),
];
const STAGED: &[Row] = &[
    (VId(1), Some(VId(11)), Some(EId(103))),
    (VId(2), Some(VId(12)), Some(EId(104))),
    (HIGH, None, None),
];
const NEGATIVE_BEFORE: &[Row] = &[
    (VId(1), Some(VId(11)), Some(EId(103))),
    (VId(2), None, None),
    (HIGH, None, None),
];
const NEGATIVE_STAGED: &[Row] = &[
    (VId(1), Some(VId(10)), Some(EId(101))),
    (VId(1), Some(VId(10)), Some(EId(102))),
    (VId(2), None, None),
    (HIGH, None, None),
];
const MASKED: &[Row] = &[
    (VId(1), Some(VId(10)), Some(EId(101))),
    (VId(1), Some(VId(10)), Some(EId(102))),
    (VId(2), None, None),
    (HIGH, None, None),
];
const ALL_NULL: &[Row] = &[
    (VId(1), None, None),
    (VId(2), None, None),
    (HIGH, None, None),
];

const EXISTS: &str = "EXISTS { MATCH (b)-[:S]->(c) WHERE c.p >= $floor }";
const COUNT: &str = "COUNT { MATCH (b)-[:S]->(c) WHERE c.p >= $floor } >= 2";
const LIST: &str = "size([(b)-[:S]->(c) WHERE c.p >= $floor | c]) >= 2";
const NOT_EXISTS: &str = "NOT EXISTS { MATCH (b)-[:S]->(c) WHERE c.p >= $floor }";

fn cases() -> [(&'static str, &'static [Row], &'static [Row]); 4] {
    [
        (EXISTS, BEFORE, STAGED),
        (COUNT, BEFORE, STAGED),
        (LIST, BEFORE, STAGED),
        (NOT_EXISTS, NEGATIVE_BEFORE, NEGATIVE_STAGED),
    ]
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb3; 32], NS, [0xb5; 32])
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 1_000, 1_000_000, 1_000_000)
}

fn arguments() -> GqlParameters {
    GqlParameters::new().with_int64("floor", 5).unwrap()
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "H") => Some(GraphSymbol::Relation(H)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}

fn statement(predicate: &str, historical: bool) -> String {
    let selector = if historical {
        " FOR SYSTEM_TIME AS OF SEQ $at"
    } else {
        ""
    };
    format!(
        "MATCH (a:Owner){selector} WITH a \
         OPTIONAL MATCH (a)-[r:R]->(b) WHERE {predicate} \
         RETURN a,b,r ORDER BY a,r"
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, physically_masked: bool) -> CommitSeq {
    let mut first = WriteBatch::new(R);
    for owner in [VId(1), VId(2), HIGH] {
        first.create_vertex(owner, vec![OWNER], vec![(P, CanonicalScalar::Int(4))]);
    }
    for (id, label, value) in [
        (10, VISIBLE, 0),
        (11, VISIBLE, 0),
        (12, VISIBLE, 0),
        (20, VISIBLE, 9),
        (21, VISIBLE, 2),
        (22, HIDDEN, 9),
        (23, VISIBLE, 9),
    ] {
        if physically_masked && label == HIDDEN {
            continue;
        }
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        if !physically_masked {
            properties.push((SECRET, CanonicalScalar::Int(777)));
        }
        first.create_vertex(VId(id), vec![label], properties);
    }
    for (id, source, target) in [(101, 1, 10), (102, 1, 10), (103, 1, 11), (104, 2, 12)] {
        first.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (id, source, target) in [
        (201, 10, 20),
        (202, 10, 20),
        (203, 11, 21),
        (204, 12, 22),
        (205, 12, 22),
    ] {
        if !physically_masked || target != 22 {
            second.add_edge(EId(id), VId(source), VId(target), vec![]);
        }
    }
    let mut batches = vec![first, second];
    if !physically_masked {
        let mut third = WriteBatch::new(H);
        third.add_edge(EId(301), VId(12), VId(23), vec![]);
        batches.push(third);
    }
    db.write_atomic(cx, batches).await.unwrap()
}

fn rows(result: QueryResult) -> Vec<Row> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected native read rows");
    };
    assert_eq!(columns, ["a", "b", "r"]);
    rows.into_iter()
        .map(|row| {
            let owner = row[0].as_value().and_then(GraphValue::as_vertex).unwrap();
            let candidate = match row[1].as_value() {
                Some(GraphValue::Vertex(vertex)) => Some(*vertex),
                Some(value) if value.is_null() => None,
                other => panic!("expected an optional candidate: {other:?}"),
            };
            let relationship = match row[2].as_value() {
                Some(GraphValue::Edge(edge)) => Some(*edge),
                Some(value) if value.is_null() => None,
                other => panic!("expected an optional relationship: {other:?}"),
            };
            (owner, candidate, relationship)
        })
        .collect()
}

#[test]
fn optional_subqueries_keep_staged_history_and_reopened_versions() {
    let ((), report) = run_async_under_lab(0x0f71_7201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit, false).await;
        let pinned = db.read_session().unwrap();
        let params = arguments();
        let historical_params = arguments().with_uint64("at", basis.0).unwrap();
        let mut plans = Vec::new();
        for (predicate, before, staged) in cases() {
            let prepared =
                PreparedNativeRead::prepare(&statement(predicate, false), &params, symbols)
                    .unwrap();
            let historical = PreparedNativeRead::prepare(
                &statement(predicate, true),
                &historical_params,
                symbols,
            )
            .unwrap();
            assert_eq!(prepared.facade_class(), NativeReadClass::Set);
            assert_eq!(historical.facade_class(), NativeReadClass::TemporalSet);
            assert_eq!(
                rows(prepared.execute(&db, &cx, &params, policy()).unwrap()),
                before,
                "{predicate}"
            );
            plans.push((prepared, historical, before, staged));
        }

        let mut txn = db.begin(&txcx).unwrap();
        let mut changes = WriteBatch::new(S);
        changes.delete_edge(EId(201));
        changes.delete_edge(EId(202));
        changes.add_edge(EId(206), VId(11), VId(20), vec![]);
        changes.add_edge(EId(207), VId(11), VId(20), vec![]);
        txn.write(&mut db, changes).unwrap();
        for (prepared, historical, before, staged) in &plans {
            assert_eq!(
                rows(
                    prepared
                        .execute_in_transaction(&txn, &db, &cx, &params, policy())
                        .unwrap()
                ),
                *staged
            );
            let (result, selected) = historical
                .execute_in_transaction_with_snapshot(&txn, &db, &cx, &historical_params, policy())
                .unwrap();
            assert_eq!(selected, Some(basis));
            assert_eq!(rows(result), *before);
            assert_eq!(
                rows(prepared.execute(&db, &cx, &params, policy()).unwrap()),
                *before
            );
        }
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        for (prepared, historical, before, staged) in &plans {
            assert_eq!(
                rows(prepared.execute(&db, &cx, &params, policy()).unwrap()),
                *staged
            );
            assert_eq!(
                rows(
                    prepared
                        .execute_in_view(&pinned, &cx, &params, policy())
                        .unwrap()
                ),
                *before
            );
            assert_eq!(
                rows(
                    historical
                        .execute(&db, &cx, &historical_params, policy())
                        .unwrap()
                ),
                *before
            );
        }

        // The same prepared child predicates must select old Strata versions
        // after history has been compacted and reopened through Chronicle.
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (prepared, historical, before, staged) in &plans {
            assert_eq!(
                rows(prepared.execute(&reopened, &cx, &params, policy()).unwrap()),
                *staged
            );
            assert_eq!(
                rows(
                    historical
                        .execute(&reopened, &cx, &historical_params, policy())
                        .unwrap()
                ),
                *before
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_subqueries_mask_inner_topology_before_existence_count_and_collection() {
    let ((), report) = run_async_under_lab(0x0f71_7202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut physical = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        seed(&mut physical, &commit, true).await;
        let issuer = Authority::new(
            AuthKey::from_seed(0x0f71_7202),
            NS,
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            BRANCH,
            1_000,
            QueryLimits {
                max_nodes: 10_000,
                max_work: 1_000_000,
                max_rows: 1_000,
            },
        );
        grant.labels = Scope::only([OWNER, VISIBLE]);
        grant.relations = Scope::only([R, S]);
        grant.properties = Scope::only([P]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let negative_masked: &[Row] = &[
            (VId(1), Some(VId(11)), Some(EId(103))),
            (VId(2), Some(VId(12)), Some(EId(104))),
            (HIGH, None, None),
        ];
        let hidden_relation: &[Row] = &[
            (VId(1), None, None),
            (VId(2), Some(VId(12)), Some(EId(104))),
            (HIGH, None, None),
        ];
        let hidden_property: &[Row] = &[
            (VId(1), Some(VId(10)), Some(EId(101))),
            (VId(1), Some(VId(10)), Some(EId(102))),
            (VId(1), Some(VId(11)), Some(EId(103))),
            (VId(2), Some(VId(12)), Some(EId(104))),
            (HIGH, None, None),
        ];
        for (predicate, unmasked, masked) in [
            (EXISTS, BEFORE, MASKED),
            (COUNT, BEFORE, MASKED),
            (LIST, BEFORE, MASKED),
            (NOT_EXISTS, NEGATIVE_BEFORE, negative_masked),
            (
                "EXISTS { MATCH (b)-[:H]->(c) WHERE c.p >= $floor }",
                hidden_relation,
                ALL_NULL,
            ),
            (
                "EXISTS { MATCH (b)-[:S]->(c) WHERE c.secret >= $floor }",
                hidden_property,
                ALL_NULL,
            ),
        ] {
            let text = statement(predicate, false);
            let params = arguments();
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            // Establish that a hidden witness really changes the unmasked
            // answer, then compare to data physically lacking every secret.
            assert_eq!(
                rows(prepared.execute(&db, &cx, &params, policy()).unwrap()),
                unmasked,
                "{predicate}"
            );
            assert_eq!(
                rows(prepared.execute(&physical, &cx, &params, policy()).unwrap()),
                masked,
                "{predicate}"
            );
            assert_eq!(
                rows(
                    prepared
                        .execute_authorized(
                            &db,
                            &cx,
                            &issuer,
                            &token,
                            BRANCH,
                            &params,
                            policy(),
                            || 100,
                        )
                        .unwrap()
                ),
                masked,
                "{predicate}"
            );
            assert_eq!(
                rows(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &text,
                        &params,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
                ),
                masked,
                "{predicate}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_subquery_refusals_keep_staged_effects_and_inner_conflict_observations() {
    let ((), report) = run_async_under_lab(0x0f71_7203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit, false).await;
        let params = arguments();
        let prepared =
            PreparedNativeRead::prepare(&statement(COUNT, false), &params, symbols).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(900), vec![VISIBLE], vec![]);
        txn.write(&mut db, staged).unwrap();

        // No successful graph query precedes this refusal. The final output
        // cap must retain the inner S-edge observations that produced it.
        let error = prepared
            .execute_in_transaction(
                &txn,
                &db,
                &cx,
                &params,
                GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000),
            )
            .unwrap_err();
        assert!(
            matches!(&error, QueryError::TransactionSet(inner)
            if matches!(inner.as_ref(), GqlQueryError::Rows(limit)
                if limit.dimension == GqlBudgetDimension::ResultRows)),
            "{error:?}"
        );
        assert!(txn.vertex(&db, VId(900)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap(), basis);
        let mut concurrent = WriteBatch::new(S);
        concurrent.delete_edge(EId(201));
        let winner = db.write(&commit, concurrent).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert_eq!(db.frontier().unwrap(), winner);
        assert!(db.vertex(VId(900)).unwrap().is_none());

        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(901), vec![VISIBLE], vec![]);
        txn.write(&mut db, staged).unwrap();
        let after_delete: &[Row] = &[
            (VId(1), None, None),
            (VId(2), Some(VId(12)), Some(EId(104))),
            (HIGH, None, None),
        ];
        assert_eq!(
            rows(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            after_delete
        );
        for (allowance, expected) in [
            (
                GqlQueryPolicy::new(0, 1_000, 1_000_000, 1_000_000),
                "snapshot",
            ),
            (GqlQueryPolicy::new(10_000, 1_000, 0, 1_000_000), "work"),
            (GqlQueryPolicy::new(10_000, 1_000, 1_000_000, 0), "scratch"),
        ] {
            let error = prepared
                .execute_in_transaction(&txn, &db, &cx, &params, allowance)
                .unwrap_err();
            let QueryError::TransactionSet(inner) = &error else {
                panic!("{expected}: {error:?}");
            };
            let admitted = match (expected, inner.as_ref()) {
                ("snapshot", GqlQueryError::Rows(limit)) => {
                    limit.dimension == GqlBudgetDimension::SnapshotRecords
                }
                ("work", GqlQueryError::Evaluator(limit)) => {
                    limit.dimension == GlaLimitDimension::WorkUnits
                }
                ("scratch", GqlQueryError::Evaluator(limit)) => {
                    limit.dimension == GlaLimitDimension::ScratchEntries
                }
                _ => false,
            };
            assert!(admitted, "{expected}: {error:?}");
            assert!(txn.vertex(&db, VId(901)).unwrap().is_some());
        }

        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        assert_eq!(
            rows(
                prepared
                    .execute_in_transaction(
                        &txn,
                        &db,
                        &cx.with_checkpoint_probe(Arc::clone(&probe)),
                        &params,
                        policy(),
                    )
                    .unwrap()
            ),
            after_delete
        );
        assert!(probe.calls() > 2);
        for stop in [1, probe.calls() / 2, probe.calls()] {
            let stopped = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let error = prepared
                .execute_in_transaction(
                    &txn,
                    &db,
                    &cx.with_checkpoint_probe(Arc::clone(&stopped)),
                    &params,
                    policy(),
                )
                .unwrap_err();
            assert!(
                matches!(&error, QueryError::TransactionSet(inner)
                if matches!(inner.as_ref(), GqlQueryError::Interrupted(_))),
                "checkpoint {stop}: {error:?}"
            );
            assert_eq!(stopped.calls(), stop);
            assert!(txn.vertex(&db, VId(901)).unwrap().is_some());
            assert_eq!(db.frontier().unwrap(), winner);
        }
        assert_eq!(
            rows(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            after_delete
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(901)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
