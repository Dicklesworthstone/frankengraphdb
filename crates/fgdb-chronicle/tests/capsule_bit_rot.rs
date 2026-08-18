//! Bit-rot campaign against capsule files a REAL commit wrote.
//!
//! §15 asks for "torn-write + bit-rot campaigns asserting RaptorQ recovery up to
//! overhead and fail-closed beyond it". `erasure_recovery.rs` proves that contract
//! at the codec layer over in-memory buffers, which is the right place for it and
//! is not this. What was untested is the COMPOSITION on the durable path: bytes
//! rotted on disk, read back through `CommitCoordinator::read_capsule`, which
//! opens the file, decodes the container, verifies each symbol's MAC, filters the
//! failures, feeds the survivors to RaptorQ, and recomputes the identity.
//!
//! **THIS ONLY BECAME MEANINGFUL WHEN CAPSULES BECAME ERASURE-CODED.** Injecting
//! bit rot into raw capsule bytes before that could prove only that they do not
//! heal. Now there is a repair budget and per-symbol MACs, so "how much damage is
//! survivable, and what happens one symbol past that" is a real question with a
//! designed answer — and the answer is the product promise in plan doctrine 5: no
//! double-write journaling anywhere, because RaptorQ heals torn and corrupt
//! symbols.
//!
//! **WHY THIS DOES NOT NEED THE LAB VFS** (fgdb-1xtp). That bead is about faults the
//! process cannot observe — an fsync that lies, a tear inside one write — which need
//! an injecting filesystem under an async durable path. Bit rot is different: it is
//! damage that appears in a file BETWEEN a successful write and a later read, so a
//! test can simply write the damage itself. The VFS is still needed for the other
//! three fault classes; it is not needed for this one, and conflating them is why
//! this campaign was waiting on a large migration it never actually required.
//!
//! **SYMBOLS ARE LOCATED BY CONTENT, NOT BY OFFSET.** The container is a fixed
//! header followed by length-prefixed symbols, and hardcoding that header's width
//! would make these tests break on an unrelated descriptor change and, worse, would
//! let them silently corrupt the wrong region. Each symbol is found by searching
//! the file for its own bytes, which `decode_container` hands us — ciphertext plus
//! a MAC, so a false match is not a practical concern.
//!
//! **SCRATCH STATE IS NEVER REUSED.** The repository's no-deletion rule means this
//! campaign deliberately retains its on-disk evidence. One exclusively created
//! suite root contains all per-test directories, and a stale root left by a prior
//! process is skipped rather than reopened. Actual reclamation requires a separate,
//! explicitly authorized lifecycle policy (`fgdb-g0f1.1`); correctness never
//! depends on it.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile, decode_container};
use fgdb_chronicle::commit::{CAPSULE_DIR, CommitCoordinator, CommitError};
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate, MarkerChain};
use fgdb_crypto::Digest;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::{BranchId, GraphId, ObjectId};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn digest(seed: u8) -> Digest {
    Digest([seed; 32])
}

fn keys() -> CapsuleKeys {
    CapsuleKeys::new(
        [0x5a; 32],
        fgdb_types::ids::DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
        0x0274,
        CapsuleProfile::balanced(),
    )
}

