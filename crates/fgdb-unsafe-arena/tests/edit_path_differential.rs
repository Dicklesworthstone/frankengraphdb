//! The differential harness for the one ledgered site in `fgdb-unsafe-arena`.
//!
//! `EditPath::Exclusive` borrows every block in a batch at once through the
//! unsafe `Region::blocks_mut`; `EditPath::Sequential` applies the same batch
//! one safe single-borrow call at a time. The relationship asserted between
//! them is **bit-identity of the whole region image**, not agreement on a
//! summary, and it is asserted on refusals as well as on successes: a fallback
//! that agrees only where both paths succeed leaves the interesting half — the
//! batches one path would have waved through — unmeasured.
//!
//! Determinism: the op script comes from a fixed-seed xorshift, so a failure is
//! a seed and a case index, not a story about flakiness. Replay:
//! `cargo test -p fgdb-unsafe-arena --test edit_path_differential`.
//!
//! Neither path is `cfg`-gated, so this matrix is the same on every target the
//! workspace compiles for. That is the cross-compilation half of the
//! bit-identical-fallback rule (§8.7 STRICT, read across to the arena island):
//! the safe path is the specification, and a drift is a defect in the ledgered
//! path rather than a tolerance to be documented.

use fgdb_unsafe_arena::{ArenaError, Edit, EditPath, Handle, Region, RegionAlloc};

/// A deterministic 64-bit xorshift. Not a security primitive and not used as
/// one — it exists so the script below is a fixed sequence rather than an
/// unrepeatable one.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0);
        usize::try_from(self.next_u64() % bound as u64).expect("bound fits usize")
    }
}

/// A region and the handles it has handed out, built identically for both
/// paths so the two runs start from byte-identical state.
struct Fixture {
    region: Region,
    handles: Vec<Handle>,
}

impl Fixture {
    fn build(seed: u64, blocks: usize) -> Self {
        let mut rng = Rng(seed);
        let mut region = Region::with_capacity(512, 1 << 16);
        let mut handles = Vec::with_capacity(blocks);
        for _ in 0..blocks {
            let len = 4 + rng.below(28);
            let align = 1_usize << rng.below(4);
            handles.push(region.alloc_block(len, align).expect("block"));
        }
        // Release a few, so the script's handle pool contains stale handles and
        // the refusal half of the comparison is exercised by construction
        // rather than by luck.
        for index in [1_usize, 4, 9] {
            if let Some(&handle) = handles.get(index) {
                region.release(handle).expect("release");
            }
        }
        Self { region, handles }
    }

    /// The whole region image, as the bytes every live block currently holds.
    /// Blocks whose handle is stale contribute a marker rather than being
    /// skipped, so a path that resurrected one would be visible here.
    fn image(&self) -> Vec<(usize, Result<Vec<u8>, ArenaError>)> {
        self.handles
            .iter()
            .enumerate()
            .map(|(index, &handle)| (index, self.region.block(handle).map(<[u8]>::to_vec)))
            .collect()
    }
}

/// One batch of edits, described in a path-independent way so the same script
/// can be replayed against two fixtures.
#[derive(Debug)]
struct Batch {
    entries: Vec<(usize, usize, Vec<u8>)>,
}

fn script(seed: u64, handle_count: usize, batches: usize) -> Vec<Batch> {
    let mut rng = Rng(seed ^ 0x5eed_5eed_5eed_5eed);
    (0..batches)
        .map(|_| {
            let width = 1 + rng.below(4);
            let entries = (0..width)
                .map(|_| {
                    let handle = rng.below(handle_count);
                    let at = rng.below(12);
                    let len = 1 + rng.below(8);
                    let bytes = (0..len)
                        .map(|_| u8::try_from(rng.below(256)).expect("byte"))
                        .collect();
                    (handle, at, bytes)
                })
                .collect();
            Batch { entries }
        })
        .collect()
}

