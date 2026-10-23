//! RETURN must not replace the operator's write policy with built-in defaults.
//! Real FGP/TCP requests, a configured host, and a separate read client share
//! the ordinary authorized engine. Reopening verifies complete durable state.

use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::{Budget, Cx};
use fgdb::{Database, DatabaseKeys};
use fgdb_gql::{GqlQueryPolicy, GraphSymbolKind, GraphWriteProgramPolicy};
use fgdb_protocol::body::{ErrorCode, ExecuteMode, Outcome, WireValue};
use fgdb_protocol::client::{Answer, Client, ClientError};
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols, issue_token, issuer};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn issuer_key() -> AuthKey {
    AuthKey::from_seed(0x7265_7475_726e)
}

fn policy(effects: u64, vertices: u64, edges: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        effects,
        vertices,
        edges,
    )
}

fn run(test: impl AsyncFnOnce(&Cx)) {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(test(&cx));
}

struct Fixture {
    path: PathBuf,
    address: SocketAddr,
    shutdown: fgdb_server::Shutdown,
    task: asupersync::runtime::TaskHandle<()>,
}

impl Fixture {
    async fn start(cx: &Cx, name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "fgdb-returning-limits-{name}-{}-{time}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        drop(
            Database::create(&contexts.commit(), &path, keys())
                .await
                .unwrap(),
        );
        let fixture = Self::serve(cx, path, policy(100_000, 100_000, 100_000)).await;
        let mut seed = fixture.client(cx, Rights::ReadWrite, 100).await;
        let answer = seed
            .execute(
                cx,
                ExecuteMode::Write,
                "CREATE (a:Person {name:'Ada',age:1}),(b:Person {name:'Bob',age:2}),\
                 (a)-[:KNOWS {age:7}]->(b)",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1,
            }
        );
        seed.close(cx).await.unwrap();
        fixture
    }

    async fn serve(cx: &Cx, path: PathBuf, write_policy: GraphWriteProgramPolicy) -> Self {
        let mut server = Server::new(
            cx,
            ServerLimits {
                max_frame_len: 4096,
                initial_window_bytes: 8192,
                initial_window_rows: 4,
                max_window_bytes: 1 << 20,
                max_window_rows: 1 << 10,
                max_connections: 8,
            },
        )
        .unwrap();
        let mut symbols = Symbols::new();
        for (kind, name, id) in [
            (GraphSymbolKind::Label, "Person", 1),
            (GraphSymbolKind::Property, "name", 1),
            (GraphSymbolKind::Property, "age", 2),
            (GraphSymbolKind::Relation, "KNOWS", 1),
        ] {
            symbols.bind(kind, name, id).unwrap();
        }
        let mut config = DatabaseConfig::new("social", keys(), issuer_key());
        config.symbols = symbols;
        // Neither a generous signed grant nor a permissive READ policy may
        // replace this narrower operator-owned write policy.
        config.write_policy = write_policy;
        server.open_database(cx, &path, config).await.unwrap();
        let shutdown = server.shutdown();
        let server = Arc::new(server);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = cx
            .spawn(move |child| async move {
                server.serve(&child, listener).await.unwrap();
            })
            .unwrap();
        Self {
            path,
            address,
            shutdown,
            task,
        }
    }

    async fn client(&self, cx: &Cx, rights: Rights, max_rows: u64) -> Client {
        let grant = Grant {
            branch: fgdb_server::TRUNK.into(),
            labels: Scope::All,
            relations: Scope::All,
            properties: Scope::All,
            rights,
            limits: QueryLimits {
                max_nodes: 1_000_000,
                max_work: 100_000_000,
                max_rows,
            },
            expires_at_ms: u64::MAX / 2,
        };
        let authority = issuer(issuer_key(), &keys(), 1).unwrap();
        let token = issue_token(&authority, &grant).unwrap();
        let mut client = Client::connect(cx, self.address, token).await.unwrap();
        client.select(cx, "social").await.unwrap();
        client
    }

    // Close clients before replacing the operator policy on the same database.
    async fn reopen(mut self, cx: &Cx, write_policy: GraphWriteProgramPolicy) -> Self {
        self.shutdown.trigger();
        self.task.join(cx).await.unwrap();
        Self::serve(cx, self.path, write_policy).await
    }

    async fn finish(self, cx: &Cx, expected: &(Answer, Answer)) {
        let mut fixture = self.reopen(cx, policy(100_000, 100_000, 100_000)).await;
        let mut observer = fixture.client(cx, Rights::Read, 100).await;
        assert_eq!(
            &state(cx, &mut observer).await,
            expected,
            "durable reopen changed state"
        );
        observer.close(cx).await.unwrap();
        fixture.shutdown.trigger();
        fixture.task.join(cx).await.unwrap();
    }
}

