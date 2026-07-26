//! Mutation-proven properties for the three relation families in
//! `fgdb-resource`: checked algebra, ledger conservation, and atomic
//! transitions.
//!
//! Every property here has been proven RED under a named mutation of the
//! kernel it constrains — the mutation is recorded in the doc comment of the
//! property it kills, so a reader can re-run the proof rather than trust it.
//! A test that passes against a broken kernel proves nothing, and the
//! wrong-kernel family for this crate is specific: saturating instead of
//! rejecting at a boundary, dropping a release on an error path, and leaving a
//! multi-field update half applied.
//!
//! Inputs are deterministic and boundary-heavy. The generator is a seeded
//! SplitMix64 with the seed written into every failure message; there is no
//! clock, no entropy source, and no dependency beyond the crate under test.

use fgdb_resource::ledger::{
    ApplyDisposition, BucketState, ChargeId, DurableChargeAxis, DurableChargeVector,
    DurableVectorError, LedgerOperation, LedgerOperationKind, LedgerSubject, OwnerHold, QuotaPath,
    QuotaSegment, ReservationId, ResourceAccountingRole, ResourceBucketPolicy, ResourceClass,
    ResourceClassRule, ResourceLedger, ResourceLedgerIdentity, ResourceLedgerTransition,
    ResourceLimitPolicy, ResourceLimitPolicyEpoch, ResourceOwnerKey, StableSubjectKey,
    TransitionHeader, TransitionId,
};
use fgdb_resource::{ResourceAxis, ResourceCeiling, ResourceError, ResourceVector};
use fgdb_types::{DatabaseId, DatabaseSecurityNamespaceId, ObjectId};

// ---------------------------------------------------------------------------
// deterministic generator — seeded, reproducible, no clock and no entropy
// ---------------------------------------------------------------------------

/// SplitMix64. Fixed seeds only; every failure message carries the seed and
/// the draw index so a red run replays exactly.
struct Seeded {
    state: u64,
    seed: u64,
    draws: u64,
}

impl Seeded {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            seed,
            draws: 0,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.draws += 1;
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Boundary-heavy draw: two thirds of values land on or beside a limit,
    /// because the wrong kernels this suite hunts only misbehave there.
    fn boundary_u64(&mut self) -> u64 {
        const EDGES: [u64; 10] = [
            0,
            1,
            2,
            u64::MAX,
            u64::MAX - 1,
            u64::MAX / 2,
            u64::MAX / 2 + 1,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            1 << 63,
        ];
        let r = self.next_u64();
        if r.is_multiple_of(3) {
            r >> 40
        } else {
            EDGES[(r >> 8) as usize % EDGES.len()]
        }
    }

    fn context(&self) -> String {
        format!("seed {} draw {}", self.seed, self.draws)
    }
}

const SEEDS: [u64; 4] = [
    0x0000_0000_0000_0001,
    0x5eed_5eed_5eed_5eed,
    0xdead_beef,
    42,
];

fn rv(c: u64, m: u64, ib: u64, io: u64, n: u64) -> ResourceVector {
    ResourceVector {
        cpu_micros: c,
        memory_bytes: m,
        io_bytes: ib,
        io_ops: io,
        network_bytes: n,
    }
}

fn dv(bytes: u64, history: u64, branches: u64) -> DurableChargeVector {
    DurableChargeVector {
        canonical_durable_bytes: bytes,
        retained_history_bytes: history,
        branch_count: branches,
        index_count: 0,
        view_count: 0,
        subscription_count: 0,
    }
}

// ===========================================================================
// FAMILY 1 — CHECKED ALGEBRA
//
// The five-axis vector and its ceiling must SATURATE-FREE: every combining
// operation either returns the exact arithmetic result or a typed rejection
// naming the axis. Wrapping and clamping are both defects, and they are
// distinguishable — a clamp returns Ok with a wrong value, a wrap returns Ok
// with a very wrong value, and only rejection is correct.
// ===========================================================================

