//! Set algebra through the real Chronicle/Strata and transaction read paths.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, MemVfs, ReadError, VertexRow,
    WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError,
    GraphSetOperation as Op, GraphSetQuantifier as Q, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const L: LabelId = LabelId(1);
const R: LabelId = LabelId(2);
const REL: RelationId = RelationId(1);
const N: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Left") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Right") => Some(GraphSymbol::Label(R)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(REL)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn query(operation: Op, quantifier: Q) -> PreparedGraphSet {
    PreparedGraphSet::from(pattern("MATCH (n:Left) RETURN n.n AS value"))
        .combine(operation, quantifier,
            pattern("MATCH (n:Right) RETURN n.n AS other_name").into()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut data = WriteBatch::new(REL);
    for (id, value, label) in [(1, Some(1), L), (2, Some(1), L), (3, Some(2), L),
        (4, None, L), (11, Some(1), R), (12, Some(3), R), (13, None, R)] {
        data.create_vertex(VId(id), vec![label],
            value.map(|value| (N, CanonicalScalar::Int(value))).into_iter().collect());
    }
    data.create_vertex(VId(20), vec![], vec![]);
    db.write(cx, data).await.unwrap()
}
fn changes() -> WriteBatch {
    let mut change = WriteBatch::new(REL);
    change.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(4)));
    change.set_vertex_property(VId(11), N, Some(CanonicalScalar::Int(2)));
    change.delete_vertex(VId(13));
    change.create_vertex(VId(5), vec![L], vec![(N, CanonicalScalar::Int(3))]);
    change
}
fn plain(rows: &[GraphValueRow]) -> Vec<CanonicalScalar> {
    rows.iter().map(|row| row.values()[0].as_scalar().unwrap().clone()).collect()
}
fn oracle(rows: &[VertexRow], operation: Op, quantifier: Q) -> Vec<CanonicalScalar> {
    let mut counts = BTreeMap::<CanonicalScalar, (usize, usize)>::new();
    for row in rows {
        let value = row.props.iter().find(|(key, _)| *key == N)
            .map_or(CanonicalScalar::Null, |(_, value)| value.clone());
        let count = counts.entry(value).or_default();
        count.0 += usize::from(row.labels.contains(&L));
        count.1 += usize::from(row.labels.contains(&R));
    }
    counts.into_iter().flat_map(|(value, (mut l, mut r))| {
        if quantifier == Q::Distinct { l = usize::from(l != 0); r = usize::from(r != 0); }
        let n = match operation {
            Op::Union if quantifier == Q::Distinct => usize::from(l + r != 0),
            Op::Union => l + r,
            Op::Intersect => l.min(r),
            Op::Except => l.saturating_sub(r),
        };
        std::iter::repeat_n(value, n)
    }).collect()
}