async fn state(cx: &Cx, client: &mut Client) -> (Answer, Answer) {
    let vertices = client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (n:Person) RETURN n,n.name AS name,n.age AS age ORDER BY name",
            vec![],
        )
        .await
        .unwrap();
    let edges = client
        .execute(
            cx,
            ExecuteMode::Read,
            "MATCH (a:Person)-[r:KNOWS]->(b:Person) \
             RETURN r,a.name AS source,b.name AS target,r.age AS age ORDER BY source,target",
            vec![],
        )
        .await
        .unwrap();
    (vertices, edges)
}

async fn write(cx: &Cx, client: &mut Client, text: &str) -> Answer {
    client
        .execute(cx, ExecuteMode::Write, text, vec![])
        .await
        .unwrap()
}

// Require a resource diagnostic, not an arbitrary parser/auth refusal. Existing
// collectors classify resource errors differently; do not change that contract.
async fn limit_refusal(cx: &Cx, client: &mut Client, text: &str) {
    let mut columns = 0;
    let mut rows = 0;
    let result = client
        .execute_streaming(
            cx,
            ExecuteMode::Write,
            text,
            vec![],
            |_| {
                columns += 1;
                Ok(())
            },
            |_| {
                rows += 1;
                Ok(())
            },
        )
        .await;
    match result {
        Err(ClientError::Server { code, message }) => {
            assert!(
                matches!(
                    code,
                    ErrorCode::Budget | ErrorCode::Execution | ErrorCode::Statement
                ),
                "wrong failure class for {text}: {code:?} {message}"
            );
            let lower = message.to_ascii_lowercase();
            assert!(
                lower.contains("limit") || lower.contains("budget"),
                "not a resource refusal for {text}: {message}"
            );
        }
        other => panic!("configured write limit must refuse {text}: {other:?}"),
    }
    assert_eq!(
        (columns, rows),
        (0, 0),
        "a refused write exposed a result prefix"
    );
}

async fn threshold_case(
    cx: &Cx,
    name: &str,
    low: GraphWriteProgramPolicy,
    high: GraphWriteProgramPolicy,
    text: &str,
    expected_vertices: usize,
    expected_edges: usize,
) {
    let fixture = Fixture::start(cx, name).await.reopen(cx, low).await;
    let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
    let mut observer = fixture.client(cx, Rights::Read, 100).await;
    let before = state(cx, &mut observer).await;
    for statement in [text.to_owned(), format!("{text} LIMIT 0")] {
        limit_refusal(cx, &mut writer, &statement).await;
        assert_eq!(state(cx, &mut observer).await, before);
    }
    writer.close(cx).await.unwrap();
    observer.close(cx).await.unwrap();
    // The SAME statement and graph at the exact allowed threshold distinguish
    // real limit enforcement from syntax, scope, or unrelated fixture failures.
    let fixture = fixture.reopen(cx, high).await;
    let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
    let mut observer = fixture.client(cx, Rights::Read, 100).await;
    assert_eq!(state(cx, &mut observer).await, before);
    let answer = write(cx, &mut writer, text).await;
    assert_eq!(
        answer.outcome,
        Outcome::WriteCommitted {
            seq: 2,
            statements: 1,
        }
    );
    assert!(!answer.columns.is_empty() && !answer.rows.is_empty());
    let after = state(cx, &mut observer).await;
    assert_eq!(
        (after.0.rows.len(), after.1.rows.len()),
        (expected_vertices, expected_edges)
    );
    writer.close(cx).await.unwrap();
    observer.close(cx).await.unwrap();
    fixture.finish(cx, &after).await;
}

