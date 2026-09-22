//! Frozen host-side selection and typed source errors. These are selection
//! recipes, not secure-view constructors or capabilities.

use crate::{
    FnxBindError, FnxExecutionError, FnxExecutionLimits, FnxResult, ProjectionError,
    ProjectionLimits, ProjectionSpec,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitSeq, EId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MissingWeightPolicy {
    Reject = 0,
    Unit = 1,
    Zero = 2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxWeightSpec {
    /// Do not read any property value.
    Unit,
    /// Null and absent are both missing. Only finite Float or EXACTLY
    /// representable Int values are admitted; no text/boolean/decimal coercion.
    Property {
        key: PropertyKeyId,
        missing: MissingWeightPolicy,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxWeightError {
    Missing,
    NotNumeric,
    NonFinite,
    InexactInteger,
}
impl FnxWeightSpec {
    pub fn property_key(self) -> Option<PropertyKeyId> {
        match self {
            Self::Unit => None,
            Self::Property { key, .. } => Some(key),
        }
    }
    pub fn resolve(self, value: Option<&CanonicalScalar>) -> Result<f64, FnxWeightError> {
        let Self::Property { missing, .. } = self else {
            return Ok(1.0);
        };
        let value = match value {
            None | Some(CanonicalScalar::Null) => {
                return match missing {
                    MissingWeightPolicy::Reject => Err(FnxWeightError::Missing),
                    MissingWeightPolicy::Unit => Ok(1.0),
                    MissingWeightPolicy::Zero => Ok(0.0),
                };
            }
            Some(CanonicalScalar::Float(value)) => value.get(),
            Some(CanonicalScalar::Int(value)) => {
                let converted = *value as f64;
                // Use i128 for the round trip: f64(i64::MAX) is 2^63, whose
                // saturating cast back to i64 would otherwise falsely pass.
                if converted as i128 != i128::from(*value) {
                    return Err(FnxWeightError::InexactInteger);
                }
                converted
            }
            _ => return Err(FnxWeightError::NotNumeric),
        };
        if !value.is_finite() {
            return Err(FnxWeightError::NonFinite);
        }
        Ok(if value == 0.0 { 0.0 } else { value })
    }
}

/// An induced projection: retain visible vertices matching the optional label,
/// then visible edges matching the optional relation whose BOTH endpoints are
/// retained. Isolates remain. None means all in the already admitted snapshot;
/// it does not authorize a wider snapshot or cross a tenant/branch boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxSelection {
    pub vertex_label: Option<LabelId>,
    pub relation: Option<RelationId>,
    pub weight: FnxWeightSpec,
}
impl FnxSelection {
    pub fn digest(self) -> Digest {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:induced-selection:v1");
        match self.vertex_label {
            None => {
                hash.update(&[0]);
            }
            Some(label) => {
                hash.update(&[1]);
                hash.update(&label.0.to_le_bytes());
            }
        }
        match self.relation {
            None => {
                hash.update(&[0]);
            }
            Some(relation) => {
                hash.update(&[1]);
                hash.update(&relation.0.to_le_bytes());
            }
        }
        match self.weight {
            FnxWeightSpec::Unit => {
                hash.update(&[0]);
            }
            FnxWeightSpec::Property { key, missing } => {
                hash.update(&[1, missing as u8]);
                hash.update(&key.0.to_le_bytes());
                hash.update(b"finite-float-or-exact-int;null-is-missing");
            }
        }
        hash.finalize()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxSourceLimits {
    pub max_work_units: u64,
    /// Existing source visitor scratch admissions (e.g. historical winners),
    /// not a byte-exact allocator quota and not an external-memory path.
    pub max_scratch_entries: u64,
    /// Vertex-ID/weighted-edge staging backing stores, separate from the
    /// admitted snapshot, visitor scratch and projection builder's budget.
    pub max_staging_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnxReadOptions {
    /// None selects the supplied immutable view's frontier, not a moving head.
    pub as_of: Option<CommitSeq>,
    pub selection: FnxSelection,
    pub projection: ProjectionSpec,
    pub source_limits: FnxSourceLimits,
    pub projection_limits: ProjectionLimits,
    pub execution_limits: FnxExecutionLimits,
}

#[derive(Debug)]
pub enum FnxReadError<S, C> {
    Bind(FnxBindError),
    Read(S),
    Cancelled(C),
    Projection(ProjectionError),
    Execution(FnxExecutionError<C>),
    SourceLimit {
        resource: &'static str,
        limit: u128,
        requested: u128,
    },
    SizeOverflow,
    AllocationFailed,
    Weight {
        edge: EId,
        reason: FnxWeightError,
    },
}
impl<S: core::fmt::Display, C: core::fmt::Display> core::fmt::Display for FnxReadError<S, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::Cancelled(error) => write!(f, "Prism source cancelled: {error}"),
            Self::Projection(error) => error.fmt(f),
            Self::Execution(error) => error.fmt(f),
            Self::SourceLimit {
                resource,
                limit,
                requested,
            } => write!(
                f,
                "Prism source {resource} admission refused: {requested} > {limit}"
            ),
            Self::SizeOverflow => f.write_str("Prism source accounting overflow"),
            Self::AllocationFailed => f.write_str("Prism source allocation failed"),
            Self::Weight { reason, .. } => write!(f, "Prism selected weight refused: {reason:?}"),
        }
    }
}
impl<S: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error
    for FnxReadError<S, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Cancelled(error) => Some(error),
            Self::Projection(error) => Some(error),
            Self::Execution(error) => Some(error),
            _ => None,
        }
    }
}

/// Bind the frozen selection recipe as well as the actual projected data,
/// compiled call, returned rows and upstream witness. This is provenance, not
/// a signed authorization proof or a completed CGSE ledger publication.
#[derive(Clone, Debug, PartialEq)]
pub struct FnxReadResult {
    pub analytics: FnxResult,
    pub selection: FnxSelection,
    pub digest: Digest,
}
impl FnxReadResult {
    pub fn bind_selection(analytics: FnxResult, selection: FnxSelection) -> Self {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:embedded-read:v1");
        hash.update(&analytics.certificate.digest.0);
        hash.update(&selection.digest().0);
        Self {
            analytics,
            selection,
            digest: hash.finalize(),
        }
    }
}