#[test]
fn compound_relations_follow_staged_effects_and_preserve_pinned_history_after_reopen() {
    let ((), report) = run_async_under_lab(0x5e70_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let old_rows = db.vertices().unwrap(); let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        for op in [Op::Union, Op::Intersect, Op::Except] { for quantifier in [Q::All, Q::Distinct] {
            let query = query(op, quantifier); let expected = oracle(&old_rows, op, quantifier);
            for result in [
                db.execute_graph_set_governed(&cx, &query, policy()).unwrap(),
                db.execute_graph_set_governed_at(&cx, &query, basis, policy()).unwrap(),
                pinned.execute_graph_set_governed(&cx, &query, policy()).unwrap(),
                pinned.execute_graph_set_governed_at(&cx, &query, basis, policy()).unwrap(),
                txn.execute_graph_set_governed(&db, &cx, &query, policy()).unwrap(),
            ] { assert_eq!(plain(&result.value), expected); }
        }}
        let query = query(Op::Except, Q::All); let frozen = query.canonical_bytes();
        assert_eq!(oracle(&old_rows, Op::Except, Q::All), vec![CanonicalScalar::Int(1), CanonicalScalar::Int(2)]);
        txn.write(&mut db, changes()).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), Op::Except, Q::All);
        let mut expected_values = vec![CanonicalScalar::Null, CanonicalScalar::Int(1), CanonicalScalar::Int(4)];
        expected_values.sort(); assert_eq!(expected, expected_values);
        assert_eq!(plain(&txn.execute_graph_set_governed(&db, &cx, &query, policy()).unwrap().value), expected);
        assert_eq!(plain(&db.execute_graph_set_governed(&cx, &query, policy()).unwrap().value),
            oracle(&old_rows, Op::Except, Q::All));
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_set_governed(&cx, &query, policy()).unwrap().value), expected);
        for result in [
            reopened.execute_graph_set_governed_at(&cx, &query, basis, policy()).unwrap(),
            pinned.execute_graph_set_governed(&cx, &query, policy()).unwrap(),
        ] { assert_eq!(plain(&result.value), oracle(&old_rows, Op::Except, Q::All)); }
        assert_eq!(frozen, query.canonical_bytes());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn negative_set_domains_survive_success_output_refusal_and_limit_zero() {
    let ((), report) = run_async_under_lab(0x5e70_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 { for mutation in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut staged = WriteBatch::new(REL); staged.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, staged).unwrap();
            let query = query(Op::Except, Q::Distinct).with_page(0, Some(u64::from(mode != 2)));
            let output = txn.execute_graph_set_governed(&db, &cx, &query,
                GqlQueryPolicy::new(1_000, u64::from(mode == 0), 5_000_000, 2_000_000));
            match mode {
                0 => assert_eq!(plain(&output.unwrap().value), vec![CanonicalScalar::Int(2)]),
                1 => assert!(matches!(output, Err(GqlQueryError::Rows(_)))),
                _ => assert!(output.unwrap().value.is_empty()),
            }
            if mutation == 3 {
                txn.commit(&mut db, &commit).await.unwrap();
                assert!(db.vertex(VId(777)).unwrap().is_some()); continue;
            }
            let mut winner = WriteBatch::new(REL);
            match mutation {
                0 => winner.create_vertex(VId(14), vec![R], vec![(N, CanonicalScalar::Int(2))]),
                1 => winner.set_vertex_property(VId(12), N, Some(CanonicalScalar::Int(2))),
                _ => winner.delete_vertex(VId(11)),
            };
            db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
            // No second query may accidentally repair a missing negative read.
            let result = txn.commit(&mut db, &commit).await;
            assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01", ..
            }))), "mode={mode}, mutation={mutation}");
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(777)).unwrap().is_none());
        }}
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_limits_cover_both_sources_and_keep_authority_error_precedence() {
    let ((), report) = run_async_under_lab(0x5e70_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = query(Op::Except, Q::Distinct);
        let measured = db.execute_graph_set_governed(&cx, &query, policy()).unwrap();
        assert_eq!(measured.rows.snapshot_records, 16); assert_eq!(measured.rows.result_rows, 1);
        let exact = GqlQueryPolicy::new(16, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_set_governed(&cx, &query, exact).unwrap(), measured);
        for denied in [
            GqlQueryPolicy::new(15, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(16, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(16, 1, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(16, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] { assert!(db.execute_graph_set_governed(&cx, &query, denied).is_err()); }
        let txn = db.begin(&txn_cx).unwrap();
        let measured_txn = txn.execute_graph_set_governed(&db, &cx, &query, policy()).unwrap();
        let exact_txn = GqlQueryPolicy::new(measured_txn.rows.snapshot_records, 1,
            measured_txn.evaluator.work_units, measured_txn.evaluator.scratch_entries);
        assert_eq!(txn.execute_graph_set_governed(&db, &cx, &query, exact_txn).unwrap(), measured_txn);
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(txn.execute_graph_set_governed(&foreign, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(WriteTxnError::WrongDatabase)))));
        assert!(matches!(db.execute_graph_set_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(GqlError::Read(ReadError::BeyondFrontier { .. }))))));
        let pinned = db.read_session().unwrap();
        assert!(matches!(pinned.execute_graph_set_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(GqlError::Read(ReadError::BeyondFrontier { .. }))))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn walk_and_optional_operands_keep_occurrences_and_null_extension_in_set_algebra() {
    let ((), report) = run_async_under_lab(0x5e70_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut links = WriteBatch::new(REL);
        links.add_edge(EId(1), VId(1), VId(11), vec![]);
        links.add_edge(EId(2), VId(1), VId(11), vec![]);
        links.add_edge(EId(3), VId(2), VId(12), vec![]);
        db.write(&commit, links).await.unwrap();
        let walks = pattern("MATCH WALK (a:Left)-[:R*0..1]->(b) RETURN b.n AS value");
        let nullable = pattern("MATCH (a:Left) OPTIONAL MATCH (a)-[:R]->(b) RETURN b.n AS value");
        for (op, mut expected) in [
            (Op::Intersect, vec![CanonicalScalar::Null, CanonicalScalar::Int(1), CanonicalScalar::Int(1), CanonicalScalar::Int(3)]),
            (Op::Except, vec![CanonicalScalar::Int(1), CanonicalScalar::Int(1), CanonicalScalar::Int(2)]),
        ] {
            expected.sort();
            let query = PreparedGraphSet::from(walks.clone()).combine(op, Q::All, nullable.clone().into()).unwrap();
            assert_eq!(plain(&db.execute_graph_set_governed(&cx, &query, policy()).unwrap().value), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
