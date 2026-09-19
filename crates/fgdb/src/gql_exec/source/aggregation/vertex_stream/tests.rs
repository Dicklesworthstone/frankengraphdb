use super::*;
use asupersync::lab::run_async_under_lab;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPatternBuilder, IntegerComparison, VertexPredicate};
use fgdb_gql::stream::VertexScanState;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::cell::Cell;
use std::sync::Arc;

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn pattern(count: Option<u64>) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::HasLabel(LABEL)).unwrap();
    builder.filter("n", VertexPredicate::IntegerProperty { key: KEY, comparison: IntegerComparison::GreaterOrEqual, value: 0 }).unwrap();
    builder.prepare("n", 0, count).unwrap()
}

#[test]
fn every_actual_history_source_checkpoint_refuses_once_and_preserves_the_delivered_prefix() {
    let ((), report) = run_async_under_lab(0x51cb_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [0, 1, 2, 3, 4, u128::MAX] {
            seed.create_vertex(VId(id), vec![LABEL], vec![(KEY, CanonicalScalar::Int(1))]);
        }
        db.write(&commit, seed).await.unwrap();
        for seq in 2..=7 {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(seq - 4)));
            edit.set_vertex_label(VId(3), LABEL, seq % 2 == 0);
            if seq == 3 { edit.delete_vertex(VId(2)); }
            if seq == 5 { edit.create_vertex(VId(5), vec![LABEL], vec![(KEY, CanonicalScalar::Int(5))]); }
            db.write(&commit, edit).await.unwrap();
        }
        let view = db.read_session().unwrap();
        let prepared = pattern(None);
        let plan = VertexScanPlan::compile(prepared.plan()).unwrap();
        for as_of in [CommitSeq(0), CommitSeq(1), CommitSeq(3), CommitSeq(7)] {
            let expected = db.execute_graph_pattern_governed_at(&cx, &prepared, as_of, wide()).unwrap().value;
            let calls = Cell::new(0);
            let mut cursor = VertexScanCursor::new(
                SnapshotVertexSource { view: view.clone(), cx: &cx, as_of, after: None },
                plan.clone(), wide(), || { calls.set(calls.get() + 1); Ok::<_, usize>(()) },
            );
            assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), expected);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let total = calls.get();
            drop(cursor);
            for stop in 1..=total {
                let refs = Arc::strong_count(&view.snapshot);
                let calls = Cell::new(0);
                let mut cursor = VertexScanCursor::new(
                    SnapshotVertexSource { view: view.clone(), cx: &cx, as_of, after: None },
                    plan.clone(), wide(), || {
                        let at = calls.get() + 1; calls.set(at);
                        if at == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert_eq!(Arc::strong_count(&view.snapshot), refs + 1);
                let mut prefix = Vec::new();
                loop {
                    match cursor.next().expect("selected checkpoint must be reached") {
                        Ok(vid) => prefix.push(vid),
                        Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); break; }
                        Err(other) => panic!("unexpected refusal: {other:?}"),
                    }
                }
                assert!(expected.starts_with(&prefix));
                assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert_eq!(Arc::strong_count(&view.snapshot), refs, "failed source must release its generation immediately");
                assert!(cursor.next().is_none()); cursor.close();
                assert_eq!(calls.get(), stop);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                let retry = VertexScanCursor::new(
                    SnapshotVertexSource { view: view.clone(), cx: &cx, as_of, after: None },
                    plan.clone(), wide(), || Ok::<_, usize>(()),
                ).collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(retry, expected);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_cursors_release_the_last_snapshot_owner_without_waiting_for_cursor_drop() {
    let ((), report) = run_async_under_lab(0x51cb_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        for mode in 0..6 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(0), vec![LABEL], vec![(KEY, CanonicalScalar::Int(0))]);
            seed.create_vertex(VId(u128::MAX), vec![LABEL], vec![(KEY, CanonicalScalar::Int(1))]);
            db.write(&commit, seed).await.unwrap();
            let view = db.read_session().unwrap();
            let generation = Arc::downgrade(&view.snapshot);
            let prepared = pattern(match mode { 1 => Some(0), 2 => Some(1), _ => None });
            let policy = if mode == 4 { GqlQueryPolicy::new(0, 0, 100, 0) } else { wide() };
            let mut cursor = view.stream_graph_vertices_governed(&cx, &prepared, policy).unwrap();
            drop(prepared); drop(view); drop(db);
            assert!(generation.upgrade().is_some(), "open cursor owns the pinned generation");
            match mode {
                0 => {
                    assert_eq!(cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(), vec![VId(0), VId(u128::MAX)]);
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                1 => { assert!(cursor.next().is_none()); }
                2 => {
                    assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                3 => { cursor.close(); assert_eq!(cursor.state(), VertexScanState::Closed); }
                4 => {
                    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                }
                _ => { drop(cursor); assert!(generation.upgrade().is_none()); continue; }
            }
            assert!(generation.upgrade().is_none(), "terminal cursor retained the generation");
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none()); cursor.close();
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn snapshot_successors_are_sorted_and_visibility_uses_the_shared_mvcc_resolver() {
    let ((), report) = run_async_under_lab(0x51cb_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [u128::MAX, 7, 0, 1_u128 << 100, 3, 1] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        let first = db.write(&commit, seed).await.unwrap();
        let mut edits = WriteBatch::new(RelationId(1));
        edits.delete_vertex(VId(3));
        edits.set_vertex_property(VId(7), KEY, Some(CanonicalScalar::Int(12)));
        edits.create_vertex(VId(2), vec![], vec![]);
        let second = db.write(&commit, edits).await.unwrap();
        let view = db.read_session().unwrap();
        for cut in [CommitSeq::ORIGIN, first, second] {
            let mut source = SnapshotVertexSource { view: view.clone(), cx: &cx, as_of: cut, after: None };
            let mut candidates = Vec::new(); let mut visible = Vec::new();
            let mut control = |_| Ok::<_, ()>(());
            while let Some(vid) = source.next_vertex(&mut control).unwrap() {
                candidates.push(vid);
                if source.vertex(vid, &mut control).unwrap().is_some() { visible.push(vid); }
            }
            assert_eq!(candidates, [0, 1, 2, 3, 7, 1_u128 << 100, u128::MAX].map(VId));
            assert_eq!(visible, db.vertices_at(cut).unwrap().iter().map(|row| row.vid).collect::<Vec<_>>());
            assert!(source.next_vertex(&mut control).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
