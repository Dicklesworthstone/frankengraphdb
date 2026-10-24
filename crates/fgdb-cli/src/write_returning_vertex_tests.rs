//! Exercise the actual CLI dispatch, staging, encoding and completion paths.

use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn options_for(command: &str, parts: &[&str]) -> Options {
    let mut args = vec![
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--label",
        "Person=1",
        "--relation",
        "R=1",
        "--property",
        "p=1",
        "--property",
        "q=2",
    ];
    args.extend_from_slice(parts);
    crate::parse(
        &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        command,
    )
    .unwrap()
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, key, value) in [(1, 1, 1), (2, 1, 2), (3, 9, 3)] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (P, CanonicalScalar::Int(key)),
                (Q, CanonicalScalar::Int(value)),
            ],
        );
    }
    batch
}

async fn execute<V: Vfs + Clone>(
    command: &str,
    database: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &Options,
    output: &mut Vec<u8>,
) -> Result<(), Failure> {
    if command == "transaction" {
        crate::transaction::run(database, contexts, options, None, true, output).await
    } else {
        let query = prepare(options)?.expect("vertex MERGE RETURN must be classified");
        run(
            database,
            contexts,
            query,
            options.budget.policy(),
            true,
            output,
        )
        .await
    }
}

#[test]
fn both_drivers_update_all_targets_before_grouping_and_preserve_limits() {
    const QUERY: &str = "MERGE (n:Person {p:1}) ON CREATE SET n.q=99 \
        ON MATCH SET n.q=n.q+1 RETURN count(n) AS total,sum(n.q) AS sum";
    let ((), report) = run_async_under_lab(0xc9e1_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for command in ["write", "transaction"] {
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            database.write(&contexts.commit(), seed()).await.unwrap();
            let basis = database.frontier().unwrap();
            let parts = |flag, limit| {
                let mut parts = vec![flag, limit];
                if command == "transaction" {
                    parts.push("--write");
                }
                parts.push(QUERY);
                parts
            };
            let options = options_for(command, &parts("--max-result-rows", "1"));
            let mut output = Vec::new();
            execute(command, &mut database, &contexts, &options, &mut output)
                .await
                .unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(
                text.contains(
                    r#""cells":[{"type":"int","value":"2"},{"type":"int","value":"5"}]"#,
                ),
                "{command}: {text}",
            );
            assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
            assert_eq!(database.vertices().unwrap().len(), 3);
            for (id, key, value) in [(1, 1, 2), (2, 1, 3), (3, 9, 3)] {
                assert_eq!(
                    database.vertex(VId(id)).unwrap().unwrap().props,
                    vec![
                        (P, CanonicalScalar::Int(key).into()),
                        (Q, CanonicalScalar::Int(value).into())
                    ],
                );
            }
            let committed = database.frontier().unwrap();
            for (flag, dimension) in [
                ("--max-snapshot-records", "SnapshotRecords"),
                ("--max-result-rows", "ResultRows"),
                ("--max-work-units", "WorkUnits"),
                ("--max-scratch-entries", "ScratchEntries"),
            ] {
                let options = options_for(command, &parts(flag, "0"));
                let mut output = Vec::new();
                let error = execute(command, &mut database, &contexts, &options, &mut output)
                    .await
                    .expect_err("zero allowances must not reset for each target");
                assert!(error.message.contains(dimension), "{}", error.message);
                assert!(output.is_empty());
                assert_eq!(database.frontier().unwrap(), committed);
                for (id, key, value) in [(1, 1, 2), (2, 1, 3), (3, 9, 3)] {
                    assert_eq!(
                        database.vertex(VId(id)).unwrap().unwrap().props,
                        vec![
                            (P, CanonicalScalar::Int(key).into()),
                            (Q, CanonicalScalar::Int(value).into())
                        ],
                    );
                }
                assert_eq!(contexts.txn().outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pagination_and_with_filters_do_not_limit_the_frozen_match_set() {
    let ((), report) = run_async_under_lab(0xc9e1_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        for tail in [
            "RETURN n.q AS q LIMIT 0",
            "RETURN n.q AS q ORDER BY q SKIP 1 LIMIT 1",
            "WITH n.q AS q WHERE q>100 RETURN q",
        ] {
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            database.write(&contexts.commit(), seed()).await.unwrap();
            let before = database.frontier().unwrap();
            let options = options_for("write", &["CREATE (n)"]);
            let text =
                format!("MERGE (n:Person {{p:1}}) ON MATCH SET n.p=n.p+1 SET n.q=n.q+10 {tail}");
            let query = prepare_statement(&text, &GqlParameters::new(), &options)
                .unwrap()
                .unwrap();
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let result = query
                .execute_in_transaction(
                    &mut transaction,
                    &mut database,
                    &cx,
                    GqlQueryPolicy::new(1_000, 1, 5_000_000, 2_000_000),
                )
                .unwrap();
            if tail.contains("SKIP") {
                assert_eq!(result.value.len(), 1);
                assert_eq!(
                    result.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(12))],
                );
            } else {
                assert!(result.value.is_empty());
            }
            assert_eq!(database.frontier().unwrap(), before);
            transaction
                .finish(&mut database, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(database.vertices().unwrap().len(), 3);
            for (id, key, value) in [(1, 2, 11), (2, 2, 12), (3, 9, 3)] {
                assert_eq!(
                    database.vertex(VId(id)).unwrap().unwrap().props,
                    vec![
                        (P, CanonicalScalar::Int(key).into()),
                        (Q, CanonicalScalar::Int(value).into())
                    ],
                );
            }
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_empty_match_creates_once_and_subsequent_execution_uses_on_match() {
    const QUERY: &str = "MERGE (n:Person {p:7}) ON CREATE SET n.q=10 \
        ON MATCH SET n.q=n.q+1 RETURN n.q AS q";
    let ((), report) = run_async_under_lab(0xc9e1_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for command in ["write", "transaction"] {
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let parts = if command == "transaction" {
                vec!["--write", QUERY]
            } else {
                vec![QUERY]
            };
            let options = options_for(command, &parts);
            for expected in [10, 11] {
                let mut output = Vec::new();
                execute(command, &mut database, &contexts, &options, &mut output)
                    .await
                    .unwrap();
                let text = String::from_utf8(output).unwrap();
                assert!(
                    text.contains(&format!(
                        r#""cells":[{{"type":"int","value":"{expected}"}}]"#,
                    )),
                    "{text}",
                );
                let vertices = database.vertices().unwrap();
                assert_eq!(vertices.len(), 1);
                assert_eq!(
                    vertices[0].props,
                    vec![
                        (P, CanonicalScalar::Int(7).into()),
                        (Q, CanonicalScalar::Int(expected).into())
                    ],
                );
                assert_eq!(contexts.txn().outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_late_return_error_restores_the_prior_staged_prefix() {
    let ((), report) = run_async_under_lab(0xc9e1_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        database.write(&contexts.commit(), seed()).await.unwrap();
        let basis = database.frontier().unwrap();
        let options = options_for("write", &["CREATE (n)"]);
        let budget = GqlQueryPolicy::new(1_000, 10, 5_000_000, 2_000_000);
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let prefix = prepare_statement(
            "CREATE (n:Person {p:8}) RETURN n.p AS p",
            &GqlParameters::new(),
            &options,
        )
        .unwrap()
        .unwrap();
        prefix
            .execute_in_transaction(&mut transaction, &mut database, &cx, budget)
            .unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let failing = prepare_statement(
            "MERGE (n:Person {p:1}) ON MATCH SET n.q=0 RETURN 10/n.q AS bad LIMIT 0",
            &GqlParameters::new(),
            &options,
        )
        .unwrap()
        .unwrap();
        assert!(
            failing
                .execute_in_transaction(&mut transaction, &mut database, &cx, budget)
                .is_err(),
        );
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(database.frontier().unwrap(), basis);
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(database.vertices().unwrap().len(), 4);
        for (id, value) in [(1, 1), (2, 2)] {
            assert_eq!(
                database.vertex(VId(id)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(1).into()),
                    (Q, CanonicalScalar::Int(value).into())
                ],
            );
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
