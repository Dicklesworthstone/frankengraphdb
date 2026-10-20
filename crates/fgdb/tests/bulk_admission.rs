//! Import receipts may be published only after the engine validates the source
//! and reconstructed prefix. All graph operations use the production database.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BulkEdge, BulkEndpoint, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow,
    BulkVertex, Database, DatabaseKeys,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};
use std::io;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn vertex(key: &str, value: i64) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: key.into(),
        labels: vec![LabelId(1)],
        props: vec![(K, CanonicalScalar::Int(value).into())],
    })
}
fn matched_edge(value: i64) -> BulkRow {
    let endpoint = BulkEndpoint::matching(Some(LabelId(1)), K, CanonicalScalar::Int(value));
    BulkRow::Edge(BulkEdge {
        key: "e".into(),
        source: endpoint.clone(),
        destination: endpoint,
        relation: R,
        props: vec![],
    })
}
fn source(rows: Vec<BulkRow>) -> impl Iterator<Item = Result<BulkRow, io::Error>> + Clone {
    rows.into_iter().map(Ok)
}
fn quiet(_: &BulkLoadCheckpoint) -> io::Result<()> {
    Ok(())
}

#[test]
fn admission_precedes_new_commits_and_runs_once_for_empty_and_completed_loads() {
    let ((), report) = run_async_under_lab(0xad11_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let rows = vec![vertex("v0", 0), vertex("v1", 1), vertex("v2", 2)];
        let mut events = Vec::new();
        let sink = &mut events;
        let cp = db
            .try_bulk_load_with_admission(
                &query,
                &commit,
                source(rows.clone()),
                BulkLoadPolicy::new(2, R),
                None,
                |checked| {
                    sink.push((false, checked.next_row, checked.frontier));
                    Ok(move |durable: &BulkLoadCheckpoint| {
                        sink.push((true, durable.next_row, durable.frontier));
                        Ok(())
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(
            events,
            [
                (false, 0, basis),
                (true, 2, CommitSeq(basis.0 + 1)),
                (true, 3, CommitSeq(basis.0 + 2)),
            ]
        );
        let mut policy = BulkLoadPolicy::new(2, R);
        policy.resume = Some(cp.clone());
        let mut admitted = 0;
        db.try_bulk_load_with_admission(&query, &commit, source(rows), policy, None, |checked| {
            admitted += 1;
            assert_eq!(checked.next_row, 3);
            assert_eq!(checked.frontier, cp.frontier);
            Ok(|_: &BulkLoadCheckpoint| -> io::Result<()> {
                panic!("completed resume must not publish a new chunk")
            })
        })
        .await
        .unwrap();
        assert_eq!(admitted, 1);
        assert_eq!(db.frontier().unwrap(), cp.frontier);
        db.try_bulk_load_with_admission(
            &query,
            &commit,
            source(Vec::new()),
            BulkLoadPolicy::new(2, R),
            None,
            |checked| {
                admitted += 1;
                assert_eq!(checked.next_row, 0);
                assert_eq!(checked.frontier, cp.frontier);
                Ok(|_: &BulkLoadCheckpoint| -> io::Result<()> {
                    panic!("empty load must not publish a new chunk")
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(admitted, 2);
        assert_eq!(db.frontier().unwrap(), cp.frontier);
        assert_eq!(db.vertices().unwrap().len(), 3);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_preflight_refusals_precede_admission_and_do_not_change_the_graph() {
    let ((), report) = run_async_under_lab(0xad11_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        for case in 0..3 {
            let rows = match case {
                0 => vec![vertex("same", 1), vertex("same", 2)],
                1 => vec![matched_edge(999)],
                _ => vec![vertex("large", 1)],
            };
            let mut policy = BulkLoadPolicy::new(1, R);
            if case == 2 {
                policy.max_chunk_bytes = 0;
            }
            let mut calls = 0;
            let failed = db
                .try_bulk_load_with_admission(&query, &commit, source(rows), policy, None, |_| {
                    calls += 1;
                    Ok(quiet as fn(&BulkLoadCheckpoint) -> io::Result<()>)
                })
                .await
                .unwrap_err();
            match (case, failed.kind) {
                (0, BulkLoadErrorKind::DuplicateCallerKey { .. })
                | (1, BulkLoadErrorKind::UnresolvedEndpoint { .. })
                | (
                    2,
                    BulkLoadErrorKind::SourceLimit {
                        dimension: "chunk_bytes",
                        ..
                    },
                ) => {}
                (_, other) => panic!("unexpected preflight refusal: {other:?}"),
            }
            assert_eq!(calls, 0);
            assert!(failed.pending.is_none());
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
            assert!(db.edges().unwrap().is_empty());
        }
        let input = (0..2).map(|index| {
            if index == 0 {
                Ok(vertex("first", 1))
            } else {
                Err(io::Error::other("decoder failed after one valid row"))
            }
        });
        let mut calls = 0;
        let failed = db
            .try_bulk_load_with_admission(
                &query,
                &commit,
                input,
                BulkLoadPolicy::new(1, R),
                None,
                |_| {
                    calls += 1;
                    Ok(quiet as fn(&BulkLoadCheckpoint) -> io::Result<()>)
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            failed.kind,
            BulkLoadErrorKind::Source { row: 1, .. }
        ));
        assert_eq!(calls, 0);
        assert_eq!(db.frontier().unwrap(), basis);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reconstructed_edge_with_wrong_matching_endpoint_never_reaches_admission() {
    let ((), report) = run_async_under_lab(0xad11_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.bulk_load(
            &query,
            &commit,
            vec![vertex("a", 1), vertex("b", 2)],
            BulkLoadPolicy::new(2, R),
        )
        .await
        .unwrap();
        // Both endpoint selectors are valid and unique at the original basis.
        // The authenticated creation has the correct kind/properties/relation
        // and engine-issued ID, but is NOT the source's intended edge.
        let cp = db
            .bulk_load(
                &query,
                &commit,
                vec![matched_edge(2)],
                BulkLoadPolicy::new(1, R),
            )
            .await
            .unwrap();
        let frontier = cp.frontier;
        let mut policy = BulkLoadPolicy::new(1, R);
        policy.resume = Some(cp);
        let mut calls = 0;
        let failed = db
            .try_bulk_load_with_admission(
                &query,
                &commit,
                source(vec![matched_edge(1)]),
                policy,
                None,
                |_| {
                    calls += 1;
                    Ok(quiet as fn(&BulkLoadCheckpoint) -> io::Result<()>)
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(failed.kind, BulkLoadErrorKind::InvalidResume));
        assert_eq!(calls, 0);
        assert!(failed.pending.is_none());
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.edges().unwrap().len(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_admission_or_durable_callback_has_an_exact_retry_boundary() {
    let ((), report) = run_async_under_lab(0xad11_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let rows = vec![vertex("v0", 0), vertex("v1", 1)];
        let failed = db
            .try_bulk_load_with_admission(
                &query,
                &commit,
                source(rows.clone()),
                BulkLoadPolicy::new(1, R),
                None,
                |_| {
                    Err::<fn(&BulkLoadCheckpoint) -> io::Result<()>, _>(io::Error::other(
                        "receipt publication failed before first commit",
                    ))
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(failed.kind, BulkLoadErrorKind::Checkpoint(_)));
        assert!(failed.pending.is_none());
        assert_eq!(failed.committed.next_row, 0);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        let failed = db
            .try_bulk_load_with_admission(
                &query,
                &commit,
                source(rows.clone()),
                BulkLoadPolicy::new(1, R),
                None,
                |_| {
                    Ok(|cp: &BulkLoadCheckpoint| -> io::Result<()> {
                        assert_eq!(cp.next_row, 1);
                        Err(io::Error::other(
                            "delivery failed after the first durable commit",
                        ))
                    })
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(failed.kind, BulkLoadErrorKind::Checkpoint(_)));
        assert!(failed.pending.is_none());
        assert_eq!(failed.committed.next_row, 1);
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 1));
        let mut policy = BulkLoadPolicy::new(1, R);
        policy.resume = Some(failed.committed);
        let mut sequence = Vec::new();
        let sink = &mut sequence;
        let completed = db
            .try_bulk_load_with_admission(&query, &commit, source(rows), policy, None, |cp| {
                sink.push((false, cp.next_row));
                Ok(move |cp: &BulkLoadCheckpoint| {
                    sink.push((true, cp.next_row));
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(sequence, [(false, 1), (true, 2)]);
        assert_eq!(completed.frontier, CommitSeq(basis.0 + 2));
        assert_eq!(db.vertices().unwrap().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn checkpoint_only_api_does_not_gain_a_baseline_callback() {
    let ((), report) = run_async_under_lab(0xad11_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let rows = vec![vertex("v0", 0), vertex("v1", 1)];
        let mut positions = Vec::new();
        let cp = db
            .try_bulk_load_with_checkpoint(
                &query,
                &commit,
                source(rows.clone()),
                BulkLoadPolicy::new(1, R),
                None,
                |cp| {
                    positions.push(cp.next_row);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(positions, [1, 2]);
        let mut policy = BulkLoadPolicy::new(1, R);
        policy.resume = Some(cp.clone());
        db.try_bulk_load_with_checkpoint(
            &query,
            &commit,
            source(rows),
            policy,
            None,
            |_| -> io::Result<()> { panic!("completed checkpoint-only resume is silent") },
        )
        .await
        .unwrap();
        assert_eq!(db.frontier().unwrap(), cp.frontier);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
