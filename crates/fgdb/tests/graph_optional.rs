//! Optional results checked against ordinary canonical transaction/storage rows.
//! The independent oracle enumerates edge records and null-extends whole or
//! chained matches; it does not execute another GLA plan to predict the answer.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValueRow,
    IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlQueryError, GqlQueryPolicy, GraphAggregate, PreparedGraphAggregate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId((1_u128 << 100) + 9);
type Plain = (VId, Option<VId>, Option<VId>, Option<i64>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 100_000)
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(RelationId(9));
    for vid in [VId(0), VId(1), VId(2), VId(3), HIGH] {
        vertices.create_vertex(vid, vec![PERSON], vec![]);
    }
    for id in [10, 11, 12] {
        vertices.create_vertex(VId(id), vec![], vec![]);
    }
    vertices.create_vertex(VId(20), vec![], vec![(SCORE, CanonicalScalar::Int(7))]);
    vertices.create_vertex(VId(21), vec![], vec![(SCORE, CanonicalScalar::Int(2))]);
    let mut first = WriteBatch::new(R);
    for (eid, owner, via) in [(1, 0, 10), (2, 0, 10), (3, 1, 11), (4, 2, 12)] {
        first.add_edge(EId(eid), VId(owner), VId(via), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, via, company) in [(5, 10, 20), (6, 10, 20), (7, 11, 21)] {
        second.add_edge(EId(eid), VId(via), VId(company), vec![]);
    }
    db.write_atomic(cx, vec![vertices, first, second])
        .await
        .unwrap()
}

fn outer() -> GraphPatternBuilder {
    let mut outer = GraphPatternBuilder::new();
    outer.vertex("person").unwrap();
    outer
        .filter("person", VertexPredicate::HasLabel(PERSON))
        .unwrap();
    outer
}
fn parts() -> (
    GraphPatternBuilder,
    GraphPatternBuilder,
    GraphPatternBuilder,
) {
    let mut first = GraphPatternBuilder::new();
    for name in ["person", "bridge"] {
        first.vertex(name).unwrap();
    }
    first
        .edge("person", R, GlaDirection::Forward, "bridge")
        .unwrap();
    let mut second = GraphPatternBuilder::new();
    for name in ["bridge", "company"] {
        second.vertex(name).unwrap();
    }
    second
        .edge("bridge", S, GlaDirection::Forward, "company")
        .unwrap();
    second
        .filter(
            "company",
            VertexPredicate::IntegerProperty {
                key: SCORE,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 5,
            },
        )
        .unwrap();
    let mut whole = first.clone();
    whole.vertex("company").unwrap();
    whole
        .edge("bridge", S, GlaDirection::Forward, "company")
        .unwrap();
    whole
        .filter(
            "company",
            VertexPredicate::IntegerProperty {
                key: SCORE,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 5,
            },
        )
        .unwrap();
    (first, second, whole)
}
fn columns() -> [GraphColumn<'static>; 4] {
    [
        GraphColumn::vertex("owner", "person"),
        GraphColumn::vertex("via", "bridge"),
        GraphColumn::vertex("company", "company"),
        GraphColumn::property("score", "company", SCORE),
    ]
}
fn pattern(split: bool) -> PreparedGraphPattern<GraphValueRow> {
    let (first, second, whole) = parts();
    let outer = outer();
    if split {
        outer
            .prepare_values_with_clauses(
                &[
                    GraphMatchClause::optional(&first),
                    GraphMatchClause::optional(&second),
                ],
                &columns(),
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
    } else {
        outer
            .prepare_values_with_clauses(&[GraphMatchClause::optional(&whole)], &columns(), 0, None)
            .unwrap()
            .with_duplicates()
    }
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter()
        .map(|row| {
            let vid = |at| {
                let cell = row.get(at).unwrap();
                assert!(cell.is_null() || cell.as_vertex().is_some());
                cell.as_vertex()
            };
            let score = match row.get(3).unwrap().as_scalar() {
                Some(CanonicalScalar::Int(value)) => Some(*value),
                Some(CanonicalScalar::Null) => None,
                other => panic!("unexpected score cell: {other:?}"),
            };
            (vid(0).unwrap(), vid(1), vid(2), score)
        })
        .collect()
}

fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], split: bool) -> Vec<Plain> {
    let mut rows = Vec::new();
    for person in vertices.iter().filter(|row| row.labels.contains(&PERSON)) {
        let before = rows.len();
        for first in edges
            .iter()
            .filter(|edge| edge.entry.src == person.vid && edge.entry.relation == R)
        {
            let previous = rows.len();
            for second in edges
                .iter()
                .filter(|edge| edge.entry.src == first.entry.dst && edge.entry.relation == S)
            {
                let company = vertices
                    .iter()
                    .find(|row| row.vid == second.entry.dst)
                    .unwrap();
                let value = company
                    .props
                    .iter()
                    .find(|(key, _)| *key == SCORE)
                    .map(|(_, value)| value);
                if let Some(CanonicalScalar::Int(score)) = value
                    && *score >= 5
                {
                    rows.push((
                        person.vid,
                        Some(first.entry.dst),
                        Some(company.vid),
                        Some(*score),
                    ));
                }
            }
            if split && rows.len() == previous {
                rows.push((person.vid, Some(first.entry.dst), None, None));
            }
        }
        if rows.len() == before {
            rows.push((person.vid, None, None, None));
        }
    }
    rows.sort();
    rows
}