fn run(path: EditPath, seed: u64, blocks: usize, batches: &[Batch]) -> Vec<RunStep> {
    let mut fixture = Fixture::build(seed, blocks);
    batches
        .iter()
        .map(|batch| {
            let edits: Vec<Edit<'_>> = batch
                .entries
                .iter()
                .map(|(handle, at, bytes)| Edit {
                    handle: fixture.handles[*handle],
                    at: *at,
                    bytes: bytes.as_slice(),
                })
                .collect();
            let outcome = fixture.region.apply(&edits, path);
            RunStep {
                outcome,
                image: fixture.image(),
            }
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct RunStep {
    outcome: Result<(), ArenaError>,
    image: Vec<(usize, Result<Vec<u8>, ArenaError>)>,
}

/// A 64-bit FNV-1a over the whole run, so the harness reports one comparable
/// number as well as a per-step diff. The digest is worthless on its own — a
/// harness that only compared digests would pass with both paths broken the
/// same way — which is why `digests_are_licensed_by_a_one_bit_perturbation`
/// exists below.
fn digest(steps: &[RunStep]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut eat = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for step in steps {
        eat(u8::from(step.outcome.is_ok()));
        for (index, block) in &step.image {
            eat(u8::try_from(index % 251).expect("index byte"));
            match block {
                Ok(bytes) => {
                    eat(1);
                    for &byte in bytes {
                        eat(byte);
                    }
                }
                Err(_) => eat(0),
            }
        }
    }
    hash
}

#[test]
fn the_two_paths_are_bit_identical_across_the_script() {
    let mut compared = 0_usize;
    for seed in [1_u64, 0x1234_5678_9abc_def0, 0xdead_beef_cafe_f00d] {
        let batches = script(seed, 12, 64);
        let sequential = run(EditPath::Sequential, seed, 12, &batches);
        let exclusive = run(EditPath::Exclusive, seed, 12, &batches);
        assert_eq!(
            sequential.len(),
            batches.len(),
            "every batch must produce a step"
        );
        for (index, (a, b)) in sequential.iter().zip(&exclusive).enumerate() {
            assert_eq!(
                a, b,
                "seed {seed:#x} batch {index}: the ledgered path drifted from its fallback"
            );
            compared += 1;
        }
        assert_eq!(
            digest(&sequential),
            digest(&exclusive),
            "seed {seed:#x}: run digests differ"
        );
    }
    assert_eq!(compared, 192, "the matrix must not silently shrink");
}

/// Both halves have to be non-trivial, or "the paths agree" is satisfied by a
/// script in which every batch is refused, or none is.
#[test]
fn the_script_exercises_both_success_and_refusal() {
    let seed = 0x1234_5678_9abc_def0_u64;
    let batches = script(seed, 12, 64);
    let steps = run(EditPath::Sequential, seed, 12, &batches);
    let ok = steps.iter().filter(|s| s.outcome.is_ok()).count();
    let refused = steps.len() - ok;
    assert!(
        ok >= 8,
        "only {ok} batches succeeded; the script is vacuous"
    );
    assert!(
        refused >= 8,
        "only {refused} batches were refused; the refusal half is vacuous"
    );
    // And the refusals must not all be the same kind, or one check is standing
    // in for the whole planner.
    let kinds: std::collections::BTreeSet<&str> = steps
        .iter()
        .filter_map(|s| s.outcome.as_ref().err())
        .map(|e| match e {
            ArenaError::AliasedBatch { .. } => "aliased",
            ArenaError::StaleHandle { .. } => "stale",
            ArenaError::EditOutOfBounds { .. } => "out-of-bounds",
            _ => "other",
        })
        .collect();
    assert!(
        kinds.len() >= 3,
        "refusals covered only {kinds:?}; the planner's branches are not all reached"
    );
}

/// The control that licenses the digest. Without it, "the digests match" would
/// also be reported by a harness that hashed nothing distinguishing.
#[test]
fn digests_are_licensed_by_a_one_bit_perturbation() {
    let seed = 1_u64;
    let batches = script(seed, 12, 64);
    let mut steps = run(EditPath::Sequential, seed, 12, &batches);
    let clean = digest(&steps);
    let mut perturbed = false;
    'search: for step in &mut steps {
        for (_, block) in &mut step.image {
            if let Ok(bytes) = block
                && !bytes.is_empty()
            {
                bytes[0] ^= 1;
                perturbed = true;
                break 'search;
            }
        }
    }
    assert!(perturbed, "no byte was available to perturb");
    assert_ne!(
        clean,
        digest(&steps),
        "flipping one bit of one block left the digest unchanged"
    );
}

/// The seam is a trait, and a consumer generic over it must compile. This is
/// the whole of what the island claims about `RegionAlloc` today: no ART,
/// succinct, or hash storage is parameterized over it yet, and the ledger row
/// says so rather than letting a trait's existence imply an integration.
#[test]
fn the_seam_is_usable_through_the_trait_alone() {
    fn fill<A: RegionAlloc>(alloc: &mut A, len: usize, byte: u8) -> Handle {
        let handle = alloc.alloc_block(len, 8).expect("block");
        alloc.block_mut(handle).expect("live").fill(byte);
        handle
    }
    let mut region = Region::with_capacity(256, 4096);
    let handle = fill(&mut region, 32, 0x5a);
    assert_eq!(region.block(handle).expect("live"), &[0x5a; 32]);
    let audit = region.close();
    assert!(audit.balanced(), "{audit:?}");
}
