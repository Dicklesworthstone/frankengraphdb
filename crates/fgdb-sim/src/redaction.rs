//! The fail-closed redaction contract (plan §15.1 line 1136).
//!
//! > "Production retains bounded mediated nondeterminism records under a
//! > **fail-closed redaction contract**. … **Crypto entropy is never
//! > recorded.** Full internal forensic bundles may be exact when policy
//! > permits; customer-safe bundles never overclaim byte identity."
//!
//! MEASURED before writing this: `redaction` appeared in `crates/` only inside
//! two doc comments. Nothing implemented it.
//!
//! # Two different words, and the difference is the whole module
//!
//! **"Fail-closed"** governs the classes we simply have not decided about: an
//! unrecognised or unconfigured class is **redacted**, never retained. A
//! contract that retained by default would leak every class someone forgot to
//! think about, and forgetting is the normal case as new record kinds appear.
//! So [`RedactionPolicy::fail_closed`] retains *nothing*, and retention is
//! opt-in per class.
//!
//! **"Never recorded"** is stronger and applies to exactly one class. Crypto
//! entropy is not a class we default to redacting — it is one that **cannot be
//! opted into**. [`RedactionPolicy::retain`] refuses it, so no configuration,
//! policy file, or "full internal forensic bundle" can turn it on. The plan
//! says *never*, and a policy knob that could flip it would make that a
//! default rather than a law.
//!
//! Keeping those two separate matters: if crypto entropy were merely
//! "redacted by default", the fix for a debugging session would be to retain
//! it, and someone would.
//!
//! # It plugs into the grading
//!
//! [`RedactionPolicy::withheld_classes`] produces exactly the list
//! [`crate::completeness::Recording::withheld_classes`] consumes, so a
//! redacted bundle is *automatically* barred from claiming byte identity —
//! `Replayable` requires nothing withheld. The two halves of §15.1's graded
//! replay are wired together rather than merely adjacent.

use std::collections::BTreeSet;

/// A kind of mediated-nondeterminism record a bundle might carry.
///
/// Mediated nondeterminism is the set of things a replay must be told because
/// it cannot re-derive them: what the scheduler chose, what the clock said,
/// which faults fired, when the network delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordClass {
    /// Which task the scheduler ran next.
    SchedulingDecision,
    /// Which faults the lab injected.
    FaultInjection,
    /// What the clock returned.
    ClockRead,
    /// When a message was delivered.
    NetworkDelivery,
    /// Bytes the user's own workload supplied.
    UserPayload,
    /// Secret entropy. **Never recordable** — see the module docs.
    CryptoEntropy,
}

impl RecordClass {
    /// The stable name used in a bundle's withheld list.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SchedulingDecision => "scheduling-decision",
            Self::FaultInjection => "fault-injection",
            Self::ClockRead => "clock-read",
            Self::NetworkDelivery => "network-delivery",
            Self::UserPayload => "user-payload",
            Self::CryptoEntropy => "crypto-entropy",
        }
    }

    /// Whether §15.1 forbids recording this class outright.
    ///
    /// Distinct from "redacted by default": a defaulted class can be turned
    /// on, and this one cannot.
    #[must_use]
    pub const fn is_never_recordable(self) -> bool {
        matches!(self, Self::CryptoEntropy)
    }

    /// Every class. Kept beside [`RecordClass::name`]'s exhaustive match so a
    /// new variant is a compile error there and a test failure here.
    pub const ALL: &'static [Self] = &[
        Self::SchedulingDecision,
        Self::FaultInjection,
        Self::ClockRead,
        Self::NetworkDelivery,
        Self::UserPayload,
        Self::CryptoEntropy,
    ];
}

/// Refusal to retain a class the plan forbids recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForbiddenRetention {
    /// The class that was asked for.
    pub class: RecordClass,
}

impl std::fmt::Display for ForbiddenRetention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} may never be recorded (plan §15.1 line 1136); this is not a default that policy can change",
            self.class.name()
        )
    }
}

impl std::error::Error for ForbiddenRetention {}

/// What happens to a class in a bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Kept in the bundle.
    Retained,
    /// Kept out, with the reason — a bundle that cannot say *why* a class is
    /// missing cannot be audited.
    Redacted {
        /// Why it was withheld.
        because: &'static str,
    },
}

/// Which record classes a bundle keeps.
///
/// Starts empty. Retention is opt-in, so a class nobody considered is
/// withheld rather than leaked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionPolicy {
    retained: BTreeSet<RecordClass>,
}

impl RedactionPolicy {
    /// A policy that retains nothing. The only constructor, so there is no
    /// "retain everything" starting point to drift from.
    #[must_use]
    pub fn fail_closed() -> Self {
        Self {
            retained: BTreeSet::new(),
        }
    }

    /// Opts `class` into retention.
    ///
    /// # Errors
    ///
    /// Returns [`ForbiddenRetention`] for a class §15.1 forbids recording.
    /// This is the law, not a policy default: there is deliberately no
    /// override, not even for internal forensic bundles.
    pub fn retain(mut self, class: RecordClass) -> Result<Self, ForbiddenRetention> {
        if class.is_never_recordable() {
            return Err(ForbiddenRetention { class });
        }
        self.retained.insert(class);
        Ok(self)
    }

    /// What this policy does with `class`.
    #[must_use]
    pub fn disposition(&self, class: RecordClass) -> Disposition {
        if class.is_never_recordable() {
            return Disposition::Redacted {
                because: "never recordable under plan §15.1",
            };
        }
        if self.retained.contains(&class) {
            return Disposition::Retained;
        }
        Disposition::Redacted {
            because: "not opted into retention; the contract is fail-closed",
        }
    }

    /// The classes this policy keeps out, by name, sorted.
    ///
    /// Feeds [`crate::completeness::Recording::withheld_classes`] directly, so
    /// a redacted bundle cannot grade as `Replayable`.
    /// Sorted, and that is load-bearing rather than tidy:
    /// [`crate::completeness::grade`] sorts the list it receives, so returning
    /// declaration order here would make an otherwise-correct grade compare
    /// unequal to the policy that produced it. Caught by
    /// `a_redacted_bundle_cannot_grade_as_replayable`.
    #[must_use]
    pub fn withheld_classes(&self) -> Vec<String> {
        let mut names: Vec<String> = RecordClass::ALL
            .iter()
            .filter(|class| self.disposition(**class) != Disposition::Retained)
            .map(|class| class.name().to_string())
            .collect();
        names.sort();
        names
    }
}
