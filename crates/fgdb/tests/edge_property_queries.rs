// The lab body's Send check nests past the default depth (next trait solver).
#![recursion_limit = "256"]

//! Relationship properties preserve the edge identity domain (fgdb-r02v).
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, MemVfs, QueryError, QueryResult, QueryWriteError,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
#[test]
fn return_relationship_property_reads_edge_not_source_vertex() {
    let ((), report) = run_async_under_lab(0x2a01_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(99))]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.ensure_edge_by_triple(
            EId(1),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(2003))],
        );
        db.write(&commit, batch).await.unwrap();
        let result = db
            .query(
                &query,
                "MATCH (a)-[r:R]->(b) RETURN r.p AS p",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .expect("fixed-length relationship property must execute");
        assert_eq!(
            result,
            QueryResult::Rows {
                columns: vec!["p".to_owned()],
                rows: vec![vec![GraphAggregateValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(2003)
                ))]],
            }
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn scalar(value: CanonicalScalar) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::Scalar(value))
}
fn edge_property(edge: &EdgeRecord) -> CanonicalScalar {
    edge.props
        .iter()
        .find(|(key, _)| *key == P)
        .map_or(CanonicalScalar::Null, |(_, value)| value.clone())
}
fn query_rows(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
) -> Vec<Vec<GraphAggregateValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, params, symbols, policy())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
    else {
        panic!("read produced a write receipt")
    };
    rows
}
fn assert_history(db: &Database<MemVfs>, cx: &QueryCx, seq: CommitSeq) {
    // Independent oracle: materialized edge records at this exact sequence,
    // never the engine's binding/projection/reducer implementations.
    let edges = db.edges_at(seq).unwrap();
    let mut values: Vec<_> = edges
        .iter()
        .filter(|e| e.entry.relation == R)
        .map(edge_property)
        .collect();
    values.sort_by(|a, b| match (a, b) {
        (CanonicalScalar::Null, CanonicalScalar::Null) => core::cmp::Ordering::Equal,
        (CanonicalScalar::Null, _) => core::cmp::Ordering::Greater,
        (_, CanonicalScalar::Null) => core::cmp::Ordering::Less,
        (CanonicalScalar::Int(a), CanonicalScalar::Int(b)) => a.cmp(b),
        _ => panic!("integer fixture"),
    });
    let head = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $at";
    assert_eq!(
        query_rows(
            db,
            cx,
            &format!("{head} RETURN r.p AS p ORDER BY r.p"),
            &GqlParameters::new().with_uint64("at", seq.0).unwrap()
        ),
        values
            .iter()
            .cloned()
            .map(|v| vec![scalar(v)])
            .collect::<Vec<_>>(),
        "sequence {seq:?}"
    );
    let positive: Vec<_> = values
        .iter()
        .filter(|v| matches!(v, CanonicalScalar::Int(n) if *n > 0))
        .cloned()
        .map(|v| vec![scalar(v)])
        .collect();
    let literal = query_rows(
        db,
        cx,
        &format!("{head} WHERE r.p > 0 RETURN r.p AS p ORDER BY p"),
        &GqlParameters::new().with_uint64("at", seq.0).unwrap(),
    );
    assert_eq!(literal, positive);
    // The parameter form must select the same snapshot as the literal form:
    // both carry $at, only the comparison operand differs.
    let both = GqlParameters::new()
        .with_uint64("at", seq.0)
        .unwrap()
        .with_int64("floor", 0)
        .unwrap();
    let observed = query_rows(
        db,
        cx,
        &format!("{head} WHERE r.p > $floor RETURN r.p AS p ORDER BY p"),
        &both,
    );
    let cell = |row: &Vec<GraphAggregateValue>| {
        row[0]
            .as_value()
            .and_then(GraphValue::as_scalar)
            .and_then(|s| match s {
                CanonicalScalar::Int(n) => Some(*n),
                _ => None,
            })
    };
    assert_eq!(
        observed.iter().map(&cell).collect::<Vec<_>>(),
        positive.iter().map(&cell).collect::<Vec<_>>()
    );
    assert_eq!(observed, positive);
    let integers: Vec<i64> = values
        .iter()
        .filter_map(|v| match v {
            CanonicalScalar::Int(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert!(!integers.is_empty());
    let sum: i128 = integers.iter().map(|n| i128::from(*n)).sum();
    let rows = query_rows(
        db,
        cx,
        &format!(
            "{head} RETURN SUM(r.p) AS total,AVG(r.p) AS mean,MIN(r.p) AS least,MAX(r.p) AS greatest,COLLECT(r.p) AS items"
        ),
        &GqlParameters::new().with_uint64("at", seq.0).unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_integer(), Some(sum));
    let mean = rows[0][1].as_average().expect("exact average");
    assert_eq!(
        mean.numerator() * integers.len() as i128,
        sum * i128::from(mean.denominator())
    );
    assert_eq!(
        rows[0][2],
        scalar(CanonicalScalar::Int(*integers.first().unwrap()))
    );
    assert_eq!(
        rows[0][3],
        scalar(CanonicalScalar::Int(*integers.last().unwrap()))
    );
    let collected = rows[0][4]
        .as_value()
        .and_then(GraphValue::as_list)
        .expect("COLLECT list");
    let mut collected: Vec<_> = collected
        .iter()
        .map(|v| match v.as_scalar() {
            Some(CanonicalScalar::Int(n)) => *n,
            _ => panic!("COLLECT must omit NULL"),
        })
        .collect();
    collected.sort_unstable();
    assert_eq!(collected, integers);
}

#[test]
fn generated_edge_property_history_matches_independent_records() {
    for seed in [0x731_u64, 0xb29, 0xf47] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut state = seed;
            let mut batch = WriteBatch::new(R);
            for id in 1..=7 {
                batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(-999))]);
            }
            for i in 0..6 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let props = match i {
                    0 => vec![],
                    1 => vec![(P, CanonicalScalar::Null)],
                    2 => vec![(P, CanonicalScalar::Int(30))],
                    _ => vec![(P, CanonicalScalar::Int((state % 41) as i64 - 10))],
                };
                batch.add_edge(EId(i + 1), VId(1), VId(i + 2), props);
            }
            let first = db.write(&commit, batch).await.unwrap();
            assert_eq!(
                db.edges_at(first)
                    .unwrap()
                    .iter()
                    .filter(|e| edge_property(e) == CanonicalScalar::Null)
                    .count(),
                2
            );
            assert_history(&db, &cx, first);
            let text = "MATCH (a)-[r:R]->(b) RETURN r.p AS p";
            // Each selected edge is one admitted snapshot record; a property
            // does not earn a second allowance or bypass edge admission.
            assert!(
                matches!(db.query(&cx, text, &GqlParameters::new(), symbols, GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)),
                Err(QueryError::Pattern(GqlQueryError::Rows(error))) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.limit == 0)
            );
            for (script, remove) in [
                (
                    "MATCH (a)-[r:R]->(b) WHERE r.p IS NOT NULL SET r.p = r.p + 7",
                    false,
                ),
                ("MATCH (a)-[r:R]->(b) WHERE r.p > 20 REMOVE r.p", true),
            ] {
                let before = db.frontier().unwrap();
                let mut expected = db.edges_at(before).unwrap();
                let mut changed = 0;
                for edge in &mut expected {
                    if let CanonicalScalar::Int(n) = edge_property(edge)
                        && (!remove || n > 20)
                    {
                        changed += 1;
                        edge.props = if remove {
                            vec![]
                        } else {
                            vec![(P, CanonicalScalar::Int(n + 7))]
                        };
                    }
                }
                assert!(changed > 0, "each seed must exercise each mutation");
                db.query_write(
                    &txcx,
                    &cx,
                    &commit,
                    script,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    GraphWriteProgramPolicy::new(policy(), 100, 100, 100),
                    |_| -> Result<fgdb_delta_types::ElementId, core::convert::Infallible> {
                        panic!("SET/REMOVE must not allocate identities")
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{script}: {error:?}"));
                let after = db.frontier().unwrap();
                assert_eq!(after.0, before.0 + 1);
                let actual = db.edges_at(after).unwrap();
                assert_eq!(
                    actual
                        .iter()
                        .map(|e| (e.entry.eid, &e.props))
                        .collect::<Vec<_>>(),
                    expected
                        .iter()
                        .map(|e| (e.entry.eid, &e.props))
                        .collect::<Vec<_>>()
                );
                assert_history(&db, &cx, first);
                assert_history(&db, &cx, after);
            }
            let mut deleted = WriteBatch::new(R);
            deleted.delete_edge(EId(6));
            let last = db.write(&commit, deleted).await.unwrap();
            assert_eq!(db.edges_at(last).unwrap().len(), 5);
            assert_history(&db, &cx, first);
            assert_history(&db, &cx, last);
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_history(&reopened, &cx, first);
            assert_history(&reopened, &cx, last);
        });
        assert!(report.lab_test_passed(), "seed={seed}: {report:?}");
    }
}

#[test]
fn variable_length_relationship_set_is_typed_refused() {
    let ((), report) = run_async_under_lab(0x2a04_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=3 {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        batch.add_edge(EId(1), VId(1), VId(2), vec![(P, CanonicalScalar::Int(1))]);
        batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        db.write(&commit, batch).await.unwrap();
        let before = db.frontier().unwrap();
        let edges = db.edges_at(before).unwrap();
        let error = db
            .query_write(
                &txcx,
                &cx,
                &commit,
                "MATCH WALK (a)-[r:R*1..2]->(b) SET r.p = 5",
                &GqlParameters::new(),
                symbols,
                R,
                GraphWriteProgramPolicy::new(policy(), 100, 100, 100),
                |_| -> Result<fgdb_delta_types::ElementId, core::convert::Infallible> {
                    panic!("refused before staging")
                },
            )
            .await
            .expect_err("variable-length property target must be refused");
        assert!(
            matches!(error, QueryWriteError::Prepare(_)),
            "expected typed prepare refusal: {error:?}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(db.edges_at(before).unwrap(), edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
