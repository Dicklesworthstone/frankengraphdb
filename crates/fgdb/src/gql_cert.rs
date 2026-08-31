//! GQL plan certificates for reproducible, auditable query execution.
//!
//! A certificate records the MVCC snapshot commit sequence and a deterministic
//! digest of every field of the [`BoundPlan`].  The digest is computed from an
//! unambiguous canonical transcript with explicit domain separation.  Any
//! change to the bound query plan or snapshot changes the certificate.
//!
//! The certificate is intentionally small and contains no runtime telemetry.
//! It answers one question: *which exact bounded plan ran against which exact
//! MVCC snapshot?*

use fgdb_crypto::hash;
use fgdb_delta_types::RelationId;
use fgdb_gql::{BoundPlan, Direction, PropCmp, PropPredicate};
use fgdb_types::CommitSeq;

use crate::gql_exec::RelationBind;

const PLAN_CERT_DOMAIN: &[u8] = b"fgdb.gql.plan-certificate.v1";
const STATEMENT_CERT_DOMAIN: &[u8] = b"fgdb.gql.statement.v1";
const BIND_CERT_DOMAIN: &[u8] = b"fgdb.gql.relation-bind.v1";

/// Deterministic certificate for one bound GQL plan at one MVCC snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlPlanCertificate {
    /// Commit sequence of the snapshot read by the executor.
    pub snapshot_seq: CommitSeq,
    /// Keyed digest of the canonical bound-plan transcript.
    pub plan_digest: [u8; 32],
}

/// Full query-execution certificate: plan + exact statement bytes + relation
/// binding map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlCertificate {
    pub plan: GqlPlanCertificate,
    pub statement_digest: [u8; 32],
    pub bind_digest: [u8; 32],
}

impl GqlPlanCertificate {
    /// Recompute this plan certificate from the exact bounded plan and
    /// snapshot it claims to describe.
    ///
    /// Verification is intentionally pure: it neither executes the plan nor
    /// consults mutable database state.  A caller can therefore persist the
    /// plan plus certificate, move them across processes, and check that the
    /// certificate still names exactly those inputs.
    #[must_use]
    pub fn verifies(&self, plan: &BoundPlan, snapshot_seq: CommitSeq) -> bool {
        self.snapshot_seq == snapshot_seq && *self == certify(plan, snapshot_seq)
    }
}

impl GqlCertificate {
    /// Verify the complete public GQL certificate transcript.
    ///
    /// The statement digest binds the caller's exact UTF-8 bytes (including
    /// whitespace), the bind digest binds the canonical name-to-relation map,
    /// and the plan certificate binds the lowered bounded plan plus snapshot.
    /// This verifies provenance of the execution inputs; result-row
    /// certification remains a distinct future contract.
    #[must_use]
    pub fn verifies_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        plan: &BoundPlan,
        snapshot_seq: CommitSeq,
    ) -> bool {
        self.statement_digest == digest_statement(statement)
            && self.bind_digest == digest_bind(bind)
            && self.plan.verifies(plan, snapshot_seq)
    }
}

/// Certify `plan` at `snapshot_seq`.
pub(crate) fn certify(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    let transcript = canonical_plan_bytes(plan, snapshot_seq);
    GqlPlanCertificate {
        snapshot_seq,
        plan_digest: hash::keyed_hash(PLAN_CERT_DOMAIN, &transcript),
    }
}

/// Certify the full execution input: exact statement text, relation binding,
/// bound plan, and snapshot sequence.
pub(crate) fn certify_execution(
    statement: &str,
    bind: &RelationBind,
    plan: &BoundPlan,
    snapshot_seq: CommitSeq,
) -> GqlCertificate {
    GqlCertificate {
        plan: certify(plan, snapshot_seq),
        statement_digest: digest_statement(statement),
        bind_digest: digest_bind(bind),
    }
}

fn digest_statement(statement: &str) -> [u8; 32] {
    let mut transcript = Vec::with_capacity(8 + statement.len());
    put_bytes(&mut transcript, statement.as_bytes());
    hash::keyed_hash(STATEMENT_CERT_DOMAIN, &transcript)
}

fn digest_bind(bind: &RelationBind) -> [u8; 32] {
    let transcript = bind.canonical_bytes();
    hash::keyed_hash(BIND_CERT_DOMAIN, &transcript)
}

