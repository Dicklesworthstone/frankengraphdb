//! Multipart summaries retain the transaction or capability boundary for
//! EVERY graph leaf. These tests intentionally distinguish native routing
//! failures from executor refusals and from privileged/live-source fallback.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const N: PropertyKeyId = PropertyKeyId(1);
const BRANCH: &str = "main";
const INNER: &str = "MATCH (a)-[:R]->(b) WITH a, b \
    MATCH (b)-[:S]->(item) WITH item.n AS amount \
    RETURN COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present";
const OPTIONAL: &str = "MATCH (a)-[:R]->(b) WITH a, b \
    OPTIONAL MATCH (b)-[:S]->(item) WITH item.n AS amount \
    RETURN COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 100_000)
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x71; 32], NS, [0x72; 32]))
        .await
        .unwrap();
    let mut first = WriteBatch::new(R);
    for (id, label, amount) in [(1, 1, 0), (2, 1, 10), (3, 1, 7), (4, 99, 11), (5, 1, 0)] {
        let props = if amount == 0 {
            vec![]
        } else {
            vec![(N, CanonicalScalar::Int(amount))]
        };
        first.create_vertex(VId(id), vec![LabelId(label)], props);
    }
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    first.add_edge(EId(11), VId(5), VId(5), vec![]);
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(2), VId(3), vec![]);
    second.add_edge(EId(21), VId(2), VId(4), vec![]);
    db.write_atomic(cx, vec![first, second]).await.unwrap();
    db
}

fn summary(result: &QueryResult) -> (u64, Option<i128>, u64) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected read rows");
    };
    assert_eq!(columns, &["paths", "total", "present"]);
    assert_eq!(rows.len(), 1);
    (
        rows[0][0].as_count().unwrap(),
        rows[0][1].as_integer(),
        rows[0][2].as_count().unwrap(),
    )
}

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xa681), NS, "graph", SchemaEpoch(0), 1).unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1_000,
        QueryLimits {
            max_nodes: 1_000,
            max_work: 1_000_000,
            max_rows: 1_000,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([R, S]);
    grant.properties = Scope::only([N]);
    grant
}

