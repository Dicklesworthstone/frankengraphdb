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
