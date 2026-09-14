//! CASE uses ordinary Chronicle/Strata reads and canonical transaction staging.
//! The oracle computes conditions directly from stored records, not VM output.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphIntegerErrorKind,
    GraphMutationError, GraphMutationPolicy, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphMutationText,
    PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const A: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const BUCKET: &str = "CASE WHEN n.p IS NULL THEN 0 WHEN n.p<0 THEN -1 WHEN n.q=0 THEN 1 ELSE 2 END";
const AMOUNT: &str = "CASE WHEN n.p IS NULL OR n.p=0 THEN 0 ELSE n.q/n.p END";
const UPDATE: &str = "MATCH WALK (a:A)-[:R*0..2]->(b) \
    SET a.p=CASE WHEN a.p IS NULL THEN $missing WHEN a.p<0 THEN -a.p ELSE a.p+$step END, \
        a.q=CASE a.p WHEN 0 THEN 100 ELSE COALESCE(a.p,0) END";
type Plain = (VId, i64, i64);
type Group = (i64, u64, i128, i128, u64);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000) }
fn mutation_policy() -> GraphMutationPolicy { GraphMutationPolicy::new(policy(), 100) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn read_query() -> PreparedGraphSet {
    PreparedGraphSetText::prepare(
        &format!("MATCH (n:A) RETURN n,{BUCKET} AS bucket,{AMOUNT} AS amount ORDER BY n"), symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn aggregate_query() -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(
        &format!("MATCH (n:A) RETURN {BUCKET} AS bucket,COUNT(*) AS count,SUM({AMOUNT}) AS total, \
            AVG({AMOUNT}) AS mean GROUP BY {BUCKET} HAVING SUM({AMOUNT})>=-100 ORDER BY bucket"), symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, p, q) in [(1, Some(-2), 8), (2, Some(0), 0), (3, Some(3), 6), (4, None, 5), (5, None, -1)] {
        let mut props = Vec::new();
        if let Some(p) = p { props.push((P, CanonicalScalar::Int(p))); }
        else if id == 4 { props.push((P, CanonicalScalar::Null)); }
        props.push((Q, CanonicalScalar::Int(q)));
        batch.create_vertex(VId(id), vec![A], props);
    }
    for (eid, src, dst) in [(101, 1, 2), (102, 1, 2), (103, 2, 3), (104, 3, 1)] {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn property(row: &VertexRow, key: PropertyKeyId) -> Option<i64> {
    match row.props.iter().find(|(candidate, _)| *candidate == key).map(|(_, value)| value) {
        Some(CanonicalScalar::Int(value)) => Some(*value),
        None | Some(CanonicalScalar::Null) => None,
        _ => panic!("unexpected oracle input"),
    }
}
fn oracle(rows: &[VertexRow]) -> Vec<Plain> {
    rows.iter().filter(|row| row.labels.contains(&A)).map(|row| {
        let p = property(row, P);
        let q = property(row, Q).unwrap();
        let bucket = match p { None => 0, Some(p) if p<0 => -1, Some(_) if q==0 => 1, _ => 2 };
        let amount = match p { None | Some(0) => 0, Some(p) => q/p };
        (row.vid, bucket, amount)
    }).collect()
}
fn cell(value: &GraphValue) -> i64 {
    let Some(CanonicalScalar::Int(value)) = value.as_scalar() else { panic!("integer result") };
    *value
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        assert_eq!(row.len(), 3);
        (row.values()[0].as_vertex().unwrap(), cell(&row.values()[1]), cell(&row.values()[2]))
    }).collect()
}
fn oracle_groups(rows: &[Plain]) -> Vec<Group> {
    let mut groups = BTreeMap::<i64, (u64, i128)>::new();
    for &(_, bucket, amount) in rows {
        let group = groups.entry(bucket).or_default(); group.0+=1; group.1+=i128::from(amount);
    }
    groups.into_iter().map(|(bucket, (count, total))| {
        let (mut a, mut b) = (total.unsigned_abs(), u128::from(count));
        while b != 0 { (a, b) = (b, a%b); }
        (bucket, count, total, total/(a as i128), count/(a as u64))
    }).collect()
}
fn grouped(rows: &[GraphAggregateRow]) -> Vec<Group> {
    rows.iter().map(|row| {
        assert_eq!(row.keys().len(), 1); assert_eq!(row.values().len(), 3);
        let average = row.values()[2].as_average().unwrap();
        (cell(&row.keys()[0]), row.values()[0].as_count().unwrap(), row.values()[1].as_integer().unwrap(),
            average.numerator(), average.denominator())
    }).collect()
}

#[test]
fn conditional_reads_groups_and_frozen_writes_survive_commit_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xca5e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.pinned_read_view().unwrap();
        let read = read_query(); let summary = aggregate_query();
        let old = oracle(&db.vertices().unwrap());
        assert_eq!(old, vec![(VId(1),-1,-4),(VId(2),1,0),(VId(3),2,2),(VId(4),0,0),(VId(5),0,0)]);
        assert_eq!(plain(&db.execute_graph_set_governed(&cx, &read, policy()).unwrap().value), old);
        assert_eq!(grouped(&db.execute_graph_aggregate_governed(&cx, &summary, policy()).unwrap().value), oracle_groups(&old));
        let mut txn = db.begin(&txcx).unwrap();
        let mut prior = WriteBatch::new(R);
        prior.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        txn.write(&mut db, prior).unwrap();
        let mut expected_vertices = txn.vertices(&db).unwrap();
        let before_statement: BTreeMap<_, _> = expected_vertices.iter()
            .map(|row| (row.vid, property(row, P))).collect();
        for row in &mut expected_vertices {
            let p = before_statement[&row.vid];
            let next_p = match p { None => 9, Some(p) if p<0 => -p, Some(p) => p+3 };
            let next_q = if p == Some(0) { 100 } else { p.unwrap_or(0) };
            row.props = vec![(P, CanonicalScalar::Int(next_p)), (Q, CanonicalScalar::Int(next_q))];
        }
        let expected = oracle(&expected_vertices);
        let template = PreparedGraphMutationText::prepare(UPDATE, R, symbols).unwrap();
        let args = GqlParameters::new().with_int64("missing", 9).unwrap().with_int64("step", 3).unwrap();
        let mutation = template.bind_parameters(&args).unwrap();
        let frozen = mutation.canonical_bytes();
        let stats = txn.execute_graph_mutation_governed(&mut db, &cx, &mutation, mutation_policy()).unwrap();
        assert_eq!(stats.selection.result_rows, 14, "duplicate WALK paths must not multiply assignments");
        assert_eq!(stats.target_vertices, 5); assert_eq!(stats.effects, 10);
        assert_eq!(oracle(&txn.vertices(&db).unwrap()), expected);
        assert_eq!(plain(&txn.execute_graph_set_governed(&db, &cx, &read, policy()).unwrap().value), expected);
        assert_eq!(grouped(&txn.execute_graph_aggregate_governed(&db, &cx, &summary, policy()).unwrap().value), oracle_groups(&expected));
        assert_eq!(plain(&db.execute_graph_set_governed(&cx, &read, policy()).unwrap().value), old);
        let committed = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(committed, CommitSeq(basis.0+1));
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_set_governed(&cx, &read, policy()).unwrap().value), expected);
        assert_eq!(grouped(&reopened.execute_graph_aggregate_governed(&cx, &summary, policy()).unwrap().value), oracle_groups(&expected));
        assert_eq!(plain(&reopened.execute_graph_set_governed_at(&cx, &read, basis, policy()).unwrap().value), old);
        assert_eq!(grouped(&reopened.execute_graph_aggregate_governed_at(&cx, &summary, basis, policy()).unwrap().value), oracle_groups(&old));
        assert_eq!(plain(&pinned.execute_graph_set_governed(&cx, &read, policy()).unwrap().value), old);
        assert_eq!(grouped(&pinned.execute_graph_aggregate_governed(&cx, &summary, policy()).unwrap().value), oracle_groups(&old));
        assert_eq!(template.bind_parameters(&args).unwrap().canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_branch_failure_and_refusals_preserve_the_complete_prior_workspace() {
    let ((), report) = run_async_under_lab(0xca5e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prior = WriteBatch::new(R);
        prior.create_vertex(VId(777), vec![], vec![]);
        prior.set_vertex_property(VId(3), Q, Some(CanonicalScalar::ucs_basic_text("private bad numeric input").unwrap()));
        txn.write(&mut db, prior).unwrap();
        let before = txn.vertices(&db).unwrap();
        for (expression, expected_kind) in [
            ("CASE WHEN n.p IS NULL OR n.p=0 THEN 1 ELSE 10/(n.p-3) END", GraphIntegerErrorKind::DivisionByZero),
            ("CASE WHEN n.p=3 THEN 9223372036854775807+1 ELSE 0 END", GraphIntegerErrorKind::Overflow),
            ("CASE WHEN n.p=3 THEN n.q+1 ELSE 0 END", GraphIntegerErrorKind::NonInteger),
        ] {
            let query = PreparedGraphMutationText::prepare(&format!("MATCH (n:A) SET n.p={expression}"), R, symbols)
                .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let result = txn.execute_graph_mutation_governed(&mut db, &cx, &query, mutation_policy());
            assert!(matches!(result, Err(GqlQueryError::Source(GraphMutationError::Arithmetic { error, .. }))
                if error.kind == expected_kind), "{expression}");
            assert_eq!(txn.vertices(&db).unwrap(), before);
            assert_eq!(db.frontier().unwrap(), basis);
        }
        let safe = PreparedGraphMutationText::prepare(
            "MATCH (n:A) SET n.p=CASE WHEN n.p IS NULL THEN 0 ELSE n.p+1 END", R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        for refusal in [GraphMutationPolicy::new(policy(), 4),
            GraphMutationPolicy::new(GqlQueryPolicy::new(10_000, 0, 10_000_000, 10_000_000), 100)] {
            assert!(txn.execute_graph_mutation_governed(&mut db, &cx, &safe, refusal).is_err());
            assert_eq!(txn.vertices(&db).unwrap(), before);
        }
        // A noninteger scalar in an unused arithmetic branch is still read,
        // but is not implicitly coerced or evaluated by the conditional VM.
        let lazy = PreparedGraphSetText::prepare(
            "MATCH (n:A) RETURN CASE WHEN TRUE THEN 7 ELSE n.q+1 END AS value", symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(txn.execute_graph_set_governed(&db, &cx, &lazy, policy()).unwrap().value.len(), 5);
        txn.abort();
        assert!(db.vertex(VId(777)).unwrap().is_none());
        assert_eq!(property(&db.vertex(VId(3)).unwrap().unwrap(), Q), Some(6));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conditional_reads_keep_unselected_inputs_and_phantom_dependencies_after_refusal() {
    let ((), report) = run_async_under_lab(0xca5e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for mode in 0..5 {
            for winner in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txcx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let predicate = if mode == 4 { "n.p>100" } else { "n.p>=0" };
                let expression = if mode == 3 {
                    "CASE WHEN n.p=3 THEN 1/0 ELSE n.q END"
                } else {
                    "CASE WHEN n.p>=0 THEN n.p ELSE n.q END"
                };
                let tail = if mode == 1 { " LIMIT 0" } else { "" };
                let query = PreparedGraphSetText::prepare(
                    &format!("MATCH (n:A) WHERE {predicate} RETURN {expression} AS value{tail}"), symbols,
                ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
                let allowance = if mode == 2 { GqlQueryPolicy::new(10_000, 0, 10_000_000, 10_000_000) }
                    else { policy() };
                let result = txn.execute_graph_set_governed(&db, &cx, &query, allowance);
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 2),
                    1 | 4 => assert!(result.unwrap().value.is_empty()),
                    2 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero)),
                }
                let mut competing = WriteBatch::new(R);
                match winner {
                    // q is projected even when CASE selects p. In the empty
                    // selection case its vertex was inspected by WHERE.
                    0 => { competing.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(99))); }
                    1 => { competing.create_vertex(VId(6), vec![A], vec![(P, CanonicalScalar::Int(999))]); }
                    _ => { competing.create_vertex(VId(6), vec![], vec![]); }
                }
                db.write(&commit, competing).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No transaction read after the query can repair a lost witness.
                let committed = txn.commit(&mut db, &commit).await;
                if winner < 2 {
                    assert!(matches!(committed, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))), "mode={mode}, winner={winner}");
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                } else {
                    committed.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
