//! Membership and ranges on real database reads, not a separate AST evaluator.
//! The fixture oracle uses owned storage rows and ordinary integer comparisons.
//! Duplicate edges, nullable OPTIONAL matches, transaction read dependencies,
//! hidden aggregate errors, historical snapshots and reopen are all observable.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError,
    WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
    PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};
use std::collections::BTreeMap;

const OWNER: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) \
    WHERE b.score BETWEEN a.score AND $ceiling AND b.score NOT IN [$excluded]";
type Plain = (VId, Option<VId>, Option<i64>);
type Summary = (VId, u64, u64);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32], DatabaseSecurityNamespaceId([0xe2; 32]), [0xe3; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}
fn arguments() -> GqlParameters {
    GqlParameters::new().with_int64("ceiling", 35).unwrap()
        .with_int64("excluded", 25).unwrap()
}
fn pattern(limit: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let suffix = limit.map_or(String::new(), |count| format!(" LIMIT {count}"));
    PreparedGraphText::prepare(
        &format!("{HEAD} RETURN a,b,b.score AS score ORDER BY a,b{suffix}"), symbols,
    ).unwrap().bind_parameters(&arguments()).unwrap()
}
fn aggregate() -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(
        &format!("{HEAD} RETURN a,COUNT(*) AS total,COUNT(b) AS present GROUP BY a \
            HAVING present IN [0,2] AND total BETWEEN 1 AND 2 ORDER BY a"), symbols,
    ).unwrap().bind_parameters(&arguments()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, score) in [(1, 10), (2, 20), (3, 30), (4, 40)] {
        batch.create_vertex(VId(id), vec![OWNER], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    for (id, score) in [(11, 15), (12, 25)] {
        batch.create_vertex(VId(id), vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
    }
    batch.create_vertex(VId(13), vec![], vec![(SCORE, CanonicalScalar::Null)]);
    batch.create_vertex(VId(14), vec![], vec![]);
    for (id, source, destination) in [
        (101, 1, 11), (102, 1, 11), (103, 1, 12), (104, 2, 12),
        (105, 2, 13), (106, 3, 14),
    ] {
        batch.add_edge(EId(id), VId(source), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn integer(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == SCORE => Some(*value),
        _ => None,
    })
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut output = Vec::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&OWNER)) {
        let before = output.len();
        for edge in edges.iter().filter(|edge| edge.entry.relation == R && edge.entry.src == owner.vid) {
            let target = vertices.iter().find(|row| row.vid == edge.entry.dst).unwrap();
            if let (Some(lower), Some(value)) = (integer(owner), integer(target)) {
                if value >= lower && value <= 35 && value != 25 {
                    output.push((owner.vid, Some(target.vid), Some(value)));
                }
            }
        }
        if output.len() == before {
            output.push((owner.vid, None, None));
        }
    }
    output.sort();
    output
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let score = match row.values()[2].as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value),
            CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture scalar"),
        };
        (row.values()[0].as_vertex().unwrap(), row.values()[1].as_vertex(), score)
    }).collect()
}
fn oracle_summary(rows: &[Plain]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, (u64, u64)> = BTreeMap::new();
    for (owner, target, _) in rows {
        let group = groups.entry(*owner).or_default();
        group.0 += 1;
        group.1 += u64::from(target.is_some());
    }
    groups.into_iter().filter_map(|(owner, (total, present))| {
        ((present == 0 || present == 2) && (1..=2).contains(&total))
            .then_some((owner, total, present))
    }).collect()
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (
        row.keys()[0].as_vertex().unwrap(),
        row.values()[0].as_count().unwrap(),
        row.values()[1].as_count().unwrap(),
    )).collect()
}

