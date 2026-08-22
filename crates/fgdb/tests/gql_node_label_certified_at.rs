//! **The labeled MATCH, certified at a pinned sequence**
//! (`fgdb-w5-parsers-nje.5`, certified-at slice).
//!
//! `execute_gql_certified_at` on the labeled statement: the pinned rows
//! answer the S1 label filter — the durable `:Person` source's destination
//! in, the post-S1 `:Person` source's destination out, the unlabeled
//! source's destination out — and the certificate names the CALLER'S S1.
//! The live certified call rides along widened, with identical
//! statement/bind digests (statement identity does not move with the
//! snapshot) at a different named seq, so a frontier-stamped pinned pass
//! fails on the seq while a label-loosened one fails on the rows.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED_B: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-label-cert-at-{}-{name}", std::process::id()))
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

fn bind_r_person() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
}

/// Pinned label filter, pinned name, widened live sibling.
#[test]
fn the_labeled_certified_at_pins_the_s1_answer_and_names_s1() {
    under_lab(0x1d_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("pinned-label");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(cx, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("healthy frontier");

        let mut widen = WriteBatch::new(R);
        widen.create_vertex(VId(5), vec![PERSON], vec![]);
        widen.create_vertex(VId(6), vec![], vec![]);
        widen.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(cx, widen)
            .await
            .expect("the post-S1 :Person source lands");

        let (pinned_rows, pinned_cert) = db
            .execute_gql_certified_at(LABELED_B, &bind_r_person(), s1)
            .expect("the pinned certified labeled MATCH executes");
        assert!(
            pinned_rows.contains(&VId(2)),
            "the durable :Person source's destination answers at S1: {pinned_rows:?}"
        );
        assert!(
            !pinned_rows.contains(&VId(6)),
            "the :Person source committed after S1 is invisible to the \
             pinned pass: {pinned_rows:?}"
        );
        assert!(
            !pinned_rows.contains(&VId(4)),
            "the unlabeled source's destination is out at S1 too — the pin \
             does not loosen the label: {pinned_rows:?}"
        );
        assert_eq!(
            pinned_cert.snapshot_seq, s1,
            "the certificate names the caller's as_of, not the live frontier"
        );

        // The live certified sibling: widened rows, live seq, identical
        // statement/bind digests.
        let (live_rows, live_cert) = db
            .execute_gql_certified(LABELED_B, &bind_r_person())
            .expect("the live certified labeled MATCH executes");
        assert!(
            live_rows.contains(&VId(2)) && live_rows.contains(&VId(6)),
            "live, both :Person sources answer: {live_rows:?}"
        );
        assert!(!live_rows.contains(&VId(4)), "{live_rows:?}");
        assert_ne!(live_cert.snapshot_seq, pinned_cert.snapshot_seq);
        assert_eq!(pinned_cert.statement_digest, live_cert.statement_digest);
        assert_eq!(pinned_cert.bind_digest, live_cert.bind_digest);
    });
}
