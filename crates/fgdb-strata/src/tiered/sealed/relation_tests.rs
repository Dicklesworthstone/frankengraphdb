//! The opaque relation receipt is a coverage boundary, including for empty
//! images. Actual source admission and whole-root negatives live in fgdb's
//! query_prism_relation_tests; these private tests exercise impossible public
//! constructors to ensure finish/reload also reject a mismatched receipt.

use super::*;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_types::{EId, ObjectId, PurposeContexts};

fn scope() -> SealedScope {
    SealedScope {
        source_root: PartitionRootVersion(ObjectId([9; 32])),
        graph: GraphId(1),
        branch: BranchId(1),
        partition: 0,
        floor: CommitSeq(1),
        publication: CommitSeq(3),
    }
}

fn image(relation: RelationId, count: usize) -> Image {
    let entries = (0..count)
        .map(|at| AdjacencyEntry {
            src: VId(1),
            relation,
            dst: VId(10 + (at / 2) as u128),
            eid: EId(at as u128),
            created_at: CommitSeq(1),
            retired_at: (at % 3 == 0).then_some(CommitSeq(3)),
        })
        .collect();
    image::build(
        crate::compact::Compaction {
            blocks: vec![entries],
            block_props: vec![None],
            dropped: 0,
            superseded: 0,
        },
        SealedLimits::default(),
        &mut || Ok(()),
    )
    .unwrap()
}

#[test]
fn relation_receipt_checks_inline_and_csr_rows_on_finish_and_reload() {
    let limits = SealedLimits::default();
    for count in [1, 33] {
        assert!(matches!(
            SealedPartition::finish_scoped(
                scope(),
                Some(RelationId(1)),
                image(RelationId(2), count),
                limits,
                &mut || Ok(()),
            ),
            Err(SealedError::RelationOutsideAnchor {
                requested: RelationId(2),
                retained: RelationId(1),
            })
        ));
        let partition = SealedPartition::finish_scoped(
            scope(),
            Some(RelationId(2)),
            image(RelationId(2), count),
            limits,
            &mut || Ok(()),
        )
        .unwrap();
        let bytes = wire::encode(&partition.image, limits, &mut || Ok(())).unwrap();
        let mut wrong = partition.anchor();
        wrong.relation = Some(RelationId(1));
        // Exact byte hash/length agreement is insufficient for a wrong scope.
        assert!(matches!(
            SealedPartition::reload_inner(wrong, &bytes, limits, None, &mut || Ok(())),
            Err(SealedError::RelationOutsideAnchor {
                requested: RelationId(2),
                retained: RelationId(1),
            })
        ));
        let restored =
            SealedPartition::reload_inner(partition.anchor(), &bytes, limits, None, &mut || Ok(()))
                .unwrap();
        assert_eq!(restored.anchor(), partition.anchor());
    }
}

#[test]
fn empty_relation_image_does_not_authorize_other_outgoing_or_incoming_rows() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let query = contexts.query();
    let limits = SealedLimits::default();
    let scoped = SealedPartition::finish_scoped(
        scope(),
        Some(RelationId(7)),
        image(RelationId(7), 0),
        limits,
        &mut || Ok(()),
    )
    .unwrap();
    let full =
        SealedPartition::finish(scope(), image(RelationId(7), 0), limits, &mut || Ok(())).unwrap();
    assert_eq!(scoped.anchor().scope(), full.anchor().scope());
    assert_eq!(
        scoped.encode(&query, limits).unwrap(),
        full.encode(&query, limits).unwrap()
    );
    assert_ne!(scoped.anchor(), full.anchor());
    assert!(
        scoped
            .row(&query, VId(1), RelationId(7), CommitSeq(1))
            .unwrap()
            .next(&query)
            .unwrap()
            .is_none()
    );
    assert!(
        full.row(&query, VId(1), RelationId(8), CommitSeq(1))
            .unwrap()
            .next(&query)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        scoped.row_from(&query, VId(1), RelationId(8), CommitSeq(1), Some(VId(2))),
        Err(SealedError::RelationOutsideAnchor { .. })
    ));
    let incoming = scoped
        .incoming_index(&query, IncomingIndexLimits::default())
        .unwrap();
    assert_eq!(incoming.stats().incidences, 0);
    assert!(
        incoming
            .row(&query, VId(1), RelationId(7), CommitSeq(1))
            .unwrap()
            .next(&query)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        incoming.row_from(&query, VId(1), RelationId(8), CommitSeq(1), Some(VId(2))),
        Err(SealedError::RelationOutsideAnchor { .. })
    ));
}

