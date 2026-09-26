//! Compound text on the real Chronicle/Strata and transaction-overlay paths.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;

const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const TEXT: &str = "(MATCH WALK (a:A)-[:R*0..2]->(b) WHERE b.p >= $min RETURN b.p AS value \
    EXCEPT ALL MATCH (c:B) WHERE c.p >= $min RETURN c.p AS other) \
    ORDER BY value DESC SKIP $skip LIMIT $limit";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn query(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, labels, value) in [
        (1, vec![A], Some(10)),
        (2, vec![], Some(20)),
        (3, vec![B], Some(30)),
        (4, vec![A], Some(40)),
        (5, vec![B], Some(20)),
        (6, vec![B], None),
    ] {
        batch.create_vertex(
            VId(id),
            labels,
            vec![(P, value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))],
        );
    }
    for (id, src, dst) in [(101, 1, 2), (102, 1, 2), (103, 2, 3)] {
        batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn scalar(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(n) if *key == P => Some(*n),
        _ => None,
    })
}
fn plain(rows: &[GraphValueRow]) -> Vec<Option<i64>> {
    rows.iter()
        .map(|row| match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Null => None,
            CanonicalScalar::Int(n) => Some(*n),
            _ => panic!("unexpected fixture scalar"),
        })
        .collect()
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Option<i64>> {
    let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
    for root in vertices.iter().filter(|row| row.labels.contains(&A)) {
        let mut layer = vec![root.vid];
        for depth in 0..=2 {
            for vid in &layer {
                let row = vertices.iter().find(|row| row.vid == *vid).unwrap();
                if let Some(n) = scalar(row).filter(|n| *n >= 0) {
                    *counts.entry(n).or_default() += 1;
                }
            }
            if depth == 2 {
                break;
            }
            layer = layer
                .into_iter()
                .flat_map(|source| {
                    edges
                        .iter()
                        .filter(move |edge| edge.entry.relation == R && edge.entry.src == source)
                        .map(|edge| edge.entry.dst)
                })
                .collect();
        }
    }
    for row in vertices.iter().filter(|row| row.labels.contains(&B)) {
        if let Some(n) = scalar(row).filter(|n| *n >= 0)
            && let Some(count) = counts.get_mut(&n)
        {
            *count = count.saturating_sub(1);
        }
    }
    counts
        .into_iter()
        .rev()
        .flat_map(|(n, count)| std::iter::repeat_n(Some(n), count))
        .skip(1)
        .take(2)
        .collect()
}

#[test]
fn compound_text_reuses_one_definition_across_snapshots_staged_changes_and_reopen() {
    let ((), report) = run_async_under_lab(0x5e77_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut calls = BTreeMap::new();
        let template = PreparedGraphSetText::prepare(TEXT, |kind, name| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls.values().all(|n| *n == 1));
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let args = GqlParameters::new()
            .with_int64("min", 0)
            .unwrap()
            .with_uint64("skip", 1)
            .unwrap()
            .with_uint64("limit", 2)
            .unwrap();
        let query = template.bind_parameters(&args).unwrap();
        let frozen = query.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(old, vec![Some(30), Some(20)]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [
            db.execute_graph_set_governed(&cx, &query, policy())
                .unwrap(),
            db.execute_graph_set_governed_at(&cx, &query, basis, policy())
                .unwrap(),
            pinned
                .execute_graph_set_governed(&cx, &query, policy())
                .unwrap(),
            pinned
                .execute_graph_set_governed_at(&cx, &query, basis, policy())
                .unwrap(),
            txn.execute_graph_set_governed(&db, &cx, &query, policy())
                .unwrap(),
        ] {
            assert_eq!(plain(&result.value), old);
        }
        let mut changed = WriteBatch::new(R);
        changed.delete_edge(EId(102));
        changed.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(35)));
        changed.create_vertex(VId(7), vec![B], vec![(P, CanonicalScalar::Int(10))]);
        changed.add_edge(EId(104), VId(4), VId(5), vec![]);
        txn.write(&mut db, changed).unwrap();
        let new = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(new, vec![Some(20)]);
        assert_eq!(
            plain(
                &txn.execute_graph_set_governed(&db, &cx, &query, policy())
                    .unwrap()
                    .value
            ),
            new
        );
        assert_eq!(
            plain(
                &db.execute_graph_set_governed(&cx, &query, policy())
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
                    .execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            new
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_set_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            plain(
                &pinned
                    .execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(query.canonical_bytes(), frozen);
        assert_eq!(
            template.bind_parameters(&args).unwrap().canonical_bytes(),
            frozen
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn right_operand_dependencies_survive_zero_output_and_final_budget_refusal() {
    let ((), report) = run_async_under_lab(0x5e77_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let template = PreparedGraphSetText::prepare(
            "MATCH (a:A) RETURN a.p AS value EXCEPT MATCH (b:B) RETURN b.p AS other LIMIT $limit",
            symbols,
        )
        .unwrap();
        for mode in 0..3 {
            for mutation in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let args = GqlParameters::new()
                    .with_uint64("limit", if mode == 2 { 0 } else { 1 })
                    .unwrap();
                let query = template.bind_parameters(&args).unwrap();
                let result = txn.execute_graph_set_governed(
                    &db,
                    &cx,
                    &query,
                    GqlQueryPolicy::new(10_000, u64::from(mode == 0), 10_000_000, 10_000_000),
                );
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 1),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                if mutation == 2 {
                    txn.commit(&mut db, &commit).await.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                    continue;
                }
                let mut winner = WriteBatch::new(R);
                if mutation == 0 {
                    winner.create_vertex(VId(8), vec![B], vec![(P, CanonicalScalar::Int(10))]);
                } else {
                    winner.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(40)));
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No intervening query can repair a missing negative-domain read.
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
fn optional_walks_and_existential_arms_obey_shared_limits_and_authority_fences() {
    let ((), report) = run_async_under_lab(0x5e77_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let optional = query(
            "MATCH (a:A) OPTIONAL MATCH WALK (a)-[:R*1..2]->(b) RETURN b.p AS value \
            UNION ALL MATCH (c:B) RETURN c.p AS other ORDER BY value NULLS FIRST",
        );
        let measured = db
            .execute_graph_set_governed(&cx, &optional, policy())
            .unwrap();
        assert_eq!(
            plain(&measured.value),
            vec![
                None,
                None,
                Some(20),
                Some(20),
                Some(20),
                Some(30),
                Some(30),
                Some(30)
            ]
        );
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records,
            8,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &optional, exact)
                .unwrap(),
            measured
        );
        for refused in [
            GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 8, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 7, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(10_000, 8, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(10_000, 8, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &optional, refused)
                    .is_err()
            );
        }
        let existence = query(
            "MATCH (a:A) WHERE NOT EXISTS { MATCH WALK (a)-[:R*1..2]->(b) WHERE b.p IN [20,30] } \
            RETURN a.p AS value UNION MATCH (c:B) WHERE c.p IS NULL RETURN c.p AS other ORDER BY value NULLS FIRST",
        );
        assert_eq!(
            plain(
                &db.execute_graph_set_governed(&cx, &existence, policy())
                    .unwrap()
                    .value
            ),
            vec![None, Some(40)]
        );
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.execute_graph_set_governed_at(&cx, &optional, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(
            txn.execute_graph_set_governed(&foreign, &cx, &optional, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
