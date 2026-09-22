//! Exercise the real loader with Clone iterators whose second observation lies.
//! Neither Clone nor a caller checkpoint is source-content authority.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BulkEdge, BulkLoadErrorKind, BulkLoadPolicy, BulkRow, BulkVertex, Database, DatabaseKeys,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::sync::Arc;

struct DriftingSource {
    preflight: Arc<[BulkRow]>,
    replay: Arc<[BulkRow]>,
    at: usize,
    audited: bool,
}
impl DriftingSource {
    fn new(preflight: Vec<BulkRow>, replay: Vec<BulkRow>) -> Self {
        Self {
            preflight: preflight.into(),
            replay: replay.into(),
            at: 0,
            audited: false,
        }
    }
}
impl Clone for DriftingSource {
    fn clone(&self) -> Self {
        Self {
            preflight: self.preflight.clone(),
            replay: self.replay.clone(),
            at: self.at,
            audited: true,
        }
    }
}
impl Iterator for DriftingSource {
    type Item = BulkRow;
    fn next(&mut self) -> Option<BulkRow> {
        let rows = if self.audited {
            &self.preflight
        } else {
            &self.replay
        };
        let row = rows.get(self.at).cloned();
        if row.is_some() {
            self.at += 1;
        }
        row
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn vertex(key: &str) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: key.into(),
        labels: vec![LabelId(1)],
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
    })
}
fn edge(key: &str, src: &str, dst: &str) -> BulkRow {
    BulkRow::Edge(BulkEdge {
        key: key.into(),
        source: src.into(),
        destination: dst.into(),
        relation: RelationId(1),
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(9))],
    })
}
fn rows() -> Vec<BulkRow> {
    vec![
        vertex("a"),
        vertex("b"),
        edge("ab", "a", "b"),
        edge("ba", "b", "a"),
    ]
}
fn policy() -> BulkLoadPolicy {
    BulkLoadPolicy::new(2, RelationId(1))
}

