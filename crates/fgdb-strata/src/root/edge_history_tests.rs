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
    locations: BTreeMap<(EId, CommitSeq), (usize, usize)>,
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
            self.locations.insert(key, (block_at, entry_at));
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
        let identities = chains.identities();
        let count = identities.len();
        let identities: std::collections::BTreeSet<_> = identities.collect();
        assert_eq!(count, identities.len(), "one entry per identity");
        assert_eq!(
            identities,
            global.statements.keys().map(|(eid, _)| *eid).collect(),
            "spent identities include retired statements and accepted prefixes"
        );
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

/// Thousands of EIds through every growth of the hashed chain index, a
/// quarter of them built to land in one bucket (so all but a probe run take
/// the ordered overflow), each restated retired later and then contiguously
/// succeeded: both validators accept, collapse identically, and refuse the
/// same final conflicting birth.
#[test]
fn the_chain_index_finds_every_eid_through_growth_and_a_full_collision_cluster() {
    let mut rng = Rng(0x2a2_1dea_c011_1de5);
    let eids: Vec<u64> = (0..6_000u64)
        .map(|at| {
            if at.is_multiple_of(4) {
                at
            } else {
                rng.next() >> 2
            }
        })
        .collect();
    let eid = |at: usize| {
        let raw = eids[at];
        // Every 4th EId has the high half that zeroes the mixed product
        // whatever its low half is: one bucket for all of them.
        if at.is_multiple_of(4) {
            EId((0xc2b2_ae3d_27d4_eb4f_u128 << 64) | u128::from(raw))
        } else {
            EId(u128::from(raw))
        }
    };
    let statement = |at: usize, created: u64, retired: Option<u64>| AdjacencyEntry {
        eid: eid(at),
        ..entry(0, 1 + (at as u64 % 2), 3, created, retired)
    };
    let published = |rows: Vec<AdjacencyEntry>| -> Vec<Vec<AdjacencyEntry>> {
        let mut rows = rows;
        rows.sort_by_key(|e| (e.src, e.relation, e.dst, e.eid, e.created_at));
        rows.chunks(200).map(<[AdjacencyEntry]>::to_vec).collect()
    };
    let mut blocks = published((0..eids.len()).map(|at| statement(at, 1, None)).collect());
    blocks.extend(published(
        (0..eids.len())
            .step_by(3)
            .map(|at| statement(at, 1, Some(4)))
            .collect(),
    ));
    blocks.extend(published(
        (0..eids.len())
            .step_by(3)
            .map(|at| statement(at, 4, None))
            .collect(),
    ));
    // A colliding EId (index 8, in the cluster) restated with another source:
    // an identity conflict both validators must refuse.
    let mut conflict = statement(8, 1, None);
    conflict.src = VId(7);
    blocks.push(vec![conflict]);

    let (mut chains, mut global) = (
        EdgeHistoryValidator::default(),
        GlobalMapValidator::default(),
    );
    for (at, block) in blocks.iter().enumerate() {
        let expected = global.observe_block(at, block);
        assert_eq!(chains.observe_block(at, block), expected, "block {at}");
        if at + 1 < blocks.len() {
            assert_eq!(expected, Ok(()), "block {at} is lawful");
        } else {
            assert!(
                matches!(expected, Err(RootError::EdgeIdentityMismatch { .. })),
                "{expected:?}"
            );
        }
    }
    assert!(
        chains.index.overflow.len() > 1_000,
        "the cluster overflowed"
    );
    assert_eq!(chains.statements(), global.statements);
    assert_eq!(global.statements.len(), 6_000 + 2_000);
}

