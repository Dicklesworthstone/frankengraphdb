//! Warden's first-party capability boundary (plan §12.1, FG-INV-20).
//!
//! A [`CapabilityToken`] is untrusted bearer material. Only [`Authority`]
//! constructs a [`VerifiedCapability`], after checking the foundation's HMAC
//! chain, the signed database/graph/catalog/policy identity, and every caveat.
//! Holders may append restrictions without an issuer key. Restrictions are
//! conjunctive; neither a later wildcard nor a larger budget restores rights.
//!
//! Clock values passed to the `*_at` methods MUST come from the trusted host's
//! `Cx` clock in the same epoch as issuance, never from request payloads. These
//! methods are pure: they do not acquire locks, read clocks, or perform I/O.
//! Long-running operators must recheck at their existing cancellation/work
//! checkpoints. This keeps the boundary usable with deterministic lab time.
//!
//! # Integration boundary
//!
//! This crate compiles authorization predicates; it does not install them in
//! existing `fgdb` sessions. The host must keep the raw database and issuer key
//! out of untrusted callers' reach and attach these predicates to ALL sources:
//! vertex/edge scans, every path transit vertex, descriptor/degree access,
//! properties, index candidates, aggregations, and updates. In particular,
//! filtering final result rows does not satisfy FG-INV-20.
//!
//! First-party restrictions are the supported final-abstraction subset.
//! Third-party discharges, runtime region/task constraints, use-count/rate
//! limits and unknown caveats are rejected, not ignored. Budgets here are
//! per-execution ceilings, NOT lifetime quotas. Revocation is an authority
//! policy-epoch change; this crate does not claim per-token revocation.

#![forbid(unsafe_code)]

mod planner;
mod token;

pub use planner::{ExecutionPermit, PlannerPredicates, ReadAccess, Usage, WriteAccess};
pub use token::{Authority, CapabilityToken, VerifiedCapability};

use core::fmt;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use std::collections::BTreeSet;

/// Bound ingress before asking the foundation decoder to allocate anything.
pub const MAX_TOKEN_BYTES: usize = 32 * 1024;
/// Matches the pinned foundation decoder's maximum caveat count.
pub const MAX_CAVEATS: usize = 64;
pub const MAX_SCOPE_ORDINALS: usize = 256;
pub const MAX_NAME_BYTES: usize = 256;
pub(crate) const MAX_VALUE_BYTES: usize = 8192;

/// An explicit universe or a finite set. An empty `Only` means DENY ALL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope<T: Ord> {
    All,
    Only(BTreeSet<T>),
}

impl<T: Ord> Scope<T> {
    #[must_use]
    pub fn only(values: impl IntoIterator<Item = T>) -> Self {
        Self::Only(values.into_iter().collect())
    }

    #[must_use]
    pub fn contains(&self, value: &T) -> bool {
        match self {
            Self::All => true,
            Self::Only(values) => values.contains(value),
        }
    }
}

impl<T: Ord> Scope<T> {
    pub(crate) fn intersect(&mut self, other: Self) {
        match (&mut *self, other) {
            (_, Self::All) => {}
            (target @ Self::All, other) => *target = other,
            (Self::Only(current), Self::Only(next)) => {
                current.retain(|value| next.contains(value));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rights {
    None,
    Read,
    Write,
    ReadWrite,
}

impl Rights {
    #[must_use]
    pub const fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    #[must_use]
    pub const fn can_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    pub(crate) const fn bits(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Read => 1,
            Self::Write => 2,
            Self::ReadWrite => 3,
        }
    }

    pub(crate) fn from_bits(bits: u64) -> Result<Self, Error> {
        match bits {
            0 => Ok(Self::None),
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            3 => Ok(Self::ReadWrite),
            _ => Err(Error::Malformed),
        }
    }

    pub(crate) fn intersect(self, other: Self) -> Self {
        match self.bits() & other.bits() {
            0 => Self::None,
            1 => Self::Read,
            2 => Self::Write,
            _ => Self::ReadWrite,
        }
    }
}

/// Ceilings for one execution. Zero is a real zero allowance, not unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLimits {
    /// Number of vertex admissions, including repeated admissions.
    pub max_nodes: u64,
    /// Host-defined deterministic work units, charged BEFORE doing the work.
    pub max_work: u64,
    /// Rows emitted, charged BEFORE exposing a row.
    pub max_rows: u64,
}

/// Issuer-side input. Construction starts with no visible graph objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub branch: String,
    pub labels: Scope<LabelId>,
    pub relations: Scope<RelationId>,
    pub properties: Scope<PropertyKeyId>,
    pub rights: Rights,
    pub limits: QueryLimits,
    pub expires_at_ms: u64,
}

impl Grant {
    #[must_use]
    pub fn read_only(branch: impl Into<String>, expires_at_ms: u64, limits: QueryLimits) -> Self {
        Self {
            branch: branch.into(),
            labels: Scope::Only(BTreeSet::new()),
            relations: Scope::Only(BTreeSet::new()),
            properties: Scope::Only(BTreeSet::new()),
            rights: Rights::Read,
            limits,
            expires_at_ms,
        }
    }
}

/// Each appended restriction is ANDed with all earlier restrictions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restriction {
    Branch(String),
    /// Each finite set is an any-of label test; multiple tests remain ANDed.
    Labels(Scope<LabelId>),
    Relations(Scope<RelationId>),
    Properties(Scope<PropertyKeyId>),
    DenyProperties(BTreeSet<PropertyKeyId>),
    Rights(Rights),
    MaxNodes(u64),
    MaxWork(u64),
    MaxRows(u64),
    ExpiresBefore(u64),
    NotBefore(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitDimension {
    Nodes,
    Work,
    Rows,
}

/// Diagnostics contain neither token bytes nor signing/attenuation keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Malformed,
    TooLarge,
    UnsupportedCaveat,
    Unauthenticated,
    WrongAuthority,
    MissingRestriction,
    ScopeDenied,
    Expired,
    NotYetValid,
    PermissionDenied,
    LimitExceeded(LimitDimension),
    ExecutionStopped,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Malformed => "malformed or noncanonical Warden token",
            Self::TooLarge => "Warden input exceeds a bounded format limit",
            Self::UnsupportedCaveat => "unsupported Warden caveat",
            Self::Unauthenticated => "Warden signature verification failed",
            Self::WrongAuthority => "Warden authority identity mismatch",
            Self::MissingRestriction => "Warden token lacks a required root restriction",
            Self::ScopeDenied => "Warden scope denied",
            Self::Expired => "Warden capability expired",
            Self::NotYetValid => "Warden capability is not yet valid",
            Self::PermissionDenied => "Warden operation denied",
            Self::LimitExceeded(_) => "Warden execution budget exceeded",
            Self::ExecutionStopped => "Warden execution already stopped",
        };
        f.write_str(message)
    }
}

impl core::error::Error for Error {}

pub(crate) fn validate_name(value: &str) -> Result<(), Error> {
    if value.len() > MAX_NAME_BYTES {
        return Err(Error::TooLarge);
    }
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(Error::Malformed);
    }
    Ok(())
}
