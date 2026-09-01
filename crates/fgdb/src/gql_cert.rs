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
    match plan.dst_label {
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
    match plan.neq {
        None => {
            hasher.update(&[0]);
        }
        Some((ref left, ref right)) => {
            hasher.update(&[1]);
            update_string(&mut hasher, left);
            update_string(&mut hasher, right);
        }
    }
    update_prop(&mut hasher, plan.src_prop);
    update_prop(&mut hasher, plan.src_prop_ne);
    update_prop(&mut hasher, plan.src_prop_gt);
    update_prop(&mut hasher, plan.src_prop_lt);
    update_prop(&mut hasher, plan.src_prop_ge);
    update_prop(&mut hasher, plan.src_prop_le);
    update_prop(&mut hasher, plan.dst_prop);
    update_prop(&mut hasher, plan.dst_prop_ne);
    update_prop(&mut hasher, plan.dst_prop_gt);
    update_prop(&mut hasher, plan.dst_prop_lt);
    update_prop(&mut hasher, plan.dst_prop_ge);
    update_prop(&mut hasher, plan.dst_prop_le);
    update_prop(&mut hasher, plan.hop2_dst_prop);
    update_prop(&mut hasher, plan.hop2_dst_prop_ne);
    update_prop(&mut hasher, plan.hop2_dst_prop_gt);
    update_prop(&mut hasher, plan.hop2_dst_prop_lt);
    update_prop(&mut hasher, plan.hop2_dst_prop_ge);
    update_prop(&mut hasher, plan.hop2_dst_prop_le);
    match plan.limit {
        None => hasher.update(&[0]),
        Some(limit) => {
            hasher.update(&[1]);
            hasher.update(&limit.to_be_bytes());
        }
    }
    match plan.skip {
        None => hasher.update(&[0]),
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

fn update_prop(hasher: &mut Hasher, prop: Option<(fgdb_delta_types::PropertyKeyId, i64)>) {
    match prop {
        None => hasher.update(&[0]),
        Some((key, value)) => {
            hasher.update(&[1]);
            hasher.update(&key.0.to_be_bytes());
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
    let canonical = bind.canonical_bytes();
    hash(&canonical)
}
