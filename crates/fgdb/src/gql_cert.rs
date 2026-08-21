use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_gql::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
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
    match plan.relation {
        None => {
            hasher.update(&[0]);
        }
        Some(relation) => {
            hasher.update(&[1]);
            hasher.update(&relation.0.to_be_bytes());
        }
    }
    update_string(&mut hasher, &plan.src_var);
    update_string(&mut hasher, &plan.dst_var);
    update_string(&mut hasher, &plan.via_var);
    match plan.hop2_relation {
        None => {
            hasher.update(&[0]);
        }
        Some(relation) => {
            hasher.update(&[1]);
            hasher.update(&relation.0.to_be_bytes());
        }
    }
    match &plan.hop2_dst_var {
        None => {
            hasher.update(&[0]);
        }
        Some(dst_var) => {
            hasher.update(&[1]);
            update_string(&mut hasher, dst_var);
        }
    }
    hasher.update(&[projection_tag(plan.projection)]);
    hasher.update(&[direction_tag(plan.direction)]);
    match plan.src_label {
        None => {
            hasher.update(&[0]);
        }
        Some(label) => {
            hasher.update(&[1]);
            hasher.update(&label.0.to_be_bytes());
        }
    }
    match &plan.eq {
        None => {
            hasher.update(&[0]);
        }
        Some((left, right)) => {
            hasher.update(&[1]);
            update_string(&mut hasher, left);
            update_string(&mut hasher, right);
        }
    }
    match plan.src_prop {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.src_prop_ne {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.src_prop_gt {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.src_prop_lt {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.src_prop_ge {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.src_prop_le {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop_ne {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop_gt {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop_lt {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop_ge {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.dst_prop_le {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.hop2_dst_prop {
        None => {
            hasher.update(&[0]);
        }
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
            hasher.update(&value.to_be_bytes());
        }
    }
    match plan.limit {
        None => {
            hasher.update(&[0]);
        }
        Some(limit) => {
            hasher.update(&[1]);
            hasher.update(&limit.to_be_bytes());
        }
    }
    match plan.skip {
        None => {
            hasher.update(&[0]);
        }
        Some(skip) => {
            hasher.update(&[1]);
            hasher.update(&skip.to_be_bytes());
        }
    }
    hasher.update(&snapshot_seq.0.to_be_bytes());

    GqlPlanCertificate {
        digest: hasher.finalize(),
        snapshot_seq,
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
    let canonical = bind.canonical_bytes();
    hash(&canonical)
}

#[cfg(test)]
mod tests {
    use super::{GqlPlanCertificate, certify, direction_tag, projection_tag};
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{BoundPlan, EdgeDirection, ReturnProjection};
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

    #[test]
    fn return_projection_tags_are_stable() {
        assert_eq!(projection_tag(ReturnProjection::Destination), 0);
        assert_eq!(projection_tag(ReturnProjection::Source), 1);
        assert_eq!(projection_tag(ReturnProjection::Hop2Destination), 2);
    }

    #[test]
    fn direction_tags_are_stable_and_change_the_digest() {
        assert_eq!(direction_tag(EdgeDirection::Outgoing), 0);
        assert_eq!(direction_tag(EdgeDirection::Incoming), 1);
        assert_eq!(direction_tag(EdgeDirection::Undirected), 2);

        let outgoing = plan(7);
        let mut incoming = outgoing.clone();
        incoming.direction = EdgeDirection::Incoming;
        let mut undirected = outgoing.clone();
        undirected.direction = EdgeDirection::Undirected;

        assert_ne!(
            certify(&outgoing, CommitSeq(11)).digest,
            certify(&incoming, CommitSeq(11)).digest
        );
        assert_ne!(
            certify(&outgoing, CommitSeq(11)).digest,
            certify(&undirected, CommitSeq(11)).digest
        );
    }

    #[test]
    fn second_hop_changes_digest() {
        let one_hop = plan(7);
        let mut two_hop = one_hop.clone();
        two_hop.via_var = "b".to_owned();
        two_hop.hop2_relation = Some(RelationId(8));
        two_hop.hop2_dst_var = Some("c".to_owned());

        assert_ne!(
            certify(&one_hop, CommitSeq(11)).digest,
            certify(&two_hop, CommitSeq(11)).digest
        );
    }

    #[test]
    fn hop2_destination_property_changes_digest() {
        let unfiltered = plan(7);
        let mut filtered = unfiltered.clone();
        filtered.hop2_dst_prop = Some((PropertyKeyId(9), 1));

        assert_ne!(
            certify(&unfiltered, CommitSeq(11)).digest,
            certify(&filtered, CommitSeq(11)).digest
        );
    }

    #[test]
    fn second_hop_projection_changes_digest() {
        let destination = plan(7);
        let mut hop2_destination = destination.clone();
        hop2_destination.projection = ReturnProjection::Hop2Destination;

        assert_ne!(
            certify(&destination, CommitSeq(11)).digest,
            certify(&hop2_destination, CommitSeq(11)).digest
        );
    }
}