#[test]
fn membership_ranges_cover_snapshots_overlays_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x1ab1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let query = pattern(None);
        let grouped = aggregate();
        let frozen = query.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(old, vec![
            (VId(1), Some(VId(11)), Some(15)), (VId(1), Some(VId(11)), Some(15)),
            (VId(2), None, None), (VId(3), None, None), (VId(4), None, None),
        ]);
        for result in [
            db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap(),
            pinned.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap(),
        ] {
            assert_eq!(plain(&result.value), old);
        }
        for result in [
            db.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &grouped, basis, policy()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx, &grouped, basis, policy()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &grouped, policy()).unwrap(),
        ] {
            assert_eq!(summaries(&result.value), oracle_summary(&old));
        }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(102));
        changes.set_vertex_property(VId(11), SCORE, Some(CanonicalScalar::Int(40)));
        changes.set_vertex_property(VId(12), SCORE, Some(CanonicalScalar::Int(22)));
        changes.set_vertex_property(VId(14), SCORE, Some(CanonicalScalar::Int(32)));
        txn.write(&mut db, changes).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected, vec![
            (VId(1), Some(VId(12)), Some(22)), (VId(2), Some(VId(12)), Some(22)),
            (VId(3), Some(VId(14)), Some(32)), (VId(4), None, None),
        ]);
        assert_eq!(oracle_summary(&expected), vec![(VId(4), 1, 0)]);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summaries(&txn.execute_graph_aggregate_governed(&db, &cx, &grouped, policy()).unwrap().value), oracle_summary(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), expected);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap().value), oracle_summary(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &query, basis, policy()).unwrap().value), old);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed_at(&cx, &grouped, basis, policy()).unwrap().value), oracle_summary(&old));
        assert_eq!(plain(&pinned.execute_graph_pattern_governed(&cx, &query, policy()).unwrap().value), old);
        assert_eq!(summaries(&pinned.execute_graph_aggregate_governed(&cx, &grouped, policy()).unwrap().value), oracle_summary(&old));
        assert_eq!(query.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn membership_does_not_duplicate_matches_or_turn_unknown_into_false() {
    let ((), report) = run_async_under_lab(0x1ab1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for (predicate, expected) in [
            ("a.score IN [10,10,30]", vec![VId(1), VId(3)]),
            ("a.score IN [10,NULL]", vec![VId(1)]),
            ("a.score NOT IN [10,NULL]", vec![]),
            ("a.score IN []", vec![]),
            ("a.score NOT IN []", vec![VId(1), VId(2), VId(3), VId(4)]),
            ("a.score BETWEEN 10 AND 30", vec![VId(1), VId(2), VId(3)]),
            ("a.score NOT BETWEEN 10 AND 30", vec![VId(4)]),
            ("a.score BETWEEN 30 AND 10", vec![]),
        ] {
            let query = PreparedGraphText::prepare(
                &format!("MATCH (a:Owner) WHERE {predicate} RETURN a ORDER BY a"), symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let actual = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
            let ids: Vec<_> = actual.value.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect();
            assert_eq!(ids, expected, "{predicate}");
        }
        for (quantifier, expected) in [
            ("EXISTS", vec![VId(1), VId(2)]),
            ("NOT EXISTS", vec![VId(3), VId(4)]),
        ] {
            let query = PreparedGraphText::prepare(
                &format!("MATCH (a:Owner) WHERE {quantifier} {{ MATCH (a)-[:R]->(b) \
                    WHERE b.score IN [15,25] AND b.score BETWEEN a.score AND 35 }} RETURN a ORDER BY a"),
                symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let actual = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
            let ids: Vec<_> = actual.value.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect();
            assert_eq!(ids, expected, "{quantifier}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_queries_obey_exact_work_scratch_source_and_output_limits() {
    let ((), report) = run_async_under_lab(0x1ab1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let query = pattern(Some(2));
        let measured = db.execute_graph_pattern_governed(&cx, &query, policy()).unwrap();
        let exact = GqlQueryPolicy::new(
            measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries,
        );
        assert_eq!(db.execute_graph_pattern_governed(&cx, &query, exact).unwrap(), measured);
        assert_eq!(measured.rows.result_rows, 2);
        assert!(measured.rows.snapshot_records > 0);
        assert!(measured.evaluator.work_units > 0);
        assert!(measured.evaluator.scratch_entries > 0);
        for refused in [
            GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1_000, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1_000, 2, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] {
            assert!(db.execute_graph_pattern_governed(&cx, &query, refused).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rejected_members_and_zero_or_refused_output_keep_transaction_read_dependencies() {
    let ((), report) = run_async_under_lab(0x1ab1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for mode in 0..3 {
            for conflict in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(R);
                staged.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let query = pattern(Some(u64::from(mode != 2)));
                let budget = GqlQueryPolicy::new(1_000, u64::from(mode == 0), 2_000_000, 1_000_000);
                let result = txn.execute_graph_pattern_governed(&db, &cx, &query, budget);
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 1),
                    1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(result.unwrap().value.is_empty()),
                }
                let mut winner = WriteBatch::new(R);
                if conflict {
                    // The excluded member becomes a valid OPTIONAL witness.
                    // No subsequent transaction read can repair missing tracking.
                    winner.set_vertex_property(VId(12), SCORE, Some(CanonicalScalar::Int(24)));
                } else {
                    winner.create_vertex(VId(888), vec![], vec![]);
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if conflict {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                } else {
                    result.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn having_ranges_keep_exact_averages_empty_groups_and_hidden_source_errors() {
    let ((), report) = run_async_under_lab(0x1ab1_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![(SCORE, CanonicalScalar::Int(1))]);
        batch.create_vertex(VId(2), vec![], vec![(SCORE, CanonicalScalar::Int(2))]);
        db.write(&commit, batch).await.unwrap();
        for (condition, expected_rows) in [
            ("AVG(n.score) BETWEEN 1 AND 2", 1),
            ("AVG(n.score) IN [1,2]", 0),
            ("AVG(n.score) NOT IN [1,2]", 1),
            ("AVG(n.score) NOT IN [1,NULL]", 0),
            ("AVG(n.score) IN []", 0),
            ("AVG(n.score) NOT IN []", 1),
            ("c IN [1,2,NULL] AND c BETWEEN 1 AND 2", 1),
        ] {
            let query = PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {condition}"), symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let actual = db.execute_graph_aggregate_governed(&cx, &query, policy()).unwrap();
            assert_eq!(actual.value.len(), expected_rows, "{condition}");
            for row in actual.value {
                assert!(row.keys().is_empty());
                assert_eq!(row.values().len(), 1, "hidden aggregate must not leak");
                assert_eq!(row.values()[0].as_count(), Some(2));
            }
        }
        let empty = PreparedGraphAggregateText::prepare(
            "MATCH (n) WHERE n.score IN [] RETURN COUNT(*) AS c \
                HAVING c IN [0] AND MIN(n.score) NOT IN []", symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let rows = db.execute_graph_aggregate_governed(&cx, &empty, policy()).unwrap().value;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values()[0].as_count(), Some(0));

        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::ucs_basic_text("not an integer").unwrap()));
        db.write(&commit, invalid).await.unwrap();
        for condition in ["SUM(n.score) IN []", "SUM(n.score) NOT IN []", "TRUE OR SUM(n.score) IN [1]"] {
            let query = PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {condition}"), symbols,
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            assert!(matches!(
                db.execute_graph_aggregate_governed(&cx, &query, policy()),
                Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { .. }))
            ), "{condition}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
