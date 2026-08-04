//! What a campaign is allowed to conclude (plan §15.1, lines 1128 and 1140).
//!
//! > "DPOR is exhaustive only within the declared bounded scenario/state model
//! > and the soundness of its independence relation; broader campaigns remain
//! > falsification, not proof of bug absence." (line 1128)
//!
//! > "its reports are claim-typed falsification-only — **structurally
//! > incapable of asserting 'verified fault-free'**" (line 1140)
//!
//! MEASURED before writing this: `CampaignSummary` and `falsification` had
//! zero occurrences across `crates/`.
//!
//! # "Structurally incapable" is the whole specification
//!
//! A doc comment saying "do not claim fault-free" is not what line 1140 asks
//! for — it asks that the claim be *unrepresentable*. So [`CampaignOutcome`]
//! has no variant meaning "clean", and there is deliberately no
//! `is_bug_free()` or `passed()` for a caller to reach for. The closest a
//! campaign can come is [`CampaignOutcome::NotFalsified`], whose name is the
//! claim: nothing was found, under a named model, within a stated budget.
//!
//! The three outcomes are not three flavours of the same thing — they carry
//! **different claim classes**, and the plan requires them reported
//! separately:
//!
//! * [`ClaimClass::Falsification`] — a counterexample exists. The only
//!   outcome that proves anything unconditionally, and it proves a bug, never
//!   its absence.
//! * [`ClaimClass::Statistical`] — exploration stopped under a sampling
//!   policy. Says nothing about what was not explored.
//! * [`ClaimClass::BoundedFormal`] — the declared bounded state model was
//!   exhausted. **Still not "fault-free"**: it is exhaustive within the model
//!   and the soundness of its independence relation, and both are assumptions
//!   the campaign cannot discharge about itself.
//!
//! Reporting the third as if it were the absence of bugs is the specific
//! error line 1128 exists to forbid, which is why `BoundedExhausted` carries
//! the model it exhausted and renders it in every message.

/// What kind of claim an outcome supports. Never "verified".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimClass {
    /// A counterexample was found. Unconditional, and about a bug.
    Falsification,
    /// Stopped under a named sampling policy. Silent about the unexplored.
    Statistical,
    /// A declared bounded model was exhausted. Bounded, and conditional on
    /// the model and its independence relation.
    BoundedFormal,
}

impl ClaimClass {
    /// How strong a claim this class licenses, in one line, for a report
    /// header. Kept beside the variants so a summary cannot be rendered with
    /// a stronger gloss than its class allows.
    #[must_use]
    pub const fn licence(self) -> &'static str {
        match self {
            Self::Falsification => "a counterexample was found",
            Self::Statistical => {
                "nothing found under a sampling policy; the unexplored space is not characterised"
            }
            Self::BoundedFormal => {
                "the declared bounded model was exhausted; outside that model nothing is claimed"
            }
        }
    }
}

/// The complete set of things a campaign may conclude.
///
/// There is no "clean" or "passed" variant, and adding one would be the
/// defect: line 1140 requires that the assertion be unrepresentable rather
/// than merely discouraged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CampaignOutcome {
    /// A counterexample was found, with the replay that reproduces it.
    Falsified {
        /// The failing replay's encoded descriptor — enough to re-run it.
        replay: String,
        /// What kind of failure it was.
        failure_kind: String,
    },
    /// Exploration stopped without finding anything, under a named policy.
    NotFalsified {
        /// The sampling model the stop was taken under. Mandatory: a stop
        /// without a named model is an opinion.
        sampling_model: String,
        /// How many cases were explored.
        explored: u64,
    },
    /// The declared bounded state model was exhausted without a
    /// counterexample. Reported separately from `NotFalsified` because it is
    /// a different claim class, not a stronger version of the same one.
    BoundedExhausted {
        /// The model that was exhausted, named so the bound is legible.
        model: String,
        /// States covered within it.
        states: u64,
    },
}

impl CampaignOutcome {
    /// The claim class this outcome carries.
    #[must_use]
    pub const fn claim_class(&self) -> ClaimClass {
        match self {
            Self::Falsified { .. } => ClaimClass::Falsification,
            Self::NotFalsified { .. } => ClaimClass::Statistical,
            Self::BoundedExhausted { .. } => ClaimClass::BoundedFormal,
        }
    }

    /// Whether a counterexample was found.
    ///
    /// Note the asymmetry, which is deliberate: `true` is a fact about a bug.
    /// `false` is **not** a claim that none exists, and there is no method
    /// here that turns it into one.
    #[must_use]
    pub const fn found_counterexample(&self) -> bool {
        matches!(self, Self::Falsified { .. })
    }
}

impl std::fmt::Display for CampaignOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Falsified {
                replay,
                failure_kind,
            } => write!(f, "falsified: {failure_kind} — reproduce with {replay}"),
            Self::NotFalsified {
                sampling_model,
                explored,
            } => write!(
                f,
                "not falsified in {explored} cases under sampling model {sampling_model:?}; \
                 the unexplored space is not characterised"
            ),
            Self::BoundedExhausted { model, states } => write!(
                f,
                "bounded model {model:?} exhausted over {states} states; \
                 outside that model nothing is claimed"
            ),
        }
    }
}

/// Phrases a campaign report may never contain.
///
/// Exported so the guard is a shared artifact rather than a private habit of
/// one test: any future report surface can assert against the same list
/// instead of inventing its own and missing a phrase.
pub const FORBIDDEN_CLAIMS: &[&str] = &[
    "verified fault-free",
    "fault-free",
    "no bugs",
    "bug-free",
    "proven correct",
    "proves correctness",
    "guaranteed correct",
];