/// PROVEN RED BY: `ResourceVector::checked_add`'s kernel
/// `lhs.checked_add(rhs)` -> `Some(lhs.wrapping_add(rhs))`.
/// Under that mutation `u64::MAX + 1` returns `Ok(0)` and this fails on the
/// first axis. Also killed by `Some(lhs.saturating_add(rhs))`, which returns
/// `Ok(u64::MAX)`.
#[test]
fn checked_add_rejects_at_the_boundary_on_every_axis_and_never_wraps() {
    for (index, &axis) in ResourceAxis::ALL.iter().enumerate() {
        let mut at_max = ResourceVector::ZERO;
        let mut one = ResourceVector::ZERO;
        match axis {
            ResourceAxis::CpuMicros => {
                at_max.cpu_micros = u64::MAX;
                one.cpu_micros = 1;
            }
            ResourceAxis::MemoryBytes => {
                at_max.memory_bytes = u64::MAX;
                one.memory_bytes = 1;
            }
            ResourceAxis::IoBytes => {
                at_max.io_bytes = u64::MAX;
                one.io_bytes = 1;
            }
            ResourceAxis::IoOps => {
                at_max.io_ops = u64::MAX;
                one.io_ops = 1;
            }
            ResourceAxis::NetworkBytes => {
                at_max.network_bytes = u64::MAX;
                one.network_bytes = 1;
            }
        }
        let err = at_max
            .checked_add(one)
            .expect_err(&format!("axis {} at u64::MAX + 1 must reject", axis.name()));
        assert_eq!(
            err,
            ResourceError::Overflow {
                axis,
                lhs: u64::MAX,
                rhs: 1
            },
            "axis {} (index {index}) must name itself in the rejection",
            axis.name()
        );
        // u64::MAX + 0 is exactly representable and must still succeed.
        assert_eq!(
            at_max.checked_add(ResourceVector::ZERO).unwrap(),
            at_max,
            "adding ZERO at the boundary must stay exact"
        );
    }
}

/// PROVEN RED BY: `checked_sub`'s kernel
/// `held.checked_sub(released)` -> `Some(held.wrapping_sub(released))`.
/// Under that mutation `0 - 1` returns `Ok(u64::MAX)` — a released charge
/// becoming a near-infinite held balance, which is the exact accounting
/// catastrophe the typed rejection exists to prevent.
#[test]
fn checked_sub_rejects_below_zero_on_every_axis_and_never_wraps() {
    for &axis in &ResourceAxis::ALL {
        let mut one = ResourceVector::ZERO;
        match axis {
            ResourceAxis::CpuMicros => one.cpu_micros = 1,
            ResourceAxis::MemoryBytes => one.memory_bytes = 1,
            ResourceAxis::IoBytes => one.io_bytes = 1,
            ResourceAxis::IoOps => one.io_ops = 1,
            ResourceAxis::NetworkBytes => one.network_bytes = 1,
        }
        let err = ResourceVector::ZERO
            .checked_sub(one)
            .expect_err(&format!("axis {} at 0 - 1 must reject", axis.name()));
        assert_eq!(
            err,
            ResourceError::Underflow {
                axis,
                held: 0,
                released: 1
            },
            "axis {} must name itself in the underflow",
            axis.name()
        );
    }
}

/// The randomized companion to the two boundary tests: for every drawn pair,
/// the result is Ok exactly when the true sum fits, and when it is Ok it
/// equals the true sum computed in u128. This is what separates "rejects
/// sometimes" from "rejects exactly at the boundary".
///
/// PROVEN RED BY: the same `wrapping_add` mutation, which produces `Ok` with a
/// value differing from the u128 sum.
#[test]
fn checked_add_agrees_with_exact_u128_arithmetic_on_seeded_inputs() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        for _ in 0..2_000 {
            let a = rv(
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
            );
            let b = rv(
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
            );
            let fits = ResourceAxis::ALL
                .iter()
                .all(|&x| u128::from(a.axis(x)) + u128::from(b.axis(x)) <= u128::from(u64::MAX));
            match a.checked_add(b) {
                Ok(sum) => {
                    assert!(fits, "{}: accepted a sum that overflows", g.context());
                    for &x in &ResourceAxis::ALL {
                        assert_eq!(
                            u128::from(sum.axis(x)),
                            u128::from(a.axis(x)) + u128::from(b.axis(x)),
                            "{}: axis {} is not the exact sum",
                            g.context(),
                            x.name()
                        );
                    }
                }
                Err(ResourceError::Overflow { axis, lhs, rhs }) => {
                    assert!(!fits, "{}: rejected a representable sum", g.context());
                    assert_eq!(
                        lhs,
                        a.axis(axis),
                        "{}: lhs must be the operand",
                        g.context()
                    );
                    assert_eq!(
                        rhs,
                        b.axis(axis),
                        "{}: rhs must be the operand",
                        g.context()
                    );
                    assert!(
                        u128::from(lhs) + u128::from(rhs) > u128::from(u64::MAX),
                        "{}: named axis must be one that actually overflows",
                        g.context()
                    );
                }
                Err(other) => panic!("{}: wrong error variant {other:?}", g.context()),
            }
        }
    }
}

