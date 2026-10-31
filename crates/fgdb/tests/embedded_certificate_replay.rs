//! Local retained-history replay; no archive leases or nondeterministic seeds.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, NativeReadClass, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x7b; 32],
        DatabaseSecurityNamespaceId([0x7c; 32]),
        [0x7d; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, p) in [(1, 10), (2, 21), (3, 30)] {
        batch.create_vertex(VId(id), vec![PERSON], vec![(P, CanonicalScalar::Int(p))]);
    }
    batch.create_vertex(VId(4), vec![PERSON], vec![(P, CanonicalScalar::Null)]);
    batch
}
fn assert_rows_equal(left: &QueryResult, right: &QueryResult) {
    let QueryResult::Rows {
        columns: lc,
        rows: lr,
    } = left
    else {
        unreachable!("read result")
    };
    let QueryResult::Rows {
        columns: rc,
        rows: rr,
    } = right
    else {
        unreachable!("read result")
    };
    assert_eq!(lc, rc);
    assert_eq!(lr, rr);
}

#[test]
fn current_time_certificates_replay_the_captured_instant_without_a_live_clock() {
    use fgdb::{MemVfs, NativeResultCertificate, ReplayRefusal, StatementClock};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalTimestamp;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(async {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let instant = CanonicalTimestamp::offset_only(1_700_000_000_123_456_789, 0).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let sampled = Arc::clone(&calls);
        let captured = instant.clone();
        db.set_statement_clock(Some(StatementClock::new(move |_| {
            sampled.fetch_add(1, Ordering::SeqCst);
            Some(captured.clone())
        })));
        let text = "UNWIND [1,2] AS occurrence \
            RETURN occurrence AS n,datetime() AS instant,datetime() AS repeated ORDER BY n";
        let params = GqlParameters::new();
        let (original, certificate) = db
            .execute_certified(&cx, text, &params, symbols, policy())
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "one capture per statement, never per row or expression"
        );
        assert_eq!(certificate.statement_time.as_ref(), Some(&instant));
        assert_eq!(certificate.plan.snapshot_seq, CommitSeq(1));
        let cell = fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Timestamp(
            instant.clone(),
        )));
        assert_eq!(
            original,
            QueryResult::Rows {
                columns: vec!["n".into(), "instant".into(), "repeated".into()],
                rows: [1, 2]
                    .into_iter()
                    .map(|value| vec![
                        fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))),
                        cell.clone(),
                        cell.clone(),
                    ])
                    .collect(),
            }
        );
        let certificate = NativeResultCertificate::decode(&certificate.canonical_bytes()).unwrap();
        db.set_statement_clock(Some(StatementClock::new(|_| {
            panic!("replay must never sample a live clock")
        })));
        assert_rows_equal(
            &original,
            &db.replay(&cx, &certificate, &params, symbols, policy())
                .unwrap(),
        );
        let explicit = params.clone().with_statement_time(instant.clone()).unwrap();
        assert_rows_equal(
            &original,
            &db.replay(&cx, &certificate, &explicit, symbols, policy())
                .unwrap(),
        );
        let later = CanonicalTimestamp::offset_only(1_700_000_001_123_456_789, 0).unwrap();
        let different = params.clone().with_statement_time(later.clone()).unwrap();
        assert!(matches!(
            db.replay(&cx, &certificate, &different, symbols, policy()),
            Err(ReplayRefusal::ParameterValuesMismatch)
        ));
        for changed in [None, Some(later.clone())] {
            let mut tampered = certificate.clone();
            tampered.statement_time = changed;
            assert!(matches!(
                db.replay(&cx, &tampered, &params, symbols, policy()),
                Err(ReplayRefusal::ParameterValuesMismatch)
            ));
        }
        let (_, rebound) = db
            .execute_certified(&cx, text, &different, symbols, policy())
            .unwrap();
        assert_eq!(
            rebound.plan.digest, certificate.plan.digest,
            "the reusable template does not freeze time"
        );
        assert_ne!(rebound.values_digest, certificate.values_digest);
        assert_ne!(rebound.result_digest, certificate.result_digest);
        let (_, timeless) = db
            .execute_certified(&cx, "RETURN 1 AS value", &params, symbols, policy())
            .unwrap();
        assert!(timeless.statement_time.is_none());
        let mut write = WriteBatch::new(R);
        write.create_vertex(VId(9), vec![PERSON], vec![(P, CanonicalScalar::Int(99))]);
        db.write(&commit, write).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_rows_equal(
            &original,
            &reopened
                .replay(&cx, &certificate, &params, symbols, policy())
                .unwrap(),
        );
        assert!(
            reopened
                .execute_certified(&cx, text, &params, symbols, policy())
                .is_err(),
            "a fresh execution cannot invent an absent statement clock"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

const READS: [(NativeReadClass, &str); 7] = [
    (
        NativeReadClass::Pattern,
        "MATCH (n:Person) RETURN n.p AS p ORDER BY p DESC",
    ),
    (
        NativeReadClass::Aggregate,
        "MATCH (n:Person) RETURN COUNT(*) AS c,AVG(n.p) AS mean,COLLECT(n.p) AS items",
    ),
    (
        NativeReadClass::PipelineAggregate,
        "MATCH (n:Person) WITH n.p AS x RETURN COUNT(*) AS c,AVG_INT(x) AS mean,COLLECT(x) AS items",
    ),
    (
        NativeReadClass::Set,
        "MATCH (a:Person) RETURN a.p AS p UNION ALL MATCH (b:Person) RETURN b.p AS p ORDER BY p DESC",
    ),
    (
        NativeReadClass::TemporalPattern,
        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p ORDER BY p DESC",
    ),
    (
        NativeReadClass::TemporalSet,
        "MATCH (a:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN a.p AS p UNION ALL MATCH (b:Person) RETURN b.p AS p ORDER BY p DESC",
    ),
    (
        NativeReadClass::TemporalAggregate,
        "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS c,AVG(n.p) AS mean,COLLECT(n.p) AS items",
    ),
];

#[test]
fn every_class_replays_original_certificate_after_commits_and_reopen() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    let dir = std::env::temp_dir().join(format!("fgdb-replay-{}", std::process::id()));
    runtime.block_on(async {
        let mut db = Database::create(&commit, &dir, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut certified = Vec::new();
        for (class, text) in READS {
            let params = if matches!(
                class,
                NativeReadClass::TemporalPattern
                    | NativeReadClass::TemporalSet
                    | NativeReadClass::TemporalAggregate
            ) {
                GqlParameters::new().with_uint64("at", 1).unwrap()
            } else {
                GqlParameters::new()
            };
            assert_eq!(
                PreparedNativeRead::prepare(text, &params, symbols)
                    .unwrap()
                    .facade_class(),
                class
            );
            let (result, certificate) = db
                .execute_certified(&cx, text, &params, symbols, policy())
                .unwrap();
            assert_eq!(certificate.plan.snapshot_seq, CommitSeq(1));
            assert_rows_equal(
                &result,
                &db.query(&cx, text, &params, symbols, policy()).unwrap(),
            );
            let QueryResult::Rows { rows, .. } = &result else { unreachable!("read") };
            if class == NativeReadClass::Pattern {
                assert!(rows.iter().flatten().any(|cell| matches!(cell, fgdb_gql::GraphAggregateValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Null)))));
            }
            if matches!(class, NativeReadClass::Aggregate | NativeReadClass::PipelineAggregate | NativeReadClass::TemporalAggregate) {
                assert!(rows.iter().flatten().any(|cell| matches!(cell, fgdb_gql::GraphAggregateValue::Average(mean) if mean.numerator() == 61 && mean.denominator() == 3)));
                assert!(rows.iter().flatten().any(|cell| matches!(cell, fgdb_gql::GraphAggregateValue::Value(fgdb_gql::algebra::GraphValue::List(_)))));
            }
            certified.push((params, result, certificate));
        }
        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(9), vec![PERSON], vec![(P, CanonicalScalar::Int(77))]);
        db.write(&commit, later).await.unwrap();
        let live = db
            .query(&cx, READS[0].1, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let QueryResult::Rows {
            rows: live_rows, ..
        } = live
        else {
            unreachable!("read")
        };
        let QueryResult::Rows { rows: old_rows, .. } = &certified[0].1 else {
            unreachable!("read")
        };
        assert_ne!(&live_rows, old_rows);
        for (params, original, cert) in &certified {
            assert_rows_equal(
                original,
                &db.replay(&cx, cert, params, symbols, policy()).unwrap(),
            );
        }
        // Mint temporal certificates at a newer frontier: their seq is still 1.
        for (class, text) in &READS[4..] {
            let params = GqlParameters::new().with_uint64("at", 1).unwrap();
            let (_, cert) = db
                .execute_certified(&cx, text, &params, symbols, policy())
                .unwrap();
            assert_eq!(cert.plan.snapshot_seq, CommitSeq(1), "{class:?}");
        }
        drop(db);
        let reopened = Database::open(&commit, &dir, keys()).await.unwrap();
        for (params, original, cert) in &certified {
            assert_rows_equal(
                original,
                &reopened
                    .replay(&cx, cert, params, symbols, policy())
                    .unwrap(),
            );
        }
        drop(reopened);
        // The authority is the namespace plus the certified history, not the
        // directory spelling: another spelling and a byte copy both replay.
        let copy = std::env::temp_dir().join(format!("fgdb-replay-copy-{}", std::process::id()));
        copy_tree(&dir, &copy);
        for path in [dir.join("."), copy] {
            let moved = Database::open(&commit, &path, keys()).await.unwrap();
            for (params, original, cert) in &certified {
                assert_rows_equal(
                    original,
                    &moved.replay(&cx, cert, params, symbols, policy()).unwrap(),
                );
            }
        }
    });
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn one_trailing_terminator_is_accepted_by_every_read_class_and_replays() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for (class, text) in READS {
            let params = if matches!(
                class,
                NativeReadClass::TemporalPattern
                    | NativeReadClass::TemporalSet
                    | NativeReadClass::TemporalAggregate
            ) {
                GqlParameters::new().with_uint64("at", 1).unwrap()
            } else {
                GqlParameters::new()
            };
            let bare = db.query(&cx, text, &params, symbols, policy()).unwrap();
            for spelled in [format!("{text};"), format!("{text} ;\n")] {
                assert_rows_equal(
                    &bare,
                    &db.query(&cx, &spelled, &params, symbols, policy()).unwrap(),
                );
                let (_, certificate) = db
                    .execute_certified(&cx, &spelled, &params, symbols, policy())
                    .unwrap();
                assert_rows_equal(
                    &bare,
                    &db.replay(&cx, &certificate, &params, symbols, policy())
                        .unwrap(),
                );
            }
            // Exactly one terminator: a second one still reaches the grammar.
            assert!(
                db.query(&cx, &format!("{text};;"), &params, symbols, policy())
                    .is_err(),
                "{class:?}"
            );
        }
        let explained = db
            .query(
                &cx,
                "EXPLAIN (CERTIFICATE) MATCH (n:Person) RETURN n.p AS p;",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = explained else {
            unreachable!("explain")
        };
        assert!(!rows.is_empty());
        // A `;` inside the final literal is data, not a terminator.
        let literal = db
            .query(
                &cx,
                "MATCH (n:Person) WHERE n.p = 10 RETURN 'a;' AS x",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let QueryResult::Rows { rows, .. } = literal else {
            unreachable!("read")
        };
        assert_eq!(rows.len(), 1);
    });
}

