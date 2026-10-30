//! The owner count source is the ordinary admission, event for event.

use super::*;
use crate::gql_exec::source::SourceEvent;
use crate::{DatabaseKeys, EdgeRecord, VertexRow, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};

fn query(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

fn source_run(
    snapshot: &Snapshot,
    aggregate: &PreparedGraphAggregate,
    at: CommitSeq,
    summary: bool,
    stop: Option<usize>,
) -> (Result<(u64, u64), usize>, Vec<SourceEvent>) {
    let source = aggregate.count_summary_source().unwrap();
    let mut events = Vec::new();
    let mut refused = false;
    let mut control = |event| {
        assert!(!refused, "source called again after a refusal");
        let ordinal = events.len();
        events.push(event);
        if stop == Some(ordinal) {
            refused = true;
            Err(ordinal)
        } else {
            Ok(())
        }
    };
    let result = if summary {
        count_input(
            snapshot,
            aggregate.input_pattern().plan(),
            source,
            at,
            &mut control,
        )
    } else {
        super::super::admit(snapshot, aggregate.input_pattern().plan(), at, &mut control).map(
            |tables| match source {
                GraphCountSource::Vertices => {
                    let count = tables.vertices.len() as u64;
                    assert_eq!(tables.snapshot_records, count);
                    (count, count)
                }
                GraphCountSource::Edges | GraphCountSource::Label(_) => {
                    panic!("this input does not admit scan summaries")
                }
                GraphCountSource::Relation(relation) => {
                    let examined = tables.edges.len() as u64;
                    let matching = tables
                        .edges
                        .iter()
                        .filter(|((_, _, actual, _), _)| *actual == relation)
                        .count() as u64;
                    assert_eq!(tables.snapshot_records, examined);
                    (examined, matching)
                }
            },
        )
    };
    (result, events)
}

#[test]
fn counts_match_materialized_sources_at_every_history_and_refusal_boundary() {
    let ((), report) = run_async_under_lab(0xc017_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let keys = DatabaseKeys::new(
            [0x27; 32],
            DatabaseSecurityNamespaceId([0x28; 32]),
            [0x29; 32],
        );
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        for id in [1, 2, 3, u128::MAX - 1] {
            first.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
        for (eid, from, to) in [(10, 1, 2), (11, 1, 2), (12, 2, 2), (13, 3, 1)] {
            first.add_edge(
                EId(u128::MAX - eid),
                VId(from),
                VId(to),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
            );
        }
        db.write(&contexts.commit(), first).await.unwrap();
        let mut second = WriteBatch::new(RelationId(2));
        second.add_edge(EId(20), VId(1), VId(2), vec![]);
        db.write(&contexts.commit(), second).await.unwrap();
        let mut third = WriteBatch::new(RelationId(1));
        third.delete_edge(EId(u128::MAX - 10));
        third.set_edge_property(
            EId(u128::MAX - 11),
            PropertyKeyId(1),
            Some(CanonicalScalar::Bool(true)),
        );
        third.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        third.delete_vertex(VId(u128::MAX - 1));
        third.create_vertex(VId(1 << 127), vec![], vec![]);
        db.write(&contexts.commit(), third).await.unwrap();

        for text in [
            "MATCH (n) RETURN COUNT(*) AS total",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total",
            "MATCH (a)<-[:R]-(b) RETURN COUNT(*) AS total",
            "MATCH (a)-[r:S]->(b) RETURN COUNT(*) AS total",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS total LIMIT 0",
        ] {
            let aggregate = query(text);
            for cut in 0..=3 {
                let at = CommitSeq(cut);
                let materialized = source_run(&db.snapshot, &aggregate, at, false, None);
                assert_eq!(
                    source_run(&db.snapshot, &aggregate, at, true, None),
                    materialized,
                    "{text}, cut {cut}"
                );
                for stop in 0..=materialized.1.len() {
                    assert_eq!(
                        source_run(&db.snapshot, &aggregate, at, true, Some(stop)),
                        source_run(&db.snapshot, &aggregate, at, false, Some(stop)),
                        "{text}, cut {cut}, stop {stop}"
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// A reader without the private admitted Snapshot can fail independently of
// the owner. Even a filterless count must consult that original source.
struct RefusingReader;

impl GqlSnapshotReader for RefusingReader {
    fn gql_vertex_at(&self, _: VId, _: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
        panic!("filterless count performed a vertex point lookup")
    }

    fn gql_edge_at(&self, _: EId, _: CommitSeq) -> Result<Option<EdgeRecord>, ReadError> {
        panic!("filterless count performed an edge point lookup")
    }

    fn gql_vertices_at(&self, asked: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
        Err(ReadError::BeyondFrontier {
            asked,
            frontier: CommitSeq(11),
        })
    }

    fn gql_edges_at(&self, asked: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
        Err(ReadError::BeyondFrontier {
            asked,
            frontier: CommitSeq(12),
        })
    }
}

#[test]
fn fallible_owned_readers_keep_their_source_errors_and_preceding_checkpoint() {
    for (text, frontier) in [
        ("MATCH (n) RETURN COUNT(*) AS total", CommitSeq(11)),
        ("MATCH (n:L) RETURN COUNT(*) AS total", CommitSeq(11)),
        (
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total",
            CommitSeq(12),
        ),
        ("MATCH (a)-[r]->(b) RETURN COUNT(*) AS total", CommitSeq(12)),
    ] {
        let aggregate = query(text);
        let policy = GqlQueryPolicy::new(0, 0, 0, 0);
        let mut calls = 0;
        let result = execute_at(&RefusingReader, &aggregate, CommitSeq(21), policy, || {
            calls += 1;
            Ok::<_, usize>(())
        });
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphAggregateError::Source(GqlError::Read(
                ReadError::BeyondFrontier { asked: CommitSeq(21), frontier: actual }
            )))) if actual == frontier
        ));
        assert_eq!(calls, 1);
        assert!(matches!(
            execute_at(&RefusingReader, &aggregate, CommitSeq(21), policy, || Err(
                7
            )),
            Err(GqlQueryError::Interrupted(7))
        ));
    }
}

#[test]
fn cold_counts_publish_only_after_complete_output_and_survive_every_cancellation() {
    let ((), report) = run_async_under_lab(0xc017_0021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let keys = DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        );
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            batch.create_vertex(VId(id), vec![LabelId(if id <= 2 { 1 } else { 2 })], vec![]);
        }
        for (eid, source, destination) in [(1, 1, 1), (2, 1, 2), (3, 1, 2), (4, 2, 3), (5, 3, 4)] {
            batch.add_edge(EId(eid), VId(source), VId(destination), vec![]);
        }
        db.write(&contexts.commit(), batch).await.unwrap();
        let policy = GqlQueryPolicy::new(100, 10, 100_000, 100_000);
        let at = db.snapshot.frontier;
        for (pattern, matching, records) in [
            ("(n)", 4, 4),
            ("(n:L)", 2, 4),
            ("(a)-[:R]->(b)", 5, 5),
            ("(a)-[r]->(b)", 5, 5),
            ("(a)<-[r]-(b)", 5, 5),
        ] {
            let plain = query(&format!("MATCH {pattern} RETURN COUNT(*) AS total"));
            let source = plain.count_metadata_source().unwrap();
            let bad = query(&format!(
                "MATCH {pattern} RETURN 1 / (COUNT(*) - COUNT(*)) AS total LIMIT 0"
            ));
            std::sync::Arc::make_mut(&mut db.snapshot).exact_counts = Default::default();
            assert!(matches!(
                execute_at(&db, &bad, at, policy, || Ok::<_, usize>(())),
                Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { .. }
                ))
            ));
            assert_eq!(
                db.snapshot
                    .exact_counts
                    .lookup(source, &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
                None
            );
            assert!(matches!(
                execute_at(&db, &plain, at, GqlQueryPolicy::new(100, 0, 100_000, 100_000), || Ok::<_, usize>(())),
                Err(GqlQueryError::Rows(error)) if error.dimension == fgdb_gql::GqlBudgetDimension::ResultRows
            ));
            assert_eq!(
                db.snapshot
                    .exact_counts
                    .lookup(source, &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
                None
            );

            let mut calls = 0;
            let full = execute_at(&db, &plain, at, policy, || {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
            assert_eq!(full.value[0].values()[0].as_count(), Some(matching));
            assert_eq!(full.rows.snapshot_records, records);
            assert_eq!(
                db.snapshot
                    .exact_counts
                    .lookup(source, &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
                Some(matching)
            );
            let warm = execute_at(&db, &plain, at, policy, || Ok::<_, usize>(())).unwrap();
            assert_eq!(warm.value, full.value);
            assert_eq!(warm.rows.snapshot_records, 1);
            assert!(warm.evaluator.work_units < full.evaluator.work_units);
            for stop in 1..=calls {
                std::sync::Arc::make_mut(&mut db.snapshot).exact_counts = Default::default();
                let mut seen = 0;
                let result = execute_at(&db, &plain, at, policy, || {
                    seen += 1;
                    assert!(seen <= stop, "polled after refusing");
                    if seen == stop { Err(stop) } else { Ok(()) }
                });
                assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
                assert_eq!(seen, stop);
                assert_eq!(
                    db.snapshot
                        .exact_counts
                        .lookup(source, &mut |_| Ok::<_, ()>(()))
                        .unwrap(),
                    None
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wildcard_counts_follow_commits_pins_failed_writes_and_reopen() {
    let ((), report) = run_async_under_lab(0xc017_0031, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let keys = DatabaseKeys::new(
            [0x51; 32],
            DatabaseSecurityNamespaceId([0x52; 32]),
            [0x53; 32],
        );
        let vfs = crate::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db =
            Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys.clone())
                .await
                .unwrap();
        let queries = [
            query("MATCH ()-[r]->() RETURN COUNT(*) AS total"),
            query("MATCH (a)<-[r]-(b) RETURN COUNT(*) AS total"),
        ];
        let tight = GqlQueryPolicy::new(1, 1, 100, 100);
        let wide = GqlQueryPolicy::new(100, 10, 100_000, 100_000);
        for aggregate in &queries {
            let result = execute_at(&db, aggregate, db.snapshot.frontier, tight, || {
                Ok::<_, ()>(())
            })
            .unwrap();
            assert_eq!(result.value[0].values()[0].as_count(), Some(0));
        }
        let mut first = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            first.create_vertex(VId(id), vec![], vec![]);
        }
        for (eid, source, destination) in [(1, 1, 2), (2, 1, 2), (3, 2, 2)] {
            first.add_edge(EId(eid), VId(source), VId(destination), vec![]);
        }
        db.write(&contexts.commit(), first).await.unwrap();
        let mut second = WriteBatch::new(RelationId(u64::MAX));
        second.add_edge(EId(4), VId(3), VId(1), vec![]);
        db.write(&contexts.commit(), second).await.unwrap();
        let pinned = db.pinned_read_view().unwrap();
        let pinned_at = db.snapshot.frontier;
        for aggregate in &queries {
            let result = execute_at(&db, aggregate, pinned_at, tight, || Ok::<_, ()>(())).unwrap();
            assert_eq!(result.value[0].values()[0].as_count(), Some(4));
            assert_eq!(result.rows.snapshot_records, 1);
        }
        let mut properties = WriteBatch::new(RelationId(1));
        properties.set_edge_property(EId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        properties.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        db.write(&contexts.commit(), properties).await.unwrap();
        let before_failure = db.snapshot.frontier;
        let mut failed = WriteBatch::new(RelationId(1));
        failed.add_edge(EId(5), VId(1), VId(2), vec![]);
        failed.add_edge(EId(6), VId(1), VId(999), vec![]);
        assert!(db.write(&contexts.commit(), failed).await.is_err());
        assert_eq!(db.snapshot.frontier, before_failure);
        for aggregate in &queries {
            let result =
                execute_at(&db, aggregate, before_failure, tight, || Ok::<_, ()>(())).unwrap();
            assert_eq!(result.value[0].values()[0].as_count(), Some(4));
        }
        let mut delete = WriteBatch::new(RelationId(1));
        delete.delete_edge(EId(1));
        db.write(&contexts.commit(), delete).await.unwrap();
        let mut detach = WriteBatch::new(RelationId(1));
        detach.delete_vertex(VId(1));
        db.write(&contexts.commit(), detach).await.unwrap();
        let latest = db.snapshot.frontier;
        for aggregate in &queries {
            let current = execute_at(&db, aggregate, latest, tight, || Ok::<_, ()>(())).unwrap();
            assert_eq!(current.value[0].values()[0].as_count(), Some(1));
            let old = execute_at(&pinned, aggregate, pinned_at, tight, || Ok::<_, ()>(())).unwrap();
            assert_eq!(old.value[0].values()[0].as_count(), Some(4));
            let historical =
                execute_at(&db, aggregate, pinned_at, wide, || Ok::<_, ()>(())).unwrap();
            assert_eq!(historical.value, old.value);
        }
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys)
            .await
            .unwrap();
        assert_eq!(
            reopened
                .snapshot
                .exact_counts
                .lookup(GraphCountSource::Edges, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            None
        );
        let cold = execute_at(&reopened, &queries[0], latest, wide, || Ok::<_, ()>(())).unwrap();
        assert_eq!(cold.value[0].values()[0].as_count(), Some(1));
        let warm = execute_at(&reopened, &queries[1], latest, tight, || Ok::<_, ()>(())).unwrap();
        assert_eq!(warm.value, cold.value);
        assert_eq!(warm.rows.snapshot_records, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
