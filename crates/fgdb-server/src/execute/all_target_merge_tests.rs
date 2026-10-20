//! The real shared server dispatcher over Chronicle/UnixVfs, not a mock
//! Returning enum or a privileged engine call. Transport framing is separate.
use super::*;
use crate::recovery::tests::served;
use asupersync::lab::run_async_under_lab;
use fgdb::WriteBatch;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::ExecuteMode;
use fgdb_types::{CanonicalScalar, CommitSeq, VId};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::sync::Arc;

const SECOND: VId = VId((1_u128 << 96) | 1);
const L: LabelId = LabelId(1);
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

async fn fixture(cx: &Cx, name: &str) -> crate::Server {
    let (mut server, _, _) = served(cx, name).await;
    let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
    for (kind, name, id) in [
        (GraphSymbolKind::Label, "Item", 1),
        (GraphSymbolKind::Property, "key", 1),
        (GraphSymbolKind::Property, "value", 2),
    ] {
        db.symbols.bind(kind, name, id).unwrap();
    }
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, label, value) in [(VId(1), L, 10), (SECOND, L, 20), (VId(3), LabelId(2), 90)] {
        batch.create_vertex(
            id,
            vec![label],
            vec![
                (K, CanonicalScalar::Int(1)),
                (P, CanonicalScalar::Int(value)),
            ],
        );
    }
    db.db
        .write(cx)
        .await
        .unwrap()
        .write(&contexts.commit(), batch)
        .await
        .unwrap();
    server
}

fn token(db: &Served, rows: u64) -> CapabilityToken {
    db.authority
        .issue_at(
            &Grant {
                branch: TRUNK.into(),
                labels: Scope::only([L]),
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: rows,
                },
                expires_at_ms: u64::MAX / 2,
            },
            unix_millis(),
        )
        .unwrap()
}

fn request(text: &str) -> Execute {
    Execute {
        mode: ExecuteMode::Write,
        statement: text.into(),
        parameters: Vec::new(),
    }
}

async fn state(cx: &Cx, db: &Served) -> (CommitSeq, Vec<fgdb::VertexRow>) {
    let guard = db.db.read(cx).await.unwrap();
    (guard.frontier().unwrap(), guard.vertices().unwrap())
}

async fn assert_values(cx: &Cx, db: &Served, first: i64, second: i64) {
    let guard = db.db.read(cx).await.unwrap();
    for (id, expected) in [(VId(1), first), (SECOND, second), (VId(3), 90)] {
        assert!(
            guard
                .vertex(id)
                .unwrap()
                .unwrap()
                .props
                .contains(&(P, CanonicalScalar::Int(expected).into()))
        );
    }
}

#[test]
fn server_merge_updates_all_visible_targets_before_one_aggregate_and_rebinds() {
    let ((), report) = run_async_under_lab(0x79a0_0801, |root| async move {
        let server = fixture(&root, "all-target-merge-aggregate").await;
        let db = &server.databases["test"];
        let token = token(db, 1);
        let before = state(&root, db).await.0;
        for (iteration, increment, first, second) in [(1, 1, 11, 21), (2, 5, 16, 26)] {
            let mut statement = request(
                "MERGE (n {key:$key}) ON MATCH SET n.value=n.value+$increment RETURN max(n.value) AS value",
            );
            statement.parameters = vec![
                ("increment".into(), WireValue::Int(increment)),
                ("key".into(), WireValue::Int(1)),
            ];
            let answer = accepted(write(&root, db, &token, &statement).await);
            assert_eq!(answer.columns, vec!["value"]);
            assert_eq!(answer.rows, vec![vec![WireValue::Int(second)]]);
            assert!(
                matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 }
                if seq == before.0 + iteration)
            );
            assert_values(&root, db, first, second).await;
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn server_merge_late_projection_and_signed_row_errors_leave_no_committed_prefix() {
    let ((), report) = run_async_under_lab(0x79a0_0802, |root| async move {
        let server = fixture(&root, "all-target-merge-refusals").await;
        let db = &server.databases["test"];
        let before = state(&root, db).await;
        for (tail, row_limit) in [
            ("RETURN 1/(n.value-21) AS value", 2),
            ("RETURN n.value AS value", 1),
        ] {
            let token = token(db, row_limit);
            let statement = request(&format!(
                "MERGE (n:Item {{key:1}}) SET n.value=n.value+1 {tail}",
            ));
            let error = write(&root, db, &token, &statement)
                .await
                .err()
                .expect("must refuse");
            if row_limit == 1 {
                assert_eq!(error.code, ErrorCode::Budget);
            }
            assert_eq!(state(&root, db).await, before);
        }
        let statement = request(
            "MERGE (n:Item {key:1}) SET n.value=n.value+1 RETURN n.value AS value ORDER BY value",
        );
        let answer = accepted(write(&root, db, &token(db, 2), &statement).await);
        assert_eq!(
            answer.rows,
            vec![vec![WireValue::Int(11)], vec![WireValue::Int(21)]]
        );
        assert!(
            matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 }
            if seq == (before.0).0 + 1)
        );
        assert_values(&root, db, 11, 21).await;
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn server_merge_zero_result_grant_does_not_suppress_effects_or_allow_read_mode_writes() {
    let ((), report) = run_async_under_lab(0x79a0_0803, |root| async move {
        for (name, tail) in [
            (
                "all-target-merge-limit-zero",
                "RETURN n.value AS value LIMIT 0",
            ),
            (
                "all-target-merge-filter-zero",
                "WITH n.value AS value WHERE value>1000 RETURN value",
            ),
        ] {
            let server = fixture(&root, name).await;
            let db = &server.databases["test"];
            let token = token(db, 0);
            let before = state(&root, db).await;
            let mut statement = request(&format!(
                "MERGE (n:Item {{key:1}}) SET n.value=n.value+1 {tail}",
            ));
            statement.mode = ExecuteMode::Read;
            assert!(read(&root, db, &token, &statement).await.is_err());
            assert_eq!(state(&root, db).await, before);
            statement.mode = ExecuteMode::Write;
            let answer = accepted(write(&root, db, &token, &statement).await);
            assert_eq!(answer.columns, vec!["value"]);
            assert!(answer.rows.is_empty());
            assert!(
                matches!(answer.outcome, Outcome::WriteCommitted { seq, statements: 1 }
                if seq == (before.0).0 + 1)
            );
            assert_values(&root, db, 11, 21).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