/// The ceiling is INCLUSIVE, so `requested == ceiling` is admitted and
/// `ceiling + 1` is rejected. This is the off-by-one a clamping kernel hides.
///
/// PROVEN RED BY: `ResourceCeiling::admit`'s comparison `req > ceil` -> `req >= ceil`,
/// which rejects the exactly-at-ceiling request this asserts is admitted.
/// Also killed by rewriting admit to clamp (`Ok(Admitted { vector: min(req, ceil) })`),
/// which admits the over-ceiling request.
#[test]
fn ceiling_admits_exactly_at_the_bound_and_rejects_one_past_it() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        for _ in 0..1_000 {
            let bound = rv(
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
            );
            let ceiling = ResourceCeiling::new(bound);
            let admitted = ceiling
                .admit(bound)
                .unwrap_or_else(|e| panic!("{}: exact bound must admit, got {e:?}", g.context()));
            assert_eq!(
                *admitted.vector(),
                bound,
                "{}: the admitted token must carry the request unchanged",
                g.context()
            );
            for &axis in &ResourceAxis::ALL {
                if bound.axis(axis) == u64::MAX {
                    continue; // no representable "one past" on this axis
                }
                let mut over = bound;
                match axis {
                    ResourceAxis::CpuMicros => over.cpu_micros += 1,
                    ResourceAxis::MemoryBytes => over.memory_bytes += 1,
                    ResourceAxis::IoBytes => over.io_bytes += 1,
                    ResourceAxis::IoOps => over.io_ops += 1,
                    ResourceAxis::NetworkBytes => over.network_bytes += 1,
                }
                let err = ceiling.admit(over).expect_err(&format!(
                    "{}: one past the bound on {} must reject",
                    g.context(),
                    axis.name()
                ));
                assert_eq!(
                    err,
                    ResourceError::CeilingExceeded {
                        axis,
                        requested: over.axis(axis),
                        ceiling: bound.axis(axis)
                    },
                    "{}: the rejection must name the axis that was exceeded",
                    g.context()
                );
            }
        }
    }
}

/// `fits_within` and `admit` are two spellings of one partial order and must
/// never disagree. A kernel that clamps one but not the other splits them.
///
/// PROVEN RED BY: `fits_within`'s `<=` -> `<`, which desynchronizes the two at
/// the exact bound.
#[test]
fn fits_within_and_admit_decide_identically_on_seeded_inputs() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        for _ in 0..2_000 {
            let bound = rv(
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
            );
            let request = rv(
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
                g.boundary_u64(),
            );
            let ceiling = ResourceCeiling::new(bound);
            assert_eq!(
                request.fits_within(&bound),
                ceiling.admit(request).is_ok(),
                "{}: fits_within and admit disagreed on {request:?} under {bound:?}",
                g.context()
            );
        }
    }
}

/// The rejection names the FIRST violated axis in `ResourceAxis::ALL` order,
/// which is what makes the diagnostic deterministic across runs.
///
/// PROVEN RED BY: iterating `ResourceAxis::ALL.iter().rev()` in `admit`, which
/// still rejects every over-ceiling request but names the last violated axis.
/// A suite that only asserts `is_err()` cannot see this mutation at all.
#[test]
fn ceiling_rejection_names_the_first_violated_axis_in_declared_order() {
    let ceiling = ResourceCeiling::new(rv(10, 10, 10, 10, 10));
    // Every axis violated at once: the first in ALL order must win.
    let err = ceiling.admit(rv(11, 11, 11, 11, 11)).unwrap_err();
    assert_eq!(
        err,
        ResourceError::CeilingExceeded {
            axis: ResourceAxis::CpuMicros,
            requested: 11,
            ceiling: 10
        }
    );
    // Violate all but the first: the second must win, and so on down.
    let expected = [
        (rv(1, 11, 11, 11, 11), ResourceAxis::MemoryBytes),
        (rv(1, 1, 11, 11, 11), ResourceAxis::IoBytes),
        (rv(1, 1, 1, 11, 11), ResourceAxis::IoOps),
        (rv(1, 1, 1, 1, 11), ResourceAxis::NetworkBytes),
    ];
    for (request, axis) in expected {
        assert_eq!(
            ceiling.admit(request).unwrap_err(),
            ResourceError::CeilingExceeded {
                axis,
                requested: 11,
                ceiling: 10
            },
            "declared-order scan must name {}",
            axis.name()
        );
    }
}