/// Identities a host might assign, `(a, a + 1)` and small counters, spread:
/// none needs the ordered overflow. Under an xor of the halves the first
/// family shares a few dozen buckets and admission is quadratic (fgdb-yzt2b).
#[test]
fn structured_identities_spread_without_overflow() {
    for structured in [true, false] {
        let rows: Vec<AdjacencyEntry> = (0..200_000u128)
            .map(|at| AdjacencyEntry {
                eid: if structured {
                    EId((at << 64) | (at + 1))
                } else {
                    EId(at)
                },
                ..entry(0, 1, 3, 1, None)
            })
            .collect();
        let mut chains = EdgeHistoryValidator::default();
        for (at, block) in rows.chunks(200).enumerate() {
            assert_eq!(chains.observe_block(at, block), Ok(()), "block {at}");
        }
        assert!(
            chains.index.overflow.is_empty(),
            "{} overflowed",
            chains.index.overflow.len()
        );
        assert_eq!(chains.statements().len(), rows.len());
    }
}

/// The replaced winning-row fold, kept as the oracle: every copy inserted in
/// publication order, so the last copy of each statement wins.
fn winning_rows_by_map(
    blocks: &[Vec<AdjacencyEntry>],
    block_props: &[Option<crate::edge_props::BlockProps>],
) -> BTreeMap<(EId, CommitSeq), crate::edge_props::EdgePropertyRow> {
    let mut rows = BTreeMap::new();
    for (block, props) in blocks.iter().zip(block_props) {
        for (index, entry) in block.iter().enumerate() {
            let row = props
                .as_ref()
                .map(|props| props.props_of(index))
                .unwrap_or_default();
            rows.insert((entry.eid, entry.created_at), row);
        }
    }
    rows
}

/// Some blocks host no patch; the others give each entry no row or one of a
/// few distinct rows.
fn block_props(
    rng: &mut Rng,
    blocks: &[Vec<AdjacencyEntry>],
) -> Vec<Option<crate::edge_props::BlockProps>> {
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::CanonicalScalar;
    blocks
        .iter()
        .map(|block| {
            rng.chance(80).then(|| {
                let rows: Vec<_> = (0..1 + rng.below(3))
                    .map(|_| {
                        let value = rng.below(1_000);
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(value as i64).into())]
                    })
                    .collect();
                let locators = block
                    .iter()
                    .map(|_| rng.below(rows.len() as u64 + 1) as u8)
                    .collect();
                crate::edge_props::BlockProps { locators, rows }
            })
        })
        .collect()
}

#[test]
fn winning_rows_answer_exactly_as_the_publication_order_map() {
    let mut rng = Rng(0x7e1d_0b10_c0de_5a17);
    let mut restated = 0;
    for trial in 0..20_000 {
        let blocks = history(&mut rng);
        let props = block_props(&mut rng, &blocks);
        let expected = winning_rows_by_map(&blocks, &props);
        restated += blocks.iter().map(Vec::len).sum::<usize>() - expected.len();
        // Ascending, as both callers ask, with absent statements between.
        let mut rows = super::WinningEdgeRows::new(&blocks, &props);
        for (&(eid, created), row) in &expected {
            let absent = (eid, CommitSeq(created.0.wrapping_sub(1)));
            if !expected.contains_key(&absent) {
                assert!(rows.row(absent).is_empty(), "trial {trial}: {absent:?}");
            }
            assert_eq!(&rows.row((eid, created)), row, "trial {trial}");
        }
        assert!(rows.row((EId(u128::MAX), CommitSeq(u64::MAX))).is_empty());
        // Any other order is answered the same.
        let mut keys: Vec<_> = expected.keys().copied().collect();
        for at in (1..keys.len()).rev() {
            keys.swap(at, rng.below(at as u64 + 1) as usize);
        }
        let mut shuffled = super::WinningEdgeRows::new(&blocks, &props);
        for key in keys {
            assert_eq!(&shuffled.row(key), &expected[&key], "trial {trial}");
        }
    }
    // The generator restates statements across blocks, so later copies
    // really do override earlier ones.
    assert!(restated > 10_000, "restated copies: {restated}");
}