#[test]
fn carried_graph_values_aggregate_inside_native_snapshot_and_authority_boundaries() {
    let ((), report) = run_async_under_lab(0xa681_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        let mut extra = WriteBatch::new(S);
        extra.add_edge(EId(22), VId(1), VId(4), vec![]);
        extra.add_edge(EId(23), VId(2), VId(3), vec![]);
        db.write(&commit, extra).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let text = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
            RETURN EXISTS((x)-[:S]->()) AS has_s,count(*) AS vertices, \
            sum(COUNT { MATCH (x)-[:S]->() }) AS edges ORDER BY has_s";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let values = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected aggregate rows");
            };
            assert_eq!(columns, ["has_s", "vertices", "edges"]);
            rows.into_iter()
                .map(|row| {
                    let Some(CanonicalScalar::Bool(connected)) =
                        row[0].as_value().and_then(|value| value.as_scalar())
                    else {
                        panic!("expected Boolean group key");
                    };
                    (
                        *connected,
                        row[1].as_count().expect("outer occurrence count"),
                        row[2].as_integer().expect("exact pattern-count sum"),
                    )
                })
                .collect::<Vec<_>>()
        };
        // EXISTS supplies one Boolean per vertex; COUNT preserves every
        // parallel witness. Hidden endpoint filtering changes both together.
        let before = vec![(false, 3, 0), (true, 2, 4)];
        assert_eq!(
            values(db.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        let issuer = authority();
        for (allowed, expected) in [
            (grant(), vec![(false, 3, 0), (true, 1, 2)]),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([R]);
                    restricted
                },
                vec![(false, 4, 0)],
            ),
        ] {
            let token = issuer.issue_at(&allowed, 100).unwrap();
            for result in [
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap(),
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
                    .unwrap(),
            ] {
                assert_eq!(values(result), expected);
            }
        }
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(S);
        staged.delete_edge(EId(20));
        staged.delete_edge(EId(23));
        staged.add_edge(EId(30), VId(5), VId(3), vec![]);
        txn.write(&mut db, staged).unwrap();
        let after = vec![(false, 2, 0), (true, 3, 3)];
        assert_eq!(
            values(
                txn.query(&db, &cx, text, &params, symbols, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(
            values(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            after
        );
        assert_eq!(
            values(pinned.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn carried_pattern_lists_preserve_native_order_nulls_and_snapshot_authority() {
    let ((), report) = run_async_under_lab(0xa681_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        let mut extra = WriteBatch::new(S);
        extra.add_edge(EId(22), VId(1), VId(4), vec![]);
        extra.add_edge(EId(23), VId(2), VId(3), vec![]);
        db.write(&commit, extra).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let tail = "RETURN x,[(x)-[:S]->(target) | target.n] AS amounts, \
                    [(x)-[:S]->() | null] AS missing, \
                    [p=(x)-[:S]->(target) | size(relationships(p))] AS lengths ORDER BY x";
        let text = format!("MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x {tail}");
        let direct = format!("MATCH (x) {tail}");
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
        let values = |result: &QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected list-valued rows");
            };
            assert_eq!(columns, &["x", "amounts", "missing", "lengths"]);
            rows.iter()
                .map(|row| {
                    let Some(fgdb_gql::algebra::GraphValue::Vertex(id)) = row[0].as_value() else {
                        panic!("expected the carried vertex");
                    };
                    let amounts = row[1].as_value().unwrap().as_list().unwrap();
                    let missing = row[2].as_value().unwrap().as_list().unwrap();
                    let lengths = row[3].as_value().unwrap().as_list().unwrap();
                    assert_eq!(missing.len(), amounts.len());
                    assert_eq!(lengths.len(), amounts.len());
                    assert!(missing.iter().all(|value| value.is_null()));
                    assert!(lengths.iter().all(|value| {
                        matches!(value.as_scalar(), Some(CanonicalScalar::Int(1)))
                    }));
                    let mut amounts = amounts
                        .iter()
                        .map(|value| match value.as_scalar() {
                            Some(CanonicalScalar::Int(amount)) => Some(*amount),
                            Some(CanonicalScalar::Null) => None,
                            _ => panic!("expected a scalar property or NULL"),
                        })
                        .collect::<Vec<_>>();
                    amounts.sort();
                    (id.0, amounts)
                })
                .collect::<Vec<_>>()
        };
        let before = vec![
            (1, vec![Some(11)]),
            (2, vec![Some(7), Some(7), Some(11)]),
            (3, vec![]),
            (4, vec![]),
            (5, vec![]),
        ];
        let first = db.query(&cx, &text, &params, symbols, policy()).unwrap();
        assert_eq!(values(&first), before);
        // The existing graph-head collector is also an order oracle: a join
        // must not sort the contents of the already-collected witness list.
        assert_eq!(
            first,
            db.query(&cx, &direct, &params, symbols, policy()).unwrap()
        );
        assert_eq!(
            prepared.execute(&db, &cx, &params, policy()).unwrap(),
            first
        );
        let issuer = authority();
        for (allowed, expected) in [
            (
                grant(),
                vec![
                    (1, vec![]),
                    (2, vec![Some(7), Some(7)]),
                    (3, vec![]),
                    (5, vec![]),
                ],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.properties = Scope::only([]);
                    restricted
                },
                vec![(1, vec![]), (2, vec![None, None]), (3, vec![]), (5, vec![])],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([R]);
                    restricted
                },
                vec![(1, vec![]), (2, vec![]), (3, vec![]), (5, vec![])],
            ),
        ] {
            let token = issuer.issue_at(&allowed, 100).unwrap();
            let direct = db
                .query_authorized(
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
                .unwrap();
            let bound = prepared
                .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &params, policy(), || 100)
                .unwrap();
            assert_eq!(values(&direct), expected);
            assert_eq!(direct, bound);
        }
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(S);
        staged.delete_edge(EId(20));
        staged.delete_edge(EId(23));
        staged.add_edge(EId(30), VId(5), VId(3), vec![]);
        staged.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(13)));
        txn.write(&mut db, staged).unwrap();
        let after = vec![
            (1, vec![Some(11)]),
            (2, vec![Some(11)]),
            (3, vec![]),
            (4, vec![]),
            (5, vec![Some(13)]),
        ];
        let staged_result = txn
            .query(&db, &cx, &text, &params, symbols, policy())
            .unwrap();
        assert_eq!(values(&staged_result), after);
        assert_eq!(
            prepared
                .execute_in_transaction(&txn, &db, &cx, &params, policy())
                .unwrap(),
            staged_result
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(
            prepared.execute(&db, &cx, &params, policy()).unwrap(),
            first
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(&prepared.execute(&db, &cx, &params, policy()).unwrap()),
            after
        );
        assert_eq!(
            pinned
                .query(&cx, &text, &params, symbols, policy())
                .unwrap(),
            first
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multipart_transaction_reads_staged_second_source_without_committing_or_falling_back() {
    let ((), report) = run_async_under_lab(0xa681_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = database(&commit).await;
        let foreign = database(&commit).await;
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(INNER, &params, symbols).unwrap();
        let basis = db.frontier().unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut staged = WriteBatch::new(S);
        staged.delete_edge(EId(21));
        staged.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(13)));
        txn.write(&mut db, staged).unwrap();
        assert_eq!(
            summary(
                &txn.query(&db, &cx, INNER, &params, symbols, policy())
                    .unwrap()
            ),
            (1, Some(13), 1)
        );
        assert_eq!(
            summary(&prepared.execute(&db, &cx, &params, policy()).unwrap()),
            (2, Some(18), 2)
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(matches!(
            prepared.execute_in_transaction(&txn, &foreign, &cx, &params, policy()),
            Err(QueryError::Transaction(error)) if matches!(*error, WriteTxnError::WrongDatabase)
        ));
        assert!(matches!(
            prepared.execute_in_transaction(
                &txn,
                &db,
                &cx,
                &params,
                GqlQueryPolicy::new(1_000, 1_000, 0, 100_000),
            ),
            Err(QueryError::TransactionAggregate(_))
        ));
        assert_eq!(
            summary(
                &prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            (1, Some(13), 1)
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            summary(&prepared.execute(&db, &cx, &params, policy()).unwrap()),
            (1, Some(13), 1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multipart_transaction_retains_second_source_observations_even_after_having_removes_rows() {
    let ((), report) = run_async_under_lab(0xa681_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        for filtered in [false, true] {
            let mut db = database(&commit).await;
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(99)));
            txn.write(&mut db, staged).unwrap();
            let text = if filtered {
                format!("{INNER} HAVING paths > 99")
            } else {
                INNER.to_owned()
            };
            let result = txn
                .query(&db, &cx, &text, &GqlParameters::new(), symbols, policy())
                .unwrap();
            let QueryResult::Rows { rows, .. } = result else {
                panic!("expected read rows");
            };
            assert_eq!(rows.len(), usize::from(!filtered));
            // No write/write overlap: the competing write is to a value read
            // only through the second MATCH, not the transaction's write target.
            let mut competing = WriteBatch::new(S);
            competing.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(17)));
            db.write_atomic(&commit, vec![competing]).await.unwrap();
            assert!(matches!(
                txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multipart_authorized_reads_mask_every_source_and_recheck_reused_templates() {
    let ((), report) = run_async_under_lab(0xa681_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(OPTIONAL, &params, symbols).unwrap();
        assert_eq!(
            summary(&prepared.execute(&db, &cx, &params, policy()).unwrap()),
            (3, Some(18), 2)
        );
        let mut no_second_relation = grant();
        no_second_relation.relations = Scope::only([R]);
        let mut no_property = grant();
        no_property.properties = Scope::only([]);
        for (grant, expected) in [
            (grant(), (2, Some(7), 1)),
            (no_second_relation, (2, None, 0)),
            (no_property, (2, None, 0)),
        ] {
            let token = issuer.issue_at(&grant, 100).unwrap();
            let result = db
                .query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    OPTIONAL,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap();
            assert_eq!(summary(&result), expected);
            assert_eq!(
                prepared
                    .execute_authorized(
                        &db,
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &params,
                        policy(),
                        || 100
                    )
                    .unwrap(),
                result
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_row_existence_uses_native_snapshot_and_capability_boundaries() {
    let ((), report) = run_async_under_lab(0xa681_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = database(&commit).await;
        let pinned = db.read_session().unwrap();
        let text = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
            WHERE (x.n IS NULL AND EXISTS((x)-[:R]->())) \
               OR CASE WHEN EXISTS((x)-[:S]->()) THEN x.n = 10 ELSE false END \
            RETURN x ORDER BY x";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let identities = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected read rows");
            };
            assert_eq!(columns, ["x"]);
            rows.into_iter()
                .map(|row| match row[0].as_value() {
                    Some(fgdb_gql::algebra::GraphValue::Vertex(id)) => *id,
                    other => panic!("expected a vertex identity: {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        let before = vec![VId(1), VId(2), VId(5)];
        assert_eq!(
            identities(db.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(
            identities(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        let issuer = authority();
        for (allowed, expected) in [
            (grant(), before.clone()),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([R]);
                    restricted
                },
                vec![VId(1), VId(5)],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.properties = Scope::only([]);
                    restricted
                },
                vec![VId(1), VId(5)],
            ),
        ] {
            let token = issuer.issue_at(&allowed, 100).unwrap();
            assert_eq!(
                identities(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        text,
                        &params,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
                ),
                expected
            );
        }
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(10));
        db.write_atomic(&commit, vec![change]).await.unwrap();
        assert_eq!(
            identities(db.query(&cx, text, &params, symbols, policy()).unwrap()),
            vec![VId(2), VId(5)]
        );
        assert_eq!(
            identities(pinned.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert!(
            db.query(
                &cx,
                text,
                &params,
                symbols,
                GqlQueryPolicy::new(1_000, 1_000, 0, 100_000),
            )
            .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_optional_conditions_share_native_snapshot_transaction_and_authorization_boundaries() {
    let ((), report) = run_async_under_lab(0xa681_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = database(&commit).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let text = "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.n = a.n + 3 \
            RETURN a.n AS original,b.n AS matched ORDER BY original NULLS FIRST,matched";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let values = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected read rows");
            };
            assert_eq!(columns, &["original", "matched"]);
            let scalar = |value: &fgdb::QueryValue| match value
                .as_value()
                .and_then(|value| value.as_scalar())
            {
                Some(CanonicalScalar::Int(n)) => Some(i128::from(*n)),
                Some(CanonicalScalar::Null) => None,
                other => panic!("unexpected fixture scalar: {other:?}"),
            };
            rows.into_iter()
                .map(|row| (scalar(&row[0]), scalar(&row[1])))
                .collect::<Vec<_>>()
        };
        let old = vec![
            (None, None),
            (None, None),
            (Some(7), Some(10)),
            (Some(10), None),
            (Some(11), None),
        ];
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            old
        );
        let issuer = authority();
        for (grant, expected) in [
            (
                grant(),
                vec![
                    (None, None),
                    (None, None),
                    (Some(7), Some(10)),
                    (Some(10), None),
                ],
            ),
            (
                {
                    let mut g = grant();
                    g.properties = Scope::only([]);
                    g
                },
                vec![(None, None); 4],
            ),
        ] {
            let token = issuer.issue_at(&grant, 100).unwrap();
            assert_eq!(
                values(
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
                expected
            );
        }
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut changed = WriteBatch::new(R);
        changed.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(13)));
        txn.write(&mut db, changed).unwrap();
        let new = vec![
            (None, None),
            (None, None),
            (Some(10), Some(13)),
            (Some(11), None),
            (Some(13), None),
        ];
        assert_eq!(
            values(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            new
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            old
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            new
        );
        let bound = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&params)
            .unwrap();
        let scalars = |rows: Vec<fgdb_gql::algebra::GraphValueRow>| {
            rows.into_iter()
                .map(|row| {
                    let value = |at| match row.get(at).unwrap().as_scalar().unwrap() {
                        CanonicalScalar::Int(n) => Some(i128::from(*n)),
                        CanonicalScalar::Null => None,
                        other => panic!("unexpected fixture scalar: {other:?}"),
                    };
                    (value(0), value(1))
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            scalars(
                db.execute_graph_set_governed_at(&cx, &bound, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            scalars(
                pinned
                    .execute_graph_set_governed(&cx, &bound, policy())
                    .unwrap()
                    .value
            ),
            old
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Authorized sessions are single-task by design (!Send;
// fgdb-authorized-cursor-send-qbfn1). This law holds one across a write's
// .await, so it runs on the real runtime's block_on, which accepts a non-Send
// future, instead of the Send-only lab runner. The body is unchanged.
#[test]
fn multipart_authorized_session_pins_sources_but_not_credential_lifetime() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    runtime.block_on(async {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let now = Cell::new(100_u64);
        let params = GqlParameters::new();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || {
                now.get()
            })
            .unwrap();
        let prepared = session.prepare(&cx, OPTIONAL, &params).unwrap();
        assert_eq!(
            summary(&session.query(&cx, OPTIONAL, &params).unwrap()),
            (2, Some(7), 1)
        );
        let mut change = WriteBatch::new(S);
        change.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(23)));
        db.write_atomic(&commit, vec![change]).await.unwrap();
        assert_eq!(
            summary(
                &db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    OPTIONAL,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap()
            ),
            (2, Some(23), 1)
        );
        drop(db);
        assert_eq!(
            summary(&session.execute(&cx, &prepared, &params).unwrap()),
            (2, Some(7), 1)
        );
        now.set(1_000);
        assert!(matches!(
            session.execute(&cx, &prepared, &params),
            Err(QueryError::Authorization(Error::Expired))
        ));
        assert!(session.is_closed());
    });
}

#[test]
fn source_free_and_single_source_native_aggregate_lanes_remain_usable() {
    let ((), report) = run_async_under_lab(0xa681_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let txn = db.begin(&contexts.txn()).unwrap();
        let view = db.read_session().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        for (prefix, sources, expected) in [
            ("WITH 7 AS amount", 0, (1, Some(7), 1)),
            (
                "MATCH (a)-[:R]->(b) WITH b.n AS amount",
                1,
                (2, Some(10), 1),
            ),
        ] {
            let text = format!(
                "{prefix} RETURN COUNT(*) AS paths, SUM(amount) AS total, COUNT(amount) AS present"
            );
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            let PreparedNativeRead::PipelineAggregate(plan) = &prepared else {
                panic!("expected pipeline aggregate");
            };
            assert_eq!(plan.graph_source_count(), sources);
            assert_eq!(plan.requires_relational_input(), sources != 1);
            for result in [
                prepared.execute(&db, &cx, &params, policy()).unwrap(),
                prepared
                    .execute_in_view(&view, &cx, &params, policy())
                    .unwrap(),
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap(),
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
                    .unwrap(),
            ] {
                assert_eq!(summary(&result), expected);
            }
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_row_existence_keeps_snapshot_transaction_and_capability_boundaries() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError};

    let ((), report) = run_async_under_lab(0xa681_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        // This source has an S edge only to a hidden endpoint. Raw reads
        // observe it; the signed EXISTS must report false.
        let mut hidden_only = WriteBatch::new(S);
        hidden_only.add_edge(EId(22), VId(1), VId(4), vec![]);
        db.write(&commit, hidden_only).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let text = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
            RETURN x, EXISTS((x)-[:R]->()) AS has_r, \
            EXISTS((x)-[:S]->()) AS has_s ORDER BY x";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let values = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected read rows");
            };
            assert_eq!(columns, ["x", "has_r", "has_s"]);
            rows.into_iter()
                .map(|row| {
                    let vertex = match row[0].as_value() {
                        Some(GraphValue::Vertex(vertex)) => *vertex,
                        other => panic!("expected a carried vertex: {other:?}"),
                    };
                    let boolean =
                        |at: usize| match row[at].as_value().and_then(|value| value.as_scalar()) {
                            Some(CanonicalScalar::Bool(value)) => *value,
                            other => panic!("expected an EXISTS boolean: {other:?}"),
                        };
                    (vertex, boolean(1), boolean(2))
                })
                .collect::<Vec<_>>()
        };
        let before = vec![
            (VId(1), true, true),
            (VId(2), false, true),
            (VId(3), false, false),
            (VId(4), false, false),
            (VId(5), true, false),
        ];
        assert_eq!(
            values(db.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        let issuer = authority();
        for (allowed, expected) in [
            (
                grant(),
                vec![
                    (VId(1), true, false),
                    (VId(2), false, true),
                    (VId(3), false, false),
                    (VId(5), true, false),
                ],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([R]);
                    restricted
                },
                vec![
                    (VId(1), true, false),
                    (VId(2), false, false),
                    (VId(3), false, false),
                    (VId(5), true, false),
                ],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([S]);
                    restricted
                },
                vec![
                    (VId(1), false, false),
                    (VId(2), false, true),
                    (VId(3), false, false),
                    (VId(5), false, false),
                ],
            ),
        ] {
            let token = issuer.issue_at(&allowed, 100).unwrap();
            assert_eq!(
                values(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        text,
                        &params,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
                ),
                expected
            );
            assert_eq!(
                values(
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
                expected
            );
        }
        // Preparation must reach the governed native owner, including its
        // source and evaluator refusals, instead of a separate EXISTS engine.
        assert!(matches!(
            prepared.execute(
                &db,
                &cx,
                &params,
                GqlQueryPolicy::new(0, 1_000, 1_000_000, 100_000),
            ),
            Err(QueryError::Set(GqlQueryError::Rows(limit)))
                if limit.dimension == GqlBudgetDimension::SnapshotRecords
        ));
        assert!(matches!(
            prepared.execute(
                &db,
                &cx,
                &params,
                GqlQueryPolicy::new(1_000, 1_000, 0, 100_000),
            ),
            Err(QueryError::Set(GqlQueryError::Evaluator(limit)))
                if limit.dimension == GlaLimitDimension::WorkUnits
        ));
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(R);
        staged.delete_edge(EId(10));
        staged.add_edge(EId(30), VId(3), VId(5), vec![]);
        txn.write(&mut db, staged).unwrap();
        let after = vec![
            (VId(1), false, true),
            (VId(2), false, true),
            (VId(3), true, false),
            (VId(4), false, false),
            (VId(5), true, false),
        ];
        assert_eq!(
            values(
                txn.query(&db, &cx, text, &params, symbols, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(
            values(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            after
        );
        assert_eq!(
            values(pinned.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn carried_pattern_counts_keep_multiplicity_snapshot_and_authorization() {
    use fgdb_gql::algebra::GraphValue;

    let ((), report) = run_async_under_lab(0xa681_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        // Parallel relationships must count independently; an authorized
        // count must omit the relationship whose endpoint is hidden.
        let mut parallel = WriteBatch::new(R);
        parallel.add_edge(EId(12), VId(1), VId(2), vec![]);
        let mut hidden = WriteBatch::new(S);
        hidden.add_edge(EId(22), VId(1), VId(4), vec![]);
        db.write_atomic(&commit, vec![parallel, hidden])
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let text = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
            RETURN x, COUNT { (x)-[:R]->() } AS r_count, \
            COUNT { (x)-[:S]->() } AS s_count ORDER BY x";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let values = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected read rows");
            };
            assert_eq!(columns, ["x", "r_count", "s_count"]);
            rows.into_iter()
                .map(|row| {
                    let Some(GraphValue::Vertex(vertex)) = row[0].as_value() else {
                        panic!("expected a carried vertex");
                    };
                    let count = |column: usize| match row[column]
                        .as_value()
                        .and_then(|value| value.as_scalar())
                    {
                        Some(CanonicalScalar::Int(value)) => *value,
                        other => panic!("expected a pattern-count integer: {other:?}"),
                    };
                    (*vertex, count(1), count(2))
                })
                .collect::<Vec<_>>()
        };
        let before = vec![
            (VId(1), 2, 1),
            (VId(2), 0, 2),
            (VId(3), 0, 0),
            (VId(4), 0, 0),
            (VId(5), 1, 0),
        ];
        assert_eq!(
            values(db.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        // The old graph-head count is an independent preparation route to
        // the same logical answer and scalar output domain.
        assert_eq!(
            values(
                db.query(
                    &cx,
                    "MATCH (x) RETURN x, COUNT { (x)-[:R]->() } AS r_count, \
                     COUNT { (x)-[:S]->() } AS s_count ORDER BY x",
                    &params,
                    symbols,
                    policy(),
                )
                .unwrap()
            ),
            before
        );
        let issuer = authority();
        for (allowed, expected) in [
            (
                grant(),
                vec![
                    (VId(1), 2, 0),
                    (VId(2), 0, 1),
                    (VId(3), 0, 0),
                    (VId(5), 1, 0),
                ],
            ),
            (
                {
                    let mut restricted = grant();
                    restricted.relations = Scope::only([R]);
                    restricted
                },
                vec![
                    (VId(1), 2, 0),
                    (VId(2), 0, 0),
                    (VId(3), 0, 0),
                    (VId(5), 1, 0),
                ],
            ),
        ] {
            let token = issuer.issue_at(&allowed, 100).unwrap();
            for result in [
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap(),
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
                    .unwrap(),
            ] {
                assert_eq!(values(result), expected);
            }
        }
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged_r = WriteBatch::new(R);
        staged_r.delete_edge(EId(10));
        txn.write(&mut db, staged_r).unwrap();
        let mut staged_s = WriteBatch::new(S);
        staged_s.add_edge(EId(23), VId(5), VId(3), vec![]);
        txn.write(&mut db, staged_s).unwrap();
        let after = vec![
            (VId(1), 1, 1),
            (VId(2), 0, 2),
            (VId(3), 0, 0),
            (VId(4), 0, 0),
            (VId(5), 1, 1),
        ];
        assert_eq!(
            values(
                txn.query(&db, &cx, text, &params, symbols, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(
            values(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(prepared.execute(&db, &cx, &params, policy()).unwrap()),
            after
        );
        assert_eq!(
            values(pinned.query(&cx, text, &params, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_exists_uses_the_pinned_staged_and_authorized_body_source() {
    use fgdb_gql::algebra::GraphValue;

    let ((), report) = run_async_under_lab(0xa681_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        let mut hidden_only = WriteBatch::new(S);
        hidden_only.add_edge(EId(22), VId(1), VId(4), vec![]);
        db.write(&commit, hidden_only).await.unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let text = "MATCH (n) WHERE EXISTS { MATCH (n)-[:S]->(m) \
                    WITH count(*) AS hits WHERE hits >= $minimum RETURN false } \
                    RETURN n ORDER BY n";
        let parameters = |minimum| GqlParameters::new().with_int64("minimum", minimum).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &parameters(1), symbols).unwrap();
        let vertices = |result: QueryResult| {
            let QueryResult::Rows { columns, rows } = result else {
                panic!("expected read rows");
            };
            assert_eq!(columns, ["n"]);
            rows.into_iter()
                .map(|row| match row[0].as_value() {
                    Some(GraphValue::Vertex(id)) => *id,
                    other => panic!("expected an outer vertex: {other:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            vertices(
                db.query(&cx, text, &parameters(1), symbols, policy())
                    .unwrap()
            ),
            [VId(1), VId(2)]
        );
        assert_eq!(
            vertices(
                prepared
                    .execute(&db, &cx, &parameters(2), policy())
                    .unwrap()
            ),
            [VId(2)]
        );
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        // Hidden endpoints are absent before grouping. Vertex 1 has only a
        // hidden target; vertex 2 has one visible and one hidden target.
        for (minimum, expected) in [(1, vec![VId(2)]), (2, vec![])] {
            assert_eq!(
                vertices(
                    prepared
                        .execute_authorized(
                            &db,
                            &cx,
                            &issuer,
                            &token,
                            BRANCH,
                            &parameters(minimum),
                            policy(),
                            || 100,
                        )
                        .unwrap()
                ),
                expected
            );
        }
        let mut relation_mask = grant();
        relation_mask.relations = Scope::only([R]);
        let masked = issuer.issue_at(&relation_mask, 100).unwrap();
        assert!(
            vertices(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &masked,
                    BRANCH,
                    text,
                    &parameters(1),
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap()
            )
            .is_empty()
        );

        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(S);
        staged.delete_edge(EId(21));
        staged.add_edge(EId(30), VId(3), VId(5), vec![]);
        txn.write(&mut db, staged).unwrap();
        assert_eq!(
            vertices(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &parameters(1), policy(),)
                    .unwrap()
            ),
            [VId(1), VId(2), VId(3)]
        );
        assert!(
            vertices(
                txn.query(&db, &cx, text, &parameters(2), symbols, policy(),)
                    .unwrap()
            )
            .is_empty()
        );
        assert_eq!(
            vertices(
                prepared
                    .execute(&db, &cx, &parameters(2), policy())
                    .unwrap()
            ),
            [VId(2)]
        );
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            vertices(
                prepared
                    .execute(&db, &cx, &parameters(1), policy())
                    .unwrap()
            ),
            [VId(1), VId(2), VId(3)]
        );
        assert!(
            vertices(
                prepared
                    .execute(&db, &cx, &parameters(2), policy())
                    .unwrap()
            )
            .is_empty()
        );
        assert_eq!(
            vertices(
                pinned
                    .query(&cx, text, &parameters(2), symbols, policy())
                    .unwrap()
            ),
            [VId(2)]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