// Real stored roots exercise both the old re-collapse and the consuming walk.
// Malformed roots below are planted only in these tests' private directories;
// each object has its ordinary identity and canonical local encoding, so the
// negative cases reach cross-object admission rather than a fabricated image.
const UNHOSTED_R: RelationId = RelationId(17);
const UNHOSTED_OTHER: RelationId = RelationId(19);
const UNHOSTED_KEY: [u8; 32] = [0x31; 32];
const UNHOSTED_NAMESPACE: fgdb_types::DatabaseSecurityNamespaceId =
    fgdb_types::DatabaseSecurityNamespaceId([0x47; 32]);
const UNHOSTED_HIGH: u128 = 1u128 << 120;

#[derive(Clone, Copy)]
enum SourceDamage {
    None,
    UnselectedHistory,
    LaterVertexHistory,
}

fn unhosted_run<F: std::future::Future<Output = ()> + Send>(
    test: impl FnOnce(PurposeContexts, std::path::PathBuf) -> F + Send + 'static,
) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-unhosted-sealing-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir(&path).unwrap();
    let ((), report) = asupersync::lab::run_async_under_lab(0x319_470, |root| async move {
        test(PurposeContexts::narrow_runtime_root(&root), path).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn unhosted_entry(id: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(UNHOSTED_HIGH + 1),
        relation: UNHOSTED_R,
        dst: VId(UNHOSTED_HIGH + 10 + id % 3),
        eid: EId(if id == 11 {
            u128::MAX
        } else {
            UNHOSTED_HIGH + id
        }),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

async fn unhosted_stored_fixture(
    contexts: &PurposeContexts,
    path: &std::path::Path,
    hosted: Option<RelationId>,
    damage: SourceDamage,
) -> (
    BlockStore,
    PartitionRootVersion,
    crate::store::RootReadLimits,
) {
    use crate::root::{BlockRef, PartitionRoot, PatchRef};
    use crate::vertex::{VertexRow, encode_patch};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;

    let commit = contexts.commit();
    let store = BlockStore::open(&commit, path, UNHOSTED_KEY, UNHOSTED_NAMESPACE)
        .await
        .unwrap();
    let other = AdjacencyEntry {
        src: VId(UNHOSTED_HIGH + 50),
        relation: UNHOSTED_OTHER,
        dst: VId(UNHOSTED_HIGH + 51),
        eid: if matches!(damage, SourceDamage::UnselectedHistory) {
            unhosted_entry(0, 1, None).eid
        } else {
            EId(UNHOSTED_HIGH + 50)
        },
        created_at: CommitSeq(1),
        retired_at: None,
    };
    let definitions = [
        (
            (0..12)
                .map(|id| unhosted_entry(id, 1, None))
                .collect::<Vec<_>>(),
            // A hosted column may appear only in a later selected block;
            // inspecting the first block cannot authorize the fast path.
            false,
        ),
        (vec![other], hosted == Some(UNHOSTED_OTHER)),
        (
            vec![unhosted_entry(0, 1, Some(2)), unhosted_entry(0, 2, None)],
            hosted == Some(UNHOSTED_R),
        ),
        // A later propertyless copy must clear an earlier hosted row. Repeating
        // the old statement must not replace its current content successor.
        (
            vec![unhosted_entry(0, 1, Some(2)), unhosted_entry(1, 1, Some(3))],
            false,
        ),
    ];
    let mut heads = std::collections::BTreeMap::new();
    let mut blocks = Vec::new();
    let mut source_bytes = 0usize;
    let mut incidences = 0usize;
    for (at, (mut entries, has_properties)) in definitions.into_iter().enumerate() {
        entries
            .sort_unstable_by_key(|row| (row.src, row.relation, row.dst, row.eid, row.created_at));
        let family = (entries[0].src, entries[0].relation);
        let predecessor = heads.get(&family).copied();
        let bytes = if has_properties {
            let properties: Vec<_> = (0..entries.len())
                .map(|row| {
                    vec![(
                        PropertyKeyId(1),
                        CanonicalScalar::Int((100 * at + row) as i64).into(),
                    )]
                })
                .collect();
            let payload = crate::edge_props::encode_property_patch(&properties).unwrap();
            source_bytes += payload.len();
            let patch = store
                .put_edge_property_patch(&commit, &payload)
                .await
                .unwrap();
            let locators: Vec<_> = (1..=entries.len()).map(|row| row as u8).collect();
            crate::encode_block_with_properties(
                0,
                predecessor,
                &entries,
                patch.0,
                &locators,
                &properties,
            )
            .unwrap()
        } else {
            crate::encode_block(0, predecessor, &entries).unwrap()
        };
        source_bytes += bytes.len();
        incidences += entries.len();
        let id = store.put(&commit, &bytes).await.unwrap();
        heads.insert(family, id);
        let (first_seq, last_seq) = crate::root::span_of(&entries).unwrap();
        blocks.push(BlockRef {
            block_id: id.0,
            first_seq,
            last_seq,
        });
    }
    let vertices: Vec<_> = [1, 10, 11, 12, 50, 51]
        .into_iter()
        .enumerate()
        .map(|(ordinal, id)| VertexRow {
            vid: VId(UNHOSTED_HIGH + id),
            birth_ordinal: ordinal as u64,
            created_at: CommitSeq(1),
            retired_at: None,
            labels: vec![LabelId(1)],
            props: Vec::new(),
        })
        .collect();
    let mut retired = vertices[0].clone();
    retired.retired_at = Some(CommitSeq(4));
    if matches!(damage, SourceDamage::LaterVertexHistory) {
        retired.birth_ordinal += 1;
    }
    let mut successor = retired.clone();
    successor.created_at = CommitSeq(4);
    successor.retired_at = None;
    successor.labels = vec![LabelId(2)];
    let mut patches = Vec::new();
    let mut versions = 0;
    for rows in [vertices, vec![retired, successor]] {
        let payload = encode_patch(&rows).unwrap();
        source_bytes += payload.len();
        versions += rows.len();
        let id = store.put_patch(&commit, &payload).await.unwrap();
        let (first_seq, last_seq) = crate::vertex::span_of_rows(&rows).unwrap();
        patches.push(PatchRef {
            patch_id: id.0,
            first_seq,
            last_seq,
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(2),
        partition: 0,
        published_at: CommitSeq(4),
        blocks,
        vertex_patches: patches,
    };
    let bytes = crate::root::encode_root(&root).unwrap();
    let id = PartitionRootVersion(crate::root::root_id(
        &UNHOSTED_KEY,
        UNHOSTED_NAMESPACE,
        &bytes,
    ));
    // Do not ask put_root to reject the deliberate cross-object mutations
    // before the two read paths being compared can inspect them themselves.
    std::fs::write(store.path(id.0), &bytes).unwrap();
    let exact = crate::store::RootReadLimits {
        max_root_bytes: bytes.len(),
        max_source_bytes: source_bytes,
        max_blocks: root.blocks.len(),
        max_vertex_patches: root.vertex_patches.len(),
        max_incidences: incidences,
        max_vertex_versions: versions,
    };
    (store, id, exact)
}

#[allow(clippy::too_many_arguments)]
async fn unhosted_sealing_variant(
    store: &BlockStore,
    cx: &QueryCx,
    root: PartitionRootVersion,
    floor: CommitSeq,
    relation: Option<RelationId>,
    keep_vertices: bool,
    limits: SealedLimits,
    mut source: crate::store::RootReadLimits,
    consume: bool,
) -> Result<(SealedPartition, Vec<crate::vertex::VertexPatchRows>), SealedError> {
    if relation.is_none() {
        source.max_incidences = source.max_incidences.min(limits.max_incidences);
    }
    if consume {
        if let Some(relation) = relation {
            if keep_vertices {
                store
                    .seal_relation_with_vertex_history(cx, root, floor, relation, limits, source)
                    .await
            } else {
                store
                    .seal_relation_with_source_limits(cx, root, floor, relation, limits, source)
                    .await
                    .map(|partition| (partition, Vec::new()))
            }
        } else {
            assert!(!keep_vertices);
            store
                .seal_partition_with_source_limits(cx, root, floor, limits, source)
                .await
                .map(|partition| (partition, Vec::new()))
        }
    } else {
        let (adjacency, patches) = store
            .reopen_sealing_revalidated(cx, root, floor, source, relation, keep_vertices)
            .await
            .map_err(sealed_source_error)?;
        let partition = seal_admitted_adjacency(cx, root, floor, relation, limits, adjacency)?;
        Ok((partition, patches))
    }
}

type UnhostedTrace = (
    Result<(SealedAnchor, Vec<u8>, Vec<crate::vertex::VertexPatchRows>), String>,
    usize,
);

#[allow(clippy::too_many_arguments)]
async fn unhosted_trace(
    store: &BlockStore,
    cx: &QueryCx,
    root: PartitionRootVersion,
    floor: CommitSeq,
    relation: Option<RelationId>,
    keep_vertices: bool,
    limits: SealedLimits,
    source: crate::store::RootReadLimits,
    consume: bool,
    stop: Option<usize>,
) -> UnhostedTrace {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    let probe = Arc::new(SimulationCheckpointProbe::new(stop));
    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
    let result = unhosted_sealing_variant(
        store,
        &controlled,
        root,
        floor,
        relation,
        keep_vertices,
        limits,
        source,
        consume,
    )
    .await;
    let calls = probe.calls();
    let result = result
        .map(|(partition, vertices)| {
            (
                partition.anchor(),
                partition.encode(cx, SealedLimits::default()).unwrap(),
                vertices,
            )
        })
        .map_err(|error| match error {
            SealedError::Interrupted(_) => "Interrupted".to_owned(),
            other => format!("{other:?}"),
        });
    (result, calls)
}

#[test]
fn unhosted_sealing_matches_revalidated_images_and_preserves_public_source_histories() {
    unhosted_run(|contexts, path| async move {
        let cx = contexts.query();
        for (at, hosted) in [None, Some(UNHOSTED_R), Some(UNHOSTED_OTHER)]
            .into_iter()
            .enumerate()
        {
            let directory = path.join(at.to_string());
            std::fs::create_dir(&directory).unwrap();
            let (store, root, source) =
                unhosted_stored_fixture(&contexts, &directory, hosted, SourceDamage::None).await;
            let original = store
                .reopen_adjacency_bounded(&cx, root, CommitSeq(1), source)
                .await
                .unwrap();
            for floor in [1, 2, 3, 4] {
                for relation in [
                    None,
                    Some(UNHOSTED_R),
                    Some(UNHOSTED_OTHER),
                    Some(RelationId(23)),
                ] {
                    for keep_vertices in [false, true]
                        .into_iter()
                        .filter(|keep| !keep || relation.is_some())
                    {
                        let old = unhosted_trace(
                            &store,
                            &cx,
                            root,
                            CommitSeq(floor),
                            relation,
                            keep_vertices,
                            SealedLimits::default(),
                            source,
                            false,
                            None,
                        )
                        .await;
                        let new = unhosted_trace(
                            &store,
                            &cx,
                            root,
                            CommitSeq(floor),
                            relation,
                            keep_vertices,
                            SealedLimits::default(),
                            source,
                            true,
                            None,
                        )
                        .await;
                        assert!(old.0.is_ok(), "{old:?}");
                        assert_eq!(
                            new, old,
                            "hosted {hosted:?}, floor {floor}, relation {relation:?}, vertices {keep_vertices}"
                        );
                    }
                }
            }
            let (composed, retained, _) = store
                .seal_relation_with_read_source(
                    &cx,
                    root,
                    CommitSeq(1),
                    UNHOSTED_R,
                    SealedLimits::default(),
                    source,
                )
                .await
                .unwrap();
            assert_eq!(
                retained, original,
                "composed reads retain original block histories"
            );
            let (direct, _) = store
                .seal_relation_with_vertex_history(
                    &cx,
                    root,
                    CommitSeq(1),
                    UNHOSTED_R,
                    SealedLimits::default(),
                    source,
                )
                .await
                .unwrap();
            assert_eq!(composed.anchor(), direct.anchor());
            assert_eq!(
                composed.encode(&cx, SealedLimits::default()).unwrap(),
                direct.encode(&cx, SealedLimits::default()).unwrap()
            );
            assert_eq!(
                store
                    .reopen_adjacency_bounded(&cx, root, CommitSeq(1), source)
                    .await
                    .unwrap(),
                original
            );
        }
    });
}

#[test]
fn unhosted_sealing_keeps_every_source_image_and_cancellation_refusal_boundary() {
    unhosted_run(|contexts, path| async move {
        let cx = contexts.query();
        let (store, root, exact) =
            unhosted_stored_fixture(&contexts, &path, Some(UNHOSTED_OTHER), SourceDamage::None)
                .await;
        let floor = CommitSeq(1);
        let relation = Some(UNHOSTED_R);
        let unlimited = SealedLimits::default();
        let expected = unhosted_trace(
            &store, &cx, root, floor, relation, true, unlimited, exact, false, None,
        )
        .await;
        assert!(expected.0.is_ok(), "{expected:?}");
        assert_eq!(
            unhosted_trace(
                &store, &cx, root, floor, relation, true, unlimited, exact, true, None
            )
            .await,
            expected
        );
        for stop in 1..=expected.1 {
            let old = unhosted_trace(
                &store,
                &cx,
                root,
                floor,
                relation,
                true,
                unlimited,
                exact,
                false,
                Some(stop),
            )
            .await;
            let new = unhosted_trace(
                &store,
                &cx,
                root,
                floor,
                relation,
                true,
                unlimited,
                exact,
                true,
                Some(stop),
            )
            .await;
            assert_eq!(old, (Err("Interrupted".to_owned()), stop));
            assert_eq!(new, old, "checkpoint {stop}");
        }
        let mut sources = vec![
            crate::store::RootReadLimits {
                max_root_bytes: exact.max_root_bytes - 1,
                ..exact
            },
            crate::store::RootReadLimits {
                max_source_bytes: exact.max_source_bytes - 1,
                ..exact
            },
            crate::store::RootReadLimits {
                max_blocks: exact.max_blocks - 1,
                ..exact
            },
            crate::store::RootReadLimits {
                max_vertex_patches: exact.max_vertex_patches - 1,
                ..exact
            },
            crate::store::RootReadLimits {
                max_incidences: exact.max_incidences - 1,
                ..exact
            },
            crate::store::RootReadLimits {
                max_vertex_versions: exact.max_vertex_versions - 1,
                ..exact
            },
        ];
        // Thirteen canonical selected statements still require admission of
        // all seventeen raw statements, including the unselected relation.
        sources.push(crate::store::RootReadLimits {
            max_incidences: 13,
            ..exact
        });
        for source in sources {
            let old = unhosted_trace(
                &store, &cx, root, floor, relation, true, unlimited, source, false, None,
            )
            .await;
            assert!(old.0.is_err(), "{source:?}");
            assert_eq!(
                unhosted_trace(
                    &store, &cx, root, floor, relation, true, unlimited, source, true, None
                )
                .await,
                old
            );
        }
        for limits in [
            SealedLimits {
                max_incidences: 12,
                ..unlimited
            },
            SealedLimits {
                max_rows: 0,
                ..unlimited
            },
            SealedLimits {
                max_image_bytes: 0,
                ..unlimited
            },
        ] {
            let old = unhosted_trace(
                &store, &cx, root, floor, relation, true, limits, exact, false, None,
            )
            .await;
            assert!(old.0.is_err(), "{limits:?}");
            assert_eq!(
                unhosted_trace(
                    &store, &cx, root, floor, relation, true, limits, exact, true, None
                )
                .await,
                old
            );
        }
    });
}

#[test]
fn unhosted_sealing_never_hides_bad_unselected_history_or_later_vertex_patches() {
    unhosted_run(|contexts, path| async move {
        let cx = contexts.query();
        for (at, damage) in [
            SourceDamage::UnselectedHistory,
            SourceDamage::LaterVertexHistory,
        ]
        .into_iter()
        .enumerate()
        {
            let directory = path.join(at.to_string());
            std::fs::create_dir(&directory).unwrap();
            let (store, root, exact) =
                unhosted_stored_fixture(&contexts, &directory, None, damage).await;
            let limit = SealedLimits::default();
            let old = unhosted_trace(
                &store,
                &cx,
                root,
                CommitSeq(1),
                Some(UNHOSTED_R),
                true,
                limit,
                exact,
                false,
                None,
            )
            .await;
            let error = old.0.as_ref().unwrap_err();
            assert!(
                error.contains(match damage {
                    SourceDamage::UnselectedHistory => "EdgeIdentityMismatch",
                    SourceDamage::LaterVertexHistory => "VertexIdentityMismatch",
                    SourceDamage::None => unreachable!(),
                }),
                "{old:?}"
            );
            assert_eq!(
                unhosted_trace(
                    &store,
                    &cx,
                    root,
                    CommitSeq(1),
                    Some(UNHOSTED_R),
                    true,
                    limit,
                    exact,
                    true,
                    None
                )
                .await,
                old
            );
            for stop in 1..=old.1 {
                let previous = unhosted_trace(
                    &store,
                    &cx,
                    root,
                    CommitSeq(1),
                    Some(UNHOSTED_R),
                    true,
                    limit,
                    exact,
                    false,
                    Some(stop),
                )
                .await;
                let consumed = unhosted_trace(
                    &store,
                    &cx,
                    root,
                    CommitSeq(1),
                    Some(UNHOSTED_R),
                    true,
                    limit,
                    exact,
                    true,
                    Some(stop),
                )
                .await;
                assert_eq!(previous, (Err("Interrupted".to_owned()), stop));
                assert_eq!(consumed, previous, "source {at}, checkpoint {stop}");
            }
            // Empty relation selections must prove the same bad root too.
            let empty = unhosted_trace(
                &store,
                &cx,
                root,
                CommitSeq(1),
                Some(RelationId(23)),
                false,
                limit,
                exact,
                true,
                None,
            )
            .await;
            assert_eq!(empty.0.as_ref().unwrap_err(), error);
        }
    });
}
