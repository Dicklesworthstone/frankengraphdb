//! **`WriteBatch::extend` — two staged writes, one batch, one commit**
//! (`fgdb-w4-g1-txn-core-qpmg.2`).
//!
//! The helper is the concatenation a transaction uses to make two staged
//! writes one capsule, so the laws here are behavioral, not structural (the
//! row list is deliberately private): a same-relation extend commits BOTH
//! batches' effects under exactly ONE commit sequence, and a mixed-relation
//! extend refuses with the typed [`WriteError::MixedRelation`] arm while
//! leaving the receiving batch exactly as staged — proven by committing that
//! batch afterwards and observing only its own effects.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-batch-extend-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

/// Same relation: the extended batch is ONE write — one consumed sequence —
/// carrying both halves' effects, exactly as if every row had been staged
/// into a single batch.
#[test]
fn same_relation_extend_commits_both_halves_under_one_sequence() {
    let dir = scratch("same-relation");
    under_lab(0x7e_01, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys())
            .await
            .expect("creates the database");
        let before = db.frontier().expect("healthy frontier");

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        let mut second = WriteBatch::new(R);
        second.add_edge(EId(10), VId(1), VId(2), vec![]);

        first
            .extend(second)
            .expect("same-relation extend concatenates");
        let committed = db
            .write(cx, first)
            .await
            .expect("the combined batch commits");

        assert_eq!(
            committed.0,
            before.0 + 1,
            "both halves rode exactly one commit sequence"
        );
        assert_eq!(
            db.frontier().expect("healthy frontier"),
            committed,
            "the frontier advanced once, to the combined commit"
        );
        // The second half's edge sees the first half's vertices: prefix
        // semantics were derived over the combined row sequence.
        assert_eq!(
            db.neighbours(VId(1), R).expect("healthy read"),
            vec![VId(2)],
            "the appended edge landed in the same commit as its endpoints"
        );
        assert!(
            db.vertex(VId(2)).expect("healthy read").is_some(),
            "the receiving batch's own rows survived the concatenation"
        );
    });
}

/// Mixed relations refuse with the typed arm, and the refusal appends
/// nothing: committing the receiving batch afterwards produces only its own
/// staged effects.
#[test]
fn mixed_relation_extend_refuses_and_leaves_the_batch_unchanged() {
    let dir = scratch("mixed-relation");
    under_lab(0x7e_02, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys())
            .await
            .expect("creates the database");

        let mut receiving = WriteBatch::new(R);
        receiving.create_vertex(VId(1), vec![], vec![]);
        let mut foreign = WriteBatch::new(OTHER);
        foreign.create_vertex(VId(2), vec![], vec![]);

        let refusal = receiving.extend(foreign);
        assert!(
            matches!(
                refusal,
                Err(WriteError::MixedRelation {
                    expected: R,
                    found: OTHER,
                })
            ),
            "a foreign-relation batch must refuse with the typed arm, got {refusal:?}"
        );

        // The refusal moved nothing: the receiving batch still commits
        // exactly its own staged row and none of the foreign batch's.
        db.write(cx, receiving)
            .await
            .expect("the untouched receiving batch still commits");
        assert!(
            db.vertex(VId(1)).expect("healthy read").is_some(),
            "the receiving batch's own row survived the refused extend"
        );
        assert!(
            db.vertex(VId(2)).expect("healthy read").is_none(),
            "the foreign batch's row must not leak through a refused extend"
        );
    });
}
