//! Text-only internal summaries share canonical sources and their observations.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cmp::Ordering;
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32]) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 2_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn template(only_keys: bool, paginated: bool) -> PreparedGraphAggregateText {
    let columns = if only_keys { "a" } else { "a,COUNT(*) AS paths" };
    let page = if paginated { " LIMIT $take" } else { "" };
    PreparedGraphAggregateText::prepare(&format!(
        "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) RETURN {columns} GROUP BY a \
         HAVING COUNT(b.p) >= $minimum OR AVG(b.p) IS NULL \
         ORDER BY AVG_INT(b.p) DESC NULLS LAST,SUM(DISTINCT b.p) DESC,a{page}"), symbols).unwrap()
}
fn query(only_keys: bool, count: Option<u64>) -> PreparedGraphAggregate {
    let mut args = GqlParameters::new().with_int64("minimum", 2).unwrap();
    if let Some(count) = count { args = args.with_uint64("take", count).unwrap(); }
    template(only_keys, count.is_some()).bind_parameters(&args).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 0..4 { batch.create_vertex(VId(id), vec![OWNER], vec![]); }
    for (id, value) in [(10, i64::MAX - 1), (11, i64::MAX), (12, 3), (13, 5)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    for (id, owner, destination) in [(1, 0, 10), (2, 0, 11), (3, 0, 11),
        (4, 1, 11), (5, 3, 12), (6, 3, 12), (7, 3, 13)] {
        batch.add_edge(EId(id), VId(owner), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}

// Independent owned-record enumeration retains no private engine state.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], minimum: i64) -> Vec<(VId, u64)> {
    let mut output = Vec::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&OWNER)) {
        let mut paths = 0_u64;
        let mut values = Vec::new();
        for edge in edges.iter().filter(|edge| edge.entry.relation == R && edge.entry.src == owner.vid) {
            paths += 1;
            let destination = vertices.iter().find(|row| row.vid == edge.entry.dst).unwrap();
            if let Some((_, CanonicalScalar::Int(value))) = destination.props.iter().find(|(key, _)| *key == P) {
                values.push(*value);
            }
        }
        if !values.is_empty() && (values.len() as i128) < i128::from(minimum) { continue; }
        let mean = (!values.is_empty()).then(|| (
            values.iter().map(|n| i128::from(*n)).sum::<i128>(), values.len() as i128,
        ));
        let total = (!values.is_empty()).then(|| values.iter().copied().collect::<BTreeSet<_>>()
            .into_iter().map(i128::from).sum::<i128>());
        output.push((owner.vid, paths.max(1), mean, total));
    }
    output.sort_by(|a, b| {
        // At most three values per fixture group: these independent products
        // fit i128 even with i64::MAX inputs; no production ratio helper used.
        let order = match (a.2, b.2) {
            (None, None) => Ordering::Equal, (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some((a, na)), Some((b, nb))) => (b * na).cmp(&(a * nb)),
        };
        order.then_with(|| b.3.cmp(&a.3)).then_with(|| a.0.cmp(&b.0))
    });
    output.into_iter().map(|row| (row.0, row.1)).collect()
}
fn check(rows: &[GraphAggregateRow], expected: &[(VId, u64)], only_keys: bool) {
    assert_eq!(rows.len(), expected.len());
    for (row, (owner, paths)) in rows.iter().zip(expected) {
        assert_eq!(row.keys().len(), 1);
        assert_eq!(row.keys()[0].as_vertex(), Some(*owner));
        assert_eq!(row.values().len(), usize::from(!only_keys));
        if !only_keys { assert_eq!(row.get(0).unwrap().as_count(), Some(*paths)); }
    }
}