fn canonical_plan_bytes(plan: &BoundPlan, snapshot_seq: CommitSeq) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    put_u64(&mut out, snapshot_seq.0);
    put_u32(&mut out, plan.relation.0);
    put_direction(&mut out, plan.direction);
    put_string(&mut out, &plan.src_var);
    put_string(&mut out, &plan.dst_var);
    put_string(&mut out, &plan.return_var);
    put_opt_string(&mut out, plan.src_label.as_deref());
    put_opt_string(&mut out, plan.dst_label.as_deref());
    put_opt_u32(&mut out, plan.src_prop_eq.map(|(key, value)| (key, value)));
    put_opt_u32(&mut out, plan.dst_prop_eq.map(|(key, value)| (key, value)));
    put_predicate(&mut out, plan.src_prop_cmp.as_ref());
    put_predicate(&mut out, plan.dst_prop_cmp.as_ref());
    put_u64(&mut out, plan.skip as u64);
    put_opt_u64(&mut out, plan.limit.map(|value| value as u64));
    put_opt_u32(&mut out, plan.hop2_relation.map(|relation| relation.0));
    put_opt_direction(&mut out, plan.hop2_direction);
    put_opt_string(&mut out, plan.hop2_mid_var.as_deref());
    put_opt_string(&mut out, plan.hop2_dst_var.as_deref());
    put_opt_string(&mut out, plan.hop2_mid_label.as_deref());
    put_opt_string(&mut out, plan.hop2_dst_label.as_deref());
    put_opt_u32(&mut out, plan.hop2_mid_prop_eq.map(|(key, value)| (key, value)));
    put_opt_u32(&mut out, plan.hop2_dst_prop_eq.map(|(key, value)| (key, value)));
    put_predicate(&mut out, plan.hop2_mid_prop_cmp.as_ref());
    put_predicate(&mut out, plan.hop2_dst_prop_cmp.as_ref());
    out
}

fn put_direction(out: &mut Vec<u8>, direction: Direction) {
    out.push(match direction {
        Direction::Outgoing => 0,
        Direction::Incoming => 1,
        Direction::Undirected => 2,
    });
}

fn put_opt_direction(out: &mut Vec<u8>, direction: Option<Direction>) {
    match direction {
        None => out.push(0),
        Some(direction) => {
            out.push(1);
            put_direction(out, direction);
        }
    }
}

fn put_cmp(out: &mut Vec<u8>, cmp: PropCmp) {
    out.push(match cmp {
        PropCmp::Eq => 0,
        PropCmp::Lt => 1,
        PropCmp::Gt => 2,
        PropCmp::Ne => 3,
        PropCmp::Le => 4,
        PropCmp::Ge => 5,
    });
}

fn put_predicate(out: &mut Vec<u8>, predicate: Option<&PropPredicate>) {
    match predicate {
        None => out.push(0),
        Some(predicate) => {
            out.push(1);
            put_u32(out, predicate.key);
            put_cmp(out, predicate.cmp);
            put_i64(out, predicate.value);
        }
    }
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    put_bytes(out, value.as_bytes());
}

fn put_opt_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            put_string(out, value);
        }
    }
}

fn put_opt_u32(out: &mut Vec<u8>, value: Option<(u32, i64)>) {
    match value {
        None => out.push(0),
        Some((key, value)) => {
            out.push(1);
            put_u32(out, key);
            put_i64(out, value);
        }
    }
}

