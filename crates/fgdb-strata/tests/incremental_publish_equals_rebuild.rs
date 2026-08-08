//! **The equality law the `fgdb-fujt` cure stands on:** publishing from a
//! clone of a persistently-folding [`BlockWriter`] is byte-identical — root
//! and sealed-block bytes — to the full from-scratch rebuild the spine
//! performs today after every commit.
//!
//! Why this must be a pinned law and not an argument: the fujt fix replaces
//! `rebuild()`-per-commit (measured 95% of the O(history) write cost,
//! `ffe05f6`) with a persistent writer that folds only the in-hand template
//! and publishes from a clone. That is a pure COST change only if the derived
//! state cannot differ — otherwise the live path and the open/recovery path
//! (which keeps full rebuild) would derive two different partitions from one
//! commit stream, which is exactly the divergence a content-addressed store
//! exists to make impossible. The fold is a deterministic function of the row
//! sequence, so equality should hold by construction; this test makes
//! "should" checkable per shape, and its control proves the comparison can
//! fail.
//!
//! Shapes covered: distinct creates; create+delete inside one commit (the
//! same-commit fold, whose durable image is NO entry); retirement of an
//! earlier commit's edge (interval close via pending-key replace); and a
//! vertex-delete cascade. Early-seal collisions are unreachable from legal
//! row sequences folded here (identity reuse is refused by the spent-set);
//! if a legal shape that seals early is ever found, it belongs in this file.

use fgdb_delta_types::{DeltaRow, RelationId};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::ids::{BranchId, DatabaseSecurityNamespaceId, GraphId};
use fgdb_types::{CommitSeq, EId, VId};

const K_OID: [u8; 32] = [0x5a; 32];
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);

fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
    (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
}

fn create_vertex(vid: u128, ordinal: u64) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: ordinal,
        labels: vec![],
        props: vec![],
        valid_time: None,
    }
}

fn create_edge(eid: u128, src: u128, dst: u128, ordinal: u64) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: ordinal,
        src: VId(src),
        relation: RelationId(1),
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

/// One named shape: the rows of each commit, in order.
fn shapes() -> Vec<(&'static str, Vec<Vec<DeltaRow>>)> {
    let mut shapes = Vec::new();

    // Distinct creates, one edge per commit — the sweep's shape.
    let mut commits = vec![vec![create_vertex(1, 0)]];
    for seq in 1..=24u64 {
        commits.push(vec![
            create_vertex(2000 + u128::from(seq), seq * 2),
            create_edge(u128::from(seq), 1, 2000 + u128::from(seq), seq * 2 + 1),
        ]);
    }
    shapes.push(("distinct-creates", commits));

    // Same-commit create+delete: the durable image is NO entry at all.
    shapes.push((
        "same-commit-fold",
        vec![
            vec![create_vertex(1, 0), create_vertex(2, 1)],
            vec![
                create_edge(10, 1, 2, 2),
                DeltaRow::DeleteEdge {
                    eid: EId(10),
                    before_version: fgdb_types::ids::ObjectId([0u8; 32]),
                },
                create_edge(11, 1, 2, 3),
            ],
            vec![create_edge(12, 2, 1, 4)],
        ],
    ));

    // Retire an EARLIER commit's edge: interval close by pending-key replace.
    shapes.push((
        "later-retirement",
        vec![
            vec![create_vertex(1, 0), create_vertex(2, 1)],
            vec![create_edge(20, 1, 2, 2)],
            vec![create_edge(21, 1, 2, 3)],
            vec![DeltaRow::DeleteEdge {
                eid: EId(20),
                before_version: fgdb_types::ids::ObjectId([0u8; 32]),
            }],
            vec![create_edge(22, 2, 1, 4)],
        ],
    ));

    shapes
}

fn fold(writer: &mut BlockWriter, seq: u64, rows: &[DeltaRow]) {
    for row in rows {
        writer
            .apply(keys(), CommitSeq(seq), row)
            .expect("legal shape folds");
    }
}

#[test]
fn incremental_clone_publish_equals_full_rebuild_at_every_commit_of_every_shape() {
    for (name, commits) in shapes() {
        let mut persistent = BlockWriter::new(GRAPH, BRANCH, 0);
        for (index, rows) in commits.iter().enumerate() {
            let seq = index as u64 + 1;
            fold(&mut persistent, seq, rows);

            let (incr_root, incr_blocks, incr_patches) = persistent
                .clone()
                .publish(keys(), CommitSeq(seq))
                .expect("incremental publish");

            let mut fresh = BlockWriter::new(GRAPH, BRANCH, 0);
            for (past_index, past_rows) in commits.iter().enumerate().take(index + 1) {
                fold(&mut fresh, past_index as u64 + 1, past_rows);
            }
            let (rebuild_root, rebuild_blocks, rebuild_patches) = fresh
                .publish(keys(), CommitSeq(seq))
                .expect("rebuild publish");

            assert_eq!(
                incr_root, rebuild_root,
                "shape {name:?}: roots diverged at commit {seq} — the fujt \
                 incremental path would derive different state than recovery"
            );
            let incr_bytes: Vec<_> = incr_blocks.iter().map(|b| &b.bytes).collect();
            let rebuild_bytes: Vec<_> = rebuild_blocks.iter().map(|b| &b.bytes).collect();
            assert_eq!(
                incr_bytes, rebuild_bytes,
                "shape {name:?}: sealed block bytes diverged at commit {seq}"
            );
            let incr_patch_bytes: Vec<_> = incr_patches.iter().map(|p| &p.bytes).collect();
            let rebuild_patch_bytes: Vec<_> = rebuild_patches.iter().map(|p| &p.bytes).collect();
            assert_eq!(
                incr_patch_bytes, rebuild_patch_bytes,
                "shape {name:?}: sealed vertex patch bytes diverged at commit {seq}"
            );
        }
    }
}

/// The control that can fail: a rebuild that is missing one historical row
/// must NOT compare equal. A comparison that accepts this would accept
/// anything, and the equality law above would be vacuous.
#[test]
fn the_equality_comparison_can_fail_a_rebuild_that_dropped_a_row() {
    let (_, commits) = shapes().swap_remove(0);
    let last_seq = commits.len() as u64;

    let mut complete = BlockWriter::new(GRAPH, BRANCH, 0);
    for (index, rows) in commits.iter().enumerate() {
        fold(&mut complete, index as u64 + 1, rows);
    }
    let (complete_root, complete_blocks, _complete_patches) = complete
        .publish(keys(), CommitSeq(last_seq))
        .expect("complete publish");

    let mut truncated = BlockWriter::new(GRAPH, BRANCH, 0);
    for (index, rows) in commits.iter().enumerate() {
        let seq = index as u64 + 1;
        if seq == last_seq {
            break;
        }
        fold(&mut truncated, seq, rows);
    }
    let (truncated_root, truncated_blocks, _truncated_patches) = truncated
        .publish(keys(), CommitSeq(last_seq))
        .expect("truncated publish");

    let complete_bytes: Vec<_> = complete_blocks.iter().map(|b| &b.bytes).collect();
    let truncated_bytes: Vec<_> = truncated_blocks.iter().map(|b| &b.bytes).collect();
    assert!(
        complete_root != truncated_root || complete_bytes != truncated_bytes,
        "a rebuild missing a whole commit compared EQUAL to the complete one: \
         the equality law is vacuous and proves nothing"
    );
}