#[test]
fn all_five_surfaces_preserve_optional_bags_and_share_one_policy() {
    let ((), report) = run_async_under_lab(0x0f71_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        for split in [false, true] {
            let pattern = pattern(split);
            let expected = oracle(&vertices, &edges, split);
            assert_eq!(expected.len(), 8);
            assert_eq!(expected.iter().filter(|row| row.0 == VId(0)).count(), 4);
            assert_eq!(
                expected.iter().find(|row| row.0 == VId(2)).unwrap().1,
                split.then_some(VId(12))
            );
            for rows in [
                db.execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value,
                db.execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap()
                    .value,
                pinned
                    .execute_graph_pattern_governed(&cx, &pattern, policy())
                    .unwrap()
                    .value,
                pinned
                    .execute_graph_pattern_governed_at(&cx, &pattern, basis, policy())
                    .unwrap()
                    .value,
                txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                    .unwrap()
                    .value,
            ] {
                assert_eq!(plain(&rows), expected);
            }
            let full = db
                .execute_graph_pattern_governed(&cx, &pattern, policy())
                .unwrap();
            assert_eq!(full.rows.snapshot_records, 17);
            let exact = GqlQueryPolicy::new(
                17,
                8,
                full.evaluator.work_units,
                full.evaluator.scratch_entries,
            );
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, &pattern, exact)
                    .unwrap(),
                full
            );
            for cap in [
                GqlQueryPolicy::new(17, 8, full.evaluator.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(17, 8, u64::MAX, full.evaluator.scratch_entries - 1),
            ] {
                assert!(
                    matches!(db.execute_graph_pattern_governed(&cx, &pattern, cap),
                    Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1)
                );
            }
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &pattern, GqlQueryPolicy::new(16, 8, u64::MAX, u64::MAX)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == 17)
            );
            assert!(
                matches!(db.execute_graph_pattern_governed(&cx, &pattern, GqlQueryPolicy::new(17, 7, u64::MAX, u64::MAX)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 8)
            );
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_ensures_property_removal_and_cascades_match_committed_and_historical_rows() {
    let ((), report) = run_async_under_lab(0x0f71_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let patterns = [pattern(false), pattern(true)];
        let before: Vec<_> = patterns
            .iter()
            .map(|pattern| {
                db.execute_graph_pattern_governed(&cx, pattern, policy())
                    .unwrap()
                    .value
            })
            .collect();
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(1));
        changes.ensure_edge_by_triple(EId(999), VId(0), VId(10), vec![]);
        changes.add_edge(EId(8), VId(3), VId(11), vec![]);
        changes.set_vertex_property(VId(20), SCORE, None);
        changes.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(9)));
        changes.delete_vertex(VId(12));
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let vertices = txn.vertices(&db).unwrap();
        let edges = txn.edges(&db).unwrap();
        assert!(vertices.iter().all(|row| row.vid != VId(12)));
        assert!(edges.iter().all(|row| row.entry.eid != EId(4)));
        let mut staged = Vec::new();
        for (at, pattern) in patterns.iter().enumerate() {
            let expected = oracle(&vertices, &edges, at == 1);
            let result = txn
                .execute_graph_pattern_governed(&db, &cx, pattern, policy())
                .unwrap();
            assert_eq!(plain(&result.value), expected);
            assert_eq!(result.value.len(), 5);
            assert_eq!(result.rows.snapshot_records, 15);
            let exact = GqlQueryPolicy::new(
                15,
                5,
                result.evaluator.work_units,
                result.evaluator.scratch_entries,
            );
            assert_eq!(
                txn.execute_graph_pattern_governed(&db, &cx, pattern, exact)
                    .unwrap(),
                result
            );
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, pattern, policy())
                    .unwrap()
                    .value,
                before[at]
            );
            staged.push(result.value);
        }
        txn.commit(&mut db, &commit).await.unwrap();
        for (at, pattern) in patterns.iter().enumerate() {
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, pattern, policy())
                    .unwrap()
                    .value,
                staged[at]
            );
            assert_eq!(
                db.execute_graph_pattern_governed_at(&cx, pattern, basis, policy())
                    .unwrap()
                    .value,
                before[at]
            );
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (at, pattern) in patterns.iter().enumerate() {
            assert_eq!(
                reopened
                    .execute_graph_pattern_governed(&cx, pattern, policy())
                    .unwrap()
                    .value,
                staged[at]
            );
            assert_eq!(
                reopened
                    .execute_graph_pattern_governed_at(&cx, pattern, basis, policy())
                    .unwrap()
                    .value,
                before[at]
            );
            assert_eq!(
                pinned
                    .execute_graph_pattern_governed(&cx, pattern, policy())
                    .unwrap()
                    .value,
                before[at]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_absence_retains_conflicts_after_refusal_without_fencing_unlabeled_insertions() {
    let ((), report) = run_async_under_lab(0x0f71_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for refused in [false, true] {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let pattern = pattern(false);
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                // Do not run txn.vertices() as an oracle here: doing so would
                // independently install a table-wide vertex-scan dependency.
                let result = txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &pattern,
                    GqlQueryPolicy::new(100, if refused { 0 } else { 100 }, 1_000_000, 100_000),
                );
                let old = if refused {
                    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
                    None
                } else {
                    let rows = result.unwrap().value;
                    assert!(plain(&rows).contains(&(VId(2), None, None, None)));
                    Some(rows)
                };
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => winner.create_vertex(VId(88), vec![], vec![]),
                    1 => winner.add_edge(EId(9), VId(2), VId(10), vec![]),
                    _ => winner.set_vertex_property(VId(21), SCORE, Some(CanonicalScalar::Int(9))),
                };
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                if let Some(old) = old {
                    assert_eq!(
                        txn.execute_graph_pattern_governed(&db, &cx, &pattern, policy())
                            .unwrap()
                            .value,
                        old
                    );
                }
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
fn optional_values_compose_with_semijoins_antijoins_and_streaming_grouping() {
    let ((), report) = run_async_under_lab(0x0f71_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let (first, second, _) = parts();
        let mut company = GraphPatternBuilder::new();
        company.vertex("company").unwrap();
        for exists in [false, true] {
            let outer = outer();
            let query = outer
                .prepare_values_with_clauses(
                    &[
                        GraphMatchClause::optional(&first),
                        GraphMatchClause::optional(&second),
                        if exists {
                            GraphMatchClause::exists(&company)
                        } else {
                            GraphMatchClause::not_exists(&company)
                        },
                    ],
                    &columns(),
                    0,
                    None,
                )
                .unwrap()
                .with_duplicates();
            let all = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), true);
            let expected: Vec<_> = all
                .into_iter()
                .filter(|row| row.2.is_some() == exists)
                .collect();
            let actual = db
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap();
            assert_eq!(plain(&actual.value), expected);
            assert_eq!(actual.value.len(), 4);
        }
        for split in [false, true] {
            let summary = PreparedGraphAggregate::prepare(
                pattern(split),
                &[0],
                &[
                    GraphAggregate::count_rows("rows"),
                    GraphAggregate::count("bridges", 1),
                    GraphAggregate::count("companies", 2),
                    GraphAggregate::count_distinct("unique_companies", 2),
                    GraphAggregate::sum_int("sum", 3),
                ],
                0,
                None,
            )
            .unwrap();
            let rows = db
                .execute_graph_aggregate_governed(&cx, &summary, policy())
                .unwrap()
                .value;
            assert_eq!(rows.len(), 5);
            for row in &rows {
                let owner = row.keys()[0].as_vertex().unwrap();
                let qualified = owner == VId(0);
                let bridge = qualified || split && [VId(1), VId(2)].contains(&owner);
                assert_eq!(
                    row.get(0).unwrap().as_count(),
                    Some(if qualified { 4 } else { 1 })
                );
                assert_eq!(
                    row.get(1).unwrap().as_count(),
                    Some(if qualified { 4 } else { u64::from(bridge) })
                );
                assert_eq!(
                    row.get(2).unwrap().as_count(),
                    Some(if qualified { 4 } else { 0 })
                );
                assert_eq!(row.get(3).unwrap().as_count(), Some(u64::from(qualified)));
                assert_eq!(row.get(4).unwrap().as_integer(), qualified.then_some(28));
                assert_eq!(row.get(4).unwrap().is_null(), !qualified);
            }
            assert_eq!(
                pinned
                    .execute_graph_aggregate_governed(&cx, &summary, policy())
                    .unwrap()
                    .value,
                rows
            );
            assert_eq!(
                txn.execute_graph_aggregate_governed(&db, &cx, &summary, policy())
                    .unwrap()
                    .value,
                rows
            );
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_read_authority_and_snapshot_errors_precede_runtime_interruption() {
    let ((), report) = run_async_under_lab(0x0f71_0005, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let txn_cx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let pinned = db.read_session().unwrap();
                let txn = db.begin(&txn_cx).unwrap();
                let query = pattern(false);
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                root.cancel_with(CancelKind::User, Some("optional scope interruption"));
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&foreign, &cx, &query, policy()),
                    Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed_at(&cx, &query, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    pinned.execute_graph_pattern_governed_at(&cx, &query, future, policy()),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &query, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&db, &cx, &query, policy()),
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
