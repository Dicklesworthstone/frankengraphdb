//! Differential harnesses for the two ledgered sites in `fgdb-unsafe-arena`.
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
//!
//! `RegionVec<T>` separately exercises the private allocator adapter through
//! standard `Vec<T, A>` and compares every typed operation, refusal, move, and
//! drop count with ordinary `Vec<T>`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asupersync::Cx;
use asupersync::cx::cap;
#[cfg(not(miri))]
use asupersync::lab::run_async_under_lab;
use fgdb_types::{PurposeContexts, QueryCx};
use fgdb_unsafe_arena::{
    ArenaError, Edit, EditPath, Handle, Region, RegionAlloc, RegionOutcome, RegionScope, RegionVec,
    RegionVecError,
};

#[cfg(not(miri))]
fn query_root() -> (Cx<cap::All>, QueryCx) {
    let (pair, report) = run_async_under_lab(0xa110_ca7e, |root| async move {
        let query = PurposeContexts::narrow_runtime_root(&root).query();
        (root, query)
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    pair
}

// Miri's default isolation deliberately refuses the wall-clock read performed
// by asupersync's lab-report oracle after quiescence. The allocator workload
// needs only checkpoint and cancellation semantics, so use asupersync's
// explicit test-internal root under Miri and keep isolation enabled.
#[cfg(miri)]
fn query_root() -> (Cx<cap::All>, QueryCx) {
    let root = Cx::for_testing();
    let query = PurposeContexts::narrow_runtime_root(&root).query();
    (root, query)
}

fn query_cx() -> QueryCx {
    query_root().1
}

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
        let mut region = Region::with_capacity(512, 1 << 16, 1 << 17);
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

/// The raw byte seam is a trait, and a raw consumer generic over it must
/// compile. Typed ART, succinct, and hash storage intentionally use
/// `RegionVec` instead; this test makes no typed-consumer claim.
#[test]
fn the_seam_is_usable_through_the_trait_alone() {
    fn fill<A: RegionAlloc>(alloc: &mut A, len: usize, byte: u8) -> Handle {
        let handle = alloc.alloc_block(len, 8).expect("block");
        alloc.block_mut(handle).expect("live").fill(byte);
        handle
    }
    let mut region = Region::with_capacity(256, 4096, 4096);
    let handle = fill(&mut region, 32, 0x5a);
    assert_eq!(region.block(handle).expect("live"), &[0x5a; 32]);
    let audit = region.close();
    assert!(audit.balanced(), "{audit:?}");
}

#[derive(Debug)]
enum VecOp {
    Push(i64),
    Pop,
    Insert(usize, i64),
    Remove(usize),
    Truncate(usize),
    Resize(usize, i64),
    Extend(Vec<i64>),
    Clear,
    Replace(usize, i64),
}

fn vec_script(seed: u64, operations: usize) -> Vec<VecOp> {
    let mut rng = Rng(seed ^ 0xa110_ca7e_5eed_0001);
    let mut modeled_len = 0_usize;
    let mut out = Vec::with_capacity(operations);
    for _ in 0..operations {
        let op = match rng.below(9) {
            0 => {
                modeled_len += 1;
                VecOp::Push(rng.next_u64() as i64)
            }
            1 => {
                modeled_len = modeled_len.saturating_sub(1);
                VecOp::Pop
            }
            2 => {
                let index = if modeled_len == 0 {
                    0
                } else {
                    rng.below(modeled_len + 1)
                };
                modeled_len += 1;
                VecOp::Insert(index, rng.next_u64() as i64)
            }
            3 if modeled_len != 0 => {
                let index = rng.below(modeled_len);
                modeled_len -= 1;
                VecOp::Remove(index)
            }
            4 => {
                let len = rng.below(24);
                modeled_len = modeled_len.min(len);
                VecOp::Truncate(len)
            }
            5 => {
                let len = rng.below(32);
                modeled_len = len;
                VecOp::Resize(len, rng.next_u64() as i64)
            }
            6 => {
                let values: Vec<i64> = (0..rng.below(7)).map(|_| rng.next_u64() as i64).collect();
                modeled_len += values.len();
                VecOp::Extend(values)
            }
            7 => {
                modeled_len = 0;
                VecOp::Clear
            }
            8 if modeled_len != 0 => VecOp::Replace(rng.below(modeled_len), rng.next_u64() as i64),
            _ => {
                modeled_len += 1;
                VecOp::Push(rng.next_u64() as i64)
            }
        };
        out.push(op);
    }
    out
}

#[test]
fn typed_operations_and_reallocations_match_ordinary_vec() {
    let cx = query_cx();
    for seed in [3_u64, 0x1111_2222_3333_4444, 0xfedc_ba98_7654_3210] {
        let scope = RegionScope::with_capacity(1 << 20, 1 << 26, 1 << 27);
        let mut actual = RegionVec::new_in(&scope).expect("typed vector");
        let mut oracle = Vec::new();
        for (index, operation) in vec_script(seed, 256).into_iter().enumerate() {
            match operation {
                VecOp::Push(value) => {
                    actual.try_push(&cx, value).expect("push");
                    oracle.push(value);
                }
                VecOp::Pop => assert_eq!(actual.pop(), oracle.pop()),
                VecOp::Insert(at, value) => {
                    actual.try_insert(&cx, at, value).expect("insert");
                    oracle.insert(at, value);
                }
                VecOp::Remove(at) => assert_eq!(actual.remove(at), oracle.remove(at)),
                VecOp::Truncate(len) => {
                    actual.truncate(len);
                    oracle.truncate(len);
                }
                VecOp::Resize(len, value) => {
                    actual.try_resize(&cx, len, value).expect("resize");
                    oracle.resize(len, value);
                }
                VecOp::Extend(values) => {
                    actual
                        .try_extend(&cx, values.iter().copied())
                        .expect("extend");
                    oracle.extend(values);
                }
                VecOp::Clear => {
                    actual.clear();
                    oracle.clear();
                }
                VecOp::Replace(at, value) => {
                    let expected = std::mem::replace(&mut oracle[at], value);
                    assert_eq!(actual.replace(at, value), Ok(expected));
                }
            }
            assert_eq!(
                actual.as_slice(),
                oracle.as_slice(),
                "seed {seed:#x}, operation {index}"
            );
        }
        assert!(
            scope.bytes_allocated() > 0,
            "the actual typed buffer must be region-backed"
        );
        drop(actual);
        let audit = scope.close().expect("no live typed owners");
        assert!(audit.balanced(), "{audit:?}");
        assert!(
            audit.blocks_allocated > 1,
            "the script must exercise real typed reallocation"
        );
    }
}

#[test]
fn allocation_refusals_leave_the_element_sequence_unchanged() {
    let (root, cx) = query_root();
    let scope = RegionScope::with_capacity(64, 64, 128);
    let mut vector = RegionVec::with_capacity_in(&scope, &cx, 4).expect("four u64 values");
    for value in 0_u64..4 {
        vector
            .try_push(&cx, value)
            .expect("within reserved capacity");
    }
    let before = vector.as_slice().to_vec();
    assert!(matches!(
        vector.try_push(&cx, 4),
        Err(RegionVecError::Arena(ArenaError::RegionFull { .. }))
    ));
    assert_eq!(vector.as_slice(), before);

    assert_eq!(
        vector.try_reserve_exact(&cx, usize::MAX),
        Err(RegionVecError::CapacityOverflow)
    );
    assert_eq!(vector.as_slice(), before);

    root.set_cancel_requested(true);
    assert_eq!(
        vector.try_insert(&cx, 0, 99),
        Err(RegionVecError::CheckpointRefused)
    );
    assert_eq!(vector.as_slice(), before);

    drop(vector);
    let audit = scope.cancel().expect("all typed owners dropped");
    assert_eq!(audit.outcome, RegionOutcome::Cancelled);
    assert!(audit.balanced(), "{audit:?}");
}

#[repr(align(2))]
struct Align2;
#[repr(align(4))]
struct Align4;
#[repr(align(8))]
struct Align8;
#[repr(align(16))]
struct Align16;
#[repr(align(32))]
struct Align32;
#[repr(align(64))]
struct Align64;
#[repr(align(128))]
struct Align128;

fn push_aligned<T>(scope: &RegionScope, cx: &QueryCx, value: T) {
    let mut vector = RegionVec::new_in(scope).expect("supported alignment");
    vector.try_push(cx, value).expect("aligned allocation");
}

#[test]
fn supported_alignments_zsts_and_overaligned_refusal_are_explicit() {
    let cx = query_cx();
    let scope = RegionScope::with_capacity(4096, 1 << 20, 1 << 21);
    push_aligned(&scope, &cx, 1_u8);
    push_aligned(&scope, &cx, Align2);
    push_aligned(&scope, &cx, Align4);
    push_aligned(&scope, &cx, Align8);
    push_aligned(&scope, &cx, Align16);
    push_aligned(&scope, &cx, Align32);
    push_aligned(&scope, &cx, Align64);

    let drops = Arc::new(AtomicUsize::new(0));
    #[repr(align(64))]
    struct AlignedZst {
        drops: Arc<AtomicUsize>,
    }
    impl Drop for AlignedZst {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    // A field would make this non-ZST, so use a separate true ZST to pin the
    // capacity rule and the drop-bearing type above to pin maximum alignment.
    #[repr(align(64))]
    struct TrueZst;
    let before = scope.bytes_allocated();
    let mut zsts = RegionVec::new_in(&scope).expect("maximum-aligned ZST");
    for _ in 0..1024 {
        zsts.try_push(&cx, TrueZst).expect("ZST push");
    }
    assert_eq!(zsts.len(), 1024);
    assert_eq!(
        scope.bytes_allocated(),
        before,
        "ZSTs must not allocate a backing block"
    );
    drop(zsts);

    let mut aligned_drop = RegionVec::new_in(&scope).expect("aligned drop value");
    aligned_drop
        .try_push(
            &cx,
            AlignedZst {
                drops: Arc::clone(&drops),
            },
        )
        .expect("aligned value");
    drop(aligned_drop);
    assert_eq!(drops.load(Ordering::Relaxed), 1);

    assert!(matches!(
        RegionVec::<Align128>::new_in(&scope),
        Err(RegionVecError::UnsupportedAlignment {
            align: 128,
            maximum: 64
        })
    ));
    let audit = scope.close().expect("alignment vectors dropped");
    assert!(audit.balanced(), "{audit:?}");
}

#[derive(Debug)]
struct DropProbe {
    created: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    value: usize,
}

impl DropProbe {
    fn new(created: &Arc<AtomicUsize>, dropped: &Arc<AtomicUsize>, value: usize) -> Self {
        created.fetch_add(1, Ordering::Relaxed);
        Self {
            created: Arc::clone(created),
            dropped: Arc::clone(dropped),
            value,
        }
    }
}

impl Clone for DropProbe {
    fn clone(&self) -> Self {
        Self::new(&self.created, &self.dropped, self.value)
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn exercise_drop_glue(outcome: RegionOutcome) {
    let cx = query_cx();
    let created = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let scope = RegionScope::with_capacity(4096, 1 << 22, 1 << 23);
    {
        let mut values = RegionVec::new_in(&scope).expect("drop vector");
        for value in 0..96 {
            values
                .try_push(&cx, DropProbe::new(&created, &dropped, value))
                .expect("growth");
        }
        let replacement = DropProbe::new(&created, &dropped, 1000);
        drop(values.replace(17, replacement).expect("replacement"));
        values
            .try_resize(&cx, 128, DropProbe::new(&created, &dropped, 2000))
            .expect("clone-backed growth");
        values.truncate(31);
        let cloned = values.try_clone(&cx).expect("fallible clone");
        drop(cloned);
    }
    assert_eq!(
        created.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        "every initialized T must run drop glue exactly once"
    );
    let audit = match outcome {
        RegionOutcome::Closed => scope.close().expect("close"),
        RegionOutcome::Cancelled => scope.cancel().expect("cancel"),
    };
    assert_eq!(audit.outcome, outcome);
    assert!(audit.balanced(), "{audit:?}");
    assert!(
        audit.blocks_allocated >= 4,
        "drop test must include typed reallocations"
    );
}

#[test]
fn generic_drop_glue_is_exact_on_close_and_cancel() {
    exercise_drop_glue(RegionOutcome::Closed);
    exercise_drop_glue(RegionOutcome::Cancelled);
}

#[test]
#[cfg(not(miri))]
fn forgotten_typed_owner_refuses_finalization() {
    let cx = query_cx();
    let scope = RegionScope::with_capacity(256, 4096, 4096);
    let mut vector = RegionVec::new_in(&scope).expect("typed vector");
    vector.try_push(&cx, 42_u64).expect("value");
    std::mem::forget(vector);
    let error = scope
        .close()
        .expect_err("a forgotten typed owner must fail closed");
    assert_eq!(error.owners_remaining(), 1);
    assert_eq!(error.allocator_fault(), None);
    // Dropping the error drops its retained scope, whose Drop deliberately
    // retains the backing region because the owner lease remains live.
    drop(error);
}

#[test]
fn recursive_and_collection_shaped_values_use_typed_region_storage() {
    struct ArtNode<'region, V> {
        value: V,
        children: RegionVec<'region, ArtNode<'region, V>>,
    }
    struct HashEntry<K, V> {
        key: K,
        value: V,
    }

    let cx = query_cx();
    let scope = RegionScope::with_capacity(1 << 16, 1 << 22, 1 << 23);
    let mut root_children = RegionVec::new_in(&scope).expect("ART children");
    let leaf = ArtNode {
        value: String::from("leaf"),
        children: RegionVec::new_in(&scope).expect("leaf children"),
    };
    root_children.try_push(&cx, leaf).expect("ART child");
    assert_eq!(root_children.get(0).expect("leaf").value, "leaf");
    assert!(root_children.get(0).expect("leaf").children.is_empty());

    let mut hash = RegionVec::new_in(&scope).expect("hash entries");
    hash.try_push(
        &cx,
        HashEntry {
            key: String::from("k"),
            value: vec![1_u8, 2, 3],
        },
    )
    .expect("hash entry");
    assert_eq!(hash.get(0).expect("entry").key, "k");
    assert_eq!(hash.get(0).expect("entry").value, [1, 2, 3]);

    let mut succinct = RegionVec::new_in(&scope).expect("succinct words");
    succinct
        .try_extend(&cx, [0_u64, 1, 3, 7, 15])
        .expect("succinct values");
    assert_eq!(succinct.as_slice(), [0, 1, 3, 7, 15]);
    assert!(
        scope.bytes_allocated() > 0,
        "all three actual buffers must come from the region"
    );

    drop(succinct);
    drop(hash);
    drop(root_children);
    let audit = scope.close().expect("shaped values dropped");
    assert!(audit.balanced(), "{audit:?}");
}