fn property_history() -> Vec<Vec<AdjacencyEntry>> {
    let mut first = entry(1, 1, 2, 1, None);
    first.eid = EId((1 << 100) | 7);
    let mut second = entry(2, 1, 3, 1, None);
    second.eid = EId((1 << 127) | 3);
    vec![
        vec![first, second],
        vec![entry(3, 2, 1, 2, None)],
        vec![AdjacencyEntry {
            retired_at: Some(CommitSeq(4)),
            ..first
        }],
        vec![AdjacencyEntry {
            created_at: CommitSeq(4),
            ..first
        }],
        vec![AdjacencyEntry {
            retired_at: Some(CommitSeq(6)),
            ..second
        }],
    ]
}

#[test]
fn absent_property_columns_need_no_statement_positions() {
    use crate::edge_props::BlockProps;

    let blocks = property_history();
    let absent: Vec<Option<BlockProps>> = vec![None; blocks.len()];
    let empty: Vec<_> = blocks
        .iter()
        .map(|block| {
            Some(BlockProps {
                locators: vec![0; block.len()],
                rows: Vec::new(),
            })
        })
        .collect();
    let mut absent_rows = super::WinningEdgeRows::new(&blocks, &absent);
    let mut empty_rows = super::WinningEdgeRows::new(&blocks, &empty);
    assert_eq!(absent_rows.positions.capacity(), 0);
    assert!(!empty_rows.positions.is_empty());
    for block in blocks.iter().rev() {
        for entry in block {
            let key = (entry.eid, entry.created_at);
            assert_eq!(absent_rows.row(key), empty_rows.row(key));
        }
    }
    assert!(absent_rows.row((EId(u128::MAX), CommitSeq(99))).is_empty());
    assert_eq!(absent_rows.positions.capacity(), 0);
    let mut no_blocks = super::WinningEdgeRows::new(&[] as &[Vec<AdjacencyEntry>], &absent[..0]);
    assert!(no_blocks.row((EId(1), CommitSeq(1))).is_empty());
}

#[test]
fn property_absence_keeps_visibility_compaction_and_last_copy_precedence() {
    use crate::compact::compact_with_props;
    use crate::edge_props::BlockProps;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::CanonicalScalar;

    let blocks = property_history();
    let absent = vec![None; blocks.len()];
    let empty: Vec<_> = blocks
        .iter()
        .map(|block| {
            Some(BlockProps {
                locators: vec![0; block.len()],
                rows: Vec::new(),
            })
        })
        .collect();
    let row = |value| vec![(PropertyKeyId(1), CanonicalScalar::Int(value).into())];
    let mixed = vec![
        Some(BlockProps {
            locators: vec![1, 2],
            rows: vec![row(11), row(22)],
        }),
        Some(BlockProps {
            locators: vec![1],
            rows: vec![row(33)],
        }),
        None, // The retirement restatement clears the first statement's row.
        Some(BlockProps {
            locators: vec![1],
            rows: vec![row(44)],
        }),
        None, // The other retirement must not resurrect the earlier row.
    ];
    let mut reference = GlobalMapValidator::default();
    for (at, block) in blocks.iter().enumerate() {
        reference.observe_block(at, block).unwrap();
    }
    for props in [&absent, &empty, &mixed] {
        let winners = winning_rows_by_map(&blocks, props);
        for floor in 0..=7 {
            let compacted = compact_with_props(&blocks, props, CommitSeq(floor)).unwrap();
            for at in floor..=7 {
                let at = CommitSeq(at);
                let expected: Vec<_> = reference
                    .statements
                    .iter()
                    .filter(|(_, entry)| entry.visible_at(at))
                    .map(|(key, entry)| (*entry, winners[key].clone()))
                    .collect();
                assert_eq!(
                    super::merge_all_edges_with_props(&blocks, props, at).unwrap(),
                    expected,
                );
                assert_eq!(
                    super::merge_all_edges_with_props(
                        &compacted.blocks,
                        &compacted.block_props,
                        at,
                    )
                    .unwrap(),
                    expected,
                );
            }
        }
    }
    let early = super::merge_all_edges_with_props(&blocks, &mixed, CommitSeq(1)).unwrap();
    assert_eq!(early.len(), 2);
    assert!(early.iter().all(|(_, row)| row.is_empty()));
    let current = super::merge_all_edges_with_props(&blocks, &mixed, CommitSeq(6)).unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].1, row(33));
    assert_eq!(current[1].1, row(44));

    // The no-property proof must never precede the full history validator.
    let mut corrupt = blocks;
    corrupt[2][0].src = VId(9);
    let expected = super::collapse_edge_history(&corrupt).unwrap_err();
    assert_eq!(
        super::merge_all_edges_with_props(&corrupt, &absent, CommitSeq(6)),
        Err(expected.clone()),
    );
    assert_eq!(
        compact_with_props(&corrupt, &absent, CommitSeq(6)),
        Err(expected),
    );
}

