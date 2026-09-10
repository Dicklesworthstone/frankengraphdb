//! Text scopes use the same durable and canonical overlay sources as typed plans.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const BLOCKED: RelationId = RelationId(3);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId((1_u128 << 100) + 7);
const HEAD: &str = "MATCH (a:Person) WHERE NOT EXISTS { MATCH (a)-[:BLOCKED]->(hidden) } \
    OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) WHERE c.n >= $floor";
type Plain = (VId, Option<VId>, Option<VId>, Option<i64>);
type Summary = (VId, u64, u64, u64, Option<i128>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}
fn arguments() -> GqlParameters {
    GqlParameters::new().with_int64("floor", 5).unwrap()
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "BLOCKED") => Some(GraphSymbol::Relation(BLOCKED)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}
fn query() -> fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!("{HEAD} RETURN a,b,c,c.n AS score"), symbols)
        .unwrap()
        .bind_parameters(&arguments())
        .unwrap()
}
fn aggregate() -> fgdb_gql::PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(
        &format!(
            "{HEAD} RETURN a,COUNT(*) AS witnesses,COUNT(c) AS qualifying,\
        COUNT(DISTINCT c) AS companies,SUM(c.n) AS total GROUP BY a HAVING witnesses >= 1 \
        ORDER BY qualifying ASC,a ASC"
        ),
        symbols,
    )
    .unwrap()
    .bind_parameters(&arguments())
    .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for vid in [VId(0), VId(1), VId(2), HIGH] {
        vertices.create_vertex(vid, vec![PERSON], vec![]);
    }
    for vid in [VId(10), VId(11), VId(90)] {
        vertices.create_vertex(vid, vec![], vec![]);
    }
    vertices.create_vertex(VId(20), vec![], vec![(SCORE, CanonicalScalar::Int(7))]);
    vertices.create_vertex(VId(21), vec![], vec![(SCORE, CanonicalScalar::Int(2))]);
    let mut first = WriteBatch::new(R);
    for (eid, source, destination) in [(10, 0, 10), (11, 0, 10), (12, 1, 11)] {
        first.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, source, destination) in [(20, 10, 20), (21, 10, 20), (22, 11, 21)] {
        second.add_edge(EId(eid), VId(source), VId(destination), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second])
        .await
        .unwrap()
}
fn integer(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == SCORE => Some(*value),
        _ => None,
    })
}
// Owned row oracle: no GLA operators, nullable slots, parser, or other query.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut result = Vec::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&PERSON)) {
        if edges
            .iter()
            .any(|edge| edge.entry.src == owner.vid && edge.entry.relation == BLOCKED)
        {
            continue;
        }
        let first: Vec<_> = edges
            .iter()
            .filter(|edge| edge.entry.src == owner.vid && edge.entry.relation == R)
            .collect();
        if first.is_empty() {
            result.push((owner.vid, None, None, None));
        }
        for edge in first {
            let mut matched = false;
            for second in edges.iter().filter(|candidate| {
                candidate.entry.src == edge.entry.dst && candidate.entry.relation == S
            }) {
                let company = vertices
                    .iter()
                    .find(|row| row.vid == second.entry.dst)
                    .unwrap();
                if let Some(score) = integer(company).filter(|score| *score >= 5) {
                    matched = true;
                    result.push((
                        owner.vid,
                        Some(edge.entry.dst),
                        Some(company.vid),
                        Some(score),
                    ));
                }
            }
            if !matched {
                result.push((owner.vid, Some(edge.entry.dst), None, None));
            }
        }
    }
    result.sort();
    result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter()
        .map(|row| {
            let nullable = |at| {
                let cell = row.get(at).unwrap();
                if cell.is_null() {
                    None
                } else {
                    Some(cell.as_vertex().unwrap())
                }
            };
            let score = match row.get(3).unwrap().as_scalar().unwrap() {
                CanonicalScalar::Int(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => panic!("noninteger fixture score"),
            };
            (
                row.get(0).unwrap().as_vertex().unwrap(),
                nullable(1),
                nullable(2),
                score,
            )
        })
        .collect()
}
fn expected_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, Vec<&Plain>> = BTreeMap::new();
    for row in rows {
        groups.entry(row.0).or_default().push(row);
    }
    let mut result: Vec<_> = groups
        .into_iter()
        .map(|(key, rows)| {
            let present: Vec<_> = rows.iter().filter_map(|row| row.3).collect();
            (
                key,
                rows.len() as u64,
                present.len() as u64,
                rows.iter()
                    .filter_map(|row| row.2)
                    .collect::<BTreeSet<_>>()
                    .len() as u64,
                (!present.is_empty()).then(|| present.into_iter().map(i128::from).sum()),
            )
        })
        .collect();
    result.sort_by_key(|row| (row.2, row.0));
    result
}
fn summary(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
                row.get(2).unwrap().as_count().unwrap(),
                row.get(3).unwrap().as_integer(),
            )
        })
        .collect()
}

