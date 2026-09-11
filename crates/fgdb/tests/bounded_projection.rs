//! Finite query pages must agree with an independent owned-row full-sort oracle.
//! Discarding a projected candidate never removes its transaction dependency.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow,
    WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const RANK: PropertyKeyId = PropertyKeyId(1);
type Plain = (Option<i64>, VId, VId);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property, "rank") => Some(GraphSymbol::Property(RANK)),
        _ => None,
    }
}
fn pattern(distinct: bool, offset: u64, count: u64) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!(
        "MATCH (a:Owner)-[:R]->(b) RETURN {} b.rank AS rank,a,b SKIP {offset} LIMIT {count}",
        if distinct { "DISTINCT" } else { "ALL" }), symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(0), vec![OWNER], vec![]);
    for (vid, rank) in [(10, 30), (11, 20), (12, 10)] {
        batch.create_vertex(VId(vid), vec![], vec![(RANK, CanonicalScalar::Int(rank))]);
    }
    for (eid, dst) in [(10, 10), (11, 10), (12, 11), (13, 12)] {
        batch.add_edge(EId(eid), VId(0), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], distinct: bool, offset: usize, count: usize) -> Vec<Plain> {
    let mut rows = Vec::new();
    for edge in edges.iter().filter(|edge| edge.entry.relation == R) {
        let source = vertices.iter().find(|row| row.vid == edge.entry.src).unwrap();
        if !source.labels.contains(&OWNER) { continue; }
        let destination = vertices.iter().find(|row| row.vid == edge.entry.dst).unwrap();
        let rank = destination.props.iter().find_map(|(key, value)| match value {
            CanonicalScalar::Int(value) if *key == RANK => Some(*value),
            _ => None,
        });
        rows.push((rank, source.vid, destination.vid));
    }
    rows.sort();
    if distinct { rows.dedup(); }
    rows.into_iter().skip(offset).take(count).collect()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let rank = match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value), CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture scalar"),
        };
        (rank, row.get(1).unwrap().as_vertex().unwrap(), row.get(2).unwrap().as_vertex().unwrap())
    }).collect()
}

#[test]
fn canonical_pages_cover_all_read_surfaces_staging_and_reopened_history() {
    let ((), report) = run_async_under_lab(0x70f0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let view = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let vertices = db.vertices().unwrap(); let edges = db.edges().unwrap();
        for distinct in [false, true] {
            for (offset, count) in [(0, 0), (0, 1), (1, 2), (2, 2), (u64::MAX, 1), (1, u64::MAX)] {
                let pattern = pattern(distinct, offset, count);
                let expected = oracle(&vertices, &edges, distinct,
                    usize::try_from(offset).unwrap_or(usize::MAX), usize::try_from(count).unwrap_or(usize::MAX));
                for result in [db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap(),
                    db.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap(),
                    view.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap(),
                    view.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap(),
                    txn.execute_graph_pattern_governed(&db, &cx, &pattern, wide()).unwrap()] {
                    assert_eq!(plain(&result.value), expected);
                }
            }
        }
        let pattern = pattern(false, 1, 2); let frozen = pattern.canonical_bytes();
        let old = oracle(&vertices, &edges, false, 1, 2);
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(10));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.set_vertex_property(VId(10), RANK, Some(CanonicalScalar::Int(-1)));
        changes.set_vertex_property(VId(11), RANK, None);
        changes.create_vertex(VId(13), vec![], vec![(RANK, CanonicalScalar::Int(0))]);
        changes.add_edge(EId(14), VId(0), VId(13), vec![]);
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), false, 1, 2);
        assert_eq!(expected, vec![(Some(-1), VId(0), VId(10)), (Some(0), VId(0), VId(13))]);
        assert_ne!(expected, old);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &pattern, wide()).unwrap().value), expected);
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), expected);
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &pattern, basis, wide()).unwrap().value), old);
        assert_eq!(plain(&view.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap().value), old);
        assert_eq!(pattern.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn excluded_candidates_and_refused_or_zero_pages_keep_conflict_dependencies() {
    let ((), report) = run_async_under_lab(0x70f0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let pattern = pattern(false, 0, u64::from(mode != 2));
                let result = txn.execute_graph_pattern_governed(&db, &cx, &pattern,
                    GqlQueryPolicy::new(100, u64::from(mode == 0), 1_000_000, 1_000_000));
                match mode {
                    0 => assert_eq!(plain(&result.unwrap().value), vec![(Some(10), VId(0), VId(12))]),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => { winner.create_vertex(VId(77), vec![], vec![]); }
                    1 => { winner.set_vertex_property(VId(10), RANK, Some(CanonicalScalar::Int(-5))); }
                    _ => {
                        winner.create_vertex(VId(13), vec![], vec![(RANK, CanonicalScalar::Int(-10))]);
                        winner.add_edge(EId(14), VId(0), VId(13), vec![]);
                    }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // No further transaction read may repair a dependency lost
                // while discarding a row or refusing the earlier page.
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap(); assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn page_sources_keep_exact_limits_and_owner_and_snapshot_error_precedence() {
    let ((), report) = run_async_under_lab(0x70f0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let pattern = pattern(false, 1, 2);
        let measured = db.execute_graph_pattern_governed(&cx, &pattern, wide()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_pattern_governed(&cx, &pattern, exact).unwrap(), measured);
        for cap in [GqlQueryPolicy::new(100, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(100, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(100, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_pattern_governed(&cx, &pattern, cap).is_err());
        }
        let txn = db.begin(&txn_cx).unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(txn.execute_graph_pattern_governed(&foreign, &cx, &pattern, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        assert!(matches!(db.execute_graph_pattern_governed_at(&cx, &pattern, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &pattern, wide()).unwrap().value), plain(&measured.value));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
