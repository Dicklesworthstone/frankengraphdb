//! **Labeled `execute_gql_certified` rows and digests**
//! (`fgdb-w5-parsers-nje.5`).
//!
//! The certificate face of `gql_node_label.rs`'s fixture: the labeled
//! certified call answers only the labeled source's destination while the
//! unlabeled certified call beside it holds both — and because the label is
//! part of the STATEMENT TEXT, the two executing certificates carry
//! genuinely different statement digests at one sequence. Re-minting the
//! labeled call is byte-identical, and the plan-certificate surface names
//! the same sequence as the executing certificate (the two surfaces hash
//! different transcripts by design — statement text + bind versus the bound
//! plan — so their agreement is sequence and determinism, never byte
//! equality across surfaces).

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
const UNLABELED_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-label-cert-{}-{name}", std::process::id()))
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

/// The product fixture: a labeled and an unlabeled source with DISJOINT
/// destinations, so the labeled answer cannot be the unlabeled one in
/// disguise.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![PERSON], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.create_vertex(VId(4), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

#[test]
fn labeled_certified_rows_and_digests_separate_from_the_unlabeled_call() {
    under_lab(0x1c_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("certified");
        let db = seeded(cx, &dir).await;
        let bind = bind_r_person();

        // The labeled certified call: only the :Person source's destination.
        let (labeled_rows, labeled_cert) = db
            .execute_gql_certified(LABELED_B, &bind)
            .expect("the labeled certified MATCH executes");
        assert_eq!(
            labeled_rows,
            vec![VId(2)],
            "the unlabeled source's destination is excluded"
        );

        // The unlabeled certified call beside it: both destinations, and a
        // genuinely different statement digest — the label is statement
        // text, so this pair separates by digest at ONE sequence.
        let (unlabeled_rows, unlabeled_cert) = db
            .execute_gql_certified(UNLABELED_B, &bind)
            .expect("the unlabeled certified MATCH executes");
        assert!(
            unlabeled_rows.contains(&VId(4)) && unlabeled_rows.contains(&VId(2)),
            "the unlabeled statement holds both destinations: {unlabeled_rows:?}"
        );
        assert_eq!(
            labeled_cert.snapshot_seq, unlabeled_cert.snapshot_seq,
            "one database, one sequence — only the statement separates them"
        );
        assert_ne!(
            labeled_cert.statement_digest, unlabeled_cert.statement_digest,
            "the label is statement text: two statements, two digests"
        );
        assert_eq!(
            labeled_cert.bind_digest, unlabeled_cert.bind_digest,
            "one bind serves both statements"
        );

        // Determinism: the same labeled certified call re-mints an EQUAL
        // certificate, rows and all.
        let (again_rows, again_cert) = db
            .execute_gql_certified(LABELED_B, &bind)
            .expect("the labeled certified MATCH re-executes");
        assert_eq!(again_rows, labeled_rows);
        assert_eq!(again_cert, labeled_cert, "byte-identical re-mint");

        // Cross-surface agreement: the plan certificate of the same labeled
        // statement names the same sequence and re-mints deterministically.
        // The two surfaces hash different transcripts by design (statement
        // text + bind vs the bound plan), so agreement here is sequence and
        // determinism — never byte equality across surfaces.
        let plan_cert = db
            .gql_plan_certificate(LABELED_B, &bind)
            .expect("the labeled plan certificate mints");
        assert_eq!(
            plan_cert.snapshot_seq, labeled_cert.snapshot_seq,
            "both certificate surfaces name the same snapshot"
        );
        assert_eq!(
            plan_cert,
            db.gql_plan_certificate(LABELED_B, &bind)
                .expect("the labeled plan certificate re-mints"),
            "the plan surface is deterministic at the same coordinates"
        );
    });
}