/// The durable six-axis vector is a second checked algebra with the same
/// obligation, and it is the one the ledger actually runs on.
///
/// PROVEN RED BY: `DurableChargeVector::checked_add`'s per-axis
/// `checked_add(..).ok_or(..)` -> `wrapping_add`, on any axis.
#[test]
fn durable_charge_vector_rejects_overflow_on_every_one_of_its_six_axes() {
    for &axis in &DurableChargeAxis::ALL {
        let mut at_max = DurableChargeVector::ZERO;
        let mut one = DurableChargeVector::ZERO;
        for (target, source) in [(&mut at_max, u64::MAX), (&mut one, 1)] {
            match axis {
                DurableChargeAxis::CanonicalDurableBytes => target.canonical_durable_bytes = source,
                DurableChargeAxis::RetainedHistoryBytes => target.retained_history_bytes = source,
                DurableChargeAxis::BranchCount => target.branch_count = source,
                DurableChargeAxis::IndexCount => target.index_count = source,
                DurableChargeAxis::ViewCount => target.view_count = source,
                DurableChargeAxis::SubscriptionCount => target.subscription_count = source,
            }
        }
        let err = at_max.checked_add(one).expect_err("overflow must reject");
        assert!(
            matches!(err, DurableVectorError::Overflow { axis: a, .. } if a == axis),
            "durable axis must name itself, got {err:?}"
        );
        let err = DurableChargeVector::ZERO
            .checked_sub(one)
            .expect_err("underflow must reject");
        assert!(
            matches!(err, DurableVectorError::Underflow { axis: a, .. } if a == axis),
            "durable axis must name itself, got {err:?}"
        );
    }
}

// ===========================================================================
// FAMILY 2 — LEDGER CONSERVATION
//
// Charges and releases balance. A charge/release pair returns the ledger's
// accounting state to exactly where it started, and no path may retire an
// entry without removing its contribution from the bucket.
// ===========================================================================

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([7u8; 32])
}

fn root_path() -> QuotaPath {
    QuotaPath::try_new(vec![QuotaSegment::Database(DatabaseId([1u8; 16]))]).expect("root path")
}

fn identity() -> ResourceLedgerIdentity {
    ResourceLedgerIdentity {
        database_security_namespace_id: namespace(),
        cluster_incarnation: [3u8; 16],
        role: ResourceAccountingRole::Meta,
        limit_policy_oid: ObjectId([9u8; 32]),
        limit_policy_epoch: 1,
    }
}

fn policy() -> ResourceLimitPolicy {
    let bucket = ResourceBucketPolicy::try_new(dv(1_000_000, 1_000_000, 1_000), dv(0, 0, 0))
        .expect("bucket policy");
    let all_ops = [
        LedgerOperationKind::Reserve,
        LedgerOperationKind::Charge,
        LedgerOperationKind::Release,
        LedgerOperationKind::Expire,
        LedgerOperationKind::Transfer,
        LedgerOperationKind::Adjust,
    ];
    ResourceLimitPolicy::try_new(
        ResourceLimitPolicyEpoch(1),
        [(root_path(), bucket)],
        [
            ResourceClassRule::try_new(ResourceClass::Ordinary, all_ops).expect("ordinary rule"),
            ResourceClassRule::try_new(ResourceClass::RegisteredMaintenance, all_ops)
                .expect("maintenance rule"),
        ],
        [],
    )
    .expect("policy")
}

fn ledger() -> ResourceLedger {
    ResourceLedger::try_new(identity(), policy()).expect("ledger")
}

