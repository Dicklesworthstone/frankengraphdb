//! Canonical binary checkpoint codec (`FGRC` v1) for [`CommittedReachability`].
//!
//! Serializes the committed edge-identity arrangement (`CommittedEdgeInput`)
//! together with the materialized directed closure pairs `(VId, VId)` without
//! `serde` (`Doctrine #1, #7`). Decoding validates coordinate bindings,
//! strictly ordered keys, positive edge support, and closure boundary
//! consistency under caller `ZSetEvent` admission before returning both the
//! restored `CommittedReachability` and its canonical `ZSet<(VId, VId)>` rows.

use std::collections::{BTreeMap, BTreeSet};

use fgdb_types::ids::ObjectId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, MarkerRef, VId};

use super::{CommittedReachability, CommittedReachabilityError};
use crate::zset::committed::{Anchor, CommittedEdgeInput, EdgeTuple};
use crate::zset::event;
use crate::zset::reachability::IncrementalReachability;
use crate::{LimbLimit, RelationId, SchemaEpoch, ZSet, ZSetError, ZSetEvent, ZWeight};

const MAGIC: [u8; 4] = *b"FGRC";
const VERSION_V1: u16 = 1;

pub type ReachabilityClosure = ZSet<(VId, VId)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachabilityCheckpointEncodeError<E> {
    Control(ZSetError<E>),
    SizeOverflow,
    /// Counting stops as soon as admission fails, so `required` may be a
    /// lower bound on the complete encoded length.
    ByteLimit {
        required: usize,
        limit: usize,
    },
    AllocationFailed {
        requested: usize,
    },
}

impl<E> From<ZSetError<E>> for ReachabilityCheckpointEncodeError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Control(error)
    }
}

impl<E: core::fmt::Display> core::fmt::Display for ReachabilityCheckpointEncodeError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::SizeOverflow => f.write_str("reachability checkpoint byte count overflow"),
            Self::ByteLimit { required, limit } => write!(
                f,
                "reachability checkpoint requires at least {required} bytes, limit {limit}"
            ),
            Self::AllocationFailed { requested } => write!(
                f,
                "allocator refused {requested} reachability checkpoint bytes"
            ),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for ReachabilityCheckpointEncodeError<E> {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachabilityCheckpointError<E> {
    Malformed(&'static str),
    CoordinateMismatch,
    Reachability(CommittedReachabilityError<E>),
}

impl<E> From<CommittedReachabilityError<E>> for ReachabilityCheckpointError<E> {
    fn from(error: CommittedReachabilityError<E>) -> Self {
        Self::Reachability(error)
    }
}

impl<E> From<ZSetError<E>> for ReachabilityCheckpointError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Reachability(error.into())
    }
}

impl<E: core::fmt::Display> core::fmt::Display for ReachabilityCheckpointError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed reachability checkpoint: {why}"),
            Self::CoordinateMismatch => f.write_str("reachability checkpoint coordinate mismatch"),
            Self::Reachability(error) => error.fmt(f),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for ReachabilityCheckpointError<E> {}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take<E>(&mut self, len: usize) -> Result<&'a [u8], ReachabilityCheckpointError<E>> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ReachabilityCheckpointError::Malformed("truncated payload"))?;
        if end > self.bytes.len() {
            return Err(ReachabilityCheckpointError::Malformed("truncated payload"));
        }
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8<E>(&mut self) -> Result<u8, ReachabilityCheckpointError<E>> {
        Ok(self.take(1)?[0])
    }

    fn u16<E>(&mut self) -> Result<u16, ReachabilityCheckpointError<E>> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u64<E>(&mut self) -> Result<u64, ReachabilityCheckpointError<E>> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    fn usize_checked<E>(&mut self) -> Result<usize, ReachabilityCheckpointError<E>> {
        usize::try_from(self.u64()?)
            .map_err(|_| ReachabilityCheckpointError::Malformed("usize count overflow"))
    }

    fn u128<E>(&mut self) -> Result<u128, ReachabilityCheckpointError<E>> {
        let b = self.take(16)?;
        let mut arr = [0u8; 16];
        arr.copy_from_slice(b);
        Ok(u128::from_le_bytes(arr))
    }

    fn bytes32<E>(&mut self) -> Result<[u8; 32], ReachabilityCheckpointError<E>> {
        let b = self.take(32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(b);
        Ok(arr)
    }

    fn finish<E>(self) -> Result<(), ReachabilityCheckpointError<E>> {
        if self.pos != self.bytes.len() {
            return Err(ReachabilityCheckpointError::Malformed("trailing bytes"));
        }
        Ok(())
    }
}

