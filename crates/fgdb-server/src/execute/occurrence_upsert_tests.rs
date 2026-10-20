//! The real shared server dispatcher, native authorization and Chronicle.
//! These are execution-boundary tests, not TCP framing tests.

use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::WriteBatch;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbolKind, GraphWriteProgramPolicy};
use fgdb_protocol::body::ExecuteMode;
use fgdb_types::{CanonicalScalar, EId, VId};
use fgdb_warden::{Grant, QueryLimits, Restriction, Rights, Scope};
use std::sync::Arc;

const E1: EId = EId(u128::MAX - 2);
const E2: EId = EId(u128::MAX - 1);
const E3: EId = EId(u128::MAX);
const VALUE: PropertyKeyId = PropertyKeyId(1);
const TEXT: &str = "MATCH (a)-[:Feed]->(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.value=r.value+$inc";

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

async fn fixture(cx: &Cx, name: &str, actions: u64) -> (crate::Server, CapabilityToken) {
    let (mut server, _, _) = crate::recovery::tests::served(cx, name).await;
    {
        let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        for (kind, name, id) in [
            (GraphSymbolKind::Relation, "Feed", 1),
            (GraphSymbolKind::Relation, "R", 2),
            (GraphSymbolKind::Property, "value", 1),
        ] {
            db.symbols.bind(kind, name, id).unwrap();
        }
        db.write_policy = GraphWriteProgramPolicy::new(db.query_policy, actions, 100, 100);
    }
    let db = &server.databases["test"];
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let mut guard = db.db.write(cx).await.unwrap();
    let mut feed = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        feed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
    }
    feed.create_vertex(VId(9), vec![LabelId(2)], vec![]);
    for (edge, src, dst) in [(101, 1, 2), (102, 1, 2), (103, 3, 4), (104, 9, 2)] {
        feed.add_edge(EId(edge), VId(src), VId(dst), vec![]);
    }
    guard.write(&contexts.commit(), feed).await.unwrap();
    let mut targets = WriteBatch::new(RelationId(2));
    for (edge, src, dst, value) in [
        (E1, 1, 2, 10),
        (E2, 1, 2, 20),
        (E3, 3, 4, 30),
        (EId(900), 9, 2, 777),
    ] {
        targets.add_edge(
            edge,
            VId(src),
            VId(dst),
            vec![(VALUE, CanonicalScalar::Int(value))],
        );
    }
    guard.write(&contexts.commit(), targets).await.unwrap();
    drop(guard);
    let token = db
        .authority
        .issue_at(
            &Grant {
                branch: TRUNK.into(),
                labels: Scope::only([LabelId(1)]),
                relations: Scope::All,
                properties: Scope::All,
                rights: Rights::ReadWrite,
                limits: QueryLimits {
                    max_nodes: 100_000,
                    max_work: 10_000_000,
                    max_rows: 100,
                },
                expires_at_ms: u64::MAX / 2,
            },
            unix_millis(),
        )
        .unwrap();
    (server, token)
}

fn request(suffix: &str, increment: i64) -> Execute {
    Execute {
        mode: ExecuteMode::Write,
        statement: format!("{TEXT} {suffix}"),
        parameters: vec![("inc".into(), WireValue::Int(increment))],
    }
}

#[test]
fn server_projects_all_occurrences_and_rebinds_under_one_output_row() {
    let ((), report) = run_async_under_lab(0x6520, |root| async move {
        let (server, token) = fixture(&root, "upsert-occurrence-projection", 100).await;
        let db = &server.databases["test"];
        let token = token.attenuate(Restriction::MaxRows(1)).unwrap();
        let start = db.db.read(&root).await.unwrap().frontier().unwrap();
        for (increment, expected) in [(1, 31), (2, 33)] {
            let answer = accepted(
                write(
                    &root,
                    db,
                    &token,
                    &request("RETURN max(r.value) AS value", increment),
                )
                .await,
            );
            assert_eq!(answer.columns, ["value"]);
            assert_eq!(answer.rows, vec![vec![WireValue::Int(expected)]]);
            assert!(matches!(
                answer.outcome,
                Outcome::WriteCommitted { statements: 1, .. }
            ));
        }
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.frontier().unwrap().0, start.0 + 2);
        for (edge, expected) in [(E1, 16), (E2, 26), (E3, 33), (EId(900), 777)] {
            assert_eq!(
                guard.edge(edge).unwrap().unwrap().props,
                vec![(VALUE, CanonicalScalar::Int(expected).into())]
            );
        }
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn server_refusals_preserve_state_and_zero_output_does_not_suppress_actions() {
    let ((), report) = run_async_under_lab(0x6521, |root| async move {
        for (name, actions, rows, suffix, fails) in [
            (
                "upsert-host-cap",
                4,
                100,
                "RETURN max(r.value) AS value",
                true,
            ),
            ("upsert-signed-cap", 100, 4, "RETURN r.value AS value", true),
            (
                "upsert-late-expression",
                100,
                0,
                "RETURN 1/(r.value-31) AS value LIMIT 0",
                true,
            ),
            (
                "upsert-zero-output",
                100,
                0,
                "RETURN r.value AS value LIMIT 0",
                false,
            ),
            ("upsert-rowless", 100, 0, "", false),
        ] {
            let (server, token) = fixture(&root, name, actions).await;
            let db = &server.databases["test"];
            let token = token.attenuate(Restriction::MaxRows(rows)).unwrap();
            let guard = db.db.read(&root).await.unwrap();
            let before = (
                guard.frontier().unwrap(),
                guard.vertices().unwrap(),
                guard.edges().unwrap(),
            );
            drop(guard);
            let result = write(&root, db, &token, &request(suffix, 1)).await;
            if fails {
                assert!(result.is_err(), "{name}");
            } else {
                let answer = accepted(result);
                assert!(answer.rows.is_empty());
                assert!(matches!(
                    answer.outcome,
                    Outcome::WriteCommitted { statements: 1, .. }
                ));
            }
            let guard = db.db.read(&root).await.unwrap();
            if fails {
                assert_eq!(
                    (
                        guard.frontier().unwrap(),
                        guard.vertices().unwrap(),
                        guard.edges().unwrap()
                    ),
                    before
                );
            } else {
                assert_eq!(guard.frontier().unwrap().0, before.0.0 + 1);
                for (edge, expected) in [(E1, 12), (E2, 22), (E3, 31), (EId(900), 777)] {
                    assert_eq!(
                        guard.edge(edge).unwrap().unwrap().props,
                        vec![(VALUE, CanonicalScalar::Int(expected).into())]
                    );
                }
            }
            drop(guard);
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