fn owner(tag: u8) -> ResourceOwnerKey {
    ResourceOwnerKey::Attempt {
        posture: 1,
        registration_identity: [tag; 32],
    }
}

/// The owner is deliberately INDEPENDENT of the transition id: a charge or
/// release must carry the same owner as the entry it targets, while every
/// transition needs its own id. Deriving both from one tag makes each
/// follow-up transition change owners and fail `OwnerMismatch` — which is the
/// binding law doing its job, not a ledger defect.
const SEQUENCE_OWNER: u8 = 0xA1;

fn header(generation: u64, tag: u8) -> TransitionHeader {
    TransitionHeader {
        expected_ledger_identity: identity(),
        transition_id: TransitionId([tag; 32]),
        idempotency_key_digest: [tag; 32],
        expected_ledger_generation: generation,
        owner: owner(SEQUENCE_OWNER),
        resource_class: ResourceClass::Ordinary,
        quota_path: root_path(),
    }
}

fn transition(generation: u64, tag: u8, operation: LedgerOperation) -> ResourceLedgerTransition {
    ResourceLedgerTransition {
        header: header(generation, tag),
        operation,
        body_digest: [tag; 32],
    }
}

/// A bucket's whole accounting state, used as the conservation observable.
/// Comparing the entire `BucketState` rather than one axis is deliberate: a
/// kernel that releases the committed component but forgets the reserved one
/// balances on the axis you happen to check and not on the state.
fn bucket_of(ledger: &ResourceLedger) -> BucketState {
    *ledger.bucket(&root_path()).expect("root bucket")
}

/// The core conservation law: reserve, charge, release — and the bucket is
/// byte-identical to where it started. Run over seeded vectors so it is not a
/// single lucky path.
///
/// PROVEN RED BY: deleting `self.install_bucket_plan(plan);` from
/// `apply_release`, i.e. the wrong kernel that retires the entry but forgets
/// to give the quota back. The entry disappears from `charges` so a
/// leak-blind test still passes; this one fails because the bucket never
/// returns to its start state.
#[test]
fn reserve_charge_release_returns_the_bucket_to_its_exact_starting_state() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        for round in 0..64u8 {
            let mut led = ledger();
            let start = bucket_of(&led);
            let vector = dv(
                1 + g.next_u64() % 4_096,
                g.next_u64() % 4_096,
                g.next_u64() % 8,
            );
            let reservation = ReservationId([round; 32]);
            let charge = ChargeId([round.wrapping_add(70); 32]);

            led.apply(&transition(
                0,
                round,
                LedgerOperation::Reserve {
                    reservation_id: reservation,
                    vector,
                    hold: OwnerHold::None,
                },
            ))
            .unwrap_or_else(|e| panic!("{}: reserve failed: {e:?}", g.context()));
            assert_ne!(
                bucket_of(&led),
                start,
                "{}: a reserve must actually move the bucket, or the test proves nothing",
                g.context()
            );

            led.apply(&transition(
                1,
                round.wrapping_add(1),
                LedgerOperation::Charge {
                    reservation_id: reservation,
                    expected_reservation_generation: 0,
                    charge_id: charge,
                    vector,
                    stable_subject_key: StableSubjectKey([round; 32]),
                },
            ))
            .unwrap_or_else(|e| panic!("{}: charge failed: {e:?}", g.context()));

            led.apply(&transition(
                2,
                round.wrapping_add(2),
                LedgerOperation::Release {
                    subject: LedgerSubject::Charge(charge),
                    expected_generation: 0,
                    exact_vector: vector,
                },
            ))
            .unwrap_or_else(|e| panic!("{}: release failed: {e:?}", g.context()));

            assert_eq!(
                bucket_of(&led),
                start,
                "{}: reserve+charge+release did not conserve the bucket",
                g.context()
            );
            assert!(
                led.charge(charge).is_none(),
                "{}: the released charge must be retired",
                g.context()
            );
        }
    }
}