#[test]
fn optional_and_anti_text_and_zero_preserving_aggregates_cover_all_five_read_surfaces() {
    let ((), report) = run_async_under_lab(0x0f71_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let query = query();
        let aggregate = aggregate();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(expected.len(), 7);
        assert_eq!(expected[4], (VId(1), Some(VId(11)), None, None));
        for result in [
            db.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                .unwrap(),
            view.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap(),
            view.execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                .unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &query, policy())
                .unwrap(),
        ] {
            assert_eq!(plain(&result.value), expected);
            assert_eq!(result.rows.snapshot_records, 15);
        }
        let sums = expected_summary(&expected);
        assert_eq!(sums[0], (VId(1), 1, 0, 0, None));
        assert_eq!(sums[3], (VId(0), 4, 4, 1, Some(28)));
        for result in [
            db.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap(),
            view.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                .unwrap(),
        ] {
            assert_eq!(summary(&result.value), sums);
        }
        let full = db
            .execute_graph_pattern_governed(&cx, &query, policy())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            15,
            7,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &query, exact)
                .unwrap(),
            full
        );
        for cap in [
            GqlQueryPolicy::new(14, 7, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(15, 6, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(15, 7, full.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(15, 7, u64::MAX, full.evaluator.scratch_entries - 1),
        ] {
            assert!(db.execute_graph_pattern_governed(&cx, &query, cap).is_err());
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_staging_changes_nullable_rows_and_summary_without_rewriting_history() {
    let ((), report) = run_async_under_lab(0x0f71_0002, |root| async move {
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
        let view = db.read_session().unwrap();
        let query = query();
        let aggregate = aggregate();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(10));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.add_edge(EId(13), VId(2), VId(10), vec![]);
        changes.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(8)));
        changes.delete_vertex(HIGH);
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected.len(), 5);
        assert_ne!(expected, old);
        let staged = txn
            .execute_graph_pattern_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(plain(&staged.value), expected);
        let exact = GqlQueryPolicy::new(
            staged.rows.snapshot_records,
            staged.rows.result_rows,
            staged.evaluator.work_units,
            staged.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &query, exact)
                .unwrap(),
            staged
        );
        assert_eq!(
            summary(
                &txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &query, policy())
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
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            summary(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&expected)
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            plain(
                &view
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            summary(
                &view
                    .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected_summary(&old)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_nulls_and_anti_absence_retain_dependencies_after_output_refusal() {
    let ((), report) = run_async_under_lab(0x0f71_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for refused in [false, true] {
            for change in 0..5 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let result = txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &query(),
                    GqlQueryPolicy::new(100, if refused { 0 } else { 100 }, 1_000_000, 100_000),
                );
                if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                } else {
                    assert!(plain(&result.unwrap().value).contains(&(VId(2), None, None, None)));
                }
                let mut winner = WriteBatch::new(if change == 3 { BLOCKED } else { R });
                match change {
                    0 => winner.create_vertex(VId(88), vec![], vec![]),
                    1 => winner.add_edge(EId(88), VId(2), VId(10), vec![]),
                    2 => winner.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(8))),
                    3 => winner.add_edge(EId(88), VId(2), VId(90), vec![]),
                    _ => winner.create_vertex(VId(88), vec![PERSON], vec![]),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if change == 0 {
                    result.unwrap();
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_outer_sources_and_authority_refusals_keep_their_meaning() {
    let ((), report) = run_async_under_lab(0x0f71_0004, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let cx = contexts.query();
                let commit = contexts.commit();
                let txn_cx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                assert!(
                    db.execute_graph_pattern_governed(&cx, &query(), policy())
                        .unwrap()
                        .value
                        .is_empty()
                );
                assert!(
                    db.execute_graph_aggregate_governed(&cx, &aggregate(), policy())
                        .unwrap()
                        .value
                        .is_empty()
                );
                let global = PreparedGraphAggregateText::prepare(
                    &format!("{HEAD} RETURN COUNT(*) AS n,COUNT(c) AS present"),
                    symbols,
                )
                .unwrap()
                .bind_parameters(&arguments())
                .unwrap();
                let zero = db
                    .execute_graph_aggregate_governed(&cx, &global, policy())
                    .unwrap();
                assert_eq!(zero.value.len(), 1);
                assert_eq!(zero.value[0].get(0).unwrap().as_count(), Some(0));
                assert_eq!(zero.value[0].get(1).unwrap().as_count(), Some(0));
                seed(&mut db, &commit).await;
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let txn = db.begin(&txn_cx).unwrap();
                let view = db.read_session().unwrap();
                let query = query();
                root.cancel_with(CancelKind::User, Some("scoped text error ordering"));
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&foreign, &cx, &query, policy()),
                    Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
                ));
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                assert!(matches!(
                    db.execute_graph_pattern_governed_at(&cx, &query, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    view.execute_graph_pattern_governed_at(&cx, &query, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &query, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                txn.abort();
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