#[test]
fn changed_vertex_fields_and_row_order_never_allocate_or_publish() {
    let ((), report) = run_async_under_lab(0xb51_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for field in 0..5 {
            let expected = rows();
            let mut changed = expected.clone();
            match &mut changed[0] {
                BulkRow::Vertex(v) => match field {
                    0 => v.key = "renamed".into(),
                    1 => v.labels = vec![LabelId(2)],
                    2 => v.props[0].0 = PropertyKeyId(2),
                    3 => v.props[0].1 = CanonicalScalar::Int(8),
                    _ => {}
                },
                _ => unreachable!(),
            }
            if field == 4 {
                changed.swap(0, 1);
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .bulk_load(
                    &cx,
                    &commit,
                    DriftingSource::new(expected, changed),
                    policy(),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error.kind,
                BulkLoadErrorKind::SourceChanged { row: 0 }
            ));
            assert_eq!(error.committed.next_row, 0);
            assert!(error.pending.is_none());
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(
                db.allocate_identity(&cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(fgdb_types::VId(1))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_edge_fields_never_publish_an_unchecked_chunk_or_panic_on_endpoints() {
    let ((), report) = run_async_under_lab(0xb51_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for field in 0..8 {
            let expected = rows();
            let mut changed = expected.clone();
            match &mut changed[2] {
                BulkRow::Edge(e) => match field {
                    0 => e.key = "renamed".into(),
                    1 => e.source = "absent".into(),
                    2 => e.destination = "absent".into(),
                    3 => e.relation = RelationId(2),
                    4 => e.props[0].0 = PropertyKeyId(2),
                    5 => e.props[0].1 = CanonicalScalar::Int(10),
                    _ => {}
                },
                _ => unreachable!(),
            }
            if field == 6 {
                changed.swap(2, 3);
            }
            if field == 7 {
                changed[2] = vertex("different-kind");
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut acknowledgements = 0;
            let error = db
                .bulk_load_with_checkpoint(
                    &cx,
                    &commit,
                    DriftingSource::new(expected, changed),
                    policy(),
                    None,
                    |_| {
                        acknowledgements += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error.kind,
                BulkLoadErrorKind::SourceChanged { row: 2 }
            ));
            assert_eq!(error.committed.next_row, 2);
            assert_eq!(error.committed.committed_chunks, 1);
            assert_eq!(db.frontier().unwrap(), error.committed.frontier);
            assert_eq!(acknowledgements, 1);
            assert!(error.pending.is_none());
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(
                db.allocate_identity(&cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
                    .unwrap(),
                ElementId::Edge(EId(1))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_truncation_and_appended_suffix_is_a_typed_refusal() {
    let ((), report) = run_async_under_lab(0xb51_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for len in 0..4 {
            let expected = rows();
            let changed = expected[..len].to_vec();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let error = db
                .bulk_load(
                    &cx,
                    &commit,
                    DriftingSource::new(expected, changed),
                    policy(),
                )
                .await
                .unwrap_err();
            assert!(matches!(error.kind, BulkLoadErrorKind::SourceChanged { row } if row == len));
            assert_eq!(error.committed.next_row, if len < 2 { 0 } else { 2 });
            assert!(error.pending.is_none());
            assert!(db.edges().unwrap().is_empty());
        }
        let expected = rows();
        let mut changed = expected.clone();
        changed.push(vertex("extra"));
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .bulk_load(
                &cx,
                &commit,
                DriftingSource::new(expected, changed),
                policy(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceChanged { row: 4 }
        ));
        assert_eq!(
            error.committed.next_row, 2,
            "check EOF before committing the last chunk"
        );
        assert!(db.edges().unwrap().is_empty());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .bulk_load(
                &cx,
                &commit,
                DriftingSource::new(vec![], vec![vertex("not-empty")]),
                policy(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceChanged { row: 0 }
        ));
        assert!(db.vertices().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn resume_verifies_skipped_source_prefix_and_can_continue_after_refusal() {
    let ((), report) = run_async_under_lab(0xb51_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let original = rows();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let stopped = db
            .bulk_load_with_checkpoint(&cx, &commit, original.clone(), policy(), None, |_| {
                Err(std::io::Error::other("stop after one durable chunk"))
            })
            .await
            .unwrap_err();
        assert!(matches!(stopped.kind, BulkLoadErrorKind::Checkpoint(_)));
        assert_eq!(stopped.committed.next_row, 2);
        let mut resumed = policy();
        resumed.resume = Some(stopped.committed.clone());
        let mut changed = original.clone();
        changed.swap(0, 1);
        let error = db
            .bulk_load(
                &cx,
                &commit,
                DriftingSource::new(original.clone(), changed),
                resumed.clone(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceChanged { row: 0 }
        ));
        assert_eq!(error.committed.next_row, 2);
        assert_eq!(error.committed.frontier, stopped.committed.frontier);
        assert!(error.pending.is_none());
        let done = db
            .bulk_load(&cx, &commit, original.clone(), resumed)
            .await
            .unwrap();
        assert_eq!(done.next_row, 4);
        assert_eq!(done.committed_chunks, 2);
        assert_eq!(db.edges().unwrap().len(), 2);
        let before = db.frontier().unwrap();
        let mut resumed = policy();
        resumed.resume = Some(done);
        let same = db.bulk_load(&cx, &commit, original, resumed).await.unwrap();
        assert_eq!(same.frontier, before);
        assert_eq!(
            db.frontier().unwrap(),
            before,
            "completed resume cannot mint another marker"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_scalars_and_length_delimited_keys_do_not_alias() {
    let ((), report) = run_async_under_lab(0xb51_05, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for replacement in [
            CanonicalScalar::Bool(true),
            CanonicalScalar::Null,
            CanonicalScalar::ucs_basic_text("7").unwrap(),
            CanonicalScalar::bytes(vec![7]).unwrap(),
        ] {
            let expected = vec![vertex("a\0bc")];
            let mut actual = expected.clone();
            if let BulkRow::Vertex(v) = &mut actual[0] {
                v.props[0].1 = replacement;
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let error = db
                .bulk_load(
                    &cx,
                    &commit,
                    DriftingSource::new(expected, actual),
                    policy(),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error.kind,
                BulkLoadErrorKind::SourceChanged { row: 0 }
            ));
            assert!(db.vertices().unwrap().is_empty());
        }
        let expected = vec![vertex("a"), vertex("bc")];
        let actual = vec![vertex("ab"), vertex("c")];
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .bulk_load(
                &cx,
                &commit,
                DriftingSource::new(expected, actual),
                policy(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceChanged { row: 0 }
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