/// Conservation must hold over an interleaved multi-entry sequence, not just
/// one pair — a kernel can conserve each pair in isolation and still drift
/// when several entries share a bucket.
///
/// PROVEN RED BY: the same deleted `install_bucket_plan` in `apply_release`,
/// and independently by making `apply_release` subtract
/// `DurableChargeVector::ZERO` instead of the entry's vector.
#[test]
fn interleaved_reserve_and_release_sequences_conserve_the_bucket() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        let mut led = ledger();
        let start = bucket_of(&led);
        let mut live: Vec<(ReservationId, DurableChargeVector)> = Vec::new();
        let mut generation = 0u64;
        let mut tag = 0u8;

        for _ in 0..48 {
            let open = live.len();
            let should_reserve = open == 0 || (g.next_u64().is_multiple_of(2) && open < 6);
            if should_reserve {
                let vector = dv(1 + g.next_u64() % 512, g.next_u64() % 512, 0);
                let id = ReservationId([tag; 32]);
                let applied = led.apply(&transition(
                    generation,
                    tag,
                    LedgerOperation::Reserve {
                        reservation_id: id,
                        vector,
                        hold: OwnerHold::None,
                    },
                ));
                if applied.is_ok() {
                    live.push((id, vector));
                    generation += 1;
                }
            } else {
                let (id, vector) = live.remove((g.next_u64() as usize) % open);
                led.apply(&transition(
                    generation,
                    tag,
                    LedgerOperation::Release {
                        subject: LedgerSubject::Reservation(id),
                        expected_generation: 0,
                        exact_vector: vector,
                    },
                ))
                .unwrap_or_else(|e| panic!("{}: release failed: {e:?}", g.context()));
                generation += 1;
            }
            tag = tag.wrapping_add(1);
        }

        // Drain whatever is still open; the bucket must land exactly at start.
        while let Some((id, vector)) = live.pop() {
            led.apply(&transition(
                generation,
                tag,
                LedgerOperation::Release {
                    subject: LedgerSubject::Reservation(id),
                    expected_generation: 0,
                    exact_vector: vector,
                },
            ))
            .unwrap_or_else(|e| panic!("{}: drain release failed: {e:?}", g.context()));
            generation += 1;
            tag = tag.wrapping_add(1);
        }
        assert_eq!(
            bucket_of(&led),
            start,
            "{}: interleaved sequence leaked quota",
            g.context()
        );
    }
}

/// A release naming the wrong vector must be REJECTED, not silently partially
/// applied. This is the conservation law's contrapositive: the ledger may not
/// accept a release it cannot balance.
///
/// PROVEN RED BY: relaxing the exact-vector binding check in
/// `validate_entry_binding` (accepting any vector that `fits_within` the
/// entry's), which lets an under-release retire the entry while leaving quota
/// charged — a leak that conserves nothing and reports success.
#[test]
fn a_release_with_the_wrong_vector_is_rejected_and_changes_nothing() {
    let mut led = ledger();
    let vector = dv(1_000, 500, 1);
    let reservation = ReservationId([1u8; 32]);
    led.apply(&transition(
        0,
        1,
        LedgerOperation::Reserve {
            reservation_id: reservation,
            vector,
            hold: OwnerHold::None,
        },
    ))
    .expect("reserve");
    let after_reserve = led.clone();

    for wrong in [
        dv(999, 500, 1),
        dv(1_001, 500, 1),
        dv(1_000, 499, 1),
        DurableChargeVector::ZERO,
    ] {
        let outcome = led.apply(&transition(
            1,
            2,
            LedgerOperation::Release {
                subject: LedgerSubject::Reservation(reservation),
                expected_generation: 0,
                exact_vector: wrong,
            },
        ));
        assert!(
            outcome.is_err(),
            "a release naming {wrong:?} instead of {vector:?} must be rejected"
        );
        assert_eq!(
            led, after_reserve,
            "a rejected release must leave the ledger untouched"
        );
    }
}

// ===========================================================================
// FAMILY 3 — ATOMIC TRANSITIONS
//
// A transition either fully applies or does not apply. No partially-applied
// state may be observable, which for a multi-field update means: on any
// rejection the WHOLE ledger — buckets, entries, id sets, generation — is
// byte-identical to its pre-state.
// ===========================================================================

