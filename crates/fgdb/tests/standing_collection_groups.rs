//! Graph-backed list/Any pipelines use the ordinary maintained group circuit.
//! The snapshot engine and explicit hand controls are independent oracles.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, StandingQueryError,
    StandingQueryFailure, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};

const B: PropertyKeyId = PropertyKeyId(1);
const N: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb4; 32],
        DatabaseSecurityNamespaceId([0xb5; 32]),
        [0xb6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "bucket") => Some(GraphSymbol::Property(B)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, bucket, amount) in [
        (1, 1, Some(7)),
        (2, 1, Some(7)),
        (3, 2, None),
        (u128::MAX, 2, Some(2)),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (B, CanonicalScalar::Int(bucket)),
                (
                    N,
                    amount.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ),
            ],
        );
    }
    batch
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
        .unwrap()
}
fn matches_snapshot(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    args: &GqlParameters,
    handle: &StandingQueryHandle,
) {
    let (at, actual) = db.standing_native_query(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(
        actual,
        db.query(cx, text, args, symbols, policy()).unwrap(),
        "{text}"
    );
}
const NINE: &str = "MATCH (n) WITH [n.amount,n.amount] AS xs UNWIND xs AS x \
    RETURN COUNT(*) AS rows,COUNT(x) AS nonnull,COUNT(DISTINCT x) AS unique,\
    SUM(x) AS total,SUM(DISTINCT x) AS unique_sum,MIN(x) AS low,MAX(x) AS high,\
    AVG(x) AS average,AVG(DISTINCT x) AS unique_average";
fn queries() -> [&'static str; 5] {
    [
        NINE,
        "MATCH (n) WITH [n.amount,NULL] AS xs UNWIND xs AS x RETURN x AS k,COUNT(*) AS copies GROUP BY x HAVING copies > 0 ORDER BY copies DESC LIMIT 3",
        "MATCH (n) WITH [n.bucket,[n.amount]] AS k,n.amount AS x RETURN k,COUNT(*) AS copies,MAX(x) AS high GROUP BY k HAVING k IS NOT NULL ORDER BY copies DESC",
        "MATCH (n) WITH [n,n.amount,[n.amount]] AS xs UNWIND xs AS x RETURN COUNT(x) AS present,COUNT(DISTINCT x) AS kinds,MIN(x) AS low,MAX(x) AS high",
        "MATCH (n) WITH [n.amount,n.bucket] AS xs WITH xs[0] AS x RETURN SUM(x) AS total,AVG(x) AS average",
    ]
}

#[test]
fn list_keys_any_extrema_and_nine_numeric_aggregates_follow_commits_and_exact_deltas() {
    let ((), report) = run_async_under_lab(0x4c47_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let texts = queries();
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                db.register_standing_native(&cx, text, &args, symbols, policy())
                    .unwrap()
            })
            .collect();
        let QueryResult::Rows { rows, .. } = db
            .standing_native_query(&cx, &handles[0], policy())
            .unwrap()
            .1
        else {
            panic!("read returned a write")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_count(), Some(8));
        assert_eq!(rows[0][1].as_count(), Some(6));
        assert_eq!(rows[0][2].as_count(), Some(2));
        assert_eq!(rows[0][3].as_integer(), Some(32));
        assert_eq!(rows[0][4].as_integer(), Some(9));
        let average = rows[0][7].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (16, 3));
        let average = rows[0][8].as_average().unwrap();
        assert_eq!((average.numerator(), average.denominator()), (9, 2));
        for handle in &handles {
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
        }
        let mut integrated: Vec<_> = handles
            .iter()
            .map(|h| copy(db.standing_query(&cx, h).unwrap().rows()))
            .collect();
        for step in 0..5 {
            for (text, handle) in texts.iter().zip(&handles) {
                matches_snapshot(&db, &cx, text, &args, handle);
            }
            let mut edit = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    edit.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(-4)));
                }
                1 => {
                    edit.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(11)));
                }
                2 => {
                    edit.delete_vertex(VId(1));
                }
                3 => {
                    edit.set_vertex_property(VId(u128::MAX), B, Some(CanonicalScalar::Int(9)));
                }
                _ => {
                    edit.create_vertex(
                        VId(99),
                        vec![],
                        vec![(B, CanonicalScalar::Int(9)), (N, CanonicalScalar::Int(11))],
                    );
                }
            }
            db.write(&commit, edit).await.unwrap();
            for ((text, handle), rows) in texts.iter().zip(&handles).zip(&mut integrated) {
                let delta = db.standing_group_delta(&cx, handle).unwrap().unwrap();
                rows.integrate(delta.rows(), LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&*rows, db.standing_query(&cx, handle).unwrap().rows());
                matches_snapshot(&db, &cx, text, &args, handle);
            }
        }
        db.compact(&commit).await.unwrap();
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_group_delta(&cx, handle).unwrap().is_none());
            matches_snapshot(&db, &cx, text, &args, handle);
        }
        // Delivery refusal is not a maintenance failure.
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &handles[0],
                GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        matches_snapshot(&db, &cx, texts[0], &args, &handles[0]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_dynamic_numeric_failure_fences_only_its_view_and_rebuild_repairs_the_circuit() {
    let ((), report) = run_async_under_lab(0x4c47_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let healthy = "MATCH (n) WITH [n.amount] AS xs UNWIND xs AS x RETURN COUNT(DISTINCT x) AS kinds,MIN(x) AS low,MAX(x) AS high";
        let numeric = db
            .register_standing_native(&cx, NINE, &args, symbols, policy())
            .unwrap();
        let other = db
            .register_standing_native(&cx, healthy, &args, symbols, policy())
            .unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(u128::MAX), N, Some(CanonicalScalar::Bool(true)));
        let durable = db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), durable);
        assert!(matches!(
            db.standing_native_query(&cx, &numeric, policy()),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::NonIntegerAggregate { .. },
                ..
            })
        ));
        assert!(db.query(&cx, NINE, &args, symbols, policy()).is_err());
        matches_snapshot(&db, &cx, healthy, &args, &other);
        assert!(db.rebuild_standing_query(&cx, &numeric, policy()).is_err());
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(u128::MAX), N, Some(CanonicalScalar::Int(13)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &numeric, policy()).is_err());
        db.rebuild_standing_query(&cx, &numeric, policy()).unwrap();
        assert!(db.standing_group_delta(&cx, &numeric).unwrap().is_none());
        matches_snapshot(&db, &cx, NINE, &args, &numeric);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        db.write(&commit, edit).await.unwrap();
        matches_snapshot(&db, &cx, NINE, &args, &numeric);
        matches_snapshot(&db, &cx, healthy, &args, &other);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_expansions_keep_global_nulls_and_zero_pages_never_hide_operand_errors() {
    let ((), report) = run_async_under_lab(0x4c47_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n) WITH [] AS xs UNWIND xs AS x RETURN SUM(x) AS total,AVG(x) AS average,COUNT(*) AS count",
            "MATCH (n) WITH [] AS xs UNWIND xs AS x RETURN x,COUNT(*) AS count GROUP BY x",
            "MATCH (n) WITH [NULL,NULL] AS xs UNWIND xs AS x RETURN SUM(DISTINCT x) AS total,AVG(DISTINCT x) AS average,COUNT(DISTINCT x) AS count",
        ] {
            let handle = db
                .register_standing_native(&cx, text, &args, symbols, policy())
                .unwrap();
            matches_snapshot(&db, &cx, text, &args, &handle);
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            matches_snapshot(&db, &cx, text, &args, &handle);
        }
        for tail in ["", " HAVING count < 0", " LIMIT 0"] {
            let text = format!(
                "MATCH (n) WITH [[n.amount]] AS xs UNWIND xs AS x RETURN COUNT(*) AS count,SUM(x) AS total{tail}"
            );
            let prepared = PreparedNativeRead::prepare(&text, &args, symbols).unwrap();
            assert!(matches!(
                prepared.register_standing(&mut db, &cx, &args, policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::NonIntegerAggregate { .. }
                ))
            ));
            assert!(prepared.execute(&db, &cx, &args, policy()).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_pipeline_parameters_are_frozen_and_reopening_rebuilds_from_storage() {
    let ((), report) = run_async_under_lab(0x4c47_0104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let directory = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &directory, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n) WITH [n.amount,$extra] AS xs UNWIND xs AS x RETURN SUM(x) AS total,COUNT(DISTINCT x) AS kinds";
        let mut args = GqlParameters::new().with_int64("extra", 3).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        let first = prepared
            .register_standing(&mut db, &cx, &args, policy())
            .unwrap();
        args = GqlParameters::new().with_int64("extra", 17).unwrap();
        let second = prepared
            .register_standing(&mut db, &cx, &args, policy())
            .unwrap();
        drop(prepared);
        let old = GqlParameters::new().with_int64("extra", 3).unwrap();
        assert_ne!(
            db.standing_native_query(&cx, &first, policy()).unwrap(),
            db.standing_native_query(&cx, &second, policy()).unwrap()
        );
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(2));
        db.write(&commit, edit).await.unwrap();
        matches_snapshot(&db, &cx, text, &old, &first);
        matches_snapshot(&db, &cx, text, &args, &second);
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &directory, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_query(&cx, &first),
            Err(StandingQueryError::ForeignHandle)
        ));
        let current = db
            .register_standing_native(&cx, text, &args, symbols, policy())
            .unwrap();
        matches_snapshot(&db, &cx, text, &args, &current);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