/// Same-process maintenance comparison of the actual admitted whole-graph
/// read, using the unchanged indexed constructor for the incumbent. History
/// admission, storage I/O and compacted block packing are outside this timer.
/// Run with `cargo test -p fgdb-strata --release
/// benchmark_propertyless_admitted_edge_reads -- --ignored --nocapture`.
#[test]
#[ignore = "manual paired read-path measurement; no wall-clock threshold"]
fn benchmark_propertyless_admitted_edge_reads() {
    use crate::edge_props::BlockProps;
    use std::hint::black_box;
    use std::time::Instant;

    for count in [16_384usize, 131_072] {
        let mut blocks = Vec::new();
        for source in 0..count / 32 {
            let mut block = Vec::new();
            for offset in 0..32 {
                let ordinal = source * 32 + offset;
                let mut edge = entry(
                    0,
                    source as u64 + 1,
                    ((source + offset + 1) % (count / 32)) as u64 + 1,
                    1,
                    None,
                );
                edge.eid =
                    EId((ordinal as u128 + 1).wrapping_mul(0x9e3779b97f4a7c15_bf58476d1ce4e5b9));
                block.push(edge);
            }
            block.sort_unstable_by_key(|edge| (edge.dst, edge.eid));
            blocks.push(block);
        }
        let props: Vec<Option<BlockProps>> = vec![None; blocks.len()];
        let (statements, _) = super::collapse_edge_history(&blocks).unwrap();
        let incumbent = || {
            // The same constructor the mixed/propertyful product path uses.
            let mut rows = super::WinningEdgeRows::new_indexed(&blocks, &props);
            statements
                .iter()
                .filter(|(_, entry)| entry.visible_at(CommitSeq(1)))
                .map(|(key, entry)| (*entry, rows.row(*key)))
                .collect::<Vec<_>>()
        };
        let candidate = || {
            super::merge_admitted_edges_with_props(&statements, &blocks, &props, CommitSeq(1))
                .unwrap()
        };
        assert_eq!(candidate(), incumbent());
        let mut samples = [Vec::new(), Vec::new()];
        for trial in 0..9 {
            let mut outputs = [None, None];
            for which in [trial % 2, 1 - trial % 2] {
                let start = Instant::now();
                let rows = black_box(if which == 0 { incumbent() } else { candidate() });
                samples[which].push(start.elapsed().as_nanos());
                outputs[which] = Some(rows);
            }
            assert_eq!(outputs[0], outputs[1]);
        }
        eprintln!(
            "propertyless_admitted_read edges={count} trials=9 incumbent_samples_ns={:?} candidate_samples_ns={:?}",
            samples[0], samples[1],
        );
        for sample in &mut samples {
            sample.sort_unstable();
        }
        let position_bytes = count * std::mem::size_of::<((EId, CommitSeq), usize, usize)>();
        eprintln!(
            "propertyless_admitted_read edges={count} incumbent_median_ns={} candidate_median_ns={} eliminated_position_bytes={position_bytes}",
            samples[0][4], samples[1][4],
        );
    }
}

