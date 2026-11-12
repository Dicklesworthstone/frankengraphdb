//! V7/V8 root membership for immutable compressed adjacency runs.
//!
//! The base frame is embedded verbatim, so its keyed identity can bind a run's
//! source without a root/run identity cycle. It still owns every Tier-D member.
//! V7 attaches runs sealed against this exact base root and publication; V8
//! carries previously sealed runs across subsequent commits, recording each
//! run's original `source_root`, `publication`, and contiguous `verified_through`
//! horizon. Neither profile discards base history or authorizes GC.

use super::{ROOT_MAGIC, RootError, RootFrame, decode_root_frame};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, ObjectId};

pub const ROOT_FORMAT_V7: u16 = 7;
pub const ROOT_FORMAT_V8: u16 = 8;
pub const MAX_ROOT_RUNS: usize = 4096;
const HEADER: usize = 4 + 2 + 8 + 4;
const REF_BYTES_V7: usize = 8 + 32 + 8;
const REF_BYTES_V8: usize = 8 + 32 + 32 + 8 + 8 + 8;
const DIGEST_BYTES: usize = 32;
pub(crate) const MAX_ENVELOPE_BYTES: usize = HEADER + MAX_ROOT_RUNS * REF_BYTES_V8 + DIGEST_BYTES;
const DOMAIN_V7: &[u8] = b"fgdb.strata.root-runs.v7";
const DOMAIN_V8: &[u8] = b"fgdb.strata.root-runs.v8";

pub const fn is_run_root_format(format: u16) -> bool {
    format == ROOT_FORMAT_V7 || format == ROOT_FORMAT_V8
}

/// One registered run, usable only under an authenticated root envelope.
/// Relations are strictly ascending and occur at most once in a V7/V8 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjRunRef {
    pub relation: RelationId,
    pub run: ObjectId,
    pub floor: CommitSeq,
    pub source_root: Option<ObjectId>,
    pub publication: Option<CommitSeq>,
    pub verified_through: Option<CommitSeq>,
}

impl AdjRunRef {
    pub const fn v7(relation: RelationId, run: ObjectId, floor: CommitSeq) -> Self {
        Self {
            relation,
            run,
            floor,
            source_root: None,
            publication: None,
            verified_through: None,
        }
    }

    pub const fn carried(
        relation: RelationId,
        run: ObjectId,
        source_root: ObjectId,
        floor: CommitSeq,
        publication: CommitSeq,
        verified_through: CommitSeq,
    ) -> Self {
        Self {
            relation,
            run,
            floor,
            source_root: Some(source_root),
            publication: Some(publication),
            verified_through: Some(verified_through),
        }
    }

    pub const fn is_v7_native(&self) -> bool {
        self.source_root.is_none() && self.publication.is_none() && self.verified_through.is_none()
    }

    pub const fn resolved_source_root(&self, base_root: ObjectId) -> ObjectId {
        match self.source_root {
            Some(source_root) => source_root,
            None => base_root,
        }
    }

    pub const fn resolved_publication(&self, base_publication: CommitSeq) -> CommitSeq {
        match self.publication {
            Some(publication) => publication,
            None => base_publication,
        }
    }

    pub const fn resolved_verified_through(&self, base_publication: CommitSeq) -> CommitSeq {
        match self.verified_through {
            Some(verified_through) => verified_through,
            None => self.resolved_publication(base_publication),
        }
    }
}

/// A borrowed base frame plus the authenticated run membership it accompanies.
/// Decoding checks structure; callers must first verify the enclosing root ID.
#[derive(Debug)]
pub struct RootRuns<'a> {
    pub base_frame: &'a [u8],
    pub runs: Vec<AdjRunRef>,
}

fn digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(domain);
    hash.update(bytes);
    hash.finalize().0
}

fn base_publication(bytes: &[u8]) -> Result<CommitSeq, RootError> {
    if bytes.len() < 6 || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::InvalidRunRoot);
    }
    if !(3..=6).contains(&u16::from_be_bytes([bytes[4], bytes[5]])) {
        return Err(RootError::InvalidRunRoot);
    }
    Ok(match decode_root_frame(bytes)? {
        RootFrame::V3(root) => root.published_at,
        RootFrame::V4(frame) => frame.published_at,
        RootFrame::V5(frame) => frame.published_at,
        RootFrame::V6(frame) => frame.membership.published_at,
    })
}

