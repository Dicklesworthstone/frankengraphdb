//! Exercise the real loader with Clone iterators whose second observation lies.
//! Neither Clone nor a caller checkpoint is source-content authority.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BulkEdge, BulkLoadErrorKind, BulkLoadPolicy, BulkRow, BulkVertex, Database, DatabaseKeys,
    IdentityPermutation,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_types::{
    CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId,
    EId, PurposeContexts,
};
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
/// The engine identity issued for `counter` under [`keys`] (fgdb-hxgm1
/// channel 2): the next engine vertex on a handle that has issued none is
/// counter 1.
fn engine_vertex(counter: u64) -> fgdb_types::VId {
    fgdb_types::VId(u128::from(
        IdentityPermutation::vertices(&keys())
            .permute(counter)
            .unwrap(),
    ))
}
fn engine_edge(counter: u64) -> EId {
    EId(u128::from(
        IdentityPermutation::edges(&keys())
            .permute(counter)
            .unwrap(),
    ))
}
fn vertex(key: &str) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: key.into(),
        labels: vec![LabelId(1)],
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(7).into())],
    })
}
fn edge(key: &str, src: &str, dst: &str) -> BulkRow {
    BulkRow::Edge(BulkEdge {
        key: key.into(),
        source: src.into(),
        destination: dst.into(),
        relation: RelationId(1),
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(9).into())],
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

fn nullable_rows() -> Vec<BulkRow> {
    let mut result = rows();
    for (ordinal, row) in result.iter_mut().enumerate() {
        let props = match row {
            BulkRow::Vertex(vertex) => &mut vertex.props,
            BulkRow::Edge(edge) => &mut edge.props,
        };
        props[0].1 = CanonicalScalar::Int(10 + ordinal as i64).into();
        props.push((PropertyKeyId(2), CanonicalScalar::Null.into()));
        props.push((
            PropertyKeyId(3),
            CanonicalPropertyValue::List(
                CanonicalList::try_new(vec![
                    CanonicalScalar::Null.into(),
                    CanonicalScalar::Int(42).into(),
                ])
                .unwrap(),
            ),
        ));
    }
    result
}
fn policy() -> BulkLoadPolicy {
    BulkLoadPolicy::new(2, RelationId(1))
}

#[test]
fn changed_vertex_fields_and_row_order_never_allocate_or_publish() {
    let ((), report) = run_async_under_lab(0x000b_5101, |root| async move {
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
                    3 => v.props[0].1 = CanonicalScalar::Int(8).into(),
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
                ElementId::Vertex(engine_vertex(1))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_edge_fields_never_publish_an_unchecked_chunk_or_panic_on_endpoints() {
    let ((), report) = run_async_under_lab(0x000b_5102, |root| async move {
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
                    5 => e.props[0].1 = CanonicalScalar::Int(10).into(),
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
            // The committed first chunk took vertex counters only, and the
            // refused edge chunk took none.
            assert_eq!(
                db.allocate_identity(&cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
                    .unwrap(),
                ElementId::Edge(engine_edge(1))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_truncation_and_appended_suffix_is_a_typed_refusal() {
    let ((), report) = run_async_under_lab(0x000b_5103, |root| async move {
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
    let ((), report) = run_async_under_lab(0x000b_5104, |root| async move {
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
    let ((), report) = run_async_under_lab(0x000b_5105, |root| async move {
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
                v.props[0].1 = replacement.into();
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

#[test]
fn null_bearing_vertex_and_edge_prefixes_resume_without_an_extra_commit() {
    let ((), report) = run_async_under_lab(0x000b_5106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let original = nullable_rows();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let stopped = db
            .bulk_load_with_checkpoint(&cx, &commit, original.clone(), policy(), None, |_| {
                Err(std::io::Error::other("stop after the vertex chunk"))
            })
            .await
            .unwrap_err();
        assert!(matches!(stopped.kind, BulkLoadErrorKind::Checkpoint(_)));
        assert_eq!(stopped.committed.next_row, 2);
        assert_eq!(stopped.committed.committed_chunks, 1);
        assert_eq!(stopped.committed.frontier, CommitSeq(basis.0 + 1));
        let mut resumed = policy();
        resumed.resume = Some(stopped.committed);
        let stopped = db
            .bulk_load_with_checkpoint(&cx, &commit, original.clone(), resumed, None, |_| {
                Err(std::io::Error::other("stop after the edge chunk"))
            })
            .await
            .unwrap_err();
        assert!(matches!(stopped.kind, BulkLoadErrorKind::Checkpoint(_)));
        let complete = stopped.committed;
        assert_eq!((complete.next_row, complete.committed_chunks), (4, 2));
        assert_eq!(complete.frontier, CommitSeq(basis.0 + 2));
        let mut resumed = policy();
        resumed.resume = Some(complete.clone());
        let repeated = db
            .bulk_load(&cx, &commit, original.clone(), resumed)
            .await
            .unwrap();
        assert_eq!(repeated.vertices, complete.vertices);
        assert_eq!(repeated.edges, complete.edges);
        assert_eq!((repeated.next_row, repeated.committed_chunks), (4, 2));
        assert_eq!(repeated.frontier, complete.frontier);
        assert_eq!(db.frontier().unwrap(), complete.frontier);
        assert_eq!(db.delta_since(basis).unwrap().count(), 2);
        let stored = |value| {
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(value).into()),
                (
                    PropertyKeyId(3),
                    CanonicalPropertyValue::List(
                        CanonicalList::try_new(vec![
                            CanonicalScalar::Null.into(),
                            CanonicalScalar::Int(42).into(),
                        ])
                        .unwrap(),
                    ),
                ),
            ]
        };
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 2);
        for (key, value) in [("a", 10), ("b", 11)] {
            let vertex = db.vertex(complete.vertices[key]).unwrap().unwrap();
            assert_eq!(vertex.labels, [LabelId(1)]);
            assert_eq!(vertex.props, stored(value));
        }
        for (key, source, destination, value) in [("ab", "a", "b", 12), ("ba", "b", "a", 13)] {
            let edge = db.edge(complete.edges[key]).unwrap().unwrap();
            assert_eq!(edge.entry.src, complete.vertices[source]);
            assert_eq!(edge.entry.dst, complete.vertices[destination]);
            assert_eq!(edge.entry.relation, RelationId(1));
            assert_eq!(edge.props, stored(value));
        }
        // The input itself remains NULL-bearing, including its raw transcript.
        for row in original {
            let props = match row {
                BulkRow::Vertex(vertex) => vertex.props,
                BulkRow::Edge(edge) => edge.props,
            };
            assert_eq!(props.len(), 3);
            assert_eq!(props[1], (PropertyKeyId(2), CanonicalScalar::Null.into()));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn logical_null_resume_keeps_nonnull_nested_and_forged_identity_refusals() {
    let ((), report) = run_async_under_lab(0x000b_5107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let original = nullable_rows();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let complete = db
            .bulk_load(&cx, &commit, original.clone(), policy())
            .await
            .unwrap();
        for fault in 0_usize..6 {
            let mut changed = original.clone();
            let mut forged = complete.clone();
            if fault < 4 {
                let row = if fault.is_multiple_of(2) {
                    &mut changed[0]
                } else {
                    &mut changed[2]
                };
                let props = match row {
                    BulkRow::Vertex(vertex) => &mut vertex.props,
                    BulkRow::Edge(edge) => &mut edge.props,
                };
                if fault < 2 {
                    props[0].1 = CanonicalScalar::Int(999).into();
                } else {
                    // Omitting a nested NULL changes the stored list, unlike
                    // omitting its top-level NULL-valued sibling property.
                    props[2].1 = CanonicalPropertyValue::List(
                        CanonicalList::try_new(vec![CanonicalScalar::Int(42).into()]).unwrap(),
                    );
                }
            } else if fault == 4 {
                let a = forged.vertices["a"];
                let b = forged.vertices["b"];
                forged.vertices.insert("a".into(), b);
                forged.vertices.insert("b".into(), a);
            } else {
                let ab = forged.edges["ab"];
                let ba = forged.edges["ba"];
                forged.edges.insert("ab".into(), ba);
                forged.edges.insert("ba".into(), ab);
            }
            let mut resumed = policy();
            resumed.resume = Some(forged);
            let refusal = db
                .bulk_load(&cx, &commit, changed, resumed)
                .await
                .unwrap_err();
            assert!(
                matches!(refusal.kind, BulkLoadErrorKind::InvalidResume),
                "fault={fault}: {refusal:?}"
            );
            assert!(refusal.pending.is_none());
            assert_eq!(db.frontier().unwrap(), complete.frontier);
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(db.edges().unwrap().len(), 2);
        }
        // None of the refused source/mapping changes may consume identities.
        assert_eq!(
            db.allocate_identity(&cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(engine_vertex(3)),
        );
        assert_eq!(
            db.allocate_identity(&cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
                .unwrap(),
            ElementId::Edge(engine_edge(3)),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_null_omission_drift_is_not_hidden_by_logical_resume_comparison() {
    let ((), report) = run_async_under_lab(0x000b_5108, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for changed_row in [0, 2] {
            for null_first in [false, true] {
                let with_null = nullable_rows();
                let mut omitted = with_null.clone();
                let props = match &mut omitted[changed_row] {
                    BulkRow::Vertex(vertex) => &mut vertex.props,
                    BulkRow::Edge(edge) => &mut edge.props,
                };
                props.remove(1);
                let (audited, replayed) = if null_first {
                    (with_null, omitted)
                } else {
                    (omitted, with_null)
                };
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let basis = db.frontier().unwrap();
                let refusal = db
                    .bulk_load(
                        &cx,
                        &commit,
                        DriftingSource::new(audited, replayed),
                        policy(),
                    )
                    .await
                    .unwrap_err();
                assert!(
                    matches!(refusal.kind, BulkLoadErrorKind::SourceChanged { row } if row == changed_row)
                );
                assert_eq!(refusal.committed.next_row, changed_row);
                assert_eq!(refusal.committed.committed_chunks, changed_row / 2);
                assert!(refusal.pending.is_none());
                assert_eq!(
                    db.frontier().unwrap(),
                    CommitSeq(basis.0 + (changed_row / 2) as u64)
                );
                assert_eq!(db.vertices().unwrap().len(), changed_row);
                assert!(db.edges().unwrap().is_empty());
                assert_eq!(
                    db.allocate_identity(&cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                        .unwrap(),
                    ElementId::Vertex(engine_vertex(changed_row as u64 + 1)),
                );
                assert_eq!(
                    db.allocate_identity(&cx, GraphInsertRequest::Edge { row: 0, edge: 0 })
                        .unwrap(),
                    ElementId::Edge(engine_edge(1)),
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