/// The reconstruction index observes the same accepted statements as the
/// independent map, including a refused block's accepted prefix. Its pointer
/// always names the last copy of the newest content statement, which need not
/// be the last statement encountered for that EId.
#[test]
fn latest_edge_rows_match_independent_history_and_property_locations() {
    let mut rng = Rng(0x000e_8674_1a7e_57ed);
    let (mut accepted, mut refused) = (0, 0);
    for trial in 0..20_000 {
        let blocks = history(&mut rng);
        let props = block_props(&mut rng, &blocks);
        let mut global = GlobalMapValidator::default();
        let mut chains = EdgeHistoryValidator::default();
        let mut latest = super::LatestEdgeRows::default();
        for (at, block) in blocks.iter().enumerate() {
            let expected = global.observe_block(at, block);
            let found = latest.observe_block(&mut chains, at, block);
            assert_eq!(found, expected, "trial {trial}, block {at}");
            assert_eq!(chains.statements(), global.statements);
            let frontier = global
                .statements
                .values()
                .map(|row| row.retired_at.unwrap_or(row.created_at))
                .max()
                .unwrap_or(CommitSeq::ORIGIN);
            let expected: Vec<_> = global
                .statements
                .iter()
                .filter(|(_, row)| row.visible_at(frontier))
                .map(|(key, row)| {
                    let (block, at) = global.locations[key];
                    let props = props[block]
                        .as_ref()
                        .map(|props| props.props_of(at))
                        .unwrap_or_default();
                    (*row, props)
                })
                .collect();
            assert_eq!(
                latest.live_edges(&chains, &props, frontier).unwrap(),
                expected,
                "trial {trial}, block {at}: {blocks:?}"
            );
            assert_eq!(latest.positions.len(), chains.chains.len());
            if found.is_err() {
                refused += 1;
                break;
            }
            if at + 1 == blocks.len() {
                accepted += 1;
            }
        }
    }
    assert!(accepted > 2_000 && refused > 2_000);
}

#[test]
fn latest_edge_rows_keep_newest_content_and_last_copy_property_precedence() {
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::CanonicalScalar;
    let early = entry(7, 1, 1, 1, Some(2));
    let current = entry(7, 1, 1, 2, None);
    // Newest content may precede an older retired statement in publication
    // order. Repeating that old statement must not point the current row at it.
    let blocks = [vec![current], vec![early], vec![current], vec![early]];
    let mut props: Vec<_> = (0..4)
        .map(|at| {
            Some(crate::edge_props::BlockProps {
                locators: vec![1],
                rows: vec![vec![(PropertyKeyId(1), CanonicalScalar::Int(at).into())]],
            })
        })
        .collect();
    let mut history = EdgeHistoryValidator::default();
    let mut latest = super::LatestEdgeRows::default();
    for (at, block) in blocks.iter().enumerate() {
        latest.observe_block(&mut history, at, block).unwrap();
    }
    assert_eq!(latest.positions, vec![(2, 0)]);
    assert_eq!(
        latest.live_edges(&history, &props, CommitSeq(2)).unwrap(),
        vec![(current, props[2].as_ref().unwrap().props_of(0))]
    );
    // A propertyless latest copy clears earlier properties; it is not a
    // signal to fall back to an older hosted row.
    props[2] = None;
    assert_eq!(
        latest.live_edges(&history, &props, CommitSeq(2)).unwrap(),
        vec![(current, Vec::new())]
    );
    assert!(matches!(
        latest.live_edges(&history, &props, CommitSeq(1)),
        Err(RootError::BlockAfterPublication { .. })
    ));
    let retired = AdjacencyEntry {
        retired_at: Some(CommitSeq(3)),
        ..current
    };
    latest.observe_block(&mut history, 4, &[retired]).unwrap();
    props.push(None);
    assert!(
        latest
            .live_edges(&history, &props, CommitSeq(3))
            .unwrap()
            .is_empty()
    );
}