#[test]
fn replay_refuses_values_plan_class_snapshot_and_result_mismatches() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(async {
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n:Person) WHERE n.p >= $floor RETURN n.p AS p ORDER BY p DESC";
        let params = GqlParameters::new().with_int64("floor", 15).unwrap();
        let (_, certificate) = db
            .execute_certified(&cx, text, &params, symbols, policy())
            .unwrap();
        let changed = GqlParameters::new().with_int64("floor", 25).unwrap();
        assert!(matches!(
            db.replay(&cx, &certificate, &changed, symbols, policy()),
            Err(fgdb::ReplayRefusal::ParameterValuesMismatch)
        ));
        let changed_type = GqlParameters::new().with_uint64("floor", 15).unwrap();
        assert!(matches!(
            db.replay(&cx, &certificate, &changed_type, symbols, policy()),
            Err(fgdb::ReplayRefusal::ParameterValuesMismatch)
        ));
        let mut tampered = certificate.clone();
        tampered.result_digest.0[0] ^= 1;
        assert!(matches!(
            db.replay(&cx, &tampered, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::ResultMismatch { .. })
        ));
        let mut tampered = certificate.clone();
        tampered.facade_class = NativeReadClass::Set;
        assert!(matches!(
            db.replay(&cx, &tampered, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::FacadeClassMismatch)
        ));
        assert!(matches!(
            db.replay(
                &cx,
                &certificate,
                &params,
                |kind: GraphSymbolKind, name: &str| {
                    if kind == GraphSymbolKind::Property {
                        Some(GraphSymbol::Property(PropertyKeyId(2)))
                    } else {
                        symbols(kind, name)
                    }
                },
                policy()
            ),
            Err(fgdb::ReplayRefusal::PlanMismatch)
        ));
        let mut future = certificate.clone();
        future.plan.snapshot_seq = CommitSeq(99);
        assert!(matches!(
            db.replay(&cx, &future, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::Snapshot)
        ));
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.replay(&cx, &certificate, &params, symbols, zero),
            Err(fgdb::ReplayRefusal::Execution { .. })
        ));
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        let mut different = WriteBatch::new(R);
        different.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(99))]);
        other.write(&commit, different).await.unwrap();
        assert!(matches!(
            other.replay(&cx, &certificate, &params, symbols, policy()),
            Err(fgdb::ReplayRefusal::SnapshotIdentityMismatch)
        ));
        // An unrelated, entirely empty database must refuse even though the
        // historical commitment at seq 0 is universally CHAIN_ORIGIN.
        let empty = Database::open_memory(
            &commit,
            DatabaseKeys::new(
                [0x31; 32],
                DatabaseSecurityNamespaceId([0x32; 32]),
                [0x33; 32],
            ),
        )
        .await
        .unwrap();
        let refusal = empty.replay(&cx, &certificate, &params, symbols, policy());
        assert!(
            matches!(
                refusal,
                Err(fgdb::ReplayRefusal::Snapshot | fgdb::ReplayRefusal::SnapshotIdentityMismatch)
            ),
            "empty database must refuse on snapshot identity, saw {refusal:?}"
        );
    });
}
