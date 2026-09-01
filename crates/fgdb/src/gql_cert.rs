//! Deterministic certificates for the bounded GQL execution slice.
//!
//! `GqlCertificate` binds the exact statement bytes, canonical relation bind,
//! and MVCC snapshot used by the public execution API. `GqlPlanCertificate`
//! separately binds the executor-ready `BoundPlan` and snapshot. Neither type
//! claims to attest result rows, runtime cost, or an operator tree.

use crate::{Database, GqlError, ReadError};
use asupersync::fs::Vfs;
use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
use fgdb_types::CommitSeq;

const GQL_PLAN_CERTIFICATE_DOMAIN_V1: &[u8] = b"fgdb:gql-bound-plan-certificate:v1";
const GQL_PLAN_CERTIFICATE_DOMAIN_V2: &[u8] = b"fgdb:gql-bound-plan-certificate:v2";

/// Replay evidence for the public statement execution surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlCertificate {
    pub snapshot_seq: CommitSeq,
    pub statement_digest: Digest,
    pub bind_digest: Digest,
}

impl GqlCertificate {
    /// Verify the statement and bind portions against this certificate's own
    /// snapshot declaration.
    ///
    /// This deliberately does not execute the query and does not claim that
    /// result rows are certified. It proves only that the supplied statement
    /// bytes and canonical bind map are the inputs this value names.
    #[must_use]
    pub fn verifies(&self, statement: &str, bind: &RelationBind) -> bool {
        digest_eq(self.statement_digest, digest_statement(statement))
            && digest_eq(self.bind_digest, digest_bind(bind))
    }

    /// Verify the complete public certificate tuple, including the explicitly
    /// expected MVCC snapshot.
    #[must_use]
    pub fn verifies_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        snapshot_seq: CommitSeq,
    ) -> bool {
        self.snapshot_seq == snapshot_seq && self.verifies(statement, bind)
    }
}

/// A replay-stable identity for a bound GQL plan at one snapshot frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlPlanCertificate {
    pub digest: Digest,
    pub snapshot_seq: CommitSeq,
}

impl GqlPlanCertificate {
    /// Verify this certificate against `plan` and the certificate's declared
    /// snapshot using the current v2 transcript.
    #[must_use]
    pub fn verifies(&self, plan: &BoundPlan) -> bool {
        let expected = certify(plan, self.snapshot_seq);
        self.snapshot_seq == expected.snapshot_seq && digest_eq(self.digest, expected.digest)
    }

    /// Verify this certificate against an explicitly expected snapshot.
    #[must_use]
    pub fn verifies_at(&self, plan: &BoundPlan, snapshot_seq: CommitSeq) -> bool {
        self.snapshot_seq == snapshot_seq && self.verifies(plan)
    }

    /// Verify a certificate produced by the historical v1 transcript.
    ///
    /// V1 predates `BoundPlan::neq` and therefore does not bind that field.
    /// This method exists only to make migration explicit; new certificates
    /// are always produced by [`certify`] under v2.
    #[must_use]
    pub fn verifies_v1_legacy(&self, plan: &BoundPlan) -> bool {
        let expected = certify_v1_legacy(plan, self.snapshot_seq);
        self.snapshot_seq == expected.snapshot_seq && digest_eq(self.digest, expected.digest)
    }
}

/// Certify every field of the executor-ready plan and its MVCC snapshot.
///
/// V2 adds `BoundPlan::neq`, the one current plan field omitted by the original
/// transcript. The domain changed with the transcript, so no v1 certificate can
/// be misread as v2 even when every other field is identical.
pub fn certify(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    certify_with_domain(plan, snapshot_seq, GQL_PLAN_CERTIFICATE_DOMAIN_V2, true)
}