impl CommittedReachability {
    /// Encode this committed reachability arrangement and closure into canonical
    /// `FGRC` v1 checkpoint bytes under caller `ZSetEvent` work accounting.
    pub fn encode_checkpoint<E>(
        &self,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Vec<u8>, ReachabilityCheckpointEncodeError<E>> {
        self.encode_checkpoint_bounded(usize::MAX, control)
    }

    /// Admit the exact fixed-width format length before allocating output.
    /// Counting visits source rows under cancellation/work control but does
    /// not copy edges, closure pairs or schema epochs. The source arrangement
    /// remains unchanged on any refusal, including a late encoding failure.
    pub fn encode_checkpoint_bounded<E>(
        &self,
        max_bytes: usize,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Vec<u8>, ReachabilityCheckpointEncodeError<E>> {
        event(control, ZSetEvent::Work)?;
        // magic/version + graph/branch/relation + anchor tag + three counts.
        let mut required = 4 + 2 + 16 + 16 + 8 + 1 + 3 * 8;
        if self.input.anchor.is_some() {
            required += 8 + 32 + 32;
        }
        required = add_encoded_rows(required, self.input.epochs.len(), 16)?;
        required = add_encoded_rows(required, self.input.edges.len(), 56)?;
        admit_bytes(required, max_bytes)?;
        let mut pair_count = 0usize;
        for destinations in self.reachability.reachable.values() {
            event(control, ZSetEvent::Work)?;
            pair_count = pair_count
                .checked_add(destinations.len())
                .ok_or(ReachabilityCheckpointEncodeError::SizeOverflow)?;
            required = add_encoded_rows(required, destinations.len(), 32)?;
            admit_bytes(required, max_bytes)?;
        }
        event(control, ZSetEvent::Work)?;
        let mut out = Vec::new();
        out.try_reserve_exact(required).map_err(|_| {
            ReachabilityCheckpointEncodeError::AllocationFailed {
                requested: required,
            }
        })?;
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION_V1.to_le_bytes());
        out.extend_from_slice(&self.input.graph.0.to_le_bytes());
        out.extend_from_slice(&self.input.branch.0.to_le_bytes());
        out.extend_from_slice(&self.relation.0.to_le_bytes());

        match &self.input.anchor {
            Some(anchor) => {
                out.push(1);
                out.extend_from_slice(&anchor.marker.commit_seq.0.to_le_bytes());
                out.extend_from_slice(&anchor.marker.marker_oid.0);
                out.extend_from_slice(&anchor.template);
            }
            None => out.push(0),
        }

        out.extend_from_slice(&(self.input.epochs.len() as u64).to_le_bytes());
        for (&rel, &epoch) in &self.input.epochs {
            event(control, ZSetEvent::Work)?;
            out.extend_from_slice(&rel.0.to_le_bytes());
            out.extend_from_slice(&epoch.0.to_le_bytes());
        }

        out.extend_from_slice(&(self.input.edges.len() as u64).to_le_bytes());
        for (&eid, &(rel, src, dst)) in &self.input.edges {
            event(control, ZSetEvent::Work)?;
            out.extend_from_slice(&eid.0.to_le_bytes());
            out.extend_from_slice(&rel.0.to_le_bytes());
            out.extend_from_slice(&src.0.to_le_bytes());
            out.extend_from_slice(&dst.0.to_le_bytes());
        }

        out.extend_from_slice(&(pair_count as u64).to_le_bytes());
        for (src, dst) in self.pairs() {
            event(control, ZSetEvent::Work)?;
            out.extend_from_slice(&src.0.to_le_bytes());
            out.extend_from_slice(&dst.0.to_le_bytes());
        }

        event(control, ZSetEvent::Work)?;
        debug_assert_eq!(out.len(), required);
        Ok(out)
    }

    /// Rehydrate a `CommittedReachability` arrangement and its materialized
    /// `ZSet<(VId, VId)>` closure rows from `FGRC` v1 checkpoint bytes without
    /// scanning Strata blocks or re-running full transitive closure.
    pub fn decode_checkpoint<E>(
        bytes: &[u8],
        expected_graph: GraphId,
        expected_branch: BranchId,
        expected_relation: RelationId,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(Self, ReachabilityClosure), ReachabilityCheckpointError<E>> {
        event(control, ZSetEvent::Work)?;
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != MAGIC {
            return Err(ReachabilityCheckpointError::Malformed("bad magic"));
        }
        if cur.u16()? != VERSION_V1 {
            return Err(ReachabilityCheckpointError::Malformed(
                "unsupported version",
            ));
        }
        let graph = GraphId(cur.u128()?);
        let branch = BranchId(cur.u128()?);
        let relation = RelationId(cur.u64()?);
        if graph != expected_graph || branch != expected_branch || relation != expected_relation {
            return Err(ReachabilityCheckpointError::CoordinateMismatch);
        }

        let anchor = match cur.u8()? {
            0 => None,
            1 => {
                let commit_seq = CommitSeq(cur.u64()?);
                if commit_seq == CommitSeq::ORIGIN {
                    return Err(ReachabilityCheckpointError::Malformed(
                        "origin commit_seq with non-None anchor",
                    ));
                }
                let marker_oid = ObjectId(cur.bytes32()?);
                let template = cur.bytes32()?;
                Some(Anchor {
                    marker: MarkerRef {
                        marker_oid,
                        commit_seq,
                    },
                    template,
                })
            }
            _ => return Err(ReachabilityCheckpointError::Malformed("invalid anchor tag")),
        };

        let epoch_count = cur.usize_checked()?;
        let mut epochs = BTreeMap::new();
        let mut prev_rel: Option<RelationId> = None;
        for _ in 0..epoch_count {
            event(control, ZSetEvent::Work)?;
            event(control, ZSetEvent::ScratchEntry)?;
            let rel = RelationId(cur.u64()?);
            if prev_rel.is_some_and(|prev| rel <= prev) {
                return Err(ReachabilityCheckpointError::Malformed(
                    "unsorted schema epochs",
                ));
            }
            prev_rel = Some(rel);
            let epoch = SchemaEpoch(cur.u64()?);
            epochs.insert(rel, epoch);
        }

        let edge_count = cur.usize_checked()?;
        if anchor.is_none() && edge_count != 0 {
            return Err(ReachabilityCheckpointError::Malformed(
                "origin anchor cannot retain edges",
            ));
        }
        let mut input_edges: BTreeMap<EId, EdgeTuple> = BTreeMap::new();
        let mut selected_edges = ZSet::new();
        let mut outgoing: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
        let mut incoming: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
        let mut prev_eid: Option<EId> = None;
        for _ in 0..edge_count {
            event(control, ZSetEvent::Work)?;
            event(control, ZSetEvent::ScratchEntry)?;
            let eid = EId(cur.u128()?);
            if prev_eid.is_some_and(|prev| eid <= prev) {
                return Err(ReachabilityCheckpointError::Malformed("unsorted edge EIds"));
            }
            prev_eid = Some(eid);
            let rel = RelationId(cur.u64()?);
            if !epochs.contains_key(&rel) {
                return Err(ReachabilityCheckpointError::Malformed(
                    "edge relation missing from schema epochs",
                ));
            }
            let src = VId(cur.u128()?);
            let dst = VId(cur.u128()?);
            input_edges.insert(eid, (rel, src, dst));
            if rel == relation {
                selected_edges.accumulate((src, dst), ZWeight::ONE, limbs, control)?;
                if outgoing.entry(src).or_default().insert(dst) {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
                if incoming.entry(dst).or_default().insert(src) {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
            }
        }

        let pair_count = cur.usize_checked()?;
        if selected_edges.is_empty() && pair_count != 0 {
            return Err(ReachabilityCheckpointError::Malformed(
                "non-empty closure over empty selected relation",
            ));
        }
        let mut reachable: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
        let mut predecessors: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
        let mut rows = ZSet::new();
        let mut prev_pair: Option<(VId, VId)> = None;
        for _ in 0..pair_count {
            event(control, ZSetEvent::Work)?;
            let src = VId(cur.u128()?);
            let dst = VId(cur.u128()?);
            let pair = (src, dst);
            if prev_pair.is_some_and(|prev| pair <= prev) {
                return Err(ReachabilityCheckpointError::Malformed(
                    "unsorted closure pairs",
                ));
            }
            if !outgoing.contains_key(&src) || !incoming.contains_key(&dst) {
                return Err(ReachabilityCheckpointError::Malformed(
                    "closure pair endpoint has no incident selected edge",
                ));
            }
            prev_pair = Some(pair);
            event(control, ZSetEvent::ScratchEntry)?;
            reachable.entry(src).or_default().insert(dst);
            event(control, ZSetEvent::ScratchEntry)?;
            predecessors.entry(dst).or_default().insert(src);
            rows.accumulate(pair, ZWeight::ONE, limbs, control)?;
        }

        for (&src, dsts) in &outgoing {
            let Some(reach_row) = reachable.get(&src) else {
                return Err(ReachabilityCheckpointError::Malformed(
                    "direct edge missing from closure",
                ));
            };
            for dst in dsts {
                if !reach_row.contains(dst) {
                    return Err(ReachabilityCheckpointError::Malformed(
                        "direct edge missing from closure",
                    ));
                }
            }
        }

        cur.finish()?;
        let committed = Self {
            input: CommittedEdgeInput {
                graph,
                branch,
                anchor,
                edges: input_edges,
                epochs,
            },
            relation,
            reachability: IncrementalReachability {
                edges: selected_edges,
                outgoing,
                incoming,
                reachable,
                predecessors,
            },
        };
        Ok((committed, rows))
    }
}

fn add_encoded_rows<E>(
    bytes: usize,
    count: usize,
    width: usize,
) -> Result<usize, ReachabilityCheckpointEncodeError<E>> {
    count
        .checked_mul(width)
        .and_then(|rows| bytes.checked_add(rows))
        .ok_or(ReachabilityCheckpointEncodeError::SizeOverflow)
}

fn admit_bytes<E>(
    required: usize,
    limit: usize,
) -> Result<(), ReachabilityCheckpointEncodeError<E>> {
    if required > limit {
        Err(ReachabilityCheckpointEncodeError::ByteLimit { required, limit })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::*;

    fn fixture() -> CommittedReachability {
        let mut control = |_: ZSetEvent| Ok::<_, ()>(());
        let relation = RelationId(7);
        let mut edges = ZSet::new();
        let mut input_edges = BTreeMap::new();
        for (id, src, dst) in [(11, 1, 2), (12, 1, 2), (13, 2, 3), (14, 3, 1)] {
            edges
                .accumulate(
                    (VId(src), VId(dst)),
                    ZWeight::ONE,
                    LimbLimit::new(4),
                    &mut control,
                )
                .unwrap();
            input_edges.insert(EId(id), (relation, VId(src), VId(dst)));
        }
        let mut reachability = IncrementalReachability::new();
        reachability
            .prepare(&edges, LimbLimit::new(4), &mut control)
            .unwrap()
            .commit();
        CommittedReachability {
            input: CommittedEdgeInput {
                graph: GraphId(1),
                branch: BranchId(2),
                anchor: Some(Anchor {
                    marker: MarkerRef {
                        marker_oid: ObjectId([3; 32]),
                        commit_seq: CommitSeq(4),
                    },
                    template: [5; 32],
                }),
                edges: input_edges,
                epochs: BTreeMap::from([(relation, SchemaEpoch(1))]),
            },
            relation,
            reachability,
        }
    }

    #[test]
    fn origin_checkpoint_keeps_the_v1_golden_bytes() {
        let state = CommittedReachability::new(GraphId(1), BranchId(2), RelationId(7));
        let mut expected = b"FGRC\x01\x00".to_vec();
        expected.extend_from_slice(&1_u128.to_le_bytes());
        expected.extend_from_slice(&2_u128.to_le_bytes());
        expected.extend_from_slice(&7_u64.to_le_bytes());
        expected.push(0);
        expected.extend_from_slice(&[0; 24]);
        assert_eq!(expected.len(), 71);
        let bytes = state
            .encode_checkpoint_bounded(71, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(bytes, expected);
        assert!(bytes.capacity() <= 71);
        assert!(matches!(
            state.encode_checkpoint_bounded(70, &mut |_| Ok::<_, ()>(())),
            Err(ReachabilityCheckpointEncodeError::ByteLimit {
                required: 71,
                limit: 70
            })
        ));
    }

    #[test]
    fn exact_bound_covers_parallel_edges_cycles_and_every_closure_pair() {
        let state = fixture();
        let mut control = |_: ZSetEvent| Ok::<_, ()>(());
        let bytes = state.encode_checkpoint(&mut control).unwrap();
        // Fixed header/anchor, one epoch, four EIds, nine nonempty-path pairs.
        assert_eq!(bytes.len(), 143 + 16 + 4 * 56 + 9 * 32);
        for limit in 0..bytes.len() {
            assert!(
                matches!(state.encode_checkpoint_bounded(limit, &mut control), Err(ReachabilityCheckpointEncodeError::ByteLimit { limit: found, .. }) if found == limit)
            );
        }
        let exact = state
            .encode_checkpoint_bounded(bytes.len(), &mut control)
            .unwrap();
        assert_eq!(exact, bytes);
        assert!(exact.capacity() <= bytes.len());
        let (restored, rows) = CommittedReachability::decode_checkpoint(
            &exact,
            GraphId(1),
            BranchId(2),
            RelationId(7),
            LimbLimit::new(4),
            &mut control,
        )
        .unwrap();
        assert!(restored == state);
        assert_eq!(rows.len(), 9);
        assert_eq!(state.encode_checkpoint(&mut control).unwrap(), bytes);
    }

    #[test]
    fn every_counting_and_encoding_checkpoint_can_cancel_without_changing_state() {
        let state = fixture();
        let mut calls = 0;
        let bytes = state
            .encode_checkpoint(&mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert!(calls > 10);
        for at in 1..=calls {
            let mut visited = 0;
            let result = state.encode_checkpoint_bounded(bytes.len(), &mut |_| {
                visited += 1;
                if visited == at { Err(at) } else { Ok(()) }
            });
            assert_eq!(
                result,
                Err(ReachabilityCheckpointEncodeError::Control(
                    ZSetError::Control(at)
                ))
            );
            assert_eq!(visited, at);
        }
        assert_eq!(
            state.encode_checkpoint(&mut |_| Ok::<_, ()>(())).unwrap(),
            bytes
        );
    }

    #[test]
    fn checkpoint_size_arithmetic_refuses_overflow_without_allocating() {
        assert_eq!(
            add_encoded_rows::<()>(71, usize::MAX, 32),
            Err(ReachabilityCheckpointEncodeError::SizeOverflow)
        );
        assert_eq!(
            add_encoded_rows::<()>(usize::MAX, 1, 1),
            Err(ReachabilityCheckpointEncodeError::SizeOverflow)
        );
        assert_eq!(add_encoded_rows::<()>(71, 0, 32), Ok(71));
    }
}
