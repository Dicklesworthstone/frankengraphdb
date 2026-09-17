//! Computed RETURN uses real Chronicle/Strata sources and canonical overlays.
//! The oracle derives hop-layer bags from stored edges, then subtracts counts.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerErrorKind, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::cell::Cell;
use std::collections::BTreeMap;

const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const TEXT: &str = "MATCH WALK (a:A)-[:R*0..2]->(b) \
    RETURN ABS(COALESCE(b.p,0))*$scale AS score,b.p/NULLIF(b.q,0) AS ratio \
    EXCEPT ALL MATCH (n:B) RETURN COALESCE(n.p,0)*$scale AS other,n.p/NULLIF(n.q,0) AS value \
    ORDER BY score DESC,ratio NULLS LAST SKIP $skip LIMIT $limit";
type Plain = (i64, Option<i64>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 5_000_000, 2_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn arguments(skip: u64, limit: u64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("scale", 2)
        .unwrap()
        .with_uint64("skip", skip)
        .unwrap()
        .with_uint64("limit", limit)
        .unwrap()
}
fn query(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, label, p, q) in [
        (1, A, Some(3), 2),
        (2, A, Some(-3), 0),
        (3, A, None, 4),
        (4, B, None, 2),
        (5, B, Some(10), 2),
    ] {
        let mut props = vec![(Q, CanonicalScalar::Int(q))];
        if let Some(p) = p {
            props.push((P, CanonicalScalar::Int(p)));
        } else if id == 3 {
            props.push((P, CanonicalScalar::Null));
        }
        props.sort_by_key(|(key, _)| *key);
        batch.create_vertex(VId(id), vec![label], props);
    }
    for (id, source, target) in [(101, 1, 2), (102, 1, 2), (103, 2, 3)] {
        batch.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn changes() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge(EId(102));
    for (id, p, q) in [(2, 4, 2), (3, 5, 1), (4, 5, 1)] {
        batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(p)));
        batch.set_vertex_property(VId(id), Q, Some(CanonicalScalar::Int(q)));
    }
    batch
}
fn integer(row: &VertexRow, key: PropertyKeyId) -> Option<i64> {
    match row
        .props
        .iter()
        .find(|(property, _)| *property == key)
        .map(|(_, value)| value)
    {
        Some(CanonicalScalar::Int(value)) => Some(*value),
        Some(CanonicalScalar::Null) | None => None,
        _ => panic!("unexpected oracle input kind"),
    }
}
fn scalar_row(row: &VertexRow, absolute: bool) -> Plain {
    let p = integer(row, P);
    let value = i128::from(p.unwrap_or(0));
    let score = i64::try_from((if absolute { value.abs() } else { value }) * 2).unwrap();
    let ratio = p
        .zip(integer(row, Q).filter(|value| *value != 0))
        .map(|(p, q)| p / q);
    (score, ratio)
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut left = BTreeMap::<Plain, u64>::new();
    for root in vertices.iter().filter(|row| row.labels.contains(&A)) {
        let mut layer = BTreeMap::from([(root.vid, 1_u64)]);
        for depth in 0..=2 {
            for (&target, &count) in &layer {
                let row = vertices.iter().find(|row| row.vid == target).unwrap();
                *left.entry(scalar_row(row, true)).or_default() += count;
            }
            if depth == 2 {
                break;
            }
            let mut next = BTreeMap::<VId, u64>::new();
            for (source, count) in layer {
                for edge in edges
                    .iter()
                    .filter(|edge| edge.entry.relation == R && edge.entry.src == source)
                {
                    *next.entry(edge.entry.dst).or_default() += count;
                }
            }
            layer = next;
        }
    }
    for row in vertices.iter().filter(|row| row.labels.contains(&B)) {
        if let Some(count) = left.get_mut(&scalar_row(row, false)) {
            *count = count.saturating_sub(1);
        }
    }
    let mut output = Vec::new();
    for (row, count) in left {
        for _ in 0..count {
            output.push(row);
        }
    }
    output.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| match (a.1, b.1) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(&b),
        })
    });
    output
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter()
        .map(|row| {
            assert_eq!(row.len(), 2, "private source columns must not escape");
            let CanonicalScalar::Int(score) = row.values()[0].as_scalar().unwrap() else {
                panic!("score kind")
            };
            let ratio = match row.values()[1].as_scalar().unwrap() {
                CanonicalScalar::Int(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => panic!("ratio kind"),
            };
            (*score, ratio)
        })
        .collect()
}

