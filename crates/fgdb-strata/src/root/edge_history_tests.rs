//! The per-EId-chain edge history validator (fgdb-2n2a2) decides exactly as
//! the global `(eid, created_at)` map it replaced. Over random publication
//! histories (lawful chains, perturbed chains and noise), both give the same
//! verdict for every block, the same refusal, and the same collapsed history,
//! including the partial history a refused block leaves behind.

use super::{EdgeBirth, EdgeHistoryValidator, EdgeIdentityConflict, RootError};
use crate::{AdjacencyEntry, BlockError};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, VId};
use std::collections::BTreeMap;

/// The replaced validator, kept as the oracle: one global map keyed
/// `(eid, created_at)`, a range search to find an EId's chain, and range
/// searches for its predecessor and successor.
#[derive(Default)]
struct GlobalMapValidator {
    statements: BTreeMap<(EId, CommitSeq), AdjacencyEntry>,
    seen: usize,
}

impl GlobalMapValidator {
    fn observe_block(
        &mut self,
        block_at: usize,
        block: &[AdjacencyEntry],
    ) -> Result<(), RootError> {
        let declared = u32::try_from(block.len()).unwrap_or(u32::MAX);
        if declared > crate::MAX_BLOCK_ENTRIES {
            return Err(RootError::Block {
                at: block_at,
                error: BlockError::ImplausibleEntryCount { declared },
            });
        }
        let mut previous_key = None;
        for (entry_at, entry) in block.iter().enumerate() {
            crate::validate_entry(entry_at, entry).map_err(|error| RootError::Block {
                at: block_at,
                error,
            })?;
            let found_key = (
                entry.src,
                entry.relation,
                entry.dst,
                entry.eid,
                entry.created_at,
            );
            if previous_key.is_some_and(|previous| previous >= found_key) {
                return Err(RootError::Block {
                    at: block_at,
                    error: BlockError::NonCanonicalOrder { at: entry_at },
                });
            }
            previous_key = Some(found_key);
            self.seen += 1;
            let birth = |entry: &AdjacencyEntry| EdgeBirth {
                src: entry.src,
                relation: entry.relation,
                dst: entry.dst,
                created_at: entry.created_at,
            };
            let key = (entry.eid, entry.created_at);
            let chain_known = self
                .statements
                .range((entry.eid, CommitSeq(0))..=(entry.eid, CommitSeq(u64::MAX)))
                .next()
                .is_some();
            if chain_known && let Some(existing) = self.statements.get(&key) {
                if birth(existing) != birth(entry) {
                    return Err(RootError::EdgeIdentityMismatch {
                        eid: entry.eid,
                        conflict: Box::new(EdgeIdentityConflict {
                            expected: birth(existing),
                            found: birth(entry),
                        }),
                    });
                }
                if existing.retired_at.is_some() && entry.retired_at != existing.retired_at {
                    return Err(RootError::EdgeRetirementMismatch {
                        eid: entry.eid,
                        expected: existing.retired_at,
                        found: entry.retired_at,
                    });
                }
            } else if chain_known {
                let predecessor = self
                    .statements
                    .range(..key)
                    .next_back()
                    .filter(|((eid, _), _)| *eid == entry.eid)
                    .map(|(_, existing)| existing);
                if let Some(predecessor) = predecessor
                    && (predecessor.retired_at != Some(entry.created_at)
                        || (predecessor.src, predecessor.relation, predecessor.dst)
                            != (entry.src, entry.relation, entry.dst))
                {
                    return Err(RootError::EdgeIdentityMismatch {
                        eid: entry.eid,
                        conflict: Box::new(EdgeIdentityConflict {
                            expected: birth(predecessor),
                            found: birth(entry),
                        }),
                    });
                }
                let successor = self
                    .statements
                    .range((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
                    .next()
                    .filter(|((eid, _), _)| *eid == entry.eid)
                    .map(|(_, existing)| existing);
                if let Some(successor) = successor
                    && (entry.retired_at != Some(successor.created_at)
                        || (successor.src, successor.relation, successor.dst)
                            != (entry.src, entry.relation, entry.dst))
                {
                    return Err(RootError::EdgeIdentityMismatch {
                        eid: entry.eid,
                        conflict: Box::new(EdgeIdentityConflict {
                            expected: birth(entry),
                            found: birth(successor),
                        }),
                    });
                }
            }
            self.statements.insert(key, *entry);
        }
        Ok(())
    }
}

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn entry(eid: u64, src: u64, dst: u64, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(u128::from(src)),
        relation: RelationId(1),
        dst: VId(u128::from(dst)),
        eid: EId(u128::from(eid)),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

/// One publication history: lawful per-EId chains, restated across blocks
/// with and without their retirement, then perturbed.
fn history(rng: &mut Rng) -> Vec<Vec<AdjacencyEntry>> {
    let mut statements = Vec::new();
    for eid in 1..=1 + rng.below(6) {
        let (src, dst) = (1 + rng.below(2), 3 + rng.below(2));
        let mut created = 1 + rng.below(3);
        for _ in 0..1 + rng.below(3) {
            let retired = rng.chance(60).then(|| created + 1 + rng.below(2));
            // Published live first, then (sometimes) restated retired.
            statements.push(entry(eid, src, dst, created, None));
            if let Some(retired) = retired {
                statements.push(entry(eid, src, dst, created, Some(retired)));
                created = retired;
            } else {
                break;
            }
        }
    }
    for statement in &mut statements {
        if rng.chance(8) {
            match rng.below(5) {
                0 => statement.src = VId(u128::from(1 + rng.below(3))),
                1 => statement.dst = VId(u128::from(3 + rng.below(3))),
                2 => statement.created_at = CommitSeq(rng.below(6)),
                3 => statement.retired_at = Some(CommitSeq(rng.below(8))),
                _ => statement.retired_at = None,
            }
        }
    }
    let blocks = 1 + rng.below(5) as usize;
    let mut published: Vec<Vec<AdjacencyEntry>> = vec![Vec::new(); blocks];
    for (at, statement) in statements.into_iter().enumerate() {
        // Keep the chain's publication order most of the time, so lawful
        // histories are common; otherwise any block.
        let block = if rng.chance(75) {
            (at * blocks / 8).min(blocks - 1)
        } else {
            rng.below(blocks as u64) as usize
        };
        published[block].push(statement);
    }
    for block in &mut published {
        if !rng.chance(3) {
            block.sort_by_key(|e| (e.src, e.relation, e.dst, e.eid, e.created_at));
            block.dedup_by_key(|e| (e.src, e.relation, e.dst, e.eid, e.created_at));
        }
    }
    published
}

#[test]
fn per_eid_chains_decide_exactly_as_the_global_statement_map() {
    let mut rng = Rng(0x2a2_0b5e_ed00_c0de);
    let (mut accepted, mut refused) = (0, 0);
    for trial in 0..20_000 {
        let blocks = history(&mut rng);
        let (mut chains, mut global) = (
            EdgeHistoryValidator::default(),
            GlobalMapValidator::default(),
        );
        let mut verdict = Ok(());
        for (at, block) in blocks.iter().enumerate() {
            let expected = global.observe_block(at, block);
            let found = chains.observe_block(at, block);
            assert_eq!(found, expected, "trial {trial} block {at}: {blocks:?}");
            verdict = found;
            if verdict.is_err() {
                break;
            }
        }
        // The same statements, refused block's prefix included.
        assert_eq!(chains.statements(), global.statements, "trial {trial}");
        let (collapsed, superseded) = chains.into_canonical();
        assert_eq!(collapsed, global.statements, "trial {trial}");
        assert_eq!(
            superseded,
            global.seen - global.statements.len(),
            "trial {trial}"
        );
        if verdict.is_ok() {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    // Both verdicts are well represented, so neither path is untested.
    assert!(
        accepted > 2_000 && refused > 2_000,
        "{accepted} accepted, {refused} refused"
    );
}