#[test]
fn returning_insert_respects_independent_vertex_and_edge_creation_ceilings() {
    run(async |cx| {
        threshold_case(
            cx,
            "insert-vertices",
            policy(100, 1, 0),
            policy(100, 2, 0),
            "CREATE (a:Person {name:'NewA'}),(b:Person {name:'NewB'}) RETURN a.name AS name",
            4,
            1,
        )
        .await;
        threshold_case(
            cx,
            "insert-edge",
            policy(100, 0, 0),
            policy(100, 0, 1),
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             CREATE (b)-[r:KNOWS {age:9}]->(a) RETURN r.age AS age",
            2,
            2,
        )
        .await;
    });
}

#[test]
fn chained_creations_share_host_limits_and_return_the_final_rows() {
    run(async |cx| {
        threshold_case(
            cx,
            "chain-vertices",
            policy(100, 1, 0),
            policy(100, 2, 0),
            "CREATE (a:Person {name:'NewA'}) WITH a \
             CREATE (b:Person {name:'NewB'}) RETURN a.name AS first,b.name AS second",
            4,
            1,
        )
        .await;
        threshold_case(
            cx,
            "chain-edges",
            policy(100, 2, 0),
            policy(100, 2, 1),
            "CREATE (a:Person {name:'NewA'}) WITH a \
             CREATE (b:Person {name:'NewB'}),(a)-[r:KNOWS]->(b) RETURN b.name AS name",
            4,
            2,
        )
        .await;
    });
}

#[test]
fn returning_mutation_counts_effects_before_output_pagination() {
    run(async |cx| {
        threshold_case(
            cx,
            "set-effects",
            policy(1, 0, 0),
            policy(2, 0, 0),
            "MATCH (n:Person) SET n.age=n.age+10 RETURN n.name AS name",
            2,
            1,
        )
        .await;
        threshold_case(
            cx,
            "remove-effects",
            policy(1, 0, 0),
            policy(2, 0, 0),
            "MATCH (n:Person) REMOVE n.age RETURN n.name AS name",
            2,
            1,
        )
        .await;
    });
}

#[test]
fn vertex_merge_returning_obeys_creation_and_selected_action_limits() {
    run(async |cx| {
        threshold_case(
            cx,
            "merge-create",
            policy(0, 0, 0),
            policy(0, 1, 0),
            "MERGE (n:Person {name:'New'}) RETURN n.name AS name",
            3,
            1,
        )
        .await;
        threshold_case(
            cx,
            "merge-actions",
            policy(0, 0, 0),
            policy(1, 0, 0),
            "MERGE (n:Person {name:'Ada'}) ON MATCH SET n.age=8 RETURN n.age AS age",
            2,
            1,
        )
        .await;
        let fixture = Fixture::start(cx, "merge-existing")
            .await
            .reopen(cx, policy(0, 0, 0))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let matched = write(
            cx,
            &mut writer,
            "MERGE (n:Person {name:'Ada'}) ON CREATE SET n.age=99 RETURN n.age AS age",
        )
        .await;
        assert_eq!(matched.rows, vec![vec![WireValue::Int(1)]]);
        assert_eq!(
            matched.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );
        let before = state(cx, &mut writer).await;
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &before).await;
    });
}

#[test]
fn relationship_merge_returning_cannot_bypass_zero_creation_policy() {
    run(async |cx| {
        threshold_case(
            cx,
            "merge-edge",
            policy(0, 0, 0),
            policy(0, 0, 1),
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             MERGE (b)-[r:KNOWS]->(a) RETURN b.name AS source",
            2,
            2,
        )
        .await;
        let fixture = Fixture::start(cx, "edge-existing")
            .await
            .reopen(cx, policy(0, 0, 0))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let matched = write(
            cx,
            &mut writer,
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             MERGE (a)-[r:KNOWS]->(b) RETURN r.age AS age",
        )
        .await;
        assert_eq!(matched.rows, vec![vec![WireValue::Int(7)]]);
        assert_eq!(
            matched.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );
        let before = state(cx, &mut writer).await;
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &before).await;
    });
}