fn marker_for(seq: u64, capsule: ObjectId, chain: &MarkerChain) -> CommitMarker {
    CommitMarker {
        logical_command_seq: seq * 10,
        commit_seq: seq,
        effect_source: EffectSource::Local {
            capsule_ref: capsule,
            logical_delta_template_digest: digest(seq as u8 + 1),
        },
        prev_global: None,
        head_updates: vec![HeadUpdate {
            graph: GraphId(1),
            branch: BranchId(1),
            expected_previous: chain.head(GraphId(1), BranchId(1)),
        }],
        merge_record_oid: None,
        coordinate_schema_transition_digest: digest(3),
        topology_epoch: 1,
        policy_epoch: 2,
        revocation_index: 3,
        txn_token: [7u8; 16],
        commit_hlc: 1_000 + seq,
        final_effect_digest: digest(seq as u8 + 4),
        authorization_decision_digest: digest(5),
        resource_effect_digest: digest(6),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

fn fresh_suite_root(parent: &Path, pid: u32) -> io::Result<PathBuf> {
    let mut attempt = 0_u64;
    loop {
        let candidate = parent.join(format!("fgdb-bitrot-{pid}-{attempt}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                attempt = attempt
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("scratch root attempt space exhausted"))?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    let root = ROOT.get_or_init(|| {
        fresh_suite_root(&std::env::temp_dir(), std::process::id())
            .expect("create fresh bit-rot suite root")
    });
    let dir = root.join(name);
    std::fs::create_dir(&dir).expect("fresh per-test scratch dir");
    dir
}

/// A prior process with the same PID must never donate its populated database
/// directory to this run. The exclusive create is the contract: a collision is
/// skipped without modifying the stale evidence, and the selected root is empty.
#[test]
fn scratch_allocation_skips_a_stale_nonempty_process_root() {
    let parent = scratch_dir("scratch-allocation-control");
    let simulated_reused_pid = 424_242;
    let stale = parent.join(format!("fgdb-bitrot-{simulated_reused_pid}-0"));
    std::fs::create_dir(&stale).expect("stale root fixture");
    let witness = stale.join("prior-run-witness");
    std::fs::write(&witness, b"must remain untouched").expect("stale witness");

    let fresh = fresh_suite_root(&parent, simulated_reused_pid).expect("allocate after stale root");
    assert_ne!(fresh, stale, "a stale process root must never be reused");
    assert_eq!(
        std::fs::read(&witness).expect("stale witness remains readable"),
        b"must remain untouched",
        "allocating a new root must not mutate prior-run evidence"
    );
    assert!(
        std::fs::read_dir(&fresh)
            .expect("fresh root")
            .next()
            .is_none(),
        "the selected root must be newly created and empty"
    );
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        // lab_test_passed() covers ALL THREE channels — quiescence, the full
        // 24-oracle suite, and the mirrored invariant list (fresh-eyes I3).
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn only_capsule_path(dir: &Path) -> PathBuf {
    let mut entries = std::fs::read_dir(dir.join(CAPSULE_DIR)).expect("capsule dir");
    let path = entries
        .next()
        .expect("one capsule")
        .expect("read capsule entry")
        .path();
    assert!(entries.next().is_none(), "exactly one capsule expected");
    path
}

/// A plaintext large enough to need many symbols, so a repair budget of N is a
/// small fraction of the object rather than most of it.
fn plaintext() -> Vec<u8> {
    (0..8_192u32).map(|i| (i % 251) as u8).collect()
}

/// Commit `plaintext()` and return the capsule's on-disk path and identity.
async fn committed(dir: &Path, cx: &CommitCx) -> (PathBuf, ObjectId) {
    let mut coordinator = CommitCoordinator::open(cx, dir, keys())
        .await
        .expect("open");
    let chain_snapshot = coordinator.chain().clone();
    let body = plaintext();
    let oid = coordinator.capsule_id(&body);
    coordinator
        .commit(cx, &body, |seq, capsule| {
            marker_for(seq, capsule, &chain_snapshot)
        })
        .await
        .expect("commit");
    drop(coordinator);
    (only_capsule_path(dir), oid)
}

/// Flip one bit inside each of the first `count` symbols of the container at
/// `path`, locating each symbol by its own bytes.
///
/// Returns how many symbols the container held, so a caller can state the damage
/// as a fraction rather than as a bare number.
fn rot_symbols(path: &Path, count: usize) -> usize {
    let mut bytes = std::fs::read(path).expect("read container");
    let (_, symbols) = decode_container(&bytes).expect("container decodes before damage");
    assert!(
        count <= symbols.len(),
        "cannot rot {count} of {} symbols",
        symbols.len()
    );
    for symbol in symbols.iter().take(count) {
        let at = bytes
            .windows(symbol.len())
            .position(|window| window == symbol.as_slice())
            .expect("each symbol appears verbatim in the container");
        // Middle of the symbol payload: a MAC covers the whole record, so any
        // interior bit is as good as any other, and the middle is furthest from
        // the framing this test is deliberately not touching.
        bytes[at + symbol.len() / 2] ^= 0x40;
    }
    std::fs::write(path, &bytes).expect("write rotted container");
    symbols.len()
}

/// CONTROL: an undamaged capsule reads back byte-for-byte.
///
/// Without it every "recovery failed" law below could be passing because the read
/// path never works, and every "recovered" law could be passing on a fixture that
/// was never damaged.
#[test]
fn an_undamaged_capsule_reads_back_exactly() {
    let dir = scratch_dir("clean");
    under_lab(1, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .expect("reads"),
            plaintext()
        );

        let (_, symbols) = decode_container(&std::fs::read(&path).expect("read")).expect("decodes");
        assert!(
            symbols.len() > keys().profile().erasure_budget(),
            "the fixture must hold more symbols than the repair budget, or \
             'within budget' and 'the whole object' are the same test"
        );
    });
}

/// ONE rotted symbol is invisible: the object reads back byte-for-byte.
///
/// The symbol fails its MAC, the capsule layer drops it, and RaptorQ rebuilds the
/// object from the survivors. That the plaintext is EXACT is the assertion that
/// matters — a healing path that returned something plausible but different would
/// be far worse than one that failed.
#[test]
fn a_single_rotted_symbol_heals_invisibly() {
    let dir = scratch_dir("one");
    under_lab(2, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        rot_symbols(&path, 1);

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .expect("heals"),
            plaintext(),
            "one rotted symbol must be invisible, not merely survivable"
        );
    });
}

/// Damage up to the FULL repair budget still heals.
#[test]
fn rot_within_the_repair_budget_heals() {
    let dir = scratch_dir("budget");
    under_lab(3, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let budget = keys().profile().erasure_budget();
        let total = rot_symbols(&path, budget);
        assert!(budget > 0 && budget < total);

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .expect("heals at the budget"),
            plaintext(),
            "the budget is a promise about the last symbol as much as the first"
        );
    });
}