/// The all-or-nothing law, asserted on the whole ledger rather than on the
/// field a given rejection happens to touch. `ResourceLedger` derives
/// `PartialEq`, so equality here covers buckets, reservations, charges, both
/// used-id sets, the applied-transition map and the generation counter at
/// once — which is what makes it able to see a half-applied update.
///
/// PROVEN RED BY: hoisting `self.used_reservation_ids.insert(reservation_id);`
/// in `apply_reserve` to before the `plan_bucket_changes(..)?` call — the
/// classic wrong kernel that records one field of a multi-field update before
/// the operation can still fail. Every existing test still passes under it,
/// because the id set is not otherwise observable; this fails because the
/// rejected transition leaves the set mutated.
#[test]
fn every_rejected_transition_leaves_the_whole_ledger_byte_identical() {
    let mut led = ledger();
    // Seed some state so "unchanged" is a real claim and not vacuously true
    // over an empty ledger.
    led.apply(&transition(
        0,
        1,
        LedgerOperation::Reserve {
            reservation_id: ReservationId([1u8; 32]),
            vector: dv(4_096, 2_048, 2),
            hold: OwnerHold::None,
        },
    ))
    .expect("seed reserve");
    assert_ne!(led, ledger(), "the seed must have changed the ledger");

    let rejects: Vec<(&str, ResourceLedgerTransition)> = vec![
        (
            "zero vector",
            transition(
                1,
                2,
                LedgerOperation::Reserve {
                    reservation_id: ReservationId([2u8; 32]),
                    vector: DurableChargeVector::ZERO,
                    hold: OwnerHold::None,
                },
            ),
        ),
        (
            "reservation id already used",
            transition(
                1,
                3,
                LedgerOperation::Reserve {
                    reservation_id: ReservationId([1u8; 32]),
                    vector: dv(1, 0, 0),
                    hold: OwnerHold::None,
                },
            ),
        ),
        (
            "over the bucket hard limit",
            transition(
                1,
                4,
                LedgerOperation::Reserve {
                    reservation_id: ReservationId([4u8; 32]),
                    vector: dv(u64::MAX, 0, 0),
                    hold: OwnerHold::None,
                },
            ),
        ),
        (
            "stale ledger generation",
            transition(
                99,
                5,
                LedgerOperation::Reserve {
                    reservation_id: ReservationId([5u8; 32]),
                    vector: dv(1, 0, 0),
                    hold: OwnerHold::None,
                },
            ),
        ),
        (
            "charge against an unknown reservation",
            transition(
                1,
                6,
                LedgerOperation::Charge {
                    reservation_id: ReservationId([200u8; 32]),
                    expected_reservation_generation: 0,
                    charge_id: ChargeId([6u8; 32]),
                    vector: dv(1, 0, 0),
                    stable_subject_key: StableSubjectKey([6u8; 32]),
                },
            ),
        ),
        (
            "release of an unknown subject",
            transition(
                1,
                7,
                LedgerOperation::Release {
                    subject: LedgerSubject::Reservation(ReservationId([201u8; 32])),
                    expected_generation: 0,
                    exact_vector: dv(1, 0, 0),
                },
            ),
        ),
        (
            "release naming the wrong generation",
            transition(
                1,
                8,
                LedgerOperation::Release {
                    subject: LedgerSubject::Reservation(ReservationId([1u8; 32])),
                    expected_generation: 77,
                    exact_vector: dv(4_096, 2_048, 2),
                },
            ),
        ),
    ];

    for (name, bad) in rejects {
        let before = led.clone();
        let outcome = led.apply(&bad);
        assert!(
            outcome.is_err(),
            "transition {name:?} was expected to be rejected but applied"
        );
        assert_eq!(
            led, before,
            "transition {name:?} was rejected but left the ledger changed — \
             a partially-applied state is observable"
        );
    }
}

