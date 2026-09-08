//! Compound writes use the actual marker boundary and ordinary recovery.
//! The unflushed-marker case explicitly models surviving bytes; truncating
//! its trailer models a torn tail. This is not a new power-loss VFS oracle.

use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::path::Path;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

async fn seeded(cx: &CommitCx, path: &Path) -> Database {
    let mut db = Database::create(cx, path, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![], vec![]);
    }
    db.write(cx, seed).await.unwrap();
    db
}

fn path_edges() -> Vec<WriteBatch> {
    let mut r = WriteBatch::new(R);
    r.add_edge(EId(10), VId(1), VId(2), vec![]);
    let mut s = WriteBatch::new(S);
    s.add_edge(EId(20), VId(2), VId(3), vec![]);
    vec![s, r]
}

#[test]
fn marker_boundary_recovers_both_relations_or_neither_including_torn_tail() {
    let ((), report) = run_async_under_lab(0xa703_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for (name, point, tear, committed) in [
            ("before", Some(CrashPoint::BeforeCapsule), false, false),
            (
                "capsule",
                Some(CrashPoint::AfterCapsuleBeforeD1),
                false,
                false,
            ),
            ("d1", Some(CrashPoint::AfterD1), false, false),
            (
                "marker-survived",
                Some(CrashPoint::AfterMarkerBeforeD2),
                false,
                true,
            ),
            (
                "marker-torn",
                Some(CrashPoint::AfterMarkerBeforeD2),
                true,
                false,
            ),
            (
                "marker-synced",
                Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync),
                false,
                true,
            ),
            ("complete", None, false, true),
        ] {
            let path = std::env::temp_dir().join(format!(
                "fgdb-atomic-recovery-{}-{name}",
                std::process::id()
            ));
            let mut db = seeded(&cx, &path).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let prepared = db.prepare_atomic_writes(path_edges()).unwrap();
            let result = db.commit_prepared_with_crash(&cx, prepared, point).await;
            assert_eq!(
                result.is_ok(),
                point.is_none(),
                "crash point must be reached: {name}"
            );
            if matches!(
                point,
                Some(
                    CrashPoint::AfterMarkerBeforeD2
                        | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                )
            ) {
                assert!(matches!(
                    result,
                    Err(WriteError::CommitOutcomeUnknown { .. })
                ));
                assert!(db.frontier().is_err());
            }
            assert!(pinned.edges().unwrap().is_empty());
            drop(db);
            if tear {
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(&path, 1).unwrap();
            }
            let reopened = Database::open(&cx, &path, keys()).await.unwrap();
            let expected_seq = CommitSeq(basis.0 + u64::from(committed));
            assert_eq!(reopened.frontier().unwrap(), expected_seq, "{name}");
            assert_eq!(
                reopened.edge(EId(10)).unwrap().is_some(),
                committed,
                "{name}"
            );
            assert_eq!(
                reopened.edge(EId(20)).unwrap().is_some(),
                committed,
                "{name}"
            );
            assert_eq!(
                reopened.delta_since(basis).unwrap().count(),
                usize::from(committed)
            );
            let vertices = reopened.vertices().unwrap();
            let edges = reopened.edges().unwrap();
            let versions = reopened.element_versions().unwrap().clone();
            drop(reopened);
            let rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
            assert_eq!(rebuilt.frontier().unwrap(), expected_seq);
            assert_eq!(rebuilt.vertices().unwrap(), vertices);
            assert_eq!(rebuilt.edges().unwrap(), edges);
            assert_eq!(rebuilt.element_versions().unwrap(), &versions);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_birth_ordinals_versions_and_history_survive_compaction_and_rebuild() {
    let ((), report) = run_async_under_lab(0xa703_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = std::env::temp_dir().join(format!("fgdb-atomic-compact-{}", std::process::id()));
        let mut db = seeded(&cx, &path).await;
        let mut r = WriteBatch::new(R);
        r.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(4))]);
        r.add_edge(EId(40), VId(4), VId(1), vec![]);
        let mut s = WriteBatch::new(S);
        s.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(5))]);
        s.add_edge(EId(50), VId(5), VId(3), vec![]);
        let created = db.write_atomic(&cx, vec![s, r]).await.unwrap();
        assert_eq!(db.vertex(VId(4)).unwrap().unwrap().birth_ordinal, 1);
        assert_eq!(db.vertex(VId(5)).unwrap().unwrap().birth_ordinal, 3);
        let pinned = db.read_session().unwrap();
        let mut r = WriteBatch::new(R);
        r.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(44)));
        let mut s = WriteBatch::new(S);
        s.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(55)));
        let updated = db.write_atomic(&cx, vec![r, s]).await.unwrap();
        db.compact(&cx).await.unwrap();
        let versions = db.element_versions().unwrap().clone();
        let historical = db.vertices_at(created).unwrap();
        let current = db.vertices().unwrap();
        // A later generation knows the retirement time of an older row;
        // compare snapshot-visible content rather than that future metadata.
        let visible = |rows: Vec<fgdb::VertexRow>| {
            rows.into_iter()
                .map(|row| (row.vid, row.birth_ordinal, row.labels, row.props))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            visible(historical.clone()),
            visible(pinned.vertices().unwrap())
        );
        assert_ne!(historical, current);
        drop(db);
        let reopened = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), updated);
        assert_eq!(reopened.vertices_at(created).unwrap(), historical);
        assert_eq!(reopened.vertices().unwrap(), current);
        assert_eq!(reopened.element_versions().unwrap(), &versions);
        drop(reopened);
        let mut rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(rebuilt.vertices_at(created).unwrap(), historical);
        assert_eq!(rebuilt.vertices().unwrap(), current);
        assert_eq!(rebuilt.element_versions().unwrap(), &versions);
        let mut r = WriteBatch::new(R);
        r.delete_vertex(VId(4));
        let mut s = WriteBatch::new(S);
        s.delete_vertex(VId(5));
        rebuilt.write_atomic(&cx, vec![s, r]).await.unwrap();
        assert!(rebuilt.edge(EId(40)).unwrap().is_none());
        assert!(rebuilt.edge(EId(50)).unwrap().is_none());
        assert_eq!(pinned.edges().unwrap().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn oversized_group_cannot_publish_an_independently_valid_group() {
    let ((), report) = run_async_under_lab(0xa703_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&cx, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let mut r = WriteBatch::new(R);
        r.create_vertex(VId(99), vec![], vec![]);
        let mut s = WriteBatch::new(S);
        s.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![0x41; 8000]).unwrap()),
        );
        s.set_vertex_property(
            VId(1),
            PropertyKeyId(2),
            Some(CanonicalScalar::bytes(vec![0x42; 9000]).unwrap()),
        );
        assert!(matches!(
            db.write_atomic(&cx, vec![r, s]).await,
            Err(WriteTxnError::Write(
                WriteError::VertexStorageAdmission { .. }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert!(db.vertex(VId(1)).unwrap().unwrap().props.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