/// ONE PAST THE BUDGET FAILS CLOSED. Never partial bytes, never wrong bytes.
///
/// This is the law the whole erasure design exists to make true, and the one whose
/// wrong answer is silent: a decoder that returned a short or scrambled object
/// would hand corrupted state to a caller that believes it recovered.
#[test]
fn rot_beyond_the_repair_budget_fails_closed() {
    let dir = scratch_dir("beyond");
    under_lab(4, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let over = keys().profile().erasure_budget() + 1;
        rot_symbols(&path, over);

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let result = coordinator.read_capsule(cx, oid, &mut Vec::new()).await;
        assert!(
            result.is_err(),
            "beyond the budget the read must FAIL, not return {} bytes",
            result.map(|bytes| bytes.len()).unwrap_or(0)
        );
    });
}

/// The boundary is exactly where it is declared: budget heals, budget+1 does not,
/// in ONE test so the pair cannot drift apart.
///
/// Two separate tests could both pass while the real threshold sat somewhere else
/// entirely — each would only be checking one side of a boundary it never names.
#[test]
fn the_repair_budget_is_the_exact_boundary() {
    let budget = keys().profile().erasure_budget();
    let heals = {
        let dir = scratch_dir("edge-at");
        under_lab(5, move |cx| async move {
            let cx = &cx;
            let (path, oid) = committed(&dir, cx).await;
            rot_symbols(&path, budget);
            CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen")
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .is_ok()
        })
    };
    let past = {
        let dir = scratch_dir("edge-past");
        under_lab(6, move |cx| async move {
            let cx = &cx;
            let (path, oid) = committed(&dir, cx).await;
            rot_symbols(&path, budget + 1);
            CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen")
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .is_ok()
        })
    };
    assert!(
        heals && !past,
        "the declared budget of {budget} must be the exact threshold: \
         at budget healed={heals}, past budget healed={past}"
    );
}

/// Rot in the container HEADER is refused rather than healed.
///
/// The header is not erasure-protected and cannot be: it carries the parameters
/// the decoder needs before it can decode anything, including the symbol size and
/// the transfer length. Damage there is unrecoverable by construction, and the
/// honest response is a typed refusal — so this law also pins that the header is
/// NOT silently treated as a droppable symbol.
#[test]
fn rot_in_the_container_header_is_refused() {
    let dir = scratch_dir("header");
    under_lab(7, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let mut bytes = std::fs::read(&path).expect("read");
        // Byte 6 is inside object_kind, past the magic and the format word: a
        // field the decoder must trust and cannot cross-check.
        bytes[6] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .is_err(),
            "header damage is not recoverable and must not read as success"
        );
    });
}

