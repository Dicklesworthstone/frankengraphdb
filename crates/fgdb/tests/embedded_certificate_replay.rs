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
