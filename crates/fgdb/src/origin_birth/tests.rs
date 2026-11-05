use super::*;
use crate::{Database, MemVfs, WriteBatch, marker_for_capsule, prepare_capsule};
use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::commit::CAPSULE_DIR;
use fgdb_chronicle::marker::EffectSource;
use fgdb_delta_types::{
    CoordinateEntry, LogicalDeltaTemplate, PropertyKeyId, RelationId, SchemaEpoch,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, ObjectId, PurposeContexts, VId};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const EARLIER: EId = EId((1_u128 << 120) | 9);
const LATER: EId = EId(3);
const UNUSED: EId = EId(44);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn request(
    edge_ids: &BTreeSet<EId>,
    through: CommitSeq,
    max_origins: usize,
) -> EdgeOriginRequest<'_> {
    EdgeOriginRequest {
        graph: crate::GRAPH,
        branch: crate::BRANCH,
        through,
        edge_ids,
        max_origins,
    }
}

async fn collect(
    db: &mut Database<MemVfs>,
    cx: &CommitCx,
    edge_ids: &BTreeSet<EId>,
    through: CommitSeq,
    max_origins: usize,
) -> Result<BTreeMap<EId, OriginBirthOrder<EId>>, OriginBirthError> {
    reconstruct(
        cx,
        &db.coordinator,
        &db.keys,
        request(edge_ids, through, max_origins),
        &mut db.crypto_verification_events,
    )
    .await
}

async fn fixture(cx: &CommitCx, vfs: &MemVfs) -> Database<MemVfs> {
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut vertices = WriteBatch::new(R);
    vertices.create_vertex(VId(1), vec![], vec![]);
    vertices.create_vertex(VId(2), vec![], vec![]);
    assert_eq!(db.write(cx, vertices).await.unwrap(), CommitSeq(1));

    // Visit ordinals count no-ops, and canonical EId order reverses the two
    // actual edge creations. Neither cardinality nor encoded order is origin.
    let mut edges = WriteBatch::new(R);
    edges.ensure_vertex(VId(1), vec![], vec![]);
    edges.add_edge(EARLIER, VId(1), VId(2), vec![]);
    edges.ensure_edge_by_triple(UNUSED, VId(1), VId(2), vec![]);
    edges.add_edge(LATER, VId(1), VId(2), vec![]);
    assert_eq!(db.write(cx, edges).await.unwrap(), CommitSeq(2));

    let mut update = WriteBatch::new(R);
    update.set_edge_property(EARLIER, P, Some(CanonicalScalar::Int(7)));
    update.delete_edge(LATER);
    assert_eq!(db.write(cx, update).await.unwrap(), CommitSeq(3));
    db
}