/// A rejected transition must not consume its transition id either: the same
/// id must remain replayable once the transition is corrected. A kernel that
/// records the transition before running it would pass the state-equality test
/// above only if the record were part of the compared state — it is, and this
/// asserts the consequence directly.
///
/// PROVEN RED BY: the Reserve dispatch arm swallowing its error —
/// `self.apply_reserve(..)?;` -> `let _ = self.apply_reserve(..);` — which
/// records a rejected transition as applied. The corrected transition then
/// comes back as an idempotency drift error instead of applying.
#[test]
fn a_rejected_transition_does_not_consume_its_id_and_can_be_corrected() {
    let mut led = ledger();
    let bad = transition(
        0,
        1,
        LedgerOperation::Reserve {
            reservation_id: ReservationId([1u8; 32]),
            vector: DurableChargeVector::ZERO, // rejected: zero vector
            hold: OwnerHold::None,
        },
    );
    assert!(led.apply(&bad).is_err(), "zero vector must be rejected");

    // Same transition id, now well formed.
    let good = ResourceLedgerTransition {
        header: bad.header.clone(),
        operation: LedgerOperation::Reserve {
            reservation_id: ReservationId([1u8; 32]),
            vector: dv(16, 0, 0),
            hold: OwnerHold::None,
        },
        body_digest: [1u8; 32],
    };
    let outcome = led
        .apply(&good)
        .expect("the corrected transition must apply under the same id");
    assert_eq!(
        outcome.disposition,
        ApplyDisposition::Applied,
        "a corrected transition must APPLY, not replay a rejection"
    );
    assert_eq!(led.generation(), 1, "exactly one transition has applied");
}

/// Idempotent replay is the other half of atomicity: re-applying an identical
/// transition reports `Replayed` and does not double-charge the bucket.
///
/// PROVEN RED BY: deleting the `applied_transitions` early-return in `apply`,
/// which re-runs the operation and charges the bucket twice.
#[test]
fn replaying_an_identical_transition_does_not_double_apply_it() {
    let mut led = ledger();
    let t = transition(
        0,
        1,
        LedgerOperation::Reserve {
            reservation_id: ReservationId([1u8; 32]),
            vector: dv(64, 32, 1),
            hold: OwnerHold::None,
        },
    );
    let first = led.apply(&t).expect("first apply");
    assert_eq!(first.disposition, ApplyDisposition::Applied);
    let after_first = led.clone();

    for _ in 0..3 {
        let again = led.apply(&t).expect("replay must succeed");
        assert_eq!(
            again.disposition,
            ApplyDisposition::Replayed,
            "a repeated transition id must report Replayed"
        );
        assert_eq!(
            led, after_first,
            "a replay must not change the ledger at all"
        );
    }
}

/// Atomicity under a seeded storm of mostly-invalid transitions: whatever the
/// mix, the ledger after each rejection equals the ledger before it, and the
/// generation counter advances by exactly the number of accepted transitions.
///
/// PROVEN RED BY: the hoisted-`used_reservation_ids` mutation named above, and
/// by bumping `self.generation` before the operation dispatch rather than
/// after a successful one.
#[test]
fn seeded_transition_storm_never_exposes_a_partial_state() {
    for &seed in &SEEDS {
        let mut g = Seeded::new(seed);
        let mut led = ledger();
        let mut applied = 0u64;

        for step in 0..256u32 {
            let tag = (step % 251) as u8;
            let choice = g.next_u64() % 6;
            let vector = match choice {
                0 => DurableChargeVector::ZERO,        // rejected
                1 => dv(u64::MAX, 0, 0),               // over the hard limit
                2 => dv(u64::MAX, u64::MAX, u64::MAX), // over on every axis
                _ => dv(1 + g.next_u64() % 256, 0, 0), // usually valid
            };
            // Deliberately mix in stale generations so the rejection set is
            // not dominated by one code path.
            let generation = if g.next_u64().is_multiple_of(4) {
                led.generation().wrapping_add(7)
            } else {
                led.generation()
            };
            let t = transition(
                generation,
                tag,
                LedgerOperation::Reserve {
                    reservation_id: ReservationId([tag; 32]),
                    vector,
                    hold: OwnerHold::None,
                },
            );
            let before = led.clone();
            match led.apply(&t) {
                Ok(outcome) => {
                    if outcome.disposition == ApplyDisposition::Applied {
                        applied += 1;
                        assert_ne!(
                            led,
                            before,
                            "{}: an applied transition must change the ledger",
                            g.context()
                        );
                    } else {
                        assert_eq!(
                            led,
                            before,
                            "{}: a replay must not change the ledger",
                            g.context()
                        );
                    }
                }
                Err(_) => assert_eq!(
                    led,
                    before,
                    "{}: a rejected transition left a partial state at step {step}",
                    g.context()
                ),
            }
            assert_eq!(
                led.generation(),
                applied,
                "{}: generation must count exactly the applied transitions",
                g.context()
            );
        }
        assert!(
            applied > 0,
            "{}: the storm must apply something, or it proves nothing",
            g.context()
        );
    }
}