fn validate_refs_v7(runs: &[AdjRunRef], publication: CommitSeq) -> Result<(), RootError> {
    if runs.is_empty() || runs.len() > MAX_ROOT_RUNS {
        return Err(RootError::InvalidRunRoot);
    }
    let mut previous = None;
    for run in runs {
        if !run.is_v7_native()
            || previous.is_some_and(|relation| relation >= run.relation)
            || run.floor > publication
        {
            return Err(RootError::InvalidRunRoot);
        }
        previous = Some(run.relation);
    }
    Ok(())
}

fn resolve_v8_entry(
    run: &AdjRunRef,
    base_root: Option<ObjectId>,
    base_pub: CommitSeq,
) -> Result<(ObjectId, CommitSeq, CommitSeq), RootError> {
    let (src_root, pub_seq, ver_seq) = if run.is_v7_native() {
        let base_root = base_root.ok_or(RootError::InvalidRunRoot)?;
        (base_root, base_pub, base_pub)
    } else {
        match (run.source_root, run.publication, run.verified_through) {
            (Some(src_root), Some(pub_seq), Some(ver_seq)) => (src_root, pub_seq, ver_seq),
            _ => return Err(RootError::InvalidRunRoot),
        }
    };
    if run.floor > pub_seq || pub_seq > ver_seq || ver_seq > base_pub {
        return Err(RootError::InvalidRunRoot);
    }
    Ok((src_root, pub_seq, ver_seq))
}

/// Encode exact base membership and a canonical registered-run inventory.
/// Emits V7 when every attachment belongs natively to `base_frame`, and V8 when
/// any attachment carries prior-lineage provenance across post-seal commits.
pub fn encode_root_runs_with_source(
    base_frame: &[u8],
    base_root_id: ObjectId,
    runs: &[AdjRunRef],
) -> Result<Vec<u8>, RootError> {
    encode_root_runs_inner(base_frame, Some(base_root_id), runs)
}

/// Encode exact base membership and a canonical registered-run inventory.
/// A V7/V8 input is refused rather than recursively retaining obsolete roots.
pub fn encode_root_runs(base_frame: &[u8], runs: &[AdjRunRef]) -> Result<Vec<u8>, RootError> {
    encode_root_runs_inner(base_frame, None, runs)
}

fn encode_root_runs_inner(
    base_frame: &[u8],
    base_root_id: Option<ObjectId>,
    runs: &[AdjRunRef],
) -> Result<Vec<u8>, RootError> {
    let publication = base_publication(base_frame)?;
    if runs.is_empty() || runs.len() > MAX_ROOT_RUNS {
        return Err(RootError::InvalidRunRoot);
    }
    if runs.iter().all(AdjRunRef::is_v7_native) {
        validate_refs_v7(runs, publication)?;
        let length = HEADER
            .checked_add(base_frame.len())
            .and_then(|n| n.checked_add(runs.len() * REF_BYTES_V7 + DIGEST_BYTES))
            .filter(|n| *n <= super::MAX_ENCODED_ROOT_BYTES)
            .ok_or(RootError::InvalidRunRoot)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| RootError::InvalidRunRoot)?;
        bytes.extend_from_slice(&ROOT_MAGIC);
        bytes.extend_from_slice(&ROOT_FORMAT_V7.to_be_bytes());
        bytes.extend_from_slice(&(base_frame.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&(runs.len() as u32).to_be_bytes());
        bytes.extend_from_slice(base_frame);
        for run in runs {
            bytes.extend_from_slice(&run.relation.0.to_be_bytes());
            bytes.extend_from_slice(&run.run.0);
            bytes.extend_from_slice(&run.floor.0.to_be_bytes());
        }
        bytes.extend_from_slice(&digest(DOMAIN_V7, &bytes));
        return Ok(bytes);
    }

    let length = HEADER
        .checked_add(base_frame.len())
        .and_then(|n| n.checked_add(runs.len() * REF_BYTES_V8 + DIGEST_BYTES))
        .filter(|n| *n <= super::MAX_ENCODED_ROOT_BYTES)
        .ok_or(RootError::InvalidRunRoot)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| RootError::InvalidRunRoot)?;
    bytes.extend_from_slice(&ROOT_MAGIC);
    bytes.extend_from_slice(&ROOT_FORMAT_V8.to_be_bytes());
    bytes.extend_from_slice(&(base_frame.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&(runs.len() as u32).to_be_bytes());
    bytes.extend_from_slice(base_frame);
    let mut previous = None;
    for run in runs {
        if previous.is_some_and(|relation| relation >= run.relation) {
            return Err(RootError::InvalidRunRoot);
        }
        previous = Some(run.relation);
        let (src_root, pub_seq, ver_seq) = resolve_v8_entry(run, base_root_id, publication)?;
        bytes.extend_from_slice(&run.relation.0.to_be_bytes());
        bytes.extend_from_slice(&run.run.0);
        bytes.extend_from_slice(&src_root.0);
        bytes.extend_from_slice(&run.floor.0.to_be_bytes());
        bytes.extend_from_slice(&pub_seq.0.to_be_bytes());
        bytes.extend_from_slice(&ver_seq.0.to_be_bytes());
    }
    bytes.extend_from_slice(&digest(DOMAIN_V8, &bytes));
    Ok(bytes)
}