#[test]
fn private_text_summaries_cover_all_reads_canonical_staging_and_reopened_history() {
    let ((), report) = run_async_under_lab(0x1dde_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 2);
        assert_eq!(old, vec![(VId(0), 3), (VId(3), 3), (VId(2), 1)]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for only_keys in [false, true] {
            let plan = query(only_keys, None);
            assert_eq!(plan.evaluation_aggregate_columns().len(), if only_keys { 3 } else { 4 });
            for result in [db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap(),
                db.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap(),
                pinned.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap(),
                pinned.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap(),
                txn.execute_graph_aggregate_governed(&db, &cx, &plan, wide()).unwrap()] { check(&result.value, &old, only_keys); }
        }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(1)); changes.ensure_edge_by_triple(EId(999), VId(0), VId(11), vec![]);
        changes.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(7)));
        changes.set_vertex_property(VId(13), P, None);
        changes.add_edge(EId(90), VId(2), VId(12), vec![]); changes.delete_vertex(VId(10));
        txn.write(&mut db, changes).unwrap(); assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), 2);
        assert_eq!(expected, vec![(VId(0), 2), (VId(3), 3)]);
        for only_keys in [false, true] {
            let plan = query(only_keys, None);
            check(&txn.execute_graph_aggregate_governed(&db, &cx, &plan, wide()).unwrap().value, &expected, only_keys);
            check(&db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &old, only_keys);
        }
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for only_keys in [false, true] {
            let template = template(only_keys, false);
            let plan = template.bind_parameters(&GqlParameters::new().with_int64("minimum", 2).unwrap()).unwrap();
            let frozen = plan.canonical_bytes();
            check(&reopened.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &expected, only_keys);
            check(&reopened.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap().value, &old, only_keys);
            check(&pinned.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &old, only_keys);
            let less = template.bind_parameters(&GqlParameters::new().with_int64("minimum", 1).unwrap()).unwrap();
            assert_ne!(less.canonical_bytes(), frozen); assert_eq!(plan.canonical_bytes(), frozen);
            let relaxed = oracle(&reopened.vertices().unwrap(), &reopened.edges().unwrap(), 1);
            check(&reopened.execute_graph_aggregate_governed(&cx, &less, wide()).unwrap().value, &relaxed, only_keys);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rejected_hidden_groups_keep_observations_after_output_refusal_and_zero_pages() {
    let ((), report) = run_async_under_lab(0x1dde_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for only_keys in [false, true] { for mode in 0..3 { for change in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap(); let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(777), vec![], vec![]); txn.write(&mut db, stage).unwrap();
            let plan = query(only_keys, Some(u64::from(mode != 2)));
            let result = txn.execute_graph_aggregate_governed(&db, &cx, &plan,
                GqlQueryPolicy::new(1000, u64::from(mode == 0), 2_000_000, 1_000_000));
            match mode {
                0 => check(&result.unwrap().value, &[(VId(0), 3)], only_keys),
                1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                _ => assert!(result.unwrap().value.is_empty()),
            }
            let mut winner = WriteBatch::new(R);
            match change {
                0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                1 => { winner.add_edge(EId(90), VId(1), VId(11), vec![]); }
                2 => {
                    winner.set_vertex_property(VId(12), P, Some(CanonicalScalar::Int(i64::MAX)));
                    winner.set_vertex_property(VId(13), P, Some(CanonicalScalar::Int(i64::MAX)));
                }
                3 => { winner.delete_edge(EId(1)); }
                _ => { winner.set_vertex_property(VId(11), P, None); }
            }
            db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
            // Commit immediately. No extra txn read may restore a dependency
            // lost while hiding a summary or rejecting a completed group.
            let result = txn.commit(&mut db, &commit).await;
            if change == 0 { result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some()); }
            else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier); assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}}
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_summary_work_is_shared_but_output_limits_count_only_returned_rows() {
    let ((), report) = run_async_under_lab(0x1dde_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        for only_keys in [false, true] {
            let plan = query(only_keys, Some(2));
            let measured = db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap();
            check(&measured.value, &[(VId(0), 3), (VId(3), 3)], only_keys);
            let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, 2,
                measured.evaluator.work_units, measured.evaluator.scratch_entries);
            assert_eq!(db.execute_graph_aggregate_governed(&cx, &plan, exact).unwrap(), measured);
            for policy in [GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 2, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(1000, 1, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(1000, 2, measured.evaluator.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(1000, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
                assert!(db.execute_graph_aggregate_governed(&cx, &plan, policy).is_err());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