fn bind_plan_for_certificate(
    statement: &str,
    bind: &RelationBind,
) -> Result<BoundPlan, GqlError> {
    bind.bind(statement).map_err(|error| match error {
        fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
        unbound => GqlError::Bind(unbound),
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Bind and certify one statement at the handle's current published frontier.
    ///
    /// This is a plan-only operation: it does not execute the plan and makes no
    /// claim about result rows or runtime work. The handle is still checked for
    /// read health before a certificate is minted.
    pub fn gql_plan_certificate(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<GqlPlanCertificate, GqlError> {
        let plan = bind_plan_for_certificate(statement, bind)?;
        let snapshot_seq = self.frontier().map_err(GqlError::Read)?;
        Ok(certify(&plan, snapshot_seq))
    }

    /// Bind and certify one statement at the exact caller-selected sequence.
    ///
    /// A future sequence is refused rather than clamped to the live frontier.
    /// Historical sequences at or below the published frontier are named
    /// verbatim in the certificate transcript; the plan is not executed and no
    /// result-row claim is made.
    pub fn gql_plan_certificate_at(
        &self,
        statement: &str,
        bind: &RelationBind,
        as_of: CommitSeq,
    ) -> Result<GqlPlanCertificate, GqlError> {
        let plan = bind_plan_for_certificate(statement, bind)?;
        let frontier = self.frontier().map_err(GqlError::Read)?;
        if as_of > frontier {
            return Err(GqlError::Read(ReadError::BeyondFrontier {
                asked: as_of,
                frontier,
            }));
        }
        Ok(certify(&plan, as_of))
    }
}

/// Recompute the historical v1 transcript for explicit migration checks.
fn certify_v1_legacy(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    certify_with_domain(
        plan,
        snapshot_seq,
        GQL_PLAN_CERTIFICATE_DOMAIN_V1,
        false,
    )
}

fn certify_with_domain(
    plan: &BoundPlan,
    snapshot_seq: CommitSeq,
    domain: &[u8],
    include_neq: bool,
) -> GqlPlanCertificate {
    let mut hasher = Hasher::new();
    hasher.update(domain);
    update_relation(&mut hasher, plan.relation);
    update_string(&mut hasher, &plan.src_var);
    update_string(&mut hasher, &plan.dst_var);
    update_string(&mut hasher, &plan.via_var);
    update_relation(&mut hasher, plan.hop2_relation);
    update_optional_string(&mut hasher, plan.hop2_dst_var.as_deref());
    hasher.update(&[projection_tag(plan.projection)]);
    hasher.update(&[direction_tag(plan.direction)]);
    update_label(&mut hasher, plan.src_label);
    update_label(&mut hasher, plan.dst_label);
    update_string_pair(&mut hasher, plan.eq.as_ref());
    if include_neq {
        update_string_pair(&mut hasher, plan.neq.as_ref());
    }
    update_property(&mut hasher, plan.src_prop);
    update_property(&mut hasher, plan.src_prop_ne);
    update_property(&mut hasher, plan.src_prop_gt);
    update_property(&mut hasher, plan.src_prop_lt);
    update_property(&mut hasher, plan.src_prop_ge);
    update_property(&mut hasher, plan.src_prop_le);
    update_property(&mut hasher, plan.dst_prop);
    update_property(&mut hasher, plan.dst_prop_ne);
    update_property(&mut hasher, plan.dst_prop_gt);
    update_property(&mut hasher, plan.dst_prop_lt);
    update_property(&mut hasher, plan.dst_prop_ge);
    update_property(&mut hasher, plan.dst_prop_le);
    update_property(&mut hasher, plan.hop2_dst_prop);
    update_property(&mut hasher, plan.hop2_dst_prop_ne);
    update_property(&mut hasher, plan.hop2_dst_prop_gt);
    update_property(&mut hasher, plan.hop2_dst_prop_lt);
    update_property(&mut hasher, plan.hop2_dst_prop_ge);
    update_property(&mut hasher, plan.hop2_dst_prop_le);
    update_optional_u64(&mut hasher, plan.limit);
    update_optional_u64(&mut hasher, plan.skip);
    hasher.update(&snapshot_seq.0.to_be_bytes());

    GqlPlanCertificate {
        digest: hasher.finalize(),
        snapshot_seq,
    }
}

fn digest_eq(left: Digest, right: Digest) -> bool {
    left.0
        .iter()
        .zip(right.0.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (*left ^ *right)
        })
        == 0
}

fn update_relation(hasher: &mut Hasher, relation: Option<RelationId>) {
    match relation {
        None => hasher.update(&[0]),
        Some(relation) => {
            hasher.update(&[1]);
            hasher.update(&relation.0.to_be_bytes());
        }
    }
}

fn update_label(hasher: &mut Hasher, label: Option<LabelId>) {
    match label {
        None => hasher.update(&[0]),
        Some(label) => {
            hasher.update(&[1]);
            hasher.update(&label.0.to_be_bytes());
        }
    }
}

fn update_optional_string(hasher: &mut Hasher, value: Option<&str>) {
    match value {
        None => hasher.update(&[0]),
        Some(value) => {
            hasher.update(&[1]);
            update_string(hasher, value);
        }
    }
}

fn update_string_pair(hasher: &mut Hasher, pair: Option<&(String, String)>) {
    match pair {
        None => hasher.update(&[0]),
        Some((left, right)) => {
            hasher.update(&[1]);
            update_string(hasher, left);
            update_string(hasher, right);
        }
    }
}

fn update_property(hasher: &mut Hasher, property: Option<(PropertyKeyId, i64)>) {
    match property {
        None => hasher.update(&[0]),
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
}

fn update_optional_u64(hasher: &mut Hasher, value: Option<u64>) {
    match value {
        None => hasher.update(&[0]),
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        }
    }
}

fn direction_tag(direction: EdgeDirection) -> u8 {
    match direction {
        EdgeDirection::Outgoing => 0,
        EdgeDirection::Incoming => 1,
        EdgeDirection::Undirected => 2,
    }
}

fn projection_tag(projection: ReturnProjection) -> u8 {
    match projection {
        ReturnProjection::Destination => 0,
        ReturnProjection::Source => 1,
        ReturnProjection::Hop2Destination => 2,
    }
}

fn update_string(hasher: &mut Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

pub fn digest_statement(src: &str) -> Digest {
    hash(src.as_bytes())
}

pub fn digest_bind(bind: &RelationBind) -> Digest {
    hash(&bind.canonical_bytes())
}

#[cfg(test)]
mod tests {
    use super::{
        GqlCertificate, GqlPlanCertificate, certify, certify_v1_legacy, digest_bind,
        digest_statement, direction_tag, projection_tag,
    };
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
    use fgdb_types::CommitSeq;

    fn plan(relation: u64) -> BoundPlan {
        BoundPlan {
            relation: Some(RelationId(relation)),
            src_var: "a".to_owned(),
            dst_var: "b".to_owned(),
            src_label: None,
            dst_label: None,
            via_var: String::new(),
            hop2_relation: None,
            hop2_dst_var: None,
            projection: ReturnProjection::Destination,
            direction: EdgeDirection::Outgoing,
            neq: None,
            eq: None,
            src_prop: None,
            src_prop_ne: None,
            src_prop_gt: None,
            src_prop_lt: None,
            src_prop_ge: None,
            src_prop_le: None,
            dst_prop: None,
            dst_prop_ne: None,
            dst_prop_gt: None,
            dst_prop_lt: None,
            dst_prop_ge: None,
            dst_prop_le: None,
            limit: None,
            skip: None,
            hop2_dst_prop: None,
            hop2_dst_prop_ne: None,
            hop2_dst_prop_gt: None,
            hop2_dst_prop_lt: None,
            hop2_dst_prop_ge: None,
            hop2_dst_prop_le: None,
        }
    }

    fn assert_plan_changed(field: &str, base: &BoundPlan, changed: BoundPlan) {
        assert_ne!(
            certify(base, CommitSeq(11)).digest,
            certify(&changed, CommitSeq(11)).digest,
            "mutating {field} must change the v2 plan certificate"
        );
    }

    #[test]
    fn same_plan_and_snapshot_have_equal_certificates() {
        let plan = plan(7);
        let first = certify(&plan, CommitSeq(11));
        let second = certify(&plan, CommitSeq(11));
        assert_eq!(first, second);
        assert!(first.verifies(&plan));
        let _: GqlPlanCertificate = first;
    }

    #[test]
    fn explicit_snapshot_verification_refuses_mismatch() {
        let plan = plan(7);
        let certificate = certify(&plan, CommitSeq(11));
        assert!(certificate.verifies_at(&plan, CommitSeq(11)));
        assert!(!certificate.verifies_at(&plan, CommitSeq(12)));
    }

    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn v2_transcript_binds_every_current_bound_plan_field() {
        let base = plan(7);
        let mut variants = Vec::<(&str, BoundPlan)>::new();
        macro_rules! variant {
            ($field:ident, $value:expr) => {{
                let mut changed = base.clone();
                changed.$field = $value;
                variants.push((stringify!($field), changed));
            }};
        }

        variant!(relation, Some(RelationId(8)));
        variant!(src_var, "source".to_owned());
        variant!(dst_var, "destination".to_owned());
        variant!(src_label, Some(LabelId(1)));
        variant!(dst_label, Some(LabelId(2)));
        variant!(via_var, "via".to_owned());
        variant!(hop2_relation, Some(RelationId(9)));
        variant!(hop2_dst_var, Some("far".to_owned()));
        variant!(projection, ReturnProjection::Source);
        variant!(direction, EdgeDirection::Incoming);
        variant!(neq, Some(("a".to_owned(), "b".to_owned())));
        variant!(eq, Some(("a".to_owned(), "b".to_owned())));
        variant!(src_prop, Some((PropertyKeyId(1), 1)));
        variant!(src_prop_ne, Some((PropertyKeyId(2), 2)));
        variant!(src_prop_gt, Some((PropertyKeyId(3), 3)));
        variant!(src_prop_lt, Some((PropertyKeyId(4), 4)));
        variant!(src_prop_ge, Some((PropertyKeyId(5), 5)));
        variant!(src_prop_le, Some((PropertyKeyId(6), 6)));
        variant!(dst_prop, Some((PropertyKeyId(7), 7)));
        variant!(dst_prop_ne, Some((PropertyKeyId(8), 8)));
        variant!(dst_prop_gt, Some((PropertyKeyId(9), 9)));
        variant!(dst_prop_lt, Some((PropertyKeyId(10), 10)));
        variant!(dst_prop_ge, Some((PropertyKeyId(11), 11)));
        variant!(dst_prop_le, Some((PropertyKeyId(12), 12)));
        variant!(limit, Some(13));
        variant!(skip, Some(14));
        variant!(hop2_dst_prop, Some((PropertyKeyId(15), 15)));
        variant!(hop2_dst_prop_ne, Some((PropertyKeyId(16), 16)));
        variant!(hop2_dst_prop_gt, Some((PropertyKeyId(17), 17)));
        variant!(hop2_dst_prop_lt, Some((PropertyKeyId(18), 18)));
        variant!(hop2_dst_prop_ge, Some((PropertyKeyId(19), 19)));
        variant!(hop2_dst_prop_le, Some((PropertyKeyId(20), 20)));

        for (field, changed) in variants {
            assert_plan_changed(field, &base, changed);
        }
    }

    #[test]
    fn v2_repairs_the_v1_neq_omission_without_cross_version_collision() {
        let base = plan(7);
        let mut with_neq = base.clone();
        with_neq.neq = Some(("a".to_owned(), "b".to_owned()));

        assert_eq!(
            certify_v1_legacy(&base, CommitSeq(11)).digest,
            certify_v1_legacy(&with_neq, CommitSeq(11)).digest,
            "the migration fixture must reproduce the historical omission"
        );
        assert_ne!(
            certify(&base, CommitSeq(11)).digest,
            certify(&with_neq, CommitSeq(11)).digest
        );
        assert_ne!(
            certify_v1_legacy(&base, CommitSeq(11)).digest,
            certify(&base, CommitSeq(11)).digest,
            "domain separation prevents a v1 value from being read as v2"
        );
    }

    #[test]
    fn public_execution_certificate_verifies_exact_inputs_only() {
        let bind = RelationBind::new().with_relation("KNOWS", RelationId(7));
        let certificate = GqlCertificate {
            snapshot_seq: CommitSeq(11),
            statement_digest: digest_statement("MATCH (a)-[:KNOWS]->(b) RETURN b"),
            bind_digest: digest_bind(&bind),
        };

        assert!(certificate.verifies_at(
            "MATCH (a)-[:KNOWS]->(b) RETURN b",
            &bind,
            CommitSeq(11)
        ));
        assert!(!certificate.verifies_at(
            "MATCH (a)-[:KNOWS]->(b) RETURN b ",
            &bind,
            CommitSeq(11)
        ));
        assert!(!certificate.verifies_at(
            "MATCH (a)-[:KNOWS]->(b) RETURN b",
            &bind,
            CommitSeq(12)
        ));

        let other_bind = RelationBind::new().with_relation("KNOWS", RelationId(8));
        assert!(!certificate.verifies(
            "MATCH (a)-[:KNOWS]->(b) RETURN b",
            &other_bind
        ));
    }

    #[test]
    fn return_projection_tags_are_stable() {
        assert_eq!(projection_tag(ReturnProjection::Destination), 0);
        assert_eq!(projection_tag(ReturnProjection::Source), 1);
        assert_eq!(projection_tag(ReturnProjection::Hop2Destination), 2);
    }

    #[test]
    fn direction_tags_are_stable() {
        assert_eq!(direction_tag(EdgeDirection::Outgoing), 0);
        assert_eq!(direction_tag(EdgeDirection::Incoming), 1);
        assert_eq!(direction_tag(EdgeDirection::Undirected), 2);
    }
}