/// Recognize V7 and V8 without changing the interpretation of earlier root frames.
/// Every declared length is bounded and checked before allocation or slicing.
pub fn decode_root_runs(bytes: &[u8]) -> Result<Option<RootRuns<'_>>, RootError> {
    if bytes.len() < 6 || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    let (ref_bytes, domain) = match format {
        ROOT_FORMAT_V7 => (REF_BYTES_V7, DOMAIN_V7),
        ROOT_FORMAT_V8 => (REF_BYTES_V8, DOMAIN_V8),
        _ => return Ok(None),
    };
    if bytes.len() < HEADER + DIGEST_BYTES || bytes.len() > super::MAX_ENCODED_ROOT_BYTES {
        return Err(RootError::InvalidRunRoot);
    }
    let base_len = usize::try_from(u64::from_be_bytes(bytes[6..14].try_into().unwrap()))
        .map_err(|_| RootError::InvalidRunRoot)?;
    let count = u32::from_be_bytes(bytes[14..18].try_into().unwrap()) as usize;
    if count == 0 || count > MAX_ROOT_RUNS {
        return Err(RootError::InvalidRunRoot);
    }
    let base_end = HEADER
        .checked_add(base_len)
        .ok_or(RootError::InvalidRunRoot)?;
    let expected = base_end
        .checked_add(count * ref_bytes + DIGEST_BYTES)
        .ok_or(RootError::InvalidRunRoot)?;
    if bytes.len() != expected {
        return Err(RootError::InvalidRunRoot);
    }
    let payload_end = expected - DIGEST_BYTES;
    if digest(domain, &bytes[..payload_end]) != bytes[payload_end..] {
        return Err(RootError::InvalidRunRoot);
    }
    let base_frame = &bytes[HEADER..base_end];
    let publication = base_publication(base_frame)?;
    let mut runs = Vec::new();
    runs.try_reserve_exact(count)
        .map_err(|_| RootError::InvalidRunRoot)?;
    if format == ROOT_FORMAT_V7 {
        for entry in bytes[base_end..payload_end].as_chunks::<REF_BYTES_V7>().0 {
            runs.push(AdjRunRef::v7(
                RelationId(u64::from_be_bytes(entry[..8].try_into().unwrap())),
                ObjectId(entry[8..40].try_into().unwrap()),
                CommitSeq(u64::from_be_bytes(entry[40..48].try_into().unwrap())),
            ));
        }
        validate_refs_v7(&runs, publication)?;
    } else {
        let mut previous = None;
        for entry in bytes[base_end..payload_end].as_chunks::<REF_BYTES_V8>().0 {
            let reference = AdjRunRef::carried(
                RelationId(u64::from_be_bytes(entry[..8].try_into().unwrap())),
                ObjectId(entry[8..40].try_into().unwrap()),
                ObjectId(entry[40..72].try_into().unwrap()),
                CommitSeq(u64::from_be_bytes(entry[72..80].try_into().unwrap())),
                CommitSeq(u64::from_be_bytes(entry[80..88].try_into().unwrap())),
                CommitSeq(u64::from_be_bytes(entry[88..96].try_into().unwrap())),
            );
            if previous.is_some_and(|relation| relation >= reference.relation) {
                return Err(RootError::InvalidRunRoot);
            }
            previous = Some(reference.relation);
            resolve_v8_entry(&reference, None, publication)?;
            runs.push(reference);
        }
    }
    Ok(Some(RootRuns { base_frame, runs }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::{PartitionRoot, encode_root};
    use fgdb_types::{BranchId, GraphId};

    fn fixture() -> (Vec<u8>, Vec<AdjRunRef>) {
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(2),
            partition: 3,
            published_at: CommitSeq(4),
            blocks: Vec::new(),
            vertex_patches: Vec::new(),
        };
        (
            encode_root(&root).unwrap(),
            vec![AdjRunRef::v7(
                RelationId(5),
                ObjectId([6; 32]),
                CommitSeq(2),
            )],
        )
    }

    #[test]
    fn envelope_retains_exact_membership_and_binds_run_identity() {
        let (base, runs) = fixture();
        let bytes = encode_root_runs(&base, &runs).unwrap();
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), ROOT_FORMAT_V7);
        let decoded = decode_root_runs(&bytes).unwrap().unwrap();
        assert_eq!(decoded.base_frame, base);
        assert_eq!(decoded.runs, runs);
        assert_eq!(
            decode_root_frame(&bytes).unwrap(),
            decode_root_frame(&base).unwrap()
        );
        let mut corrupted = bytes.clone();
        corrupted[HEADER + base.len() + 8] ^= 1;
        assert!(decode_root_runs(&corrupted).is_err());
        for end in [0, 5, HEADER, bytes.len() - 1] {
            assert!(decode_root_runs(&bytes[..end]).is_err());
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_root_runs(&trailing).is_err());
    }

    #[test]
    fn noncanonical_duplicate_future_and_nested_attachments_refuse() {
        let (base, runs) = fixture();
        assert!(decode_root_runs(&base).unwrap().is_none());
        assert!(encode_root_runs(&base, &[]).is_err());
        assert!(encode_root_runs(&base, &[runs[0], runs[0]]).is_err());
        let mut future = runs[0];
        future.floor = CommitSeq(5);
        assert!(encode_root_runs(&base, &[future]).is_err());
        let envelope = encode_root_runs(&base, &runs).unwrap();
        assert!(encode_root_runs(&envelope, &runs).is_err());
        let mut forged_length = envelope;
        forged_length[6..14].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(decode_root_runs(&forged_length).is_err());
    }

    #[test]
    fn v8_carried_envelope_roundtrips_and_enforces_monotonic_lineage_bounds() {
        let (base, _) = fixture();
        let carried = vec![
            AdjRunRef::carried(
                RelationId(5),
                ObjectId([6; 32]),
                ObjectId([0xaa; 32]),
                CommitSeq(1),
                CommitSeq(3),
                CommitSeq(4),
            ),
            AdjRunRef::v7(RelationId(9), ObjectId([7; 32]), CommitSeq(2)),
        ];
        // Mixing a V7-native ref with a carried V8 ref requires explicit base_root_id.
        assert!(encode_root_runs(&base, &carried).is_err());
        let base_root_id = ObjectId([0xbb; 32]);
        let bytes = encode_root_runs_with_source(&base, base_root_id, &carried).unwrap();
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), ROOT_FORMAT_V8);
        let decoded = decode_root_runs(&bytes).unwrap().unwrap();
        assert_eq!(decoded.base_frame, base);
        assert_eq!(
            decoded.runs,
            vec![
                carried[0],
                AdjRunRef::carried(
                    RelationId(9),
                    ObjectId([7; 32]),
                    base_root_id,
                    CommitSeq(2),
                    CommitSeq(4),
                    CommitSeq(4),
                ),
            ]
        );
        assert_eq!(
            decode_root_frame(&bytes).unwrap(),
            decode_root_frame(&base).unwrap()
        );
        // Nested V8 envelope must refuse just like V7.
        assert!(encode_root_runs_with_source(&bytes, base_root_id, &decoded.runs).is_err());
        // Violating floor <= publication <= verified_through <= base_publication must refuse.
        for bad in [
            AdjRunRef::carried(
                RelationId(5),
                ObjectId([6; 32]),
                ObjectId([0xaa; 32]),
                CommitSeq(4),
                CommitSeq(3),
                CommitSeq(4),
            ),
            AdjRunRef::carried(
                RelationId(5),
                ObjectId([6; 32]),
                ObjectId([0xaa; 32]),
                CommitSeq(1),
                CommitSeq(4),
                CommitSeq(3),
            ),
            AdjRunRef::carried(
                RelationId(5),
                ObjectId([6; 32]),
                ObjectId([0xaa; 32]),
                CommitSeq(1),
                CommitSeq(3),
                CommitSeq(5),
            ),
        ] {
            assert!(encode_root_runs(&base, &[bad]).is_err());
        }
    }
}