#[test]
fn returning_uses_write_query_allowances_not_the_more_permissive_read_policy() {
    run(async |cx| {
        let text = "MATCH (n:Person) SET n.age=n.age+1 RETURN n.name AS name";
        // LIMIT 0 cannot waive input/work/scratch. Final rows differ below.
        for dimension in [0, 2, 3] {
            let mut limits = [100_000, 100_000, 10_000_000, 10_000_000];
            limits[dimension] = 0;
            let [records, rows, work, scratch] = limits;
            let low = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(records, rows, work, scratch),
                100,
                100,
                100,
            );
            threshold_case(
                cx,
                &format!("write-query-{dimension}"),
                low,
                policy(100, 100, 100),
                text,
                2,
                1,
            )
            .await;
        }
        let with_rows = |rows| {
            GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000),
                2,
                0,
                0,
            )
        };
        let fixture = Fixture::start(cx, "write-result-rows")
            .await
            .reopen(cx, with_rows(1))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let mut observer = fixture.client(cx, Rights::Read, 100).await;
        let before = state(cx, &mut observer).await;
        limit_refusal(cx, &mut writer, text).await;
        assert_eq!(state(cx, &mut observer).await, before);
        let one = write(cx, &mut writer, &format!("{text} LIMIT 1")).await;
        assert_eq!(one.rows.len(), 1);
        assert_eq!(
            one.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1,
            }
        );
        let ages = observer
            .execute(
                cx,
                ExecuteMode::Read,
                "MATCH (n:Person) RETURN n.age AS age ORDER BY age",
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            ages.rows,
            vec![vec![WireValue::Int(2)], vec![WireValue::Int(3)]]
        );
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        let fixture = fixture.reopen(cx, with_rows(0)).await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let empty = write(cx, &mut writer, &format!("{text} LIMIT 0")).await;
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1,
            }
        );
        let after = state(cx, &mut writer).await;
        assert_eq!(
            after
                .0
                .rows
                .iter()
                .map(|row| row[2].clone())
                .collect::<Vec<_>>(),
            vec![WireValue::Int(3), WireValue::Int(4)]
        );
        limit_refusal(cx, &mut writer, text).await;
        assert_eq!(state(cx, &mut writer).await, after);
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &after).await;
    });
}

#[test]
fn signed_rows_and_late_expression_refusals_still_precede_commit() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "signed-and-late")
            .await
            .reopen(cx, policy(1, 0, 0))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 0).await;
        let mut observer = fixture.client(cx, Rights::Read, 100).await;
        let before = state(cx, &mut observer).await;
        let text = "MATCH (n:Person {name:'Ada'}) SET n.age=8 RETURN n.age AS age";
        limit_refusal(cx, &mut writer, text).await;
        assert_eq!(state(cx, &mut observer).await, before);
        let empty = write(cx, &mut writer, &format!("{text} LIMIT 0")).await;
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1,
            }
        );
        let after = state(cx, &mut observer).await;
        assert_eq!(
            after
                .0
                .rows
                .iter()
                .map(|row| row[2].clone())
                .collect::<Vec<_>>(),
            vec![WireValue::Int(8), WireValue::Int(2)]
        );
        // Returning no rows does not waive read rights.
        let mut write_only = fixture.client(cx, Rights::Write, 0).await;
        let denied = write_only
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person {name:'Ada'}) SET n.age=9 RETURN n.age LIMIT 0",
                vec![],
            )
            .await;
        assert!(
            matches!(
                denied,
                Err(ClientError::Server {
                    code: ErrorCode::PermissionDenied,
                    ..
                })
            ),
            "{denied:?}"
        );
        assert_eq!(state(cx, &mut observer).await, after);
        write_only.close(cx).await.unwrap();
        writer.close(cx).await.unwrap();
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let mut columns = 0;
        let mut rows = 0;
        let failure = writer
            .execute_streaming(
                cx,
                ExecuteMode::Write,
                "MATCH (n:Person {name:'Ada'}) SET n.age=9 RETURN 10/(n.age-9) AS bad",
                vec![],
                |_| {
                    columns += 1;
                    Ok(())
                },
                |_| {
                    rows += 1;
                    Ok(())
                },
            )
            .await;
        match failure {
            Err(ClientError::Server { message, .. }) => assert!(
                message.contains("DivisionByZero") || message.contains("division by zero"),
                "not the expected late value failure: {message}"
            ),
            other => panic!("late expression must refuse: {other:?}"),
        }
        assert_eq!((columns, rows), (0, 0));
        assert_eq!(state(cx, &mut observer).await, after);
        writer.close(cx).await.unwrap();
        observer.close(cx).await.unwrap();
        fixture.finish(cx, &after).await;
    });
}

