//! V7 root membership for immutable compressed adjacency runs.
//!
//! The base frame is embedded verbatim, so its keyed identity can bind a run's
//! source without a root/run identity cycle. It still owns every Tier-D member.
//! A successor must explicitly earn new attachments; ordinary V3–V6 publication
//! drops them. This profile never discards the base history or authorizes GC.

use super::{ROOT_MAGIC, RootError, RootFrame, decode_root_frame};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, ObjectId};

pub const ROOT_FORMAT_V7: u16 = 7;
pub const MAX_ROOT_RUNS: usize = 4096;
const HEADER: usize = 4 + 2 + 8 + 4;
const REF_BYTES: usize = 8 + 32 + 8;
const DIGEST_BYTES: usize = 32;
pub(crate) const MAX_ENVELOPE_BYTES: usize = HEADER + MAX_ROOT_RUNS * REF_BYTES + DIGEST_BYTES;
const DOMAIN: &[u8] = b"fgdb.strata.root-runs.v7";

/// One registered run, usable only under this root's exact embedded lineage.
/// Relations are strictly ascending and occur at most once in a V7 frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdjRunRef {
    pub relation: RelationId,
    pub run: ObjectId,
    pub floor: CommitSeq,
}

/// A borrowed base frame plus the authenticated run membership it accompanies.
/// Decoding checks structure; callers must first verify the enclosing root ID.
#[derive(Debug)]
pub struct RootRuns<'a> {
    pub base_frame: &'a [u8],
    pub runs: Vec<AdjRunRef>,
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(DOMAIN);
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

fn validate_refs(runs: &[AdjRunRef], publication: CommitSeq) -> Result<(), RootError> {
    if runs.is_empty() || runs.len() > MAX_ROOT_RUNS {
        return Err(RootError::InvalidRunRoot);
    }
    let mut previous = None;
    for run in runs {
        if previous.is_some_and(|relation| relation >= run.relation) || run.floor > publication {
            return Err(RootError::InvalidRunRoot);
        }
        previous = Some(run.relation);
    }
    Ok(())
}

/// Encode exact base membership and a canonical registered-run inventory.
/// A V7 input is refused rather than recursively retaining obsolete roots.
pub fn encode_root_runs(base_frame: &[u8], runs: &[AdjRunRef]) -> Result<Vec<u8>, RootError> {
    let publication = base_publication(base_frame)?;
    validate_refs(runs, publication)?;
    let length = HEADER
        .checked_add(base_frame.len())
        .and_then(|n| n.checked_add(runs.len() * REF_BYTES + DIGEST_BYTES))
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
    bytes.extend_from_slice(&digest(&bytes));
    Ok(bytes)
}

/// Recognize V7 without changing the interpretation of earlier root frames.
/// Every declared length is bounded and checked before allocation or slicing.
pub fn decode_root_runs(bytes: &[u8]) -> Result<Option<RootRuns<'_>>, RootError> {
    if bytes.len() < 6 || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    if u16::from_be_bytes([bytes[4], bytes[5]]) != ROOT_FORMAT_V7 {
        return Ok(None);
    }
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
        .checked_add(count * REF_BYTES + DIGEST_BYTES)
        .ok_or(RootError::InvalidRunRoot)?;
    if bytes.len() != expected {
        return Err(RootError::InvalidRunRoot);
    }
    let payload_end = expected - DIGEST_BYTES;
    if digest(&bytes[..payload_end]) != bytes[payload_end..] {
        return Err(RootError::InvalidRunRoot);
    }
    let base_frame = &bytes[HEADER..base_end];
    let publication = base_publication(base_frame)?;
    let mut runs = Vec::new();
    runs.try_reserve_exact(count)
        .map_err(|_| RootError::InvalidRunRoot)?;
    for entry in bytes[base_end..payload_end].as_chunks::<REF_BYTES>().0 {
        runs.push(AdjRunRef {
            relation: RelationId(u64::from_be_bytes(entry[..8].try_into().unwrap())),
            run: ObjectId(entry[8..40].try_into().unwrap()),
            floor: CommitSeq(u64::from_be_bytes(entry[40..48].try_into().unwrap())),
        });
    }
    validate_refs(&runs, publication)?;
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
            vec![AdjRunRef {
                relation: RelationId(5),
                run: ObjectId([6; 32]),
                floor: CommitSeq(2),
            }],
        )
    }

    #[test]
    fn envelope_retains_exact_membership_and_binds_run_identity() {
        let (base, runs) = fixture();
        let bytes = encode_root_runs(&base, &runs).unwrap();
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
}