#[test]
fn origins_survive_versions_compaction_and_checkpoint_reopen() {
    let ((), report) = run_async_under_lab(0xb170_0001, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let mut db = fixture(&cx, &vfs).await;
        let selected = BTreeSet::from([EARLIER, LATER]);
        let expected = BTreeMap::from([
            (EARLIER, OriginBirthOrder::new(CommitSeq(2), 2, 0, EARLIER)),
            (LATER, OriginBirthOrder::new(CommitSeq(2), 4, 0, LATER)),
        ]);
        assert!(expected[&EARLIER] < expected[&LATER]);
        assert_eq!(db.writer.live_edge(EARLIER).unwrap().3, CommitSeq(3));
        assert!(db.writer.live_edge(LATER).is_none());
        let versions = db.element_versions().unwrap().clone();
        let snapshot = Arc::clone(&db.snapshot);
        assert_eq!(
            collect(&mut db, &cx, &selected, CommitSeq(3), 2)
                .await
                .unwrap(),
            expected
        );
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        assert_eq!(db.element_versions().unwrap(), &versions);
        db.compact(&cx).await.unwrap();
        assert_eq!(
            collect(&mut db, &cx, &selected, CommitSeq(3), 2)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(db.element_versions().unwrap(), &versions);
        drop(db);

        let mut reopened = Database::open_with_vfs(&cx, vfs, &dir, keys())
            .await
            .unwrap();
        assert_eq!(reopened.delta_materialized_after, CommitSeq(3));
        assert!(reopened.delta_index().unwrap().is_empty());
        assert_eq!(
            collect(&mut reopened, &cx, &selected, CommitSeq(3), 2)
                .await
                .unwrap(),
            expected
        );
        assert_eq!(reopened.delta_materialized_after, CommitSeq(3));
        assert!(reopened.delta_index().unwrap().is_empty());
        assert_eq!(reopened.element_versions().unwrap(), &versions);
        assert_eq!(reopened.writer.live_edge(EARLIER).unwrap().3, CommitSeq(3));
        // Selecting one edge retains exactly that origin, including full-width
        // identities, despite authenticating the same complete prefix.
        assert_eq!(
            collect(
                &mut reopened,
                &cx,
                &BTreeSet::from([EARLIER]),
                CommitSeq(3),
                1,
            )
            .await
            .unwrap(),
            BTreeMap::from([(EARLIER, expected[&EARLIER])])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn incomplete_out_of_cut_and_over_limit_origin_requests_refuse() {
    let ((), report) = run_async_under_lab(0xb170_0002, |runtime| async move {
        let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = fixture(&cx, &vfs).await;
        let selected = BTreeSet::from([EARLIER, LATER]);
        let snapshot = Arc::clone(&db.snapshot);
        assert!(matches!(
            collect(&mut db, &cx, &selected, CommitSeq(3), 1).await,
            Err(OriginBirthError::Limit {
                requested: 2,
                limit: 1
            })
        ));
        assert!(matches!(
            collect(&mut db, &cx, &selected, CommitSeq(4), 2).await,
            Err(OriginBirthError::CutBeyondFrontier {
                through: CommitSeq(4),
                frontier: CommitSeq(3),
            })
        ));
        assert!(matches!(
            collect(&mut db, &cx, &BTreeSet::from([EARLIER]), CommitSeq(1), 1).await,
            Err(OriginBirthError::Missing {
                eid: EARLIER,
                through: CommitSeq(1)
            })
        ));
        assert!(matches!(
            collect(&mut db, &cx, &BTreeSet::from([UNUSED]), CommitSeq(3), 1).await,
            Err(OriginBirthError::Missing {
                eid: UNUSED,
                through: CommitSeq(3)
            })
        ));
        assert!(
            collect(&mut db, &cx, &BTreeSet::new(), CommitSeq::ORIGIN, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reconstruction_authenticates_the_entire_selected_prefix_even_after_all_births() {
    let ((), report) = run_async_under_lab(0xb170_0003, |runtime| async move {
        let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = fixture(&cx, &vfs).await;
        let selected = BTreeSet::from([EARLIER]);
        let EffectSource::Local { capsule_ref, .. } = &db
            .coordinator
            .chain()
            .entries()
            .last()
            .unwrap()
            .marker
            .effect_source;
        let hex: String = capsule_ref
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let capsule = vfs
            .database_dir()
            .join(CAPSULE_DIR)
            .join(format!("{hex}.capsule"));
        let original = vfs.read(&capsule).await.unwrap();
        vfs.write(&capsule, b"truncated later capsule")
            .await
            .unwrap();
        let snapshot = Arc::clone(&db.snapshot);
        for edge_ids in [&selected, &BTreeSet::new()] {
            assert!(matches!(
                collect(&mut db, &cx, edge_ids, CommitSeq(3), 1).await,
                Err(OriginBirthError::Source(RebuildError::Commit(_)))
            ));
        }
        // A cut before the damaged capsule has no obligation to read it.
        assert_eq!(
            collect(&mut db, &cx, &selected, CommitSeq(2), 1)
                .await
                .unwrap()[&EARLIER],
            OriginBirthOrder::new(CommitSeq(2), 2, 0, EARLIER)
        );
        assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
        vfs.write(&capsule, &original).await.unwrap();
        assert_eq!(
            collect(&mut db, &cx, &selected, CommitSeq(3), 1)
                .await
                .unwrap()[&EARLIER],
            OriginBirthOrder::new(CommitSeq(2), 2, 0, EARLIER)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn two_authenticated_creation_effects_never_overwrite_one_origin() {
    let ((), report) = run_async_under_lab(0xb170_0004, |runtime| async move {
        let cx = PurposeContexts::narrow_runtime_root(&runtime).commit();
        let vfs = MemVfs::new().unwrap();
        let dir = vfs.database_dir();
        let db = Database::create_with_vfs(&cx, vfs.clone(), &dir, keys())
            .await
            .unwrap();
        drop(db);
        // Construct contradictory but authentically committed source effects
        // through Chronicle itself. A normal database writer already refuses
        // identity reuse; reconstruction must independently refuse it too.
        let keys = keys();
        let mut coordinator = CommitCoordinator::open_with_vfs(&cx, vfs, &dir, keys.capsule_keys())
            .await
            .unwrap();
        for ordinal in [7, 9] {
            let template = LogicalDeltaTemplate::build(
                ObjectId([0xc1; 32]),
                [0xc2; 32],
                vec![CoordinateEntry {
                    graph: crate::GRAPH,
                    branch: crate::BRANCH,
                    relation: R,
                    schema_epoch: SchemaEpoch(1),
                    schema_transition: None,
                    rows: vec![DeltaRow::CreateEdge {
                        eid: EARLIER,
                        birth_ordinal: ordinal,
                        src: VId(1),
                        relation: R,
                        dst: VId(2),
                        canonical_key: None,
                        props: vec![],
                        valid_time: None,
                    }],
                }],
            )
            .unwrap();
            let capsule = prepare_capsule(keys.k_oid(), keys.namespace, &template).unwrap();
            coordinator
                .commit(&cx, &capsule.bytes, |seq, oid| {
                    marker_for_capsule(seq, oid, &capsule, vec![])
                })
                .await
                .unwrap();
        }
        let selected = BTreeSet::from([EARLIER]);
        let error = reconstruct(
            &cx,
            &coordinator,
            &keys,
            request(&selected, CommitSeq(2), 1),
            &mut vec![],
        )
        .await
        .unwrap_err();
        let OriginBirthError::Duplicate {
            eid,
            first,
            duplicate,
        } = error
        else {
            panic!("expected duplicate origin refusal, got {error:?}");
        };
        assert_eq!(eid, EARLIER);
        assert_eq!(first, OriginBirthOrder::new(CommitSeq(1), 7, 0, EARLIER));
        assert_eq!(
            duplicate,
            OriginBirthOrder::new(CommitSeq(2), 9, 0, EARLIER)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_reconstruction_does_not_change_the_retained_generation() {
    let ((), report) = run_async_under_lab(0xb170_0005, |runtime| async move {
        let mut handle = runtime
            .spawn(|child| async move {
                let cx = PurposeContexts::narrow_runtime_root(&child).commit();
                let vfs = MemVfs::new().unwrap();
                let mut db = fixture(&cx, &vfs).await;
                let snapshot = Arc::clone(&db.snapshot);
                child.set_cancel_reason(
                    asupersync::CancelReason::new(asupersync::CancelKind::User)
                        .with_message("cancel origin reconstruction"),
                );
                assert!(matches!(
                    collect(&mut db, &cx, &BTreeSet::from([EARLIER]), CommitSeq(3), 1).await,
                    Err(OriginBirthError::Source(RebuildError::Interrupted(_)))
                ));
                assert!(Arc::ptr_eq(&snapshot, &db.snapshot));
                assert_eq!(db.frontier().unwrap(), CommitSeq(3));
            })
            .unwrap();
        assert_eq!(handle.join(&runtime).await, Ok(()));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