/// Rot in the MAGIC is refused, and distinctly: a file that is not a capsule at
/// all is a different failure from a capsule whose contents rotted.
#[test]
fn a_destroyed_magic_is_refused() {
    let dir = scratch_dir("magic");
    under_lab(8, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let mut bytes = std::fs::read(&path).expect("read");
        bytes[0] ^= 0xff;
        std::fs::write(&path, &bytes).expect("write");

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert!(matches!(
            coordinator.read_capsule(cx, oid, &mut Vec::new()).await,
            Err(CommitError::Capsule(_))
        ));
    });
}

/// HEALED BYTES MUST RECOMPUTE THE REQUESTED IDENTITY (FG-INV-09).
///
/// Asserted against the coordinator's own derivation rather than a constant, so
/// this cannot pass by agreeing with a number the test invented. Recovery that
/// produced bytes with a different identity would be the one outcome worse than
/// failing: a content-addressed store returning content that is not the address.
#[test]
fn healed_bytes_still_recompute_their_identity() {
    let dir = scratch_dir("identity");
    under_lab(9, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        rot_symbols(&path, keys().profile().erasure_budget());

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let healed = coordinator
            .read_capsule(cx, oid, &mut Vec::new())
            .await
            .expect("heals");
        assert_eq!(
            coordinator.capsule_id(&healed),
            oid,
            "the recovered bytes must hash back to the identity that was asked for"
        );
    });
}

/// Damage is durable: healing a read does not repair the file, so a second read
/// faces the same damage and must give the same answer.
///
/// A read path that silently rewrote the file would turn a diagnostic into a
/// mutation, and would make every "beyond budget" measurement depend on how many
/// times the object had been read. Scrub is a separate, deliberate operation.
#[test]
fn healing_a_read_does_not_repair_the_file() {
    let dir = scratch_dir("nonrepair");
    under_lab(10, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        rot_symbols(&path, 1);
        let after_damage = std::fs::read(&path).expect("read container");

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .expect("heals"),
            plaintext()
        );
        assert_eq!(
            coordinator
                .read_capsule(cx, oid, &mut Vec::new())
                .await
                .expect("heals again"),
            plaintext()
        );
        assert_eq!(
            std::fs::read(&path).expect("read container"),
            after_damage,
            "reading healed the ANSWER, not the file"
        );
    });
}

/// Rot in the DECLARED REPAIR COUNT is refused, naming the mismatch.
///
/// Found by mutation: silencing the repair-budget check left this whole campaign
/// green, because every other law damages symbols or a field the decoder cannot
/// cross-check. That field CAN be cross-checked — against the closed profile
/// registry the authenticated `fec_profile` selects — and the check exists
/// precisely because surviving symbols do not reveal how many repair symbols
/// originally existed. A frame that got to state its own durability class could
/// claim a budget it never had, and the object would read as healthy while being
/// one flip from unrecoverable.
///
/// The field is located STRUCTURALLY, not by a hardcoded offset: `encode_container`
/// writes the repair count, then the symbol count, then the symbols, so the two
/// `u32`s immediately precede the first symbol's length prefix. The test asserts it
/// decoded the value it expected before touching it, so a layout change makes this
/// fail loudly rather than corrupt an unrelated field.
#[test]
fn rot_in_the_declared_repair_count_is_refused() {
    let dir = scratch_dir("budget-field");
    under_lab(11, move |cx| async move {
        let cx = &cx;
        let (path, oid) = committed(&dir, cx).await;
        let mut bytes = std::fs::read(&path).expect("read");
        let (descriptor, symbols) = decode_container(&bytes).expect("decodes");

        let first = symbols.first().expect("at least one symbol");
        let first_at = bytes
            .windows(first.len())
            .position(|w| w == first.as_slice())
            .expect("first symbol is present");
        // [ .. repair_symbols u32 | symbol_count u32 | len u32 | symbol .. ]
        let repair_at = first_at - 12;
        let found = u32::from_be_bytes(
            bytes[repair_at..repair_at + 4]
                .try_into()
                .expect("four bytes"),
        );
        assert_eq!(
            found, descriptor.repair_symbols,
            "the structural derivation must land on the repair count itself"
        );

        bytes[repair_at + 3] ^= 0x01;
        std::fs::write(&path, &bytes).expect("write");

        let coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert!(
            matches!(
                coordinator.read_capsule(cx, oid, &mut Vec::new()).await,
                Err(CommitError::Capsule(
                    fgdb_chronicle::capsule::CapsuleError::RepairBudgetMismatch { .. }
                ))
            ),
            "a frame may not restate its own durability class"
        );
    });
}