#[test]
fn relationship_upsert_returning_obeys_creation_and_combined_action_limits() {
    run(async |cx| {
        // An ON CREATE statement still obeys the independent edge ceiling.
        threshold_case(
            cx,
            "upsert-create-edge",
            policy(1, 0, 0),
            policy(1, 0, 1),
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             MERGE (b)-[r:KNOWS]->(a) ON CREATE SET r.age=8 RETURN r.age AS age",
            2,
            2,
        )
        .await;
        // Here creation fits, but a refused action must roll that new edge back.
        threshold_case(
            cx,
            "upsert-create-action",
            policy(0, 0, 1),
            policy(1, 0, 1),
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             MERGE (b)-[r:KNOWS]->(a) ON CREATE SET r.age=8 RETURN r.age AS age",
            2,
            2,
        )
        .await;
        // Branch and trailing assignments share ONE action allowance, even
        // when both write the same property and only one final value is stored.
        let text = "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
                    MERGE (a)-[r:KNOWS]->(b) ON MATCH SET r.age=r.age+1 \
                    SET r.age=r.age+1 RETURN r.age AS age";
        let fixture = Fixture::start(cx, "upsert-two-actions")
            .await
            .reopen(cx, policy(1, 0, 0))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let before = state(cx, &mut writer).await;
        for statement in [text.to_owned(), format!("{text} LIMIT 0")] {
            limit_refusal(cx, &mut writer, &statement).await;
            assert_eq!(state(cx, &mut writer).await, before);
        }
        writer.close(cx).await.unwrap();
        let fixture = fixture.reopen(cx, policy(2, 0, 0)).await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        assert_eq!(state(cx, &mut writer).await, before);
        let updated = write(cx, &mut writer, text).await;
        assert_eq!(updated.rows, vec![vec![WireValue::Int(9)]]);
        assert_eq!(
            updated.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1,
            }
        );
        let after = state(cx, &mut writer).await;
        assert_eq!(after.1.rows.len(), 1);
        assert_eq!(after.1.rows[0][3], WireValue::Int(9));
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &after).await;
    });
}

#[test]
fn relationship_upsert_zero_effects_admit_only_unchosen_branches_and_no_input() {
    run(async |cx| {
        let fixture = Fixture::start(cx, "upsert-zero-effects")
            .await
            .reopen(cx, policy(0, 0, 0))
            .await;
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let before = state(cx, &mut writer).await;
        // Matching the existing edge never runs or bills ON CREATE.
        let matched = write(
            cx,
            &mut writer,
            "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
             MERGE (a)-[r:KNOWS]->(b) ON CREATE SET r.age=99 RETURN r.age AS age",
        )
        .await;
        assert_eq!(matched.rows, vec![vec![WireValue::Int(7)]]);
        assert_eq!(
            matched.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );
        assert_eq!(state(cx, &mut writer).await, before);
        // An empty MATCH executes neither ON branch nor the trailing SET.
        let empty = write(
            cx,
            &mut writer,
            "MATCH (a:Person {name:'absent'}),(b:Person {name:'Bob'}) \
             MERGE (a)-[r:KNOWS]->(b) ON CREATE SET r.age=99 \
             ON MATCH SET r.age=99 SET r.age=99 RETURN r.age AS age",
        )
        .await;
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1,
            }
        );
        assert_eq!(state(cx, &mut writer).await, before);
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &before).await;
    });
}