fn put_opt_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            put_u64(out, value);
        }
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u64(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate_inputs() -> (&'static str, BoundPlan, RelationBind) {
        let statement = "MATCH (a:Person {1: 10})-[:KNOWS]->(b:Person {2: 20})-[:LIKES]-(c:Thing) WHERE b.3 >= 30 AND c.4 != 40 RETURN c SKIP 5 LIMIT 7";
        let plan = BoundPlan {
            relation: RelationId(11),
            direction: Direction::Outgoing,
            src_var: "a".to_string(),
            dst_var: "b".to_string(),
            return_var: "c".to_string(),
            src_label: Some("Person".to_string()),
            dst_label: Some("Person".to_string()),
            src_prop_eq: Some((1, 10)),
            dst_prop_eq: Some((2, 20)),
            src_prop_cmp: None,
            dst_prop_cmp: Some(PropPredicate {
                key: 3,
                cmp: PropCmp::Ge,
                value: 30,
            }),
            skip: 5,
            limit: Some(7),
            hop2_relation: Some(RelationId(12)),
            hop2_direction: Some(Direction::Undirected),
            hop2_mid_var: Some("b".to_string()),
            hop2_dst_var: Some("c".to_string()),
            hop2_mid_label: Some("Person".to_string()),
            hop2_dst_label: Some("Thing".to_string()),
            hop2_mid_prop_eq: None,
            hop2_dst_prop_eq: None,
            hop2_mid_prop_cmp: Some(PropPredicate {
                key: 3,
                cmp: PropCmp::Ge,
                value: 30,
            }),
            hop2_dst_prop_cmp: Some(PropPredicate {
                key: 4,
                cmp: PropCmp::Ne,
                value: 40,
            }),
        };
        let bind = RelationBind::new()
            .with_relation("KNOWS", RelationId(11))
            .with_relation("LIKES", RelationId(12));
        (statement, plan, bind)
    }

    #[test]
    fn same_plan_and_snapshot_produce_same_certificate() {
        let (_, plan, _) = certificate_inputs();
        assert_eq!(certify(&plan, CommitSeq(5)), certify(&plan, CommitSeq(5)));
    }

    #[test]
    fn snapshot_changes_certificate() {
        let (_, plan, _) = certificate_inputs();
        assert_ne!(certify(&plan, CommitSeq(5)), certify(&plan, CommitSeq(6)));
    }

    #[test]
    fn statement_bytes_and_bind_mapping_are_certified() {
        let (statement, plan, bind) = certificate_inputs();
        let cert = certify_execution(statement, &bind, &plan, CommitSeq(5));
        assert_eq!(cert, certify_execution(statement, &bind, &plan, CommitSeq(5)));

        let whitespace_changed = format!("{statement} ");
        assert_ne!(
            cert.statement_digest,
            certify_execution(&whitespace_changed, &bind, &plan, CommitSeq(5)).statement_digest
        );

        let changed_bind = bind.clone().with_relation("KNOWS", RelationId(99));
        assert_ne!(
            cert.bind_digest,
            certify_execution(statement, &changed_bind, &plan, CommitSeq(5)).bind_digest
        );
    }

    #[test]
    fn public_plan_verifier_binds_plan_and_snapshot() {
        let (_, plan, _) = certificate_inputs();
        let snapshot_seq = CommitSeq(17);
        let certificate = certify(&plan, snapshot_seq);

        assert!(certificate.verifies(&plan, snapshot_seq));

        let mut changed_plan = plan.clone();
        changed_plan.limit = Some(19);
        assert!(!certificate.verifies(&changed_plan, snapshot_seq));
        assert!(!certificate.verifies(&plan, CommitSeq(18)));
    }

    #[test]
    fn public_complete_verifier_rejects_every_transcript_axis() {
        let (statement, plan, bind) = certificate_inputs();
        let snapshot_seq = CommitSeq(23);
        let certificate = certify_execution(statement, &bind, &plan, snapshot_seq);

        assert!(certificate.verifies_at(statement, &bind, &plan, snapshot_seq));

        assert!(!certificate.verifies_at(
            &format!("{statement} "),
            &bind,
            &plan,
            snapshot_seq,
        ));

        let changed_bind = bind.clone().with_relation("KNOWS", RelationId(99));
        assert!(!certificate.verifies_at(statement, &changed_bind, &plan, snapshot_seq));

        let mut changed_plan = plan.clone();
        changed_plan.skip = 1;
        assert!(!certificate.verifies_at(statement, &bind, &changed_plan, snapshot_seq));

        assert!(!certificate.verifies_at(statement, &bind, &plan, CommitSeq(24)));
    }

    #[test]
    fn all_plan_fields_affect_digest() {
        let (_, plan, _) = certificate_inputs();
        let base = certify(&plan, CommitSeq(9));
        let mut variants = Vec::new();

        let mut changed = plan.clone();
        changed.relation = RelationId(99);
        variants.push(changed);

        let mut changed = plan.clone();
        changed.direction = Direction::Incoming;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.src_var = "source".to_string();
        variants.push(changed);

        let mut changed = plan.clone();
        changed.dst_var = "dest".to_string();
        variants.push(changed);

        let mut changed = plan.clone();
        changed.return_var = "b".to_string();
        variants.push(changed);

        let mut changed = plan.clone();
        changed.src_label = Some("Other".to_string());
        variants.push(changed);

        let mut changed = plan.clone();
        changed.dst_label = None;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.src_prop_eq = Some((9, 9));
        variants.push(changed);

        let mut changed = plan.clone();
        changed.dst_prop_eq = None;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.src_prop_cmp = Some(PropPredicate {
            key: 8,
            cmp: PropCmp::Lt,
            value: 80,
        });
        variants.push(changed);

        let mut changed = plan.clone();
        changed.dst_prop_cmp = Some(PropPredicate {
            key: 3,
            cmp: PropCmp::Gt,
            value: 30,
        });
        variants.push(changed);

        let mut changed = plan.clone();
        changed.skip = 6;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.limit = None;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_relation = Some(RelationId(13));
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_direction = Some(Direction::Incoming);
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_mid_var = Some("middle".to_string());
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_dst_var = Some("end".to_string());
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_mid_label = None;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_dst_label = Some("Other".to_string());
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_mid_prop_eq = Some((5, 50));
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_dst_prop_eq = Some((6, 60));
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_mid_prop_cmp = None;
        variants.push(changed);

        let mut changed = plan.clone();
        changed.hop2_dst_prop_cmp = Some(PropPredicate {
            key: 4,
            cmp: PropCmp::Eq,
            value: 40,
        });
        variants.push(changed);

        for (index, variant) in variants.into_iter().enumerate() {
            assert_ne!(
                base.plan_digest,
                certify(&variant, CommitSeq(9)).plan_digest,
                "field mutation {index} did not change the digest",
            );
        }
    }
}
