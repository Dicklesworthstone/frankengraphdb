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
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
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
            summary(&txn.query(&db, &cx, INNER, &params, symbols, policy()).unwrap()),
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
                    &cx, &issuer, &token, BRANCH, OPTIONAL, &params, symbols, policy(), || 100,
                )
                .unwrap();
            assert_eq!(summary(&result), expected);
            assert_eq!(
                prepared
                    .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &params, policy(), || 100)
                    .unwrap(),
                result
            );
        }
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
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || now.get())
            .unwrap();
        let prepared = session.prepare(&cx, OPTIONAL, &params).unwrap();
        assert_eq!(summary(&session.query(&cx, OPTIONAL, &params).unwrap()), (2, Some(7), 1));
        let mut change = WriteBatch::new(S);
        change.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(23)));
        db.write_atomic(&commit, vec![change]).await.unwrap();
        assert_eq!(
            summary(
                &db.query_authorized(
                    &cx, &issuer, &token, BRANCH, OPTIONAL, &params, symbols, policy(), || 100,
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
            ("MATCH (a)-[:R]->(b) WITH b.n AS amount", 1, (2, Some(10), 1)),
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
                prepared.execute_in_view(&view, &cx, &params, policy()).unwrap(),
                prepared.execute_in_transaction(&txn, &db, &cx, &params, policy()).unwrap(),
                prepared
                    .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &params, policy(), || 100)
                    .unwrap(),
            ] {
                assert_eq!(summary(&result), expected);
            }
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
