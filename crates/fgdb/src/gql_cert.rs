use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_gql::{BoundPlan, RelationBind, ReturnProjection};
use fgdb_types::CommitSeq;

const GQL_PLAN_CERTIFICATE_DOMAIN: &[u8] = b"fgdb:gql-bound-plan-certificate:v1";

#[derive(Debug, PartialEq, Eq)]
pub struct GqlCertificate {
    pub snapshot_seq: CommitSeq,
    pub statement_digest: Digest,
    pub bind_digest: Digest,
}

/// A replay-stable identity for a bound GQL plan at one snapshot frontier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlPlanCertificate {
    pub digest: Digest,
    pub snapshot_seq: CommitSeq,
}

/// Certify the executor-ready plan and the snapshot at which it is evaluated.
pub fn certify(plan: &BoundPlan, snapshot_seq: CommitSeq) -> GqlPlanCertificate {
    let mut hasher = Hasher::new();
    hasher.update(GQL_PLAN_CERTIFICATE_DOMAIN);
    hasher.update(&plan.relation.0.to_be_bytes());
    update_string(&mut hasher, &plan.src_var);
    update_string(&mut hasher, &plan.dst_var);
    hasher.update(&[match plan.projection {
        ReturnProjection::Destination => 0,
        ReturnProjection::Source => 1,
    }]);
    hasher.update(&snapshot_seq.0.to_be_bytes());

    GqlPlanCertificate {
        digest: hasher.finalize(),
        snapshot_seq,
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
    let canonical = bind.canonical_bytes();
    hash(&canonical)
}

#[cfg(test)]
mod tests {
    use super::{certify, GqlPlanCertificate};
    use fgdb_delta_types::RelationId;
    use fgdb_gql::{BoundPlan, ReturnProjection};
    use fgdb_types::CommitSeq;

    fn plan(relation: u64) -> BoundPlan {
        BoundPlan {
            relation: RelationId(relation),
            src_var: "a".to_owned(),
            dst_var: "b".to_owned(),
            projection: ReturnProjection::Destination,
        }
    }

    #[test]
    fn same_plan_and_snapshot_have_equal_certificates() {
        let plan = plan(7);
        let first = certify(&plan, CommitSeq(11));
        let second = certify(&plan, CommitSeq(11));

        assert_eq!(first, second);
        let _: GqlPlanCertificate = first;
    }

    #[test]
    fn snapshot_sequence_changes_digest() {
        let plan = plan(7);

        assert_ne!(
            certify(&plan, CommitSeq(11)).digest,
            certify(&plan, CommitSeq(12)).digest
        );
    }

    #[test]
    fn relation_id_changes_digest() {
        assert_ne!(
            certify(&plan(7), CommitSeq(11)).digest,
            certify(&plan(8), CommitSeq(11)).digest
        );
    }

    #[test]
    fn return_projection_changes_digest() {
        let destination = plan(7);
        let mut source = destination.clone();
        source.projection = ReturnProjection::Source;

        assert_ne!(
            certify(&destination, CommitSeq(11)).digest,
            certify(&source, CommitSeq(11)).digest
        );
    }
}
