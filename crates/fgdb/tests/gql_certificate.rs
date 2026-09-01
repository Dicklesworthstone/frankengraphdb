//! **The BoundPlan certificate: deterministic, seq-sensitive, bind-sensitive**
//! (`fgdb-gql-oracle-cert-jjn0`).
//!
//! The plan certificate is a domain-separated hash over the BOUND plan plus
//! the snapshot seq the answer would come from. Three laws pin what "over"
//! means:
//! same statement + same bind + same seq ⇒ byte-identical digest; a commit
//! that advances the frontier changes the digest (seq is IN the transcript);
//! the same statement bound to a different `RelationId` changes the digest
//! (the BOUND plan is hashed, not the source text). Not EXPLAIN, not G0
//! language-contracts, not TCK.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's names):
//! - `Database::gql_plan_certificate(&self, src: &str, bind: &RelationBind)
//!    -> Result<GqlPlanCertificate, GqlError>`
//! - `GqlPlanCertificate { snapshot_seq: CommitSeq, digest: Digest }`,
//!   comparable with `==` and verifiable against an exact bound plan + seq.
//!
//! Until that lands this file fails to compile — deliberately. It is the
//! executable acceptance criteria; do not weaken it to make it compile.
//!
//! **WHAT EACH TEST KILLS.** A digest over the source text alone passes
//! test 1 and fails 2 and 3. A digest over text+seq passes 1 and 2 and fails
//! 3 — binding `R` to a different relation MUST move the digest, or the
//! certificate cannot distinguish two plans that answer differently from the
//! same graph. A digest over the plan alone (no seq) fails 2. Only
//! plan+seq survives all three. The verifier checks the inverse direction:
//! the exact plan+seq is accepted while any field or seq change is refused.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlPlanCertificate, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R1: RelationId = RelationId(1);
const R2: RelationId = RelationId(2);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-plan-cert-{}-{name}", std::process::id()))
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

fn bind_to(relation: RelationId) -> RelationBind {
    RelationBind::new().with_relation("R", relation)
}

/// One committed `:R` edge, so the certified plan is over a graph with an
/// answer, and so the frontier below is a real committed seq.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut batch = WriteBatch::new(R1);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.expect("commits");
    db
}

/// Same statement, same bind, no intervening write: two certificates are
/// byte-identical — digest AND seq. B5's baseline: a certificate that is not
/// reproducible from the same inputs certifies nothing. The public verifier
/// also accepts exactly that plan+seq and refuses both a sequence mismatch and
/// a single-field `neq` mutation.
#[test]
fn same_inputs_yield_an_identical_and_verifiable_certificate() {
    under_lab(0x9c_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("deterministic");
        let db = seeded(cx, &dir).await;
        let bind = bind_to(R1);
        let plan = bind.bind(PINNED).expect("the pinned query binds");

        let first = db
            .gql_plan_certificate(PINNED, &bind)
            .expect("first certificate");
        let second = db
            .gql_plan_certificate(PINNED, &bind)
            .expect("second certificate");
        let frontier = db.frontier().expect("healthy frontier");
        assert_eq!(
            first.snapshot_seq, frontier,
            "the certificate names the frontier it certifies"
        );
        assert_eq!(first.snapshot_seq, second.snapshot_seq);
        assert_eq!(first.digest, second.digest);
        let whole: &GqlPlanCertificate = &second;
        assert_eq!(&first, whole, "equal field-wise must be equal whole");
        assert!(
            first.verifies_at(&plan, frontier),
            "the public verifier accepts the exact bound plan and snapshot"
        );
        assert!(
            !first.verifies_at(&plan, fgdb_types::CommitSeq(frontier.0 + 1)),
            "the same plan at another snapshot is a different certificate"
        );

        let mut changed = plan.clone();
        changed.neq = Some(("a".to_owned(), "b".to_owned()));
        assert!(
            !first.verifies_at(&changed, frontier),
            "v2 binds neq: the historical v1 omission cannot survive verification"
        );
    });
}

/// A commit that advances the frontier changes the digest: seq is in the
/// transcript. Field-level assertions on BOTH seq and digest, so a
/// certificate that carries the new seq beside an unchanged plan-only hash
/// still fails.
#[test]
fn an_advanced_frontier_changes_the_digest() {
    under_lab(0x9c_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("seq-sensitive");
        let mut db = seeded(cx, &dir).await;
        let bind = bind_to(R1);
        let plan = bind.bind(PINNED).expect("the pinned query binds");

        let before = db
            .gql_plan_certificate(PINNED, &bind)
            .expect("certificate before the write");
        assert!(before.verifies(&plan));

        let mut batch = WriteBatch::new(R1);
        batch.create_vertex(VId(9), vec![], vec![]);
        batch.add_edge(EId(11), VId(1), VId(9), vec![]);
        db.write(cx, batch).await.expect("intervening commit");

        let after = db
            .gql_plan_certificate(PINNED, &bind)
            .expect("certificate after the write");
        assert_ne!(
            before.snapshot_seq, after.snapshot_seq,
            "the commit advanced the frontier the certificate must name"
        );
        assert_ne!(
            before.digest, after.digest,
            "seq is in the hash transcript: a plan-only digest is identical \
             across a commit and certifies no particular snapshot"
        );
        assert!(after.verifies(&plan));
        assert!(!before.verifies_at(&plan, after.snapshot_seq));
        assert!(!after.verifies_at(&plan, before.snapshot_seq));
    });
}

/// Same graph, same statement, different bound `RelationId`: the digests
/// differ, because the BOUND plan is hashed — not the source text. The seqs
/// are equal (no write between the calls), so the digest difference is
/// attributable to the bind alone.
#[test]
fn a_different_bound_relation_changes_the_digest() {
    under_lab(0x9c_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("bind-sensitive");
        let db = seeded(cx, &dir).await;
        let bind_r1 = bind_to(R1);
        let bind_r2 = bind_to(R2);
        let plan_r1 = bind_r1.bind(PINNED).expect("R1 plan binds");
        let plan_r2 = bind_r2.bind(PINNED).expect("R2 plan binds");

        let bound_r1 = db
            .gql_plan_certificate(PINNED, &bind_r1)
            .expect("certificate bound to RelationId(1)");
        let bound_r2 = db
            .gql_plan_certificate(PINNED, &bind_r2)
            .expect("certificate bound to RelationId(2)");
        assert_eq!(
            bound_r1.snapshot_seq, bound_r2.snapshot_seq,
            "no write happened: any digest difference below is the bind's"
        );
        assert_ne!(
            bound_r1.digest, bound_r2.digest,
            "the same source text bound to a different relation is a \
             DIFFERENT plan; a text-hashing certificate cannot tell them apart"
        );
        assert!(bound_r1.verifies(&plan_r1));
        assert!(bound_r2.verifies(&plan_r2));
        assert!(!bound_r1.verifies(&plan_r2));
        assert!(!bound_r2.verifies(&plan_r1));
    });
}