#[test]
fn computed_walk_difference_reads_staged_values_and_retains_history_after_reopen() {
    let ((), report) = run_async_under_lab(0xc011_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let calls = Cell::new(0);
        let template = PreparedGraphSetText::prepare(TEXT, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.get(), 5);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let plan = template.bind_parameters(&arguments(0, 100)).unwrap();
        assert_eq!(plan.columns(), &["score", "ratio"]);
        let frozen = plan.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(
            old,
            vec![
                (6, Some(1)),
                (6, None),
                (6, None),
                (6, None),
                (0, None),
                (0, None),
                (0, None)
            ]
        );
        let mut txn = db.begin(&txcx).unwrap();
        for result in [
            db.execute_graph_set_governed(&cx, &plan, policy()).unwrap(),
            db.execute_graph_set_governed_at(&cx, &plan, basis, policy())
                .unwrap(),
            pinned
                .execute_graph_set_governed(&cx, &plan, policy())
                .unwrap(),
            pinned
                .execute_graph_set_governed_at(&cx, &plan, basis, policy())
                .unwrap(),
            txn.execute_graph_set_governed(&db, &cx, &plan, policy())
                .unwrap(),
        ] {
            assert_eq!(plain(&result.value), old);
        }
        let page = template.bind_parameters(&arguments(1, 2)).unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_set_governed(&cx, &page, policy())
                    .unwrap()
                    .value
            ),
            old[1..3]
        );
        assert_eq!(calls.get(), 5, "binding must not reopen the catalog");
        txn.write(&mut db, changes()).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(
            expected,
            vec![
                (10, Some(5)),
                (10, Some(5)),
                (8, Some(2)),
                (8, Some(2)),
                (6, Some(1))
            ]
        );
        assert_eq!(
            plain(
                &txn.execute_graph_set_governed(&db, &cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            plain(
                &db.execute_graph_set_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_set_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_set_governed_at(&cx, &plan, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            plain(
                &pinned
                    .execute_graph_set_governed(&cx, &plan, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(plan.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_or_zero_output_computed_reads_retain_rejected_candidate_and_phantom_dependencies() {
    let ((), report) = run_async_under_lab(0xc011_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        for mode in 0..4 {
            for insertion in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                let plan = query(match mode {
                    2 => "MATCH (n:A) WHERE n.p>=0 RETURN ABS(n.p) AS score LIMIT 0",
                    3 => "MATCH (n:A) WHERE n.p>=0 RETURN n.p/(n.p-3) AS score",
                    _ => "MATCH (n:A) WHERE n.p>=0 RETURN ABS(n.p) AS score",
                });
                let budget =
                    GqlQueryPolicy::new(10_000, u64::from(mode != 1), 5_000_000, 2_000_000);
                let result = txn.execute_graph_set_governed(&db, &cx, &plan, budget);
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 1),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    2 => assert!(result.unwrap().value.is_empty()),
                    _ => assert!(
                        matches!(result,Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error,.. }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero)
                    ),
                }
                let mut winner = WriteBatch::new(R);
                if insertion {
                    winner.create_vertex(VId(6), vec![A], vec![(P, CanonicalScalar::Int(7))]);
                } else {
                    winner.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // Do not run another transaction read: it could repair a missed
                // observation and make this dependency regression vacuous.
                assert!(matches!(
                    txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_computed_read_limits_and_source_authority_precede_result_publication() {
    let ((), report) = run_async_under_lab(0xc011_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let plan = PreparedGraphSetText::prepare(TEXT, symbols)
            .unwrap()
            .bind_parameters(&arguments(1, 2))
            .unwrap();
        let measured = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(measured.rows.result_rows, 2);
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &plan, exact).unwrap(),
            measured
        );
        for refused in [
            GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(10_000, 2, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] {
            assert!(db.execute_graph_set_governed(&cx, &plan, refused).is_err());
        }
        assert_eq!(
            db.frontier().unwrap(),
            basis,
            "computed reads cannot publish a write"
        );
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            txn.execute_graph_set_governed(&foreign, &cx, &plan, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        assert!(matches!(
            db.execute_graph_set_governed_at(&cx, &plan, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
