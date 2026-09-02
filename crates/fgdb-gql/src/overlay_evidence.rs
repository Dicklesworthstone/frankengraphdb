use fgdb_crypto::{Digest, Hasher};
use fgdb_types::{CommitSeq, VId};

const OVERLAY_RESULT_DOMAIN_V1: &[u8] = b"fgdb:gql-staged-overlay-result:v1";

/// Exact ordered-result evidence for a query evaluated over one staged overlay.
///
/// The certificate binds an external plan digest, an external canonical staged-
/// effect digest, the transaction basis, exact row count, row order, and every
/// returned [`VId`]. The composition layer owns construction of the plan and
/// staged-effect identities; this type owns only their domain-separated result
/// transcript.
///
/// It is deliberately not a portable replay artifact. The certificate carries
/// identities, not the staged effect bytes or a database snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlOverlayResultCertificate {
    pub basis: CommitSeq,
    pub plan_digest: Digest,
    pub staged_effect_digest: Digest,
    pub row_count: u64,
    pub result_digest: Digest,
}

impl GqlOverlayResultCertificate {
    /// Certify one exact ordered result under the supplied plan and staged-
    /// effect identities.
    #[must_use]
    pub fn new(
        basis: CommitSeq,
        plan_digest: Digest,
        staged_effect_digest: Digest,
        rows: &[VId],
    ) -> Self {
        let row_count = count_as_u64(rows.len());
        let result_digest =
            digest_result(basis, plan_digest, staged_effect_digest, row_count, rows);
        Self {
            basis,
            plan_digest,
            staged_effect_digest,
            row_count,
            result_digest,
        }
    }

    /// Verify every bound input and every ordered result row.
    ///
    /// Digest comparisons use constant work over all digest bytes. Scalar
    /// metadata comparisons are public transcript metadata, not secrets.
    #[must_use]
    pub fn verifies(
        &self,
        basis: CommitSeq,
        plan_digest: Digest,
        staged_effect_digest: Digest,
        rows: &[VId],
    ) -> bool {
        let row_count = count_as_u64(rows.len());
        let expected = digest_result(basis, plan_digest, staged_effect_digest, row_count, rows);
        self.basis == basis
            && self.row_count == row_count
            && digest_eq(self.plan_digest, plan_digest)
            && digest_eq(self.staged_effect_digest, staged_effect_digest)
            && digest_eq(self.result_digest, expected)
    }
}

fn digest_result(
    basis: CommitSeq,
    plan_digest: Digest,
    staged_effect_digest: Digest,
    row_count: u64,
    rows: &[VId],
) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(OVERLAY_RESULT_DOMAIN_V1);
    hasher.update(&basis.0.to_be_bytes());
    hasher.update(&plan_digest.0);
    hasher.update(&staged_effect_digest.0);
    hasher.update(&row_count.to_be_bytes());
    for row in rows {
        hasher.update(&row.0.to_be_bytes());
    }
    hasher.finalize()
}

fn count_as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
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

#[cfg(test)]
mod tests {
    use super::GqlOverlayResultCertificate;
    use fgdb_crypto::Digest;
    use fgdb_types::{CommitSeq, VId};

    fn digest(byte: u8) -> Digest {
        Digest([byte; 32])
    }

    #[test]
    fn certificate_binds_basis_plan_overlay_count_order_and_rows() {
        let rows = [VId(2), VId(9)];
        let certificate =
            GqlOverlayResultCertificate::new(CommitSeq(7), digest(0x11), digest(0x22), &rows);

        assert!(certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &rows));
        assert!(!certificate.verifies(CommitSeq(8), digest(0x11), digest(0x22), &rows));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x12), digest(0x22), &rows));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x11), digest(0x23), &rows));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &[VId(9), VId(2)]));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &[VId(2)]));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &[VId(2), VId(8)]));
    }

    #[test]
    fn empty_result_is_stable_and_distinct_from_one_row() {
        let certificate =
            GqlOverlayResultCertificate::new(CommitSeq(7), digest(0x11), digest(0x22), &[]);

        assert!(certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &[]));
        assert!(!certificate.verifies(CommitSeq(7), digest(0x11), digest(0x22), &[VId(0)]));
    }
}