#[test]
fn relationship_upsert_uses_write_query_limits_and_final_row_admission() {
    run(async |cx| {
        let text = "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
                    MERGE (a)-[r:KNOWS]->(b) ON MATCH SET r.age=8 RETURN r.age AS age";
        for dimension in [0, 2, 3] {
            let mut limits = [100_000, 100_000, 10_000_000, 10_000_000];
            limits[dimension] = 0;
            let [records, rows, work, scratch] = limits;
            threshold_case(
                cx,
                &format!("upsert-write-query-{dimension}"),
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(records, rows, work, scratch),
                    1,
                    0,
                    0,
                ),
                policy(1, 0, 0),
                text,
                2,
                1,
            )
            .await;
        }
        let no_rows = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
            1,
            0,
            0,
        );
        let fixture = Fixture::start(cx, "upsert-no-result-rows")
            .await
            .reopen(cx, no_rows)
            .await;
        // This grant is permissive: the refusal must come from the HOST's
        // write result limit, not an accidentally narrow capability.
        let mut writer = fixture.client(cx, Rights::ReadWrite, 100).await;
        let before = state(cx, &mut writer).await;
        limit_refusal(cx, &mut writer, text).await;
        assert_eq!(state(cx, &mut writer).await, before);
        let empty = write(cx, &mut writer, &format!("{text} LIMIT 0")).await;
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1,
            }
        );
        let after = state(cx, &mut writer).await;
        assert_eq!(after.1.rows.len(), 1);
        assert_eq!(after.1.rows[0][3], WireValue::Int(8));
        // LIMIT 0 only changes output admission, not RETURN evaluation. The
        // native executor must discard the tentative zero property on failure.
        let failure = writer
            .execute(
                cx,
                ExecuteMode::Write,
                "MATCH (a:Person {name:'Ada'}),(b:Person {name:'Bob'}) \
                 MERGE (a)-[r:KNOWS]->(b) ON MATCH SET r.age=0 \
                 RETURN 10/r.age AS bad LIMIT 0",
                vec![],
            )
            .await;
        match failure {
            Err(ClientError::Server { message, .. }) => assert!(
                message.contains("DivisionByZero") || message.contains("division by zero"),
                "not the expected late value failure: {message}"
            ),
            other => panic!("LIMIT 0 must not suppress evaluation: {other:?}"),
        }
        assert_eq!(state(cx, &mut writer).await, after);
        writer.close(cx).await.unwrap();
        fixture.finish(cx, &after).await;
    });
}

#[test]
fn mixed_chains_preserve_host_effect_limits_and_rollback_the_creation_prefix() {
    run(async |cx| {
        threshold_case(
            cx,
            "chain-set-effects",
            policy(0, 1, 0),
            policy(1, 1, 0),
            "CREATE (n:Person {name:'New',age:3}) WITH n \
             SET n.age=n.age+4 RETURN n.age AS age",
            3,
            1,
        )
        .await;
        threshold_case(
            cx,
            "chain-remove-effects",
            policy(0, 1, 0),
            policy(1, 1, 0),
            "CREATE (n:Person {name:'New',age:3}) WITH n \
             REMOVE n.age RETURN n.name AS name",
            3,
            1,
        )
        .await;
        threshold_case(
            cx,
            "chain-delete-effects",
            policy(0, 2, 0),
            policy(1, 2, 0),
            "CREATE (a:Person {name:'Keep'}),(b:Person {name:'Drop'}) WITH a,b \
             DELETE b RETURN a.name AS name",
            3,
            1,
        )
        .await;
    });
}

#[test]
fn relationship_merge_chains_share_the_hosts_creation_allowance() {
    run(async |cx| {
        threshold_case(
            cx,
            "chain-merge-edge",
            policy(0, 2, 0),
            policy(0, 2, 1),
            "CREATE (a:Person {name:'NewA'}),(b:Person {name:'NewB'}) \
             MERGE (a)-[r:KNOWS {age:9}]->(b) RETURN r.age AS age",
            4,
            2,
        )
        .await;
    });
}
