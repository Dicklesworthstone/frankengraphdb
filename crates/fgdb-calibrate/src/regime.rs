//! Identity-bound regime signals over asupersync's change-point monitor.
//!
//! Asupersync owns the detector implementations and their integer arithmetic.
//! This module installs the foundation's Page-Hinkley, upward CUSUM, and
//! downward CUSUM detectors in one stable registration order, then binds that
//! combined signal to immutable FrankenGraphDB identities, an exact stream
//! window, bounded receipt retention, and a pinned deterministic fallback.
//!
//! A detector receipt is advisory statistical evidence. It is neither proof
//! that the workload changed nor proof that an interval before the receipt
//! satisfied a model assumption. The only policy action produced here is the
//! conservative transition from the named candidate to its pinned fallback.

use core::fmt;

use asupersync::runtime::changepoint::{
    ChangeDirection as FoundationDirection, ChangePointDetection, ChangePointDetectorKind,
    ChangePointMonitor, ChangePointMonitorConfig, ChangePointSeriesConfig, ChangePointSnapshot,
};
pub use asupersync::runtime::changepoint::{
    CusumConfig, MetricSample, PageHinkleyConfig, RuntimeMetricSeries,
};
use fgdb_types::ObjectId;

/// Stable identity of the combined asupersync change-point signal.
pub const COMBINED_REGIME_SIGNAL_ID: &str = "asupersync.runtime.changepoint.combined";

/// Version of the detector composition and deterministic registration order.
pub const COMBINED_REGIME_SIGNAL_VERSION: u32 = 1;

/// Number of foundation detectors in the version-1 combined signal.
pub const COMBINED_DETECTOR_COUNT: usize = 3;

/// Domain separator for canonical persisted regime-signal evidence.
pub const REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN: &[u8] = b"fgdb:regime-signal-evidence";

/// Current canonical regime-signal evidence encoding version.
pub const REGIME_SIGNAL_EVIDENCE_ENCODING_VERSION: u16 = 1;

const COMBINED_DETECTOR_SLOTS: [RegimeDetectorKind; COMBINED_DETECTOR_COUNT] = [
    RegimeDetectorKind::PageHinkley,
    RegimeDetectorKind::UpwardCusum,
    RegimeDetectorKind::DownwardCusum,
];

/// Maximum byte length of a stable detector identity.
pub const MAX_DETECTOR_ID_BYTES: usize = 256;

/// Absolute observation ceiling for one regime-signal window.
pub const MAX_REGIME_OBSERVATIONS: usize = 1_048_576;

/// Absolute retained-receipt ceiling for one regime-signal window.
pub const MAX_RETAINED_REGIME_RECEIPTS: usize = 4_096;

const OBJECT_ID_BYTES: usize = 32;
const BUILTIN_SERIES_PAYLOAD: u16 = 0;

/// Strict canonical regime-evidence encoding or decoding failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegimeSignalEvidenceCodecError {
    /// Canonical length arithmetic overflowed.
    LengthOverflow,
    /// The canonical byte buffer could not be allocated.
    AllocationFailed,
    /// The input ended before a complete value could be read.
    Truncated {
        /// Byte offset at which the read began.
        offset: usize,
        /// Number of bytes requested.
        needed: usize,
        /// Number of bytes still available.
        remaining: usize,
    },
    /// The domain separator length was not canonical.
    DomainLengthMismatch {
        /// Encoded domain length.
        actual: usize,
    },
    /// The domain separator did not match this evidence type.
    DomainMismatch,
    /// The encoding version is not supported.
    UnsupportedVersion {
        /// Version found in the input.
        actual: u16,
    },
    /// A bounded vector or string length exceeded its canonical ceiling.
    LengthLimitExceeded {
        /// Name of the bounded component.
        component: &'static str,
        /// Encoded length.
        actual: usize,
        /// Maximum accepted length.
        maximum: usize,
    },
    /// A canonical string was not UTF-8.
    InvalidUtf8,
    /// A discriminant or canonical boolean was unknown.
    InvalidTag {
        /// Name of the tagged component.
        component: &'static str,
        /// Rejected tag.
        actual: u8,
    },
    /// A built-in metric-series tag carried a non-zero custom payload.
    NonCanonicalSeriesPayload {
        /// Built-in metric-series tag.
        tag: u8,
        /// Rejected payload.
        payload: u16,
    },
    /// Reconstructing the validated immutable identity or profile failed.
    Build(RegimeBuildError),
    /// Decoded dynamic evidence violated a canonical state invariant.
    InvalidState {
        /// Stable invariant diagnostic.
        reason: &'static str,
    },
    /// Bytes remained after one complete canonical evidence record.
    TrailingBytes {
        /// Number of unconsumed bytes.
        remaining: usize,
    },
}

impl fmt::Display for RegimeSignalEvidenceCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LengthOverflow => {
                formatter.write_str("canonical regime evidence length overflowed")
            }
            Self::AllocationFailed => {
                formatter.write_str("could not allocate canonical regime evidence")
            }
            Self::Truncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "canonical regime evidence is truncated at {offset}: need {needed} bytes, have {remaining}"
            ),
            Self::DomainLengthMismatch { actual } => write!(
                formatter,
                "canonical regime evidence domain length {actual} is invalid"
            ),
            Self::DomainMismatch => {
                formatter.write_str("canonical regime evidence domain does not match")
            }
            Self::UnsupportedVersion { actual } => write!(
                formatter,
                "canonical regime evidence version {actual} is unsupported"
            ),
            Self::LengthLimitExceeded {
                component,
                actual,
                maximum,
            } => write!(
                formatter,
                "canonical regime evidence {component} length {actual} exceeds {maximum}"
            ),
            Self::InvalidUtf8 => {
                formatter.write_str("canonical regime evidence string is not UTF-8")
            }
            Self::InvalidTag { component, actual } => write!(
                formatter,
                "canonical regime evidence {component} tag {actual} is invalid"
            ),
            Self::NonCanonicalSeriesPayload { tag, payload } => write!(
                formatter,
                "canonical regime evidence series tag {tag} has non-zero payload {payload}"
            ),
            Self::Build(error) => write!(
                formatter,
                "canonical regime evidence identity or profile is invalid: {error}"
            ),
            Self::InvalidState { reason } => {
                write!(
                    formatter,
                    "canonical regime evidence state is invalid: {reason}"
                )
            }
            Self::TrailingBytes { remaining } => write!(
                formatter,
                "canonical regime evidence has {remaining} trailing bytes"
            ),
        }
    }
}

impl std::error::Error for RegimeSignalEvidenceCodecError {}

impl From<RegimeBuildError> for RegimeSignalEvidenceCodecError {
    fn from(error: RegimeBuildError) -> Self {
        Self::Build(error)
    }
}

/// A finite inclusive source-stream sequence window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegimeSequenceWindow {
    first: u64,
    last: u64,
    length: u64,
}

impl RegimeSequenceWindow {
    /// Validates an inclusive sequence window.
    pub fn try_new(first: u64, last: u64) -> Result<Self, RegimeBuildError> {
        let distance = last
            .checked_sub(first)
            .ok_or(RegimeBuildError::ReversedWindow { first, last })?;
        let length = distance
            .checked_add(1)
            .ok_or(RegimeBuildError::WindowLengthOverflow { first, last })?;
        Ok(Self {
            first,
            last,
            length,
        })
    }

    /// Inclusive first source sequence.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Inclusive last source sequence.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }

    /// Number of positions in the inclusive window.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.length
    }

    /// A validated inclusive window is never empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        false
    }
}

/// Complete immutable identity of one regime-signal window.
///
/// The detector profile OID identifies its exact parameter object. The
/// detector string and version additionally pin the implementation contract
/// used to interpret that object.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RegimeSignalIdentity {
    signal_oid: ObjectId,
    metric_stream_oid: ObjectId,
    detector_profile_oid: ObjectId,
    detector_id: String,
    detector_version: u32,
    window: RegimeSequenceWindow,
    regime_epoch: u64,
    candidate_decision_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
}

impl RegimeSignalIdentity {
    /// Validates and owns a complete regime-signal identity.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        signal_oid: ObjectId,
        metric_stream_oid: ObjectId,
        detector_profile_oid: ObjectId,
        detector_id: &str,
        detector_version: u32,
        window: RegimeSequenceWindow,
        regime_epoch: u64,
        candidate_decision_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
    ) -> Result<Self, RegimeBuildError> {
        if detector_version == 0 {
            return Err(RegimeBuildError::ZeroDetectorVersion);
        }
        if candidate_decision_oid == pinned_fallback_oid {
            return Err(RegimeBuildError::CandidateEqualsFallback);
        }

        Ok(Self {
            signal_oid,
            metric_stream_oid,
            detector_profile_oid,
            detector_id: copy_detector_id(detector_id)?,
            detector_version,
            window,
            regime_epoch,
            candidate_decision_oid,
            pinned_fallback_oid,
        })
    }

    /// OID of this particular signal definition.
    #[must_use]
    pub const fn signal_oid(&self) -> ObjectId {
        self.signal_oid
    }

    /// OID of the exact metric stream being interpreted.
    #[must_use]
    pub const fn metric_stream_oid(&self) -> ObjectId {
        self.metric_stream_oid
    }

    /// OID of the complete detector parameter profile.
    #[must_use]
    pub const fn detector_profile_oid(&self) -> ObjectId {
        self.detector_profile_oid
    }

    /// Stable detector-composition identity.
    #[must_use]
    pub fn detector_id(&self) -> &str {
        &self.detector_id
    }

    /// Detector-composition version.
    #[must_use]
    pub const fn detector_version(&self) -> u32 {
        self.detector_version
    }

    /// Fixed source-stream window.
    #[must_use]
    pub const fn window(&self) -> RegimeSequenceWindow {
        self.window
    }

    /// Regime epoch in which this signal is evaluated.
    #[must_use]
    pub const fn regime_epoch(&self) -> u64 {
        self.regime_epoch
    }

    /// Candidate policy active before a change receipt.
    #[must_use]
    pub const fn candidate_decision_oid(&self) -> ObjectId {
        self.candidate_decision_oid
    }

    /// Deterministic policy selected after a change receipt.
    #[must_use]
    pub const fn pinned_fallback_oid(&self) -> ObjectId {
        self.pinned_fallback_oid
    }
}

/// Versioned, bounded profile for the combined foundation monitor.
///
/// Version 1 always registers detectors in this order:
///
/// 1. Page-Hinkley
/// 2. upward CUSUM
/// 3. downward CUSUM
///
/// Asupersync advances every matching detector and returns the first receipt
/// in registration order. This wrapper preserves that behavior exactly; it
/// does not vote across detectors or add another detection formula.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RegimeSignalProfile {
    detector_profile_oid: ObjectId,
    detector_id: String,
    detector_version: u32,
    series: RuntimeMetricSeries,
    page_hinkley_tolerance_micro_units: i64,
    page_hinkley_threshold: i64,
    page_hinkley_reset_after_detection: bool,
    cusum_baseline_micro_units: i64,
    upward_cusum_drift_micro_units: i64,
    upward_cusum_threshold: i64,
    upward_cusum_reset_after_detection: bool,
    downward_cusum_drift_micro_units: i64,
    downward_cusum_threshold: i64,
    downward_cusum_reset_after_detection: bool,
    max_observations: usize,
    max_retained_receipts: usize,
}

impl RegimeSignalProfile {
    /// Validates one version-1 combined foundation profile.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        detector_profile_oid: ObjectId,
        series: RuntimeMetricSeries,
        page_hinkley: PageHinkleyConfig,
        upward_cusum: CusumConfig,
        downward_cusum: CusumConfig,
        max_observations: usize,
        max_retained_receipts: usize,
    ) -> Result<Self, RegimeBuildError> {
        if page_hinkley.tolerance.as_micro_units() < 0 {
            return Err(RegimeBuildError::NegativePageHinkleyTolerance {
                micro_units: page_hinkley.tolerance.as_micro_units(),
            });
        }
        if page_hinkley.threshold <= 0 {
            return Err(RegimeBuildError::NonPositivePageHinkleyThreshold {
                threshold: page_hinkley.threshold,
            });
        }
        if upward_cusum.direction != FoundationDirection::Increase {
            return Err(RegimeBuildError::InvalidUpwardCusumDirection);
        }
        if downward_cusum.direction != FoundationDirection::Decrease {
            return Err(RegimeBuildError::InvalidDownwardCusumDirection);
        }
        if upward_cusum.baseline != downward_cusum.baseline {
            return Err(RegimeBuildError::CusumBaselineMismatch {
                upward_micro_units: upward_cusum.baseline.as_micro_units(),
                downward_micro_units: downward_cusum.baseline.as_micro_units(),
            });
        }
        validate_cusum_config(CombinedCusumSlot::Upward, upward_cusum)?;
        validate_cusum_config(CombinedCusumSlot::Downward, downward_cusum)?;
        if max_observations == 0 {
            return Err(RegimeBuildError::ZeroObservationLimit);
        }
        if max_observations > MAX_REGIME_OBSERVATIONS {
            return Err(RegimeBuildError::ObservationLimitTooLarge {
                actual: max_observations,
                maximum: MAX_REGIME_OBSERVATIONS,
            });
        }
        let _ = u64::try_from(max_observations).map_err(|_| {
            RegimeBuildError::ObservationLimitUnrepresentable {
                maximum: max_observations,
            }
        })?;
        if max_retained_receipts == 0 {
            return Err(RegimeBuildError::ZeroReceiptLimit);
        }
        if max_retained_receipts > MAX_RETAINED_REGIME_RECEIPTS {
            return Err(RegimeBuildError::ReceiptLimitTooLarge {
                actual: max_retained_receipts,
                maximum: MAX_RETAINED_REGIME_RECEIPTS,
            });
        }
        if max_retained_receipts > max_observations {
            return Err(RegimeBuildError::ReceiptLimitExceedsObservationLimit {
                receipts: max_retained_receipts,
                observations: max_observations,
            });
        }

        Ok(Self {
            detector_profile_oid,
            detector_id: copy_detector_id(COMBINED_REGIME_SIGNAL_ID)?,
            detector_version: COMBINED_REGIME_SIGNAL_VERSION,
            series,
            page_hinkley_tolerance_micro_units: page_hinkley.tolerance.as_micro_units(),
            page_hinkley_threshold: page_hinkley.threshold,
            page_hinkley_reset_after_detection: page_hinkley.reset_after_detection,
            cusum_baseline_micro_units: upward_cusum.baseline.as_micro_units(),
            upward_cusum_drift_micro_units: upward_cusum.drift.as_micro_units(),
            upward_cusum_threshold: upward_cusum.threshold,
            upward_cusum_reset_after_detection: upward_cusum.reset_after_detection,
            downward_cusum_drift_micro_units: downward_cusum.drift.as_micro_units(),
            downward_cusum_threshold: downward_cusum.threshold,
            downward_cusum_reset_after_detection: downward_cusum.reset_after_detection,
            max_observations,
            max_retained_receipts,
        })
    }

    /// OID of the exact detector parameter profile.
    #[must_use]
    pub const fn detector_profile_oid(&self) -> ObjectId {
        self.detector_profile_oid
    }

    /// Stable detector composition identity.
    #[must_use]
    pub fn detector_id(&self) -> &str {
        &self.detector_id
    }

    /// Detector composition version.
    #[must_use]
    pub const fn detector_version(&self) -> u32 {
        self.detector_version
    }

    /// Foundation metric-series routing key.
    #[must_use]
    pub const fn series(&self) -> RuntimeMetricSeries {
        self.series
    }

    /// Page-Hinkley tolerated drift in exact fixed-point micro-units.
    #[must_use]
    pub const fn page_hinkley_tolerance_micro_units(&self) -> i64 {
        self.page_hinkley_tolerance_micro_units
    }

    /// Page-Hinkley receipt threshold.
    #[must_use]
    pub const fn page_hinkley_threshold(&self) -> i64 {
        self.page_hinkley_threshold
    }

    /// Whether Page-Hinkley resets after producing a receipt.
    #[must_use]
    pub const fn page_hinkley_resets_after_detection(&self) -> bool {
        self.page_hinkley_reset_after_detection
    }

    /// Shared CUSUM baseline in exact fixed-point micro-units.
    #[must_use]
    pub const fn cusum_baseline_micro_units(&self) -> i64 {
        self.cusum_baseline_micro_units
    }

    /// Upward CUSUM drift in exact fixed-point micro-units.
    #[must_use]
    pub const fn upward_cusum_drift_micro_units(&self) -> i64 {
        self.upward_cusum_drift_micro_units
    }

    /// Upward CUSUM receipt threshold.
    #[must_use]
    pub const fn upward_cusum_threshold(&self) -> i64 {
        self.upward_cusum_threshold
    }

    /// Whether upward CUSUM resets after producing a receipt.
    #[must_use]
    pub const fn upward_cusum_resets_after_detection(&self) -> bool {
        self.upward_cusum_reset_after_detection
    }

    /// Downward CUSUM drift in exact fixed-point micro-units.
    #[must_use]
    pub const fn downward_cusum_drift_micro_units(&self) -> i64 {
        self.downward_cusum_drift_micro_units
    }

    /// Downward CUSUM receipt threshold.
    #[must_use]
    pub const fn downward_cusum_threshold(&self) -> i64 {
        self.downward_cusum_threshold
    }

    /// Whether downward CUSUM resets after producing a receipt.
    #[must_use]
    pub const fn downward_cusum_resets_after_detection(&self) -> bool {
        self.downward_cusum_reset_after_detection
    }

    /// Maximum accepted observations.
    #[must_use]
    pub const fn max_observations(&self) -> usize {
        self.max_observations
    }

    /// Maximum retained detection receipts.
    #[must_use]
    pub const fn max_retained_receipts(&self) -> usize {
        self.max_retained_receipts
    }

    fn foundation_monitor(&self) -> ChangePointMonitor {
        let page_hinkley = PageHinkleyConfig {
            tolerance: MetricSample::from_micro_units(self.page_hinkley_tolerance_micro_units),
            threshold: self.page_hinkley_threshold,
            reset_after_detection: self.page_hinkley_reset_after_detection,
        };
        let upward_cusum = CusumConfig {
            baseline: MetricSample::from_micro_units(self.cusum_baseline_micro_units),
            drift: MetricSample::from_micro_units(self.upward_cusum_drift_micro_units),
            threshold: self.upward_cusum_threshold,
            direction: FoundationDirection::Increase,
            reset_after_detection: self.upward_cusum_reset_after_detection,
        };
        let downward_cusum = CusumConfig {
            baseline: MetricSample::from_micro_units(self.cusum_baseline_micro_units),
            drift: MetricSample::from_micro_units(self.downward_cusum_drift_micro_units),
            threshold: self.downward_cusum_threshold,
            direction: FoundationDirection::Decrease,
            reset_after_detection: self.downward_cusum_reset_after_detection,
        };

        ChangePointMonitorConfig::disabled()
            .with_series(ChangePointSeriesConfig::page_hinkley(
                self.series,
                page_hinkley,
            ))
            .with_series(ChangePointSeriesConfig::cusum(self.series, upward_cusum))
            .with_series(ChangePointSeriesConfig::cusum(self.series, downward_cusum))
            .enable()
            .build_monitor()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CombinedCusumSlot {
    Upward,
    Downward,
}

/// Identity, profile, resource, or monitor-construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegimeBuildError {
    /// The detector identity was empty.
    EmptyDetectorId,
    /// The detector identity exceeded [`MAX_DETECTOR_ID_BYTES`].
    DetectorIdTooLong {
        /// Actual byte length.
        actual: usize,
        /// Maximum byte length.
        maximum: usize,
    },
    /// The detector identity contained a byte outside printable ASCII.
    NonCanonicalDetectorId {
        /// Offset of the first rejected byte.
        offset: usize,
    },
    /// Space for the owned detector identity could not be reserved.
    DetectorIdAllocationFailed,
    /// Detector version zero is not a valid durable version.
    ZeroDetectorVersion,
    /// Candidate and pinned fallback identities were equal.
    CandidateEqualsFallback,
    /// The inclusive sequence window was reversed.
    ReversedWindow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// The inclusive window length was not representable.
    WindowLengthOverflow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// Page-Hinkley tolerance was negative.
    NegativePageHinkleyTolerance {
        /// Supplied tolerance in fixed-point micro-units.
        micro_units: i64,
    },
    /// Page-Hinkley threshold was zero or negative.
    NonPositivePageHinkleyThreshold {
        /// Supplied threshold.
        threshold: i64,
    },
    /// The upward CUSUM slot did not monitor increases.
    InvalidUpwardCusumDirection,
    /// The downward CUSUM slot did not monitor decreases.
    InvalidDownwardCusumDirection,
    /// The two CUSUM slots did not share one baseline.
    CusumBaselineMismatch {
        /// Upward slot baseline in fixed-point micro-units.
        upward_micro_units: i64,
        /// Downward slot baseline in fixed-point micro-units.
        downward_micro_units: i64,
    },
    /// A CUSUM drift was negative.
    NegativeCusumDrift {
        /// Slot containing the rejected drift.
        upward: bool,
        /// Supplied drift in fixed-point micro-units.
        micro_units: i64,
    },
    /// A CUSUM threshold was zero or negative.
    NonPositiveCusumThreshold {
        /// Slot containing the rejected threshold.
        upward: bool,
        /// Supplied threshold.
        threshold: i64,
    },
    /// At least one observation must be accepted.
    ZeroObservationLimit,
    /// Observation limit exceeded the absolute resource ceiling.
    ObservationLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Absolute ceiling.
        maximum: usize,
    },
    /// Observation limit could not be represented in canonical evidence.
    ObservationLimitUnrepresentable {
        /// Requested limit.
        maximum: usize,
    },
    /// At least one detection receipt must be retained.
    ZeroReceiptLimit,
    /// Receipt limit exceeded the absolute resource ceiling.
    ReceiptLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Absolute ceiling.
        maximum: usize,
    },
    /// More receipts were requested than observations can produce.
    ReceiptLimitExceedsObservationLimit {
        /// Receipt limit.
        receipts: usize,
        /// Observation limit.
        observations: usize,
    },
    /// The identity and profile named different profile objects.
    IdentityProfileOidMismatch {
        /// OID named by the identity.
        identity: ObjectId,
        /// OID named by the profile.
        profile: ObjectId,
    },
    /// The identity and profile named different detector compositions.
    IdentityDetectorIdMismatch,
    /// The identity and profile named different detector versions.
    IdentityDetectorVersionMismatch {
        /// Version named by the identity.
        identity: u32,
        /// Version named by the profile.
        profile: u32,
    },
    /// Observation limit exceeded the fixed identity window.
    ObservationLimitExceedsWindow {
        /// Configured observation limit.
        maximum: usize,
        /// Identity window length.
        window: u64,
    },
    /// Space for the bounded receipt inventory could not be reserved.
    ReceiptAllocationFailed {
        /// Receipt slots requested.
        requested: usize,
    },
}

impl fmt::Display for RegimeBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::EmptyDetectorId => formatter.write_str("detector identity must not be empty"),
            Self::DetectorIdTooLong { actual, maximum } => write!(
                formatter,
                "detector identity is {actual} bytes; maximum is {maximum}"
            ),
            Self::NonCanonicalDetectorId { offset } => write!(
                formatter,
                "detector identity contains a non-canonical byte at offset {offset}"
            ),
            Self::DetectorIdAllocationFailed => {
                formatter.write_str("could not allocate detector identity")
            }
            Self::ZeroDetectorVersion => {
                formatter.write_str("detector version must be greater than zero")
            }
            Self::CandidateEqualsFallback => {
                formatter.write_str("candidate and pinned fallback identities must differ")
            }
            Self::ReversedWindow { first, last } => {
                write!(formatter, "regime window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "regime window {first}..={last} has an unrepresentable length"
            ),
            Self::NegativePageHinkleyTolerance { micro_units } => write!(
                formatter,
                "Page-Hinkley tolerance {micro_units} micro-units is negative"
            ),
            Self::NonPositivePageHinkleyThreshold { threshold } => {
                write!(
                    formatter,
                    "Page-Hinkley threshold {threshold} is not positive"
                )
            }
            Self::InvalidUpwardCusumDirection => {
                formatter.write_str("upward CUSUM slot must monitor increases")
            }
            Self::InvalidDownwardCusumDirection => {
                formatter.write_str("downward CUSUM slot must monitor decreases")
            }
            Self::CusumBaselineMismatch {
                upward_micro_units,
                downward_micro_units,
            } => write!(
                formatter,
                "CUSUM baselines differ: upward={upward_micro_units}, downward={downward_micro_units}"
            ),
            Self::NegativeCusumDrift {
                upward,
                micro_units,
            } => write!(
                formatter,
                "{} CUSUM drift {micro_units} micro-units is negative",
                if upward { "upward" } else { "downward" }
            ),
            Self::NonPositiveCusumThreshold { upward, threshold } => write!(
                formatter,
                "{} CUSUM threshold {threshold} is not positive",
                if upward { "upward" } else { "downward" }
            ),
            Self::ZeroObservationLimit => {
                formatter.write_str("regime observation limit must be greater than zero")
            }
            Self::ObservationLimitTooLarge { actual, maximum } => write!(
                formatter,
                "regime observation limit {actual} exceeds ceiling {maximum}"
            ),
            Self::ObservationLimitUnrepresentable { maximum } => write!(
                formatter,
                "regime observation limit {maximum} is not canonically representable"
            ),
            Self::ZeroReceiptLimit => {
                formatter.write_str("regime receipt limit must be greater than zero")
            }
            Self::ReceiptLimitTooLarge { actual, maximum } => write!(
                formatter,
                "regime receipt limit {actual} exceeds ceiling {maximum}"
            ),
            Self::ReceiptLimitExceedsObservationLimit {
                receipts,
                observations,
            } => write!(
                formatter,
                "regime receipt limit {receipts} exceeds observation limit {observations}"
            ),
            Self::IdentityProfileOidMismatch { identity, profile } => write!(
                formatter,
                "identity profile OID {identity:?} does not match profile OID {profile:?}"
            ),
            Self::IdentityDetectorIdMismatch => {
                formatter.write_str("identity detector name does not match profile")
            }
            Self::IdentityDetectorVersionMismatch { identity, profile } => write!(
                formatter,
                "identity detector version {identity} does not match profile version {profile}"
            ),
            Self::ObservationLimitExceedsWindow { maximum, window } => write!(
                formatter,
                "regime observation limit {maximum} exceeds window length {window}"
            ),
            Self::ReceiptAllocationFailed { requested } => {
                write!(
                    formatter,
                    "could not reserve {requested} regime receipt slots"
                )
            }
        }
    }
}

impl std::error::Error for RegimeBuildError {}

/// One profile- and identity-bound metric sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequencedRegimeSample {
    identity: RegimeSignalIdentity,
    profile: RegimeSignalProfile,
    stream_sequence: u64,
    sample: MetricSample,
}

impl SequencedRegimeSample {
    /// Constructs an immutable sample envelope.
    #[must_use]
    pub const fn new(
        identity: RegimeSignalIdentity,
        profile: RegimeSignalProfile,
        stream_sequence: u64,
        sample: MetricSample,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            sample,
        }
    }

    /// Complete signal identity.
    #[must_use]
    pub const fn identity(&self) -> &RegimeSignalIdentity {
        &self.identity
    }

    /// Exact detector profile.
    #[must_use]
    pub const fn profile(&self) -> &RegimeSignalProfile {
        &self.profile
    }

    /// Exact source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Exact foundation fixed-point sample.
    #[must_use]
    pub const fn sample(&self) -> MetricSample {
        self.sample
    }
}

/// Stable slot identity in the versioned combined foundation monitor.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RegimeDetectorKind {
    /// Page-Hinkley running-mean detector.
    PageHinkley,
    /// One-sided cumulative-sum detector for increases.
    UpwardCusum,
    /// One-sided cumulative-sum detector for decreases.
    DownwardCusum,
}

/// Stable projection of a detected shift's direction.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RegimeDirection {
    /// Metric values shifted upward.
    Increase,
    /// Metric values shifted downward.
    Decrease,
}

/// Exact evidence projection of one foundation detection receipt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RegimeDetectionReceipt {
    stream_sequence: u64,
    detector_sample_index: u64,
    series: RuntimeMetricSeries,
    detector: RegimeDetectorKind,
    direction: RegimeDirection,
    sample_micro_units: i64,
    statistic: i64,
    threshold: i64,
}

impl RegimeDetectionReceipt {
    /// Source-stream sequence that supplied the crossing sample.
    #[must_use]
    pub const fn stream_sequence(self) -> u64 {
        self.stream_sequence
    }

    /// Foundation detector's one-based lifetime sample index.
    #[must_use]
    pub const fn detector_sample_index(self) -> u64 {
        self.detector_sample_index
    }

    /// Foundation metric series routed to the detector.
    #[must_use]
    pub const fn series(self) -> RuntimeMetricSeries {
        self.series
    }

    /// Foundation detector that emitted the receipt.
    #[must_use]
    pub const fn detector(self) -> RegimeDetectorKind {
        self.detector
    }

    /// Detected shift direction.
    #[must_use]
    pub const fn direction(self) -> RegimeDirection {
        self.direction
    }

    /// Crossing sample in exact fixed-point micro-units.
    #[must_use]
    pub const fn sample_micro_units(self) -> i64 {
        self.sample_micro_units
    }

    /// Detector statistic at the crossing.
    #[must_use]
    pub const fn statistic(self) -> i64 {
        self.statistic
    }

    /// Configured detector threshold.
    #[must_use]
    pub const fn threshold(self) -> i64 {
        self.threshold
    }

    /// A statistical detector receipt is never a ground-truth claim.
    #[must_use]
    pub const fn is_ground_truth(self) -> bool {
        false
    }
}

/// Exact projection of one foundation detector's current state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RegimeDetectorSnapshot {
    series: RuntimeMetricSeries,
    detector: RegimeDetectorKind,
    sample_count: u64,
    mean_micro_units: i64,
    statistic: i64,
    threshold: i64,
}

impl RegimeDetectorSnapshot {
    /// Foundation metric series represented by this state.
    #[must_use]
    pub const fn series(self) -> RuntimeMetricSeries {
        self.series
    }

    /// Detector represented by this state.
    #[must_use]
    pub const fn detector(self) -> RegimeDetectorKind {
        self.detector
    }

    /// Number of matching samples consumed by the detector.
    #[must_use]
    pub const fn sample_count(self) -> u64 {
        self.sample_count
    }

    /// Current mean or configured baseline in fixed-point micro-units.
    #[must_use]
    pub const fn mean_micro_units(self) -> i64 {
        self.mean_micro_units
    }

    /// Current detector statistic.
    #[must_use]
    pub const fn statistic(self) -> i64 {
        self.statistic
    }

    /// Configured detector threshold.
    #[must_use]
    pub const fn threshold(self) -> i64 {
        self.threshold
    }
}

/// Advisory state of the combined signal.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RegimeSignalStatus {
    /// No configured detector has emitted a receipt in this window.
    NoChangeDetected,
    /// At least one configured detector emitted an advisory receipt.
    ChangeDetected,
}

impl RegimeSignalStatus {
    /// Neither status constitutes a ground-truth claim about the workload.
    #[must_use]
    pub const fn is_ground_truth(self) -> bool {
        false
    }
}

/// Policy selected by the regime-signal wrapper.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RegimePolicySelection {
    /// Continue the candidate while the configured signal remains quiet.
    CandidateDecision,
    /// Use the deterministic fallback after any detector receipt.
    PinnedFallback,
}

/// Deterministic evidence after zero or more accepted samples.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegimeSignalEvidence {
    identity: RegimeSignalIdentity,
    profile: RegimeSignalProfile,
    through_sequence: Option<u64>,
    observation_count: u64,
    detection_count: u64,
    dropped_receipt_count: u64,
    fallback_sequence: Option<u64>,
    status: RegimeSignalStatus,
    selection: RegimePolicySelection,
    detector_snapshots: Vec<RegimeDetectorSnapshot>,
    retained_receipts: Vec<RegimeDetectionReceipt>,
}

impl RegimeSignalEvidence {
    /// Complete immutable signal identity.
    #[must_use]
    pub const fn identity(&self) -> &RegimeSignalIdentity {
        &self.identity
    }

    /// Complete immutable detector profile.
    #[must_use]
    pub const fn profile(&self) -> &RegimeSignalProfile {
        &self.profile
    }

    /// Last accepted source-stream sequence.
    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    /// Total accepted samples.
    #[must_use]
    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// Total combined-signal receipts surfaced by the foundation monitor,
    /// including receipts no longer retained.
    ///
    /// When several detectors cross on the same sample, asupersync advances
    /// them all but surfaces only the first receipt in registration order.
    #[must_use]
    pub const fn detection_count(&self) -> u64 {
        self.detection_count
    }

    /// Number of oldest receipts evicted by the retention bound.
    #[must_use]
    pub const fn dropped_receipt_count(&self) -> u64 {
        self.dropped_receipt_count
    }

    /// First source sequence that forced the pinned fallback.
    #[must_use]
    pub const fn fallback_sequence(&self) -> Option<u64> {
        self.fallback_sequence
    }

    /// Current advisory signal status.
    #[must_use]
    pub const fn status(&self) -> RegimeSignalStatus {
        self.status
    }

    /// Current deterministic policy selection.
    #[must_use]
    pub const fn selection(&self) -> RegimePolicySelection {
        self.selection
    }

    /// Exact selected policy identity.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection {
            RegimePolicySelection::CandidateDecision => self.identity.candidate_decision_oid,
            RegimePolicySelection::PinnedFallback => self.identity.pinned_fallback_oid,
        }
    }

    /// Foundation detector states in versioned registration order.
    #[must_use]
    pub fn detector_snapshots(&self) -> &[RegimeDetectorSnapshot] {
        &self.detector_snapshots
    }

    /// Retained receipts in ascending source-sequence order.
    #[must_use]
    pub fn retained_receipts(&self) -> &[RegimeDetectionReceipt] {
        &self.retained_receipts
    }

    /// Encodes every immutable and dynamic evidence field in one strict,
    /// versioned canonical representation.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, RegimeSignalEvidenceCodecError> {
        validate_regime_evidence_state(self)?;
        let capacity = regime_evidence_encoded_len(self)?;
        let mut encoder = RegimeEvidenceEncoder::with_capacity(capacity)?;
        encoder.write_u16(
            u16::try_from(REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN.len())
                .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?,
        );
        encoder.write_bytes(REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN);
        encoder.write_u16(REGIME_SIGNAL_EVIDENCE_ENCODING_VERSION);
        encode_regime_identity(&mut encoder, &self.identity)?;
        encode_regime_profile(&mut encoder, &self.profile)?;
        encoder.write_optional_u64(self.through_sequence);
        encoder.write_u64(self.observation_count);
        encoder.write_u64(self.detection_count);
        encoder.write_u64(self.dropped_receipt_count);
        encoder.write_optional_u64(self.fallback_sequence);
        encoder.write_u8(encode_regime_status(self.status));
        encoder.write_u8(encode_regime_selection(self.selection));
        encoder.write_u32(
            u32::try_from(self.detector_snapshots.len())
                .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?,
        );
        for snapshot in &self.detector_snapshots {
            encode_regime_snapshot(&mut encoder, *snapshot);
        }
        encoder.write_u32(
            u32::try_from(self.retained_receipts.len())
                .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?,
        );
        for receipt in &self.retained_receipts {
            encode_regime_receipt(&mut encoder, *receipt);
        }
        debug_assert_eq!(encoder.len(), capacity);
        Ok(encoder.finish())
    }

    /// Decodes one complete canonical representation and rejects unknown
    /// versions, non-canonical tags, trailing bytes, and impossible state.
    pub fn try_from_canonical_bytes(bytes: &[u8]) -> Result<Self, RegimeSignalEvidenceCodecError> {
        let mut decoder = RegimeEvidenceDecoder::new(bytes);
        let domain_len = usize::from(decoder.read_u16()?);
        if domain_len != REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN.len() {
            return Err(RegimeSignalEvidenceCodecError::DomainLengthMismatch {
                actual: domain_len,
            });
        }
        if decoder.read_bytes(domain_len)? != REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN {
            return Err(RegimeSignalEvidenceCodecError::DomainMismatch);
        }
        let version = decoder.read_u16()?;
        if version != REGIME_SIGNAL_EVIDENCE_ENCODING_VERSION {
            return Err(RegimeSignalEvidenceCodecError::UnsupportedVersion { actual: version });
        }

        let identity = decode_regime_identity(&mut decoder)?;
        let profile = decode_regime_profile(&mut decoder)?;
        let through_sequence = decoder.read_optional_u64("through-sequence")?;
        let observation_count = decoder.read_u64()?;
        let detection_count = decoder.read_u64()?;
        let dropped_receipt_count = decoder.read_u64()?;
        let fallback_sequence = decoder.read_optional_u64("fallback-sequence")?;
        let status = decode_regime_status(decoder.read_u8()?)?;
        let selection = decode_regime_selection(decoder.read_u8()?)?;

        let snapshot_count = usize::try_from(decoder.read_u32()?)
            .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
        if snapshot_count != COMBINED_DETECTOR_COUNT {
            return Err(RegimeSignalEvidenceCodecError::LengthLimitExceeded {
                component: "detector-snapshot",
                actual: snapshot_count,
                maximum: COMBINED_DETECTOR_COUNT,
            });
        }
        let mut detector_snapshots = Vec::new();
        detector_snapshots
            .try_reserve_exact(snapshot_count)
            .map_err(|_| RegimeSignalEvidenceCodecError::AllocationFailed)?;
        for _ in 0..snapshot_count {
            detector_snapshots.push(decode_regime_snapshot(&mut decoder)?);
        }

        let receipt_count = usize::try_from(decoder.read_u32()?)
            .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
        if receipt_count > MAX_RETAINED_REGIME_RECEIPTS {
            return Err(RegimeSignalEvidenceCodecError::LengthLimitExceeded {
                component: "retained-receipt",
                actual: receipt_count,
                maximum: MAX_RETAINED_REGIME_RECEIPTS,
            });
        }
        let mut retained_receipts = Vec::new();
        retained_receipts
            .try_reserve_exact(receipt_count)
            .map_err(|_| RegimeSignalEvidenceCodecError::AllocationFailed)?;
        for _ in 0..receipt_count {
            retained_receipts.push(decode_regime_receipt(&mut decoder)?);
        }
        if decoder.remaining() != 0 {
            return Err(RegimeSignalEvidenceCodecError::TrailingBytes {
                remaining: decoder.remaining(),
            });
        }

        let evidence = Self {
            identity,
            profile,
            through_sequence,
            observation_count,
            detection_count,
            dropped_receipt_count,
            fallback_sequence,
            status,
            selection,
            detector_snapshots,
            retained_receipts,
        };
        validate_regime_evidence_state(&evidence)?;
        Ok(evidence)
    }

    /// This evidence remains an advisory signal, never ground truth.
    #[must_use]
    pub const fn is_ground_truth(&self) -> bool {
        false
    }
}

fn regime_evidence_encoded_len(
    evidence: &RegimeSignalEvidence,
) -> Result<usize, RegimeSignalEvidenceCodecError> {
    let identity_len = 5_usize
        .checked_mul(OBJECT_ID_BYTES)
        .and_then(|length| length.checked_add(2))
        .and_then(|length| length.checked_add(evidence.identity.detector_id.len()))
        .and_then(|length| length.checked_add(4 + 8 + 8 + 8))
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
    let profile_len = OBJECT_ID_BYTES
        .checked_add(2)
        .and_then(|length| length.checked_add(evidence.profile.detector_id.len()))
        .and_then(|length| length.checked_add(4 + 3))
        .and_then(|length| length.checked_add(7 * 8 + 3 + 2 * 8))
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
    let dynamic_header_len = optional_u64_encoded_len(evidence.through_sequence)
        .checked_add(3 * 8)
        .and_then(|length| length.checked_add(optional_u64_encoded_len(evidence.fallback_sequence)))
        .and_then(|length| length.checked_add(2 + 4))
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
    let snapshots_len = evidence
        .detector_snapshots
        .len()
        .checked_mul(3 + 1 + 4 * 8)
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
    let receipts_len = evidence
        .retained_receipts
        .len()
        .checked_mul(2 * 8 + 3 + 1 + 1 + 3 * 8)
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;

    2_usize
        .checked_add(REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN.len())
        .and_then(|length| length.checked_add(2))
        .and_then(|length| length.checked_add(identity_len))
        .and_then(|length| length.checked_add(profile_len))
        .and_then(|length| length.checked_add(dynamic_header_len))
        .and_then(|length| length.checked_add(snapshots_len))
        .and_then(|length| length.checked_add(4))
        .and_then(|length| length.checked_add(receipts_len))
        .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)
}

const fn optional_u64_encoded_len(value: Option<u64>) -> usize {
    if value.is_some() { 1 + 8 } else { 1 }
}

fn encode_regime_identity(
    encoder: &mut RegimeEvidenceEncoder,
    identity: &RegimeSignalIdentity,
) -> Result<(), RegimeSignalEvidenceCodecError> {
    encoder.write_oid(identity.signal_oid);
    encoder.write_oid(identity.metric_stream_oid);
    encoder.write_oid(identity.detector_profile_oid);
    encoder.write_string(&identity.detector_id)?;
    encoder.write_u32(identity.detector_version);
    encoder.write_u64(identity.window.first);
    encoder.write_u64(identity.window.last);
    encoder.write_u64(identity.regime_epoch);
    encoder.write_oid(identity.candidate_decision_oid);
    encoder.write_oid(identity.pinned_fallback_oid);
    Ok(())
}

fn decode_regime_identity(
    decoder: &mut RegimeEvidenceDecoder<'_>,
) -> Result<RegimeSignalIdentity, RegimeSignalEvidenceCodecError> {
    let signal_oid = decoder.read_oid()?;
    let metric_stream_oid = decoder.read_oid()?;
    let detector_profile_oid = decoder.read_oid()?;
    let detector_id = decoder.read_string("identity-detector-id", MAX_DETECTOR_ID_BYTES)?;
    let detector_version = decoder.read_u32()?;
    let first = decoder.read_u64()?;
    let last = decoder.read_u64()?;
    let regime_epoch = decoder.read_u64()?;
    let candidate_decision_oid = decoder.read_oid()?;
    let pinned_fallback_oid = decoder.read_oid()?;
    let window = RegimeSequenceWindow::try_new(first, last)?;
    RegimeSignalIdentity::try_new(
        signal_oid,
        metric_stream_oid,
        detector_profile_oid,
        detector_id,
        detector_version,
        window,
        regime_epoch,
        candidate_decision_oid,
        pinned_fallback_oid,
    )
    .map_err(Into::into)
}

fn encode_regime_profile(
    encoder: &mut RegimeEvidenceEncoder,
    profile: &RegimeSignalProfile,
) -> Result<(), RegimeSignalEvidenceCodecError> {
    encoder.write_oid(profile.detector_profile_oid);
    encoder.write_string(&profile.detector_id)?;
    encoder.write_u32(profile.detector_version);
    encoder.write_series(profile.series);
    encoder.write_i64(profile.page_hinkley_tolerance_micro_units);
    encoder.write_i64(profile.page_hinkley_threshold);
    encoder.write_bool(profile.page_hinkley_reset_after_detection);
    encoder.write_i64(profile.cusum_baseline_micro_units);
    encoder.write_i64(profile.upward_cusum_drift_micro_units);
    encoder.write_i64(profile.upward_cusum_threshold);
    encoder.write_bool(profile.upward_cusum_reset_after_detection);
    encoder.write_i64(profile.downward_cusum_drift_micro_units);
    encoder.write_i64(profile.downward_cusum_threshold);
    encoder.write_bool(profile.downward_cusum_reset_after_detection);
    encoder.write_u64(
        u64::try_from(profile.max_observations)
            .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?,
    );
    encoder.write_u64(
        u64::try_from(profile.max_retained_receipts)
            .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?,
    );
    Ok(())
}

fn decode_regime_profile(
    decoder: &mut RegimeEvidenceDecoder<'_>,
) -> Result<RegimeSignalProfile, RegimeSignalEvidenceCodecError> {
    let detector_profile_oid = decoder.read_oid()?;
    let detector_id = decoder.read_string("profile-detector-id", MAX_DETECTOR_ID_BYTES)?;
    let detector_version = decoder.read_u32()?;
    if detector_id != COMBINED_REGIME_SIGNAL_ID {
        return Err(RegimeSignalEvidenceCodecError::InvalidState {
            reason: "profile detector identity is not the versioned combined signal",
        });
    }
    if detector_version != COMBINED_REGIME_SIGNAL_VERSION {
        return Err(RegimeSignalEvidenceCodecError::InvalidState {
            reason: "profile detector version is not supported",
        });
    }
    let series = decoder.read_series()?;
    let page_hinkley = PageHinkleyConfig {
        tolerance: MetricSample::from_micro_units(decoder.read_i64()?),
        threshold: decoder.read_i64()?,
        reset_after_detection: decoder.read_bool("page-hinkley-reset")?,
    };
    let cusum_baseline = MetricSample::from_micro_units(decoder.read_i64()?);
    let upward_cusum = CusumConfig {
        baseline: cusum_baseline,
        drift: MetricSample::from_micro_units(decoder.read_i64()?),
        threshold: decoder.read_i64()?,
        direction: FoundationDirection::Increase,
        reset_after_detection: decoder.read_bool("upward-cusum-reset")?,
    };
    let downward_cusum = CusumConfig {
        baseline: cusum_baseline,
        drift: MetricSample::from_micro_units(decoder.read_i64()?),
        threshold: decoder.read_i64()?,
        direction: FoundationDirection::Decrease,
        reset_after_detection: decoder.read_bool("downward-cusum-reset")?,
    };
    let max_observations = usize::try_from(decoder.read_u64()?)
        .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
    let max_retained_receipts = usize::try_from(decoder.read_u64()?)
        .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
    RegimeSignalProfile::try_new(
        detector_profile_oid,
        series,
        page_hinkley,
        upward_cusum,
        downward_cusum,
        max_observations,
        max_retained_receipts,
    )
    .map_err(Into::into)
}

fn encode_regime_snapshot(encoder: &mut RegimeEvidenceEncoder, snapshot: RegimeDetectorSnapshot) {
    encoder.write_series(snapshot.series);
    encoder.write_u8(encode_detector_kind(snapshot.detector));
    encoder.write_u64(snapshot.sample_count);
    encoder.write_i64(snapshot.mean_micro_units);
    encoder.write_i64(snapshot.statistic);
    encoder.write_i64(snapshot.threshold);
}

fn decode_regime_snapshot(
    decoder: &mut RegimeEvidenceDecoder<'_>,
) -> Result<RegimeDetectorSnapshot, RegimeSignalEvidenceCodecError> {
    Ok(RegimeDetectorSnapshot {
        series: decoder.read_series()?,
        detector: decode_detector_kind(decoder.read_u8()?)?,
        sample_count: decoder.read_u64()?,
        mean_micro_units: decoder.read_i64()?,
        statistic: decoder.read_i64()?,
        threshold: decoder.read_i64()?,
    })
}

fn encode_regime_receipt(encoder: &mut RegimeEvidenceEncoder, receipt: RegimeDetectionReceipt) {
    encoder.write_u64(receipt.stream_sequence);
    encoder.write_u64(receipt.detector_sample_index);
    encoder.write_series(receipt.series);
    encoder.write_u8(encode_detector_kind(receipt.detector));
    encoder.write_u8(encode_regime_direction(receipt.direction));
    encoder.write_i64(receipt.sample_micro_units);
    encoder.write_i64(receipt.statistic);
    encoder.write_i64(receipt.threshold);
}

fn decode_regime_receipt(
    decoder: &mut RegimeEvidenceDecoder<'_>,
) -> Result<RegimeDetectionReceipt, RegimeSignalEvidenceCodecError> {
    Ok(RegimeDetectionReceipt {
        stream_sequence: decoder.read_u64()?,
        detector_sample_index: decoder.read_u64()?,
        series: decoder.read_series()?,
        detector: decode_detector_kind(decoder.read_u8()?)?,
        direction: decode_regime_direction(decoder.read_u8()?)?,
        sample_micro_units: decoder.read_i64()?,
        statistic: decoder.read_i64()?,
        threshold: decoder.read_i64()?,
    })
}

fn validate_regime_evidence_state(
    evidence: &RegimeSignalEvidence,
) -> Result<(), RegimeSignalEvidenceCodecError> {
    let identity = &evidence.identity;
    let profile = &evidence.profile;
    if identity.detector_profile_oid != profile.detector_profile_oid {
        return Err(RegimeBuildError::IdentityProfileOidMismatch {
            identity: identity.detector_profile_oid,
            profile: profile.detector_profile_oid,
        }
        .into());
    }
    if identity.detector_id != profile.detector_id {
        return Err(RegimeBuildError::IdentityDetectorIdMismatch.into());
    }
    if identity.detector_version != profile.detector_version {
        return Err(RegimeBuildError::IdentityDetectorVersionMismatch {
            identity: identity.detector_version,
            profile: profile.detector_version,
        }
        .into());
    }
    let maximum = u64::try_from(profile.max_observations)
        .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
    if maximum > identity.window.length {
        return Err(RegimeBuildError::ObservationLimitExceedsWindow {
            maximum: profile.max_observations,
            window: identity.window.length,
        }
        .into());
    }
    if evidence.observation_count > maximum {
        return invalid_regime_state("observation count exceeds the profile limit");
    }
    let expected_through_sequence = if evidence.observation_count == 0 {
        None
    } else {
        Some(
            identity
                .window
                .first
                .checked_add(evidence.observation_count - 1)
                .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?,
        )
    };
    if evidence.through_sequence != expected_through_sequence {
        return invalid_regime_state(
            "through sequence does not match the contiguous observation count",
        );
    }

    if evidence.detector_snapshots.len() != COMBINED_DETECTOR_COUNT {
        return invalid_regime_state("detector snapshot inventory is incomplete");
    }
    for (snapshot, expected_detector) in evidence
        .detector_snapshots
        .iter()
        .zip(COMBINED_DETECTOR_SLOTS)
    {
        if snapshot.series != profile.series {
            return invalid_regime_state("detector snapshot names the wrong metric series");
        }
        if snapshot.detector != expected_detector {
            return invalid_regime_state("detector snapshots are not in registration order");
        }
        if snapshot.sample_count != evidence.observation_count {
            return invalid_regime_state("detector snapshot sample count is inconsistent");
        }
        let expected_threshold = detector_threshold(profile, expected_detector);
        if snapshot.threshold != expected_threshold {
            return invalid_regime_state("detector snapshot threshold is inconsistent");
        }
        if expected_detector != RegimeDetectorKind::PageHinkley
            && snapshot.mean_micro_units != profile.cusum_baseline_micro_units
        {
            return invalid_regime_state("CUSUM snapshot baseline is inconsistent");
        }
        if snapshot.statistic < 0 {
            return invalid_regime_state("detector snapshot statistic is negative");
        }
    }

    if evidence.retained_receipts.len() > profile.max_retained_receipts {
        return invalid_regime_state("retained receipt inventory exceeds the profile limit");
    }
    let retained_count = u64::try_from(evidence.retained_receipts.len())
        .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
    if evidence.dropped_receipt_count.checked_add(retained_count) != Some(evidence.detection_count)
    {
        return invalid_regime_state("detection, dropped, and retained counts do not reconcile");
    }
    if evidence.detection_count > evidence.observation_count {
        return invalid_regime_state("detection count exceeds observation count");
    }

    match evidence.detection_count {
        0 => {
            if evidence.fallback_sequence.is_some()
                || evidence.status != RegimeSignalStatus::NoChangeDetected
                || evidence.selection != RegimePolicySelection::CandidateDecision
                || evidence.dropped_receipt_count != 0
                || !evidence.retained_receipts.is_empty()
            {
                return invalid_regime_state("quiet signal carries fallback state");
            }
        }
        _ => {
            if evidence.fallback_sequence.is_none()
                || evidence.status != RegimeSignalStatus::ChangeDetected
                || evidence.selection != RegimePolicySelection::PinnedFallback
                || evidence.retained_receipts.is_empty()
            {
                return invalid_regime_state("detected signal does not carry fallback state");
            }
        }
    }

    let mut previous_sequence = None;
    for receipt in &evidence.retained_receipts {
        let Some(through_sequence) = evidence.through_sequence else {
            return invalid_regime_state("receipt exists without an accepted observation");
        };
        if receipt.stream_sequence < identity.window.first
            || receipt.stream_sequence > through_sequence
        {
            return invalid_regime_state("receipt sequence is outside the observed prefix");
        }
        if previous_sequence.is_some_and(|previous| receipt.stream_sequence <= previous) {
            return invalid_regime_state("retained receipts are not strictly sequenced");
        }
        previous_sequence = Some(receipt.stream_sequence);
        let expected_sample_index = receipt
            .stream_sequence
            .checked_sub(identity.window.first)
            .and_then(|offset| offset.checked_add(1))
            .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
        if receipt.detector_sample_index != expected_sample_index {
            return invalid_regime_state("receipt sample index is inconsistent");
        }
        if receipt.series != profile.series {
            return invalid_regime_state("receipt names the wrong metric series");
        }
        if receipt.threshold != detector_threshold(profile, receipt.detector) {
            return invalid_regime_state("receipt threshold is inconsistent");
        }
        if receipt.statistic < receipt.threshold {
            return invalid_regime_state("receipt statistic did not cross its threshold");
        }
        let expected_direction = match receipt.detector {
            RegimeDetectorKind::PageHinkley | RegimeDetectorKind::UpwardCusum => {
                RegimeDirection::Increase
            }
            RegimeDetectorKind::DownwardCusum => RegimeDirection::Decrease,
        };
        if receipt.direction != expected_direction {
            return invalid_regime_state("receipt direction is inconsistent with its detector");
        }
    }

    if let Some(fallback_sequence) = evidence.fallback_sequence {
        let Some(through_sequence) = evidence.through_sequence else {
            return invalid_regime_state("fallback exists without an accepted observation");
        };
        if fallback_sequence < identity.window.first || fallback_sequence > through_sequence {
            return invalid_regime_state("fallback sequence is outside the observed prefix");
        }
        let first_retained = evidence
            .retained_receipts
            .first()
            .ok_or(RegimeSignalEvidenceCodecError::InvalidState {
                reason: "fallback exists without a retained receipt",
            })?
            .stream_sequence;
        if evidence.dropped_receipt_count == 0 && fallback_sequence != first_retained {
            return invalid_regime_state("fallback sequence is not the first detection");
        }
        if evidence.dropped_receipt_count > 0 && fallback_sequence >= first_retained {
            return invalid_regime_state("dropped receipts do not precede retained receipts");
        }
    }
    Ok(())
}

const fn detector_threshold(profile: &RegimeSignalProfile, detector: RegimeDetectorKind) -> i64 {
    match detector {
        RegimeDetectorKind::PageHinkley => profile.page_hinkley_threshold,
        RegimeDetectorKind::UpwardCusum => profile.upward_cusum_threshold,
        RegimeDetectorKind::DownwardCusum => profile.downward_cusum_threshold,
    }
}

fn invalid_regime_state<T>(reason: &'static str) -> Result<T, RegimeSignalEvidenceCodecError> {
    Err(RegimeSignalEvidenceCodecError::InvalidState { reason })
}

const fn encode_detector_kind(kind: RegimeDetectorKind) -> u8 {
    match kind {
        RegimeDetectorKind::PageHinkley => 0,
        RegimeDetectorKind::UpwardCusum => 1,
        RegimeDetectorKind::DownwardCusum => 2,
    }
}

fn decode_detector_kind(tag: u8) -> Result<RegimeDetectorKind, RegimeSignalEvidenceCodecError> {
    match tag {
        0 => Ok(RegimeDetectorKind::PageHinkley),
        1 => Ok(RegimeDetectorKind::UpwardCusum),
        2 => Ok(RegimeDetectorKind::DownwardCusum),
        actual => Err(RegimeSignalEvidenceCodecError::InvalidTag {
            component: "detector-kind",
            actual,
        }),
    }
}

const fn encode_regime_direction(direction: RegimeDirection) -> u8 {
    match direction {
        RegimeDirection::Increase => 0,
        RegimeDirection::Decrease => 1,
    }
}

fn decode_regime_direction(tag: u8) -> Result<RegimeDirection, RegimeSignalEvidenceCodecError> {
    match tag {
        0 => Ok(RegimeDirection::Increase),
        1 => Ok(RegimeDirection::Decrease),
        actual => Err(RegimeSignalEvidenceCodecError::InvalidTag {
            component: "regime-direction",
            actual,
        }),
    }
}

const fn encode_regime_status(status: RegimeSignalStatus) -> u8 {
    match status {
        RegimeSignalStatus::NoChangeDetected => 0,
        RegimeSignalStatus::ChangeDetected => 1,
    }
}

fn decode_regime_status(tag: u8) -> Result<RegimeSignalStatus, RegimeSignalEvidenceCodecError> {
    match tag {
        0 => Ok(RegimeSignalStatus::NoChangeDetected),
        1 => Ok(RegimeSignalStatus::ChangeDetected),
        actual => Err(RegimeSignalEvidenceCodecError::InvalidTag {
            component: "regime-status",
            actual,
        }),
    }
}

const fn encode_regime_selection(selection: RegimePolicySelection) -> u8 {
    match selection {
        RegimePolicySelection::CandidateDecision => 0,
        RegimePolicySelection::PinnedFallback => 1,
    }
}

fn decode_regime_selection(
    tag: u8,
) -> Result<RegimePolicySelection, RegimeSignalEvidenceCodecError> {
    match tag {
        0 => Ok(RegimePolicySelection::CandidateDecision),
        1 => Ok(RegimePolicySelection::PinnedFallback),
        actual => Err(RegimeSignalEvidenceCodecError::InvalidTag {
            component: "regime-selection",
            actual,
        }),
    }
}

struct RegimeEvidenceEncoder {
    bytes: Vec<u8>,
}

impl RegimeEvidenceEncoder {
    fn with_capacity(capacity: usize) -> Result<Self, RegimeSignalEvidenceCodecError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| RegimeSignalEvidenceCodecError::AllocationFailed)?;
        Ok(Self { bytes })
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn write_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn write_bool(&mut self, value: bool) {
        self.write_u8(u8::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_i64(&mut self, value: i64) {
        self.write_bytes(&value.to_le_bytes());
    }

    fn write_oid(&mut self, value: ObjectId) {
        self.write_bytes(&value.0);
    }

    fn write_string(&mut self, value: &str) -> Result<(), RegimeSignalEvidenceCodecError> {
        let length = u16::try_from(value.len())
            .map_err(|_| RegimeSignalEvidenceCodecError::LengthOverflow)?;
        self.write_u16(length);
        self.write_bytes(value.as_bytes());
        Ok(())
    }

    fn write_optional_u64(&mut self, value: Option<u64>) {
        match value {
            None => self.write_u8(0),
            Some(value) => {
                self.write_u8(1);
                self.write_u64(value);
            }
        }
    }

    fn write_series(&mut self, series: RuntimeMetricSeries) {
        let (tag, payload) = match series {
            RuntimeMetricSeries::ReadyQueueDepth => (0, BUILTIN_SERIES_PAYLOAD),
            RuntimeMetricSeries::WakeToRunLatencyMicros => (1, BUILTIN_SERIES_PAYLOAD),
            RuntimeMetricSeries::CancelStreakReward => (2, BUILTIN_SERIES_PAYLOAD),
            RuntimeMetricSeries::DrainRate => (3, BUILTIN_SERIES_PAYLOAD),
            RuntimeMetricSeries::Custom(payload) => (4, payload),
        };
        self.write_u8(tag);
        self.write_u16(payload);
    }
}

struct RegimeEvidenceDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> RegimeEvidenceDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], RegimeSignalEvidenceCodecError> {
        let remaining = self.remaining();
        if remaining < length {
            return Err(RegimeSignalEvidenceCodecError::Truncated {
                offset: self.offset,
                needed: length,
                remaining,
            });
        }
        let end = self
            .offset
            .checked_add(length)
            .ok_or(RegimeSignalEvidenceCodecError::LengthOverflow)?;
        let value =
            self.bytes
                .get(self.offset..end)
                .ok_or(RegimeSignalEvidenceCodecError::Truncated {
                    offset: self.offset,
                    needed: length,
                    remaining,
                })?;
        self.offset = end;
        Ok(value)
    }

    fn read_array<const LENGTH: usize>(
        &mut self,
    ) -> Result<[u8; LENGTH], RegimeSignalEvidenceCodecError> {
        let mut value = [0_u8; LENGTH];
        value.copy_from_slice(self.read_bytes(LENGTH)?);
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, RegimeSignalEvidenceCodecError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_bool(
        &mut self,
        component: &'static str,
    ) -> Result<bool, RegimeSignalEvidenceCodecError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            actual => Err(RegimeSignalEvidenceCodecError::InvalidTag { component, actual }),
        }
    }

    fn read_u16(&mut self) -> Result<u16, RegimeSignalEvidenceCodecError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_u32(&mut self) -> Result<u32, RegimeSignalEvidenceCodecError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, RegimeSignalEvidenceCodecError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    fn read_i64(&mut self) -> Result<i64, RegimeSignalEvidenceCodecError> {
        Ok(i64::from_le_bytes(self.read_array()?))
    }

    fn read_oid(&mut self) -> Result<ObjectId, RegimeSignalEvidenceCodecError> {
        Ok(ObjectId(self.read_array()?))
    }

    fn read_string(
        &mut self,
        component: &'static str,
        maximum: usize,
    ) -> Result<&'a str, RegimeSignalEvidenceCodecError> {
        let length = usize::from(self.read_u16()?);
        if length > maximum {
            return Err(RegimeSignalEvidenceCodecError::LengthLimitExceeded {
                component,
                actual: length,
                maximum,
            });
        }
        core::str::from_utf8(self.read_bytes(length)?)
            .map_err(|_| RegimeSignalEvidenceCodecError::InvalidUtf8)
    }

    fn read_optional_u64(
        &mut self,
        component: &'static str,
    ) -> Result<Option<u64>, RegimeSignalEvidenceCodecError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u64()?)),
            actual => Err(RegimeSignalEvidenceCodecError::InvalidTag { component, actual }),
        }
    }

    fn read_series(&mut self) -> Result<RuntimeMetricSeries, RegimeSignalEvidenceCodecError> {
        let tag = self.read_u8()?;
        let payload = self.read_u16()?;
        match tag {
            0..=3 if payload != BUILTIN_SERIES_PAYLOAD => {
                Err(RegimeSignalEvidenceCodecError::NonCanonicalSeriesPayload { tag, payload })
            }
            0 => Ok(RuntimeMetricSeries::ReadyQueueDepth),
            1 => Ok(RuntimeMetricSeries::WakeToRunLatencyMicros),
            2 => Ok(RuntimeMetricSeries::CancelStreakReward),
            3 => Ok(RuntimeMetricSeries::DrainRate),
            4 => Ok(RuntimeMetricSeries::Custom(payload)),
            actual => Err(RegimeSignalEvidenceCodecError::InvalidTag {
                component: "runtime-metric-series",
                actual,
            }),
        }
    }
}

/// Canonical accepted input plus the evidence state it produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegimeObservationUpdate {
    /// Accepted sample envelope.
    pub observation: SequencedRegimeSample,
    /// Evidence immediately after accepting the sample.
    pub evidence: RegimeSignalEvidence,
}

/// Non-mutating rejection of a regime-signal sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegimeObserveError {
    /// Input belongs to a different immutable signal identity.
    IdentityMismatch,
    /// Input carries a different immutable detector profile.
    ProfileMismatch,
    /// Input was not the exact next stream sequence.
    UnexpectedSequence {
        /// Required next sequence.
        expected: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// The fixed source window has already ended.
    WindowComplete {
        /// Inclusive last sequence.
        last: u64,
    },
    /// The profile's bounded observation budget is exhausted.
    ObservationLimitReached {
        /// Configured maximum.
        maximum: usize,
    },
    /// An internal exact counter could not advance.
    CounterExhausted,
}

impl fmt::Display for RegimeObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IdentityMismatch => formatter.write_str("regime signal identity does not match"),
            Self::ProfileMismatch => formatter.write_str("regime signal profile does not match"),
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected regime sequence {expected}, received {actual}"
            ),
            Self::WindowComplete { last } => {
                write!(formatter, "regime signal window ended at {last}")
            }
            Self::ObservationLimitReached { maximum } => {
                write!(
                    formatter,
                    "regime signal observation limit {maximum} reached"
                )
            }
            Self::CounterExhausted => formatter.write_str("regime signal counter is exhausted"),
        }
    }
}

impl std::error::Error for RegimeObserveError {}

/// Exactly sequenced wrapper around asupersync's combined change-point monitor.
#[derive(Debug)]
pub struct RegimeSignalMonitor {
    identity: RegimeSignalIdentity,
    profile: RegimeSignalProfile,
    core: ChangePointMonitor,
    next_sequence: Option<u64>,
    through_sequence: Option<u64>,
    observation_count: usize,
    detection_count: u64,
    dropped_receipt_count: u64,
    fallback_sequence: Option<u64>,
    retained_receipts: Vec<RegimeDetectionReceipt>,
}

impl RegimeSignalMonitor {
    /// Constructs a monitor after validating identity/profile bindings and
    /// reserving the complete bounded receipt inventory.
    pub fn try_new(
        identity: RegimeSignalIdentity,
        profile: RegimeSignalProfile,
    ) -> Result<Self, RegimeBuildError> {
        if identity.detector_profile_oid != profile.detector_profile_oid {
            return Err(RegimeBuildError::IdentityProfileOidMismatch {
                identity: identity.detector_profile_oid,
                profile: profile.detector_profile_oid,
            });
        }
        if identity.detector_id != profile.detector_id {
            return Err(RegimeBuildError::IdentityDetectorIdMismatch);
        }
        if identity.detector_version != profile.detector_version {
            return Err(RegimeBuildError::IdentityDetectorVersionMismatch {
                identity: identity.detector_version,
                profile: profile.detector_version,
            });
        }
        let maximum = u64::try_from(profile.max_observations).map_err(|_| {
            RegimeBuildError::ObservationLimitUnrepresentable {
                maximum: profile.max_observations,
            }
        })?;
        if maximum > identity.window.length {
            return Err(RegimeBuildError::ObservationLimitExceedsWindow {
                maximum: profile.max_observations,
                window: identity.window.length,
            });
        }

        let mut retained_receipts = Vec::new();
        retained_receipts
            .try_reserve_exact(profile.max_retained_receipts)
            .map_err(|_| RegimeBuildError::ReceiptAllocationFailed {
                requested: profile.max_retained_receipts,
            })?;
        let core = profile.foundation_monitor();
        let first_sequence = identity.window.first;

        Ok(Self {
            identity,
            profile,
            core,
            next_sequence: Some(first_sequence),
            through_sequence: None,
            observation_count: 0,
            detection_count: 0,
            dropped_receipt_count: 0,
            fallback_sequence: None,
            retained_receipts,
        })
    }

    /// Complete immutable signal identity.
    #[must_use]
    pub const fn identity(&self) -> &RegimeSignalIdentity {
        &self.identity
    }

    /// Complete immutable detector profile.
    #[must_use]
    pub const fn profile(&self) -> &RegimeSignalProfile {
        &self.profile
    }

    /// Exact next accepted sequence, or `None` after the identity window or
    /// observation budget ends.
    #[must_use]
    pub fn next_sequence(&self) -> Option<u64> {
        if self.observation_count >= self.profile.max_observations {
            None
        } else {
            self.next_sequence
        }
    }

    /// Number of accepted observations.
    #[must_use]
    pub const fn observation_count(&self) -> usize {
        self.observation_count
    }

    /// Current deterministic evidence without changing detector state.
    #[must_use]
    pub fn evidence(&self) -> RegimeSignalEvidence {
        self.project_evidence()
    }

    /// Accepts one exact stream position after preflighting every rejection.
    pub fn observe(
        &mut self,
        input: SequencedRegimeSample,
    ) -> Result<RegimeObservationUpdate, RegimeObserveError> {
        if input.identity != self.identity {
            return Err(RegimeObserveError::IdentityMismatch);
        }
        if input.profile != self.profile {
            return Err(RegimeObserveError::ProfileMismatch);
        }
        let Some(expected) = self.next_sequence else {
            return Err(RegimeObserveError::WindowComplete {
                last: self.identity.window.last,
            });
        };
        if self.observation_count >= self.profile.max_observations {
            return Err(RegimeObserveError::ObservationLimitReached {
                maximum: self.profile.max_observations,
            });
        }
        if input.stream_sequence != expected {
            return Err(RegimeObserveError::UnexpectedSequence {
                expected,
                actual: input.stream_sequence,
            });
        }

        let next_count = self
            .observation_count
            .checked_add(1)
            .ok_or(RegimeObserveError::CounterExhausted)?;
        let next_detection_count = self
            .detection_count
            .checked_add(1)
            .ok_or(RegimeObserveError::CounterExhausted)?;
        let next_dropped_receipt_count =
            if self.retained_receipts.len() == self.profile.max_retained_receipts {
                self.dropped_receipt_count
                    .checked_add(1)
                    .ok_or(RegimeObserveError::CounterExhausted)?
            } else {
                self.dropped_receipt_count
            };
        let next_sequence = if input.stream_sequence == self.identity.window.last {
            None
        } else {
            Some(
                input
                    .stream_sequence
                    .checked_add(1)
                    .ok_or(RegimeObserveError::CounterExhausted)?,
            )
        };

        let detection = self.core.observe(self.profile.series, input.sample);
        self.observation_count = next_count;
        self.through_sequence = Some(input.stream_sequence);
        self.next_sequence = next_sequence;
        if let Some(detection) = detection {
            self.record_detection(
                input.stream_sequence,
                detection,
                next_detection_count,
                next_dropped_receipt_count,
            );
        }

        let evidence = self.project_evidence();
        Ok(RegimeObservationUpdate {
            observation: input,
            evidence,
        })
    }

    fn record_detection(
        &mut self,
        stream_sequence: u64,
        detection: ChangePointDetection,
        next_detection_count: u64,
        next_dropped_receipt_count: u64,
    ) {
        let receipt = project_detection(stream_sequence, detection);
        self.detection_count = next_detection_count;
        if self.fallback_sequence.is_none() {
            self.fallback_sequence = Some(stream_sequence);
        }

        if self.retained_receipts.len() == self.profile.max_retained_receipts {
            self.retained_receipts.rotate_left(1);
            if let Some(last) = self.retained_receipts.last_mut() {
                *last = receipt;
            }
            self.dropped_receipt_count = next_dropped_receipt_count;
        } else {
            self.retained_receipts.push(receipt);
        }
    }

    fn project_evidence(&self) -> RegimeSignalEvidence {
        let detector_snapshots = self
            .core
            .snapshots()
            .into_iter()
            .zip(COMBINED_DETECTOR_SLOTS)
            .map(|(snapshot, slot)| project_snapshot(slot, snapshot))
            .collect();
        let status = if self.fallback_sequence.is_some() {
            RegimeSignalStatus::ChangeDetected
        } else {
            RegimeSignalStatus::NoChangeDetected
        };
        let selection = if self.fallback_sequence.is_some() {
            RegimePolicySelection::PinnedFallback
        } else {
            RegimePolicySelection::CandidateDecision
        };
        RegimeSignalEvidence {
            identity: self.identity.clone(),
            profile: self.profile.clone(),
            through_sequence: self.through_sequence,
            observation_count: canonical_count(self.observation_count),
            detection_count: self.detection_count,
            dropped_receipt_count: self.dropped_receipt_count,
            fallback_sequence: self.fallback_sequence,
            status,
            selection,
            detector_snapshots,
            retained_receipts: self.retained_receipts.clone(),
        }
    }
}

fn validate_cusum_config(
    slot: CombinedCusumSlot,
    config: CusumConfig,
) -> Result<(), RegimeBuildError> {
    let upward = slot == CombinedCusumSlot::Upward;
    if config.drift.as_micro_units() < 0 {
        return Err(RegimeBuildError::NegativeCusumDrift {
            upward,
            micro_units: config.drift.as_micro_units(),
        });
    }
    if config.threshold <= 0 {
        return Err(RegimeBuildError::NonPositiveCusumThreshold {
            upward,
            threshold: config.threshold,
        });
    }
    Ok(())
}

const fn project_detection_kind(
    kind: ChangePointDetectorKind,
    direction: FoundationDirection,
) -> RegimeDetectorKind {
    match (kind, direction) {
        (ChangePointDetectorKind::PageHinkley, _) => RegimeDetectorKind::PageHinkley,
        (ChangePointDetectorKind::Cusum, FoundationDirection::Increase) => {
            RegimeDetectorKind::UpwardCusum
        }
        (ChangePointDetectorKind::Cusum, FoundationDirection::Decrease) => {
            RegimeDetectorKind::DownwardCusum
        }
    }
}

const fn project_direction(direction: FoundationDirection) -> RegimeDirection {
    match direction {
        FoundationDirection::Increase => RegimeDirection::Increase,
        FoundationDirection::Decrease => RegimeDirection::Decrease,
    }
}

const fn project_detection(
    stream_sequence: u64,
    detection: ChangePointDetection,
) -> RegimeDetectionReceipt {
    RegimeDetectionReceipt {
        stream_sequence,
        detector_sample_index: detection.sample_index,
        series: detection.series,
        detector: project_detection_kind(detection.detector, detection.direction),
        direction: project_direction(detection.direction),
        sample_micro_units: detection.sample.as_micro_units(),
        statistic: detection.statistic,
        threshold: detection.threshold,
    }
}

const fn project_snapshot(
    slot: RegimeDetectorKind,
    snapshot: ChangePointSnapshot,
) -> RegimeDetectorSnapshot {
    RegimeDetectorSnapshot {
        series: snapshot.series,
        detector: slot,
        sample_count: snapshot.sample_count,
        mean_micro_units: snapshot.mean.as_micro_units(),
        statistic: snapshot.statistic,
        threshold: snapshot.threshold,
    }
}

fn canonical_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn copy_detector_id(value: &str) -> Result<String, RegimeBuildError> {
    if value.is_empty() {
        return Err(RegimeBuildError::EmptyDetectorId);
    }
    if value.len() > MAX_DETECTOR_ID_BYTES {
        return Err(RegimeBuildError::DetectorIdTooLong {
            actual: value.len(),
            maximum: MAX_DETECTOR_ID_BYTES,
        });
    }
    if let Some((offset, _)) = value
        .as_bytes()
        .iter()
        .enumerate()
        .find(|(_, byte)| !(0x21..=0x7e).contains(*byte))
    {
        return Err(RegimeBuildError::NonCanonicalDetectorId { offset });
    }

    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RegimeBuildError::DetectorIdAllocationFailed)?;
    owned.push_str(value);
    Ok(owned)
}

/// Stable identity of the model-qualified BOCPD plus Shiryaev--Roberts signal.
///
/// This is deliberately distinct from [`COMBINED_REGIME_SIGNAL_ID`].  The
/// latter permanently identifies the version-1 Page-Hinkley/CUSUM format;
/// silently changing its meaning would make persisted evidence ambiguous.
pub const BOCPD_SR_REGIME_SIGNAL_ID: &str = "fgdb.calibrate.regime.bocpd-sr";

/// First version of the bounded Beta-Bernoulli BOCPD plus SR composition.
pub const BOCPD_SR_REGIME_SIGNAL_VERSION: u32 = 1;

/// Exact probability and likelihood-ratio scale used by the version-1 model.
pub const BOCPD_SR_SCALE: u64 = 1_000_000;

/// Absolute bound on retained BOCPD run-length hypotheses.
pub const MAX_BOCPD_SR_RUN_LENGTH: usize = 4_096;

/// Domain separator for canonical BOCPD plus SR evidence.
pub const BOCPD_SR_EVIDENCE_ENCODING_DOMAIN: &[u8] = b"fgdb:bocpd-sr-regime-evidence";

/// Current canonical BOCPD plus SR evidence encoding version.
pub const BOCPD_SR_EVIDENCE_ENCODING_VERSION: u16 = 1;

/// Construction, observation, arithmetic, or canonical-codec failure for the
/// BOCPD plus SR regime signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BocpdSrError {
    /// The shared immutable identity was malformed.
    Identity(RegimeBuildError),
    /// The identity did not name the BOCPD plus SR contract exactly.
    WrongDetectorContract,
    /// Identity and profile referred to different immutable profile objects.
    IdentityProfileOidMismatch,
    /// A fixed-point probability was not strictly between zero and one.
    InvalidProbability {
        /// Stable profile-field name.
        component: &'static str,
        /// Rejected fixed-point mass.
        value: u64,
    },
    /// A positive integer model parameter was zero.
    ZeroParameter {
        /// Stable profile-field name.
        component: &'static str,
    },
    /// The run-length bound exceeded the implementation ceiling.
    RunLengthLimitTooLarge {
        /// Supplied bound.
        actual: usize,
        /// Largest supported bound.
        maximum: usize,
    },
    /// The receipt bound exceeded the shared regime-evidence ceiling.
    ReceiptLimitTooLarge {
        /// Supplied bound.
        actual: usize,
        /// Largest supported bound.
        maximum: usize,
    },
    /// The receipt bound exceeded the observation budget.
    ReceiptLimitExceedsObservationLimit,
    /// The observation budget exceeded the immutable source window.
    ObservationLimitExceedsWindow,
    /// The input belongs to another signal identity.
    IdentityMismatch,
    /// The input carries another detector profile.
    ProfileMismatch,
    /// The input was not the exact next source sequence.
    UnexpectedSequence {
        /// Required sequence.
        expected: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// The immutable source window is complete.
    WindowComplete,
    /// The bounded observation budget is complete.
    ObservationLimitReached,
    /// Exact arithmetic could not represent the next state.
    ArithmeticOverflow {
        /// Stable operation name.
        component: &'static str,
    },
    /// A bounded state or evidence allocation failed.
    AllocationFailed,
    /// Canonical bytes ended before one complete value was available.
    Truncated {
        /// Offset at which the read began.
        offset: usize,
        /// Bytes requested.
        needed: usize,
        /// Bytes remaining.
        remaining: usize,
    },
    /// A canonical length could not be represented.
    LengthOverflow,
    /// A canonical discriminator was unknown.
    InvalidTag {
        /// Stable component name.
        component: &'static str,
        /// Rejected value.
        actual: u8,
    },
    /// Canonical evidence violated a dynamic invariant.
    InvalidState {
        /// Stable diagnostic.
        reason: &'static str,
    },
    /// Bytes remained after one complete evidence value.
    TrailingBytes {
        /// Unconsumed byte count.
        remaining: usize,
    },
}

impl From<RegimeBuildError> for BocpdSrError {
    fn from(value: RegimeBuildError) -> Self {
        Self::Identity(value)
    }
}

impl fmt::Display for BocpdSrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(error) => write!(formatter, "invalid BOCPD+SR identity: {error}"),
            Self::WrongDetectorContract => {
                formatter.write_str("identity does not name BOCPD+SR version 1")
            }
            Self::IdentityProfileOidMismatch => {
                formatter.write_str("BOCPD+SR identity/profile OIDs differ")
            }
            Self::InvalidProbability { component, value } => write!(
                formatter,
                "BOCPD+SR probability {component}={value} is outside 1..{BOCPD_SR_SCALE}"
            ),
            Self::ZeroParameter { component } => {
                write!(formatter, "BOCPD+SR parameter {component} is zero")
            }
            Self::RunLengthLimitTooLarge { actual, maximum } => write!(
                formatter,
                "BOCPD+SR run-length limit {actual} exceeds {maximum}"
            ),
            Self::ReceiptLimitTooLarge { actual, maximum } => write!(
                formatter,
                "BOCPD+SR receipt limit {actual} exceeds {maximum}"
            ),
            Self::ReceiptLimitExceedsObservationLimit => {
                formatter.write_str("BOCPD+SR receipt limit exceeds the observation limit")
            }
            Self::ObservationLimitExceedsWindow => {
                formatter.write_str("BOCPD+SR observation limit exceeds the identity window")
            }
            Self::IdentityMismatch => formatter.write_str("BOCPD+SR identity mismatch"),
            Self::ProfileMismatch => formatter.write_str("BOCPD+SR profile mismatch"),
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected BOCPD+SR sequence {expected}, received {actual}"
            ),
            Self::WindowComplete => formatter.write_str("BOCPD+SR window is complete"),
            Self::ObservationLimitReached => {
                formatter.write_str("BOCPD+SR observation limit reached")
            }
            Self::ArithmeticOverflow { component } => {
                write!(formatter, "BOCPD+SR arithmetic overflow in {component}")
            }
            Self::AllocationFailed => formatter.write_str("BOCPD+SR allocation failed"),
            Self::Truncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "truncated BOCPD+SR evidence at {offset}: need {needed}, have {remaining}"
            ),
            Self::LengthOverflow => formatter.write_str("BOCPD+SR length overflow"),
            Self::InvalidTag { component, actual } => {
                write!(formatter, "invalid BOCPD+SR {component} tag {actual}")
            }
            Self::InvalidState { reason } => {
                write!(formatter, "invalid BOCPD+SR evidence state: {reason}")
            }
            Self::TrailingBytes { remaining } => {
                write!(formatter, "{remaining} trailing BOCPD+SR evidence bytes")
            }
        }
    }
}

impl std::error::Error for BocpdSrError {}

/// Immutable, bounded model profile for one binary BOCPD plus SR signal.
///
/// The BOCPD branch uses a pinned Beta-Bernoulli reset-prior recurrence.  Each
/// update quantizes predictive probabilities to [`BOCPD_SR_SCALE`], folds all
/// run lengths beyond `max_run_length` into one declared tail cell, and uses
/// deterministic largest-remainder normalization.  This is model-qualified
/// advisory evidence, not a universal change-point or false-alarm guarantee.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BocpdSrProfile {
    detector_profile_oid: ObjectId,
    binary_input_contract_oid: ObjectId,
    null_one_mass: u64,
    alternative_one_mass: u64,
    beta_alpha: u64,
    beta_beta: u64,
    hazard_mass: u64,
    sr_threshold: u128,
    sr_cap: u128,
    changepoint_threshold_mass: u64,
    max_run_length: usize,
    max_observations: usize,
    max_retained_receipts: usize,
}

impl BocpdSrProfile {
    /// Validates one complete version-1 binary model profile.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        detector_profile_oid: ObjectId,
        binary_input_contract_oid: ObjectId,
        null_one_mass: u64,
        alternative_one_mass: u64,
        beta_alpha: u64,
        beta_beta: u64,
        hazard_mass: u64,
        sr_threshold: u128,
        sr_cap: u128,
        changepoint_threshold_mass: u64,
        max_run_length: usize,
        max_observations: usize,
        max_retained_receipts: usize,
    ) -> Result<Self, BocpdSrError> {
        for (component, value) in [
            ("null_one_mass", null_one_mass),
            ("alternative_one_mass", alternative_one_mass),
            ("hazard_mass", hazard_mass),
            ("changepoint_threshold_mass", changepoint_threshold_mass),
        ] {
            if value == 0 || value >= BOCPD_SR_SCALE {
                return Err(BocpdSrError::InvalidProbability { component, value });
            }
        }
        for (component, value) in [
            ("beta_alpha", beta_alpha),
            ("beta_beta", beta_beta),
            ("max_run_length", canonical_count(max_run_length)),
            ("max_observations", canonical_count(max_observations)),
            (
                "max_retained_receipts",
                canonical_count(max_retained_receipts),
            ),
        ] {
            if value == 0 {
                return Err(BocpdSrError::ZeroParameter { component });
            }
        }
        if sr_threshold == 0 {
            return Err(BocpdSrError::ZeroParameter {
                component: "sr_threshold",
            });
        }
        if sr_cap < sr_threshold {
            return Err(BocpdSrError::InvalidState {
                reason: "SR cap is below the alarm threshold",
            });
        }
        let maximum_sr_cap = u128::MAX
            .checked_div(u128::from(BOCPD_SR_SCALE))
            .and_then(|value| value.checked_sub(u128::from(BOCPD_SR_SCALE)))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "sr-cap-bound",
            })?;
        if sr_cap > maximum_sr_cap {
            return Err(BocpdSrError::InvalidState {
                reason: "SR cap exceeds the checked recurrence bound",
            });
        }
        if max_run_length > MAX_BOCPD_SR_RUN_LENGTH {
            return Err(BocpdSrError::RunLengthLimitTooLarge {
                actual: max_run_length,
                maximum: MAX_BOCPD_SR_RUN_LENGTH,
            });
        }
        if max_observations > MAX_REGIME_OBSERVATIONS {
            return Err(BocpdSrError::ZeroParameter {
                component: "max_observations_out_of_range",
            });
        }
        if max_retained_receipts > MAX_RETAINED_REGIME_RECEIPTS {
            return Err(BocpdSrError::ReceiptLimitTooLarge {
                actual: max_retained_receipts,
                maximum: MAX_RETAINED_REGIME_RECEIPTS,
            });
        }
        if max_retained_receipts > max_observations {
            return Err(BocpdSrError::ReceiptLimitExceedsObservationLimit);
        }
        let observation_bound =
            u64::try_from(max_observations).map_err(|_| BocpdSrError::LengthOverflow)?;
        if beta_alpha
            .checked_add(beta_beta)
            .and_then(|sum| sum.checked_add(observation_bound))
            .is_none()
        {
            return Err(BocpdSrError::InvalidState {
                reason: "Beta prior cannot represent the declared observation budget",
            });
        }
        Ok(Self {
            detector_profile_oid,
            binary_input_contract_oid,
            null_one_mass,
            alternative_one_mass,
            beta_alpha,
            beta_beta,
            hazard_mass,
            sr_threshold,
            sr_cap,
            changepoint_threshold_mass,
            max_run_length,
            max_observations,
            max_retained_receipts,
        })
    }

    /// OID of the complete parameter object.
    #[must_use]
    pub const fn detector_profile_oid(&self) -> ObjectId {
        self.detector_profile_oid
    }

    /// Identity of the already-projected binary input contract.
    ///
    /// Version 1 deliberately does not transform arbitrary raw metrics.  A
    /// producer must register and persist that upstream transformation, then
    /// feed this monitor the resulting immutable binary stream.
    #[must_use]
    pub const fn binary_input_contract_oid(&self) -> ObjectId {
        self.binary_input_contract_oid
    }

    /// Null probability of a one, scaled by [`BOCPD_SR_SCALE`].
    #[must_use]
    pub const fn null_one_mass(&self) -> u64 {
        self.null_one_mass
    }

    /// Alternative probability of a one, scaled by [`BOCPD_SR_SCALE`].
    #[must_use]
    pub const fn alternative_one_mass(&self) -> u64 {
        self.alternative_one_mass
    }

    /// Constant BOCPD hazard, scaled by [`BOCPD_SR_SCALE`].
    #[must_use]
    pub const fn hazard_mass(&self) -> u64 {
        self.hazard_mass
    }

    /// SR alarm threshold in fixed-point likelihood-ratio units.
    #[must_use]
    pub const fn sr_threshold(&self) -> u128 {
        self.sr_threshold
    }

    /// Saturating bound for the SR statistic.  The cap is immutable evidence
    /// identity and must be no smaller than the alarm threshold.
    #[must_use]
    pub const fn sr_cap(&self) -> u128 {
        self.sr_cap
    }

    /// BOCPD run-zero posterior threshold.
    #[must_use]
    pub const fn changepoint_threshold_mass(&self) -> u64 {
        self.changepoint_threshold_mass
    }

    /// Largest exact run-length cell before tail folding.
    #[must_use]
    pub const fn max_run_length(&self) -> usize {
        self.max_run_length
    }

    /// Maximum accepted observations.
    #[must_use]
    pub const fn max_observations(&self) -> usize {
        self.max_observations
    }
}

/// Binary observation vocabulary for the version-1 model.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum BocpdSrObservation {
    /// The bound pre-projected binary stream emitted zero.
    Zero = 0,
    /// The bound pre-projected binary stream emitted one.
    One = 1,
}

/// One immutable, exactly sequenced BOCPD plus SR input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SequencedBocpdSrObservation {
    identity: RegimeSignalIdentity,
    profile: BocpdSrProfile,
    stream_sequence: u64,
    observation: BocpdSrObservation,
}

impl SequencedBocpdSrObservation {
    /// Constructs one immutable input envelope.
    #[must_use]
    pub const fn new(
        identity: RegimeSignalIdentity,
        profile: BocpdSrProfile,
        stream_sequence: u64,
        observation: BocpdSrObservation,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            observation,
        }
    }
}

/// One bounded posterior run-length hypothesis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BocpdSrRunState {
    run_length: u32,
    posterior_mass: u64,
    successes: u64,
    failures: u64,
}

impl BocpdSrRunState {
    /// Run length represented by this cell.
    #[must_use]
    pub const fn run_length(self) -> u32 {
        self.run_length
    }

    /// Normalized posterior mass over [`BOCPD_SR_SCALE`].
    #[must_use]
    pub const fn posterior_mass(self) -> u64 {
        self.posterior_mass
    }

    /// Effective one count retained by the Beta-Bernoulli cell.
    #[must_use]
    pub const fn successes(self) -> u64 {
        self.successes
    }

    /// Effective zero count retained by the Beta-Bernoulli cell.
    #[must_use]
    pub const fn failures(self) -> u64 {
        self.failures
    }
}

/// One same-sample crossing of both SR and BOCPD thresholds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BocpdSrReceipt {
    stream_sequence: u64,
    sr_statistic: u128,
    sr_threshold: u128,
    changepoint_mass: u64,
    changepoint_threshold_mass: u64,
    map_run_length: u32,
    tail_mass: u64,
    successor_regime_epoch: u64,
    successor_effective_sequence: u64,
}

impl BocpdSrReceipt {
    /// Source sequence where both detectors crossed.
    #[must_use]
    pub const fn stream_sequence(self) -> u64 {
        self.stream_sequence
    }

    /// Updated SR statistic.
    #[must_use]
    pub const fn sr_statistic(self) -> u128 {
        self.sr_statistic
    }

    /// Configured SR crossing threshold.
    #[must_use]
    pub const fn sr_threshold(self) -> u128 {
        self.sr_threshold
    }

    /// Updated BOCPD run-zero posterior mass.
    #[must_use]
    pub const fn changepoint_mass(self) -> u64 {
        self.changepoint_mass
    }

    /// Configured BOCPD run-zero posterior threshold.
    #[must_use]
    pub const fn changepoint_threshold_mass(self) -> u64 {
        self.changepoint_threshold_mass
    }

    /// Maximum-a-posteriori run-length bucket at the crossing.
    #[must_use]
    pub const fn map_run_length(self) -> u32 {
        self.map_run_length
    }

    /// Folded tail mass at the crossing.
    #[must_use]
    pub const fn tail_mass(self) -> u64 {
        self.tail_mass
    }

    /// New regime epoch created by this terminal receipt.
    #[must_use]
    pub const fn successor_regime_epoch(self) -> u64 {
        self.successor_regime_epoch
    }

    /// First source sequence belonging to the successor regime.
    #[must_use]
    pub const fn successor_effective_sequence(self) -> u64 {
        self.successor_effective_sequence
    }

    /// A receipt is assumption-bearing evidence, never ground truth.
    #[must_use]
    pub const fn is_ground_truth(self) -> bool {
        false
    }
}

/// Complete deterministic evidence after a binary observation prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BocpdSrEvidence {
    identity: RegimeSignalIdentity,
    profile: BocpdSrProfile,
    through_sequence: Option<u64>,
    observation_count: u64,
    sr_statistic: u128,
    changepoint_mass: u64,
    map_run_length: u32,
    tail_mass: u64,
    detection_count: u64,
    dropped_receipt_count: u64,
    fallback_sequence: Option<u64>,
    observations: Vec<BocpdSrObservation>,
    run_states: Vec<BocpdSrRunState>,
    retained_receipts: Vec<BocpdSrReceipt>,
}

impl BocpdSrEvidence {
    /// Immutable signal identity.
    #[must_use]
    pub const fn identity(&self) -> &RegimeSignalIdentity {
        &self.identity
    }

    /// Immutable model profile.
    #[must_use]
    pub const fn profile(&self) -> &BocpdSrProfile {
        &self.profile
    }

    /// Last accepted sequence.
    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    /// Accepted binary observations.
    #[must_use]
    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// Exact accepted binary prefix.  Retaining this bounded prefix makes a
    /// decoded resume state mechanically replay-checkable instead of merely
    /// shape-valid.
    #[must_use]
    pub fn observations(&self) -> &[BocpdSrObservation] {
        &self.observations
    }

    /// Current SR statistic.
    #[must_use]
    pub const fn sr_statistic(&self) -> u128 {
        self.sr_statistic
    }

    /// Current BOCPD run-zero posterior mass.
    #[must_use]
    pub const fn changepoint_mass(&self) -> u64 {
        self.changepoint_mass
    }

    /// Current maximum-a-posteriori run length.
    #[must_use]
    pub const fn map_run_length(&self) -> u32 {
        self.map_run_length
    }

    /// Posterior mass in the declared folded tail cell.
    #[must_use]
    pub const fn tail_mass(&self) -> u64 {
        self.tail_mass
    }

    /// Total same-sample joint crossings, including evicted receipts.
    #[must_use]
    pub const fn detection_count(&self) -> u64 {
        self.detection_count
    }

    /// First sequence that selected the deterministic fallback.
    #[must_use]
    pub const fn fallback_sequence(&self) -> Option<u64> {
        self.fallback_sequence
    }

    /// Advisory combined-signal status.
    #[must_use]
    pub const fn status(&self) -> RegimeSignalStatus {
        if self.fallback_sequence.is_some() {
            RegimeSignalStatus::ChangeDetected
        } else {
            RegimeSignalStatus::NoChangeDetected
        }
    }

    /// Sticky policy selection derived from the first combined receipt.
    #[must_use]
    pub const fn selection(&self) -> RegimePolicySelection {
        if self.fallback_sequence.is_some() {
            RegimePolicySelection::PinnedFallback
        } else {
            RegimePolicySelection::CandidateDecision
        }
    }

    /// Exact selected policy identity.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection() {
            RegimePolicySelection::CandidateDecision => self.identity.candidate_decision_oid,
            RegimePolicySelection::PinnedFallback => self.identity.pinned_fallback_oid,
        }
    }

    /// Ordered posterior cells, starting at run length zero.
    #[must_use]
    pub fn run_states(&self) -> &[BocpdSrRunState] {
        &self.run_states
    }

    /// Retained joint-crossing receipts in ascending sequence order.
    #[must_use]
    pub fn retained_receipts(&self) -> &[BocpdSrReceipt] {
        &self.retained_receipts
    }

    /// Canonical, strict evidence encoding.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, BocpdSrError> {
        validate_bocpd_sr_evidence(self)?;
        let mut encoder = BocpdSrEncoder::try_with_capacity(bocpd_sr_encoded_len(self)?)?;
        encoder.write_u16(
            u16::try_from(BOCPD_SR_EVIDENCE_ENCODING_DOMAIN.len())
                .map_err(|_| BocpdSrError::LengthOverflow)?,
        );
        encoder.write_bytes(BOCPD_SR_EVIDENCE_ENCODING_DOMAIN);
        encoder.write_u16(BOCPD_SR_EVIDENCE_ENCODING_VERSION);
        encode_bocpd_sr_identity(&mut encoder, &self.identity)?;
        encode_bocpd_sr_profile(&mut encoder, &self.profile)?;
        encoder.write_optional_u64(self.through_sequence);
        encoder.write_u64(self.observation_count);
        encoder.write_u128(self.sr_statistic);
        encoder.write_u64(self.changepoint_mass);
        encoder.write_u32(self.map_run_length);
        encoder.write_u64(self.tail_mass);
        encoder.write_u64(self.detection_count);
        encoder.write_u64(self.dropped_receipt_count);
        encoder.write_optional_u64(self.fallback_sequence);
        encoder.write_u32(
            u32::try_from(self.observations.len()).map_err(|_| BocpdSrError::LengthOverflow)?,
        );
        for observation in &self.observations {
            encoder.write_u8(*observation as u8);
        }
        encoder.write_u32(
            u32::try_from(self.run_states.len()).map_err(|_| BocpdSrError::LengthOverflow)?,
        );
        for state in &self.run_states {
            encoder.write_u32(state.run_length);
            encoder.write_u64(state.posterior_mass);
            encoder.write_u64(state.successes);
            encoder.write_u64(state.failures);
        }
        encoder.write_u32(
            u32::try_from(self.retained_receipts.len())
                .map_err(|_| BocpdSrError::LengthOverflow)?,
        );
        for receipt in &self.retained_receipts {
            encoder.write_u64(receipt.stream_sequence);
            encoder.write_u128(receipt.sr_statistic);
            encoder.write_u128(receipt.sr_threshold);
            encoder.write_u64(receipt.changepoint_mass);
            encoder.write_u64(receipt.changepoint_threshold_mass);
            encoder.write_u32(receipt.map_run_length);
            encoder.write_u64(receipt.tail_mass);
            encoder.write_u64(receipt.successor_regime_epoch);
            encoder.write_u64(receipt.successor_effective_sequence);
        }
        Ok(encoder.finish())
    }

    /// Strictly decodes and validates one complete canonical evidence record.
    pub fn try_from_canonical_bytes(bytes: &[u8]) -> Result<Self, BocpdSrError> {
        let mut decoder = BocpdSrDecoder::new(bytes);
        let domain_length = usize::from(decoder.read_u16()?);
        if domain_length != BOCPD_SR_EVIDENCE_ENCODING_DOMAIN.len()
            || decoder.read_bytes(domain_length)? != BOCPD_SR_EVIDENCE_ENCODING_DOMAIN
        {
            return Err(BocpdSrError::InvalidState {
                reason: "encoding domain mismatch",
            });
        }
        let version = decoder.read_u16()?;
        if version != BOCPD_SR_EVIDENCE_ENCODING_VERSION {
            return Err(BocpdSrError::InvalidState {
                reason: "unsupported encoding version",
            });
        }
        let identity = decode_bocpd_sr_identity(&mut decoder)?;
        let profile = decode_bocpd_sr_profile(&mut decoder)?;
        let through_sequence = decoder.read_optional_u64("through-sequence")?;
        let observation_count = decoder.read_u64()?;
        let sr_statistic = decoder.read_u128()?;
        let changepoint_mass = decoder.read_u64()?;
        let map_run_length = decoder.read_u32()?;
        let tail_mass = decoder.read_u64()?;
        let detection_count = decoder.read_u64()?;
        let dropped_receipt_count = decoder.read_u64()?;
        let fallback_sequence = decoder.read_optional_u64("fallback-sequence")?;
        let observation_length =
            usize::try_from(decoder.read_u32()?).map_err(|_| BocpdSrError::LengthOverflow)?;
        if observation_length > profile.max_observations {
            return Err(BocpdSrError::InvalidState {
                reason: "observation prefix exceeds the profile bound",
            });
        }
        let mut observations = Vec::new();
        observations
            .try_reserve_exact(observation_length)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        for _ in 0..observation_length {
            observations.push(match decoder.read_u8()? {
                0 => BocpdSrObservation::Zero,
                1 => BocpdSrObservation::One,
                actual => {
                    return Err(BocpdSrError::InvalidTag {
                        component: "binary-observation",
                        actual,
                    });
                }
            });
        }
        let state_count =
            usize::try_from(decoder.read_u32()?).map_err(|_| BocpdSrError::LengthOverflow)?;
        if state_count == 0 || state_count > profile.max_run_length + 1 {
            return Err(BocpdSrError::InvalidState {
                reason: "posterior state count is outside the profile bound",
            });
        }
        let mut run_states = Vec::new();
        run_states
            .try_reserve_exact(state_count)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        for _ in 0..state_count {
            run_states.push(BocpdSrRunState {
                run_length: decoder.read_u32()?,
                posterior_mass: decoder.read_u64()?,
                successes: decoder.read_u64()?,
                failures: decoder.read_u64()?,
            });
        }
        let receipt_count =
            usize::try_from(decoder.read_u32()?).map_err(|_| BocpdSrError::LengthOverflow)?;
        if receipt_count > profile.max_retained_receipts {
            return Err(BocpdSrError::InvalidState {
                reason: "receipt count exceeds the profile bound",
            });
        }
        let mut retained_receipts = Vec::new();
        retained_receipts
            .try_reserve_exact(receipt_count)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        for _ in 0..receipt_count {
            retained_receipts.push(BocpdSrReceipt {
                stream_sequence: decoder.read_u64()?,
                sr_statistic: decoder.read_u128()?,
                sr_threshold: decoder.read_u128()?,
                changepoint_mass: decoder.read_u64()?,
                changepoint_threshold_mass: decoder.read_u64()?,
                map_run_length: decoder.read_u32()?,
                tail_mass: decoder.read_u64()?,
                successor_regime_epoch: decoder.read_u64()?,
                successor_effective_sequence: decoder.read_u64()?,
            });
        }
        if decoder.remaining() != 0 {
            return Err(BocpdSrError::TrailingBytes {
                remaining: decoder.remaining(),
            });
        }
        let evidence = Self {
            identity,
            profile,
            through_sequence,
            observation_count,
            sr_statistic,
            changepoint_mass,
            map_run_length,
            tail_mass,
            detection_count,
            dropped_receipt_count,
            fallback_sequence,
            observations,
            run_states,
            retained_receipts,
        };
        validate_bocpd_sr_evidence(&evidence)?;
        Ok(evidence)
    }
}

/// Accepted binary input plus the evidence state it produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BocpdSrObservationUpdate {
    /// Accepted immutable observation.
    pub observation: SequencedBocpdSrObservation,
    /// Evidence immediately after acceptance.
    pub evidence: BocpdSrEvidence,
}

/// Deterministic, bounded BOCPD plus Shiryaev--Roberts monitor.
#[derive(Debug)]
pub struct BocpdSrMonitor {
    identity: RegimeSignalIdentity,
    profile: BocpdSrProfile,
    next_sequence: Option<u64>,
    through_sequence: Option<u64>,
    observation_count: usize,
    sr_statistic: u128,
    run_states: Vec<BocpdSrRunState>,
    detection_count: u64,
    dropped_receipt_count: u64,
    fallback_sequence: Option<u64>,
    observations: Vec<BocpdSrObservation>,
    retained_receipts: Vec<BocpdSrReceipt>,
}

impl BocpdSrMonitor {
    /// Constructs an empty monitor after validating all immutable bindings.
    pub fn try_new(
        identity: RegimeSignalIdentity,
        profile: BocpdSrProfile,
    ) -> Result<Self, BocpdSrError> {
        if identity.detector_id() != BOCPD_SR_REGIME_SIGNAL_ID
            || identity.detector_version() != BOCPD_SR_REGIME_SIGNAL_VERSION
        {
            return Err(BocpdSrError::WrongDetectorContract);
        }
        if identity.detector_profile_oid() != profile.detector_profile_oid {
            return Err(BocpdSrError::IdentityProfileOidMismatch);
        }
        if canonical_count(profile.max_observations) > identity.window().len() {
            return Err(BocpdSrError::ObservationLimitExceedsWindow);
        }
        let mut run_states = Vec::new();
        run_states
            .try_reserve_exact(profile.max_run_length + 1)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        run_states.push(BocpdSrRunState {
            run_length: 0,
            posterior_mass: BOCPD_SR_SCALE,
            successes: 0,
            failures: 0,
        });
        let mut retained_receipts = Vec::new();
        retained_receipts
            .try_reserve_exact(profile.max_retained_receipts)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        let mut observations = Vec::new();
        observations
            .try_reserve_exact(profile.max_observations)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        let first = identity.window().first();
        Ok(Self {
            identity,
            profile,
            next_sequence: Some(first),
            through_sequence: None,
            observation_count: 0,
            sr_statistic: 0,
            run_states,
            detection_count: 0,
            dropped_receipt_count: 0,
            fallback_sequence: None,
            observations,
            retained_receipts,
        })
    }

    /// Restores an exactly replayable monitor from strict canonical evidence.
    /// A terminal detected prefix remains terminal; a quiet prefix resumes at
    /// the exact next source sequence.
    pub fn try_resume_from_evidence(evidence: BocpdSrEvidence) -> Result<Self, BocpdSrError> {
        validate_bocpd_sr_evidence(&evidence)?;
        let observation_count = usize::try_from(evidence.observation_count)
            .map_err(|_| BocpdSrError::LengthOverflow)?;
        let next_sequence = if evidence.fallback_sequence.is_some()
            || observation_count >= evidence.profile.max_observations
            || evidence.through_sequence == Some(evidence.identity.window().last())
        {
            None
        } else {
            match evidence.through_sequence {
                None => Some(evidence.identity.window().first()),
                Some(sequence) => Some(sequence.checked_add(1).ok_or(
                    BocpdSrError::ArithmeticOverflow {
                        component: "resume-sequence",
                    },
                )?),
            }
        };
        Ok(Self {
            identity: evidence.identity,
            profile: evidence.profile,
            next_sequence,
            through_sequence: evidence.through_sequence,
            observation_count,
            sr_statistic: evidence.sr_statistic,
            run_states: evidence.run_states,
            detection_count: evidence.detection_count,
            dropped_receipt_count: evidence.dropped_receipt_count,
            fallback_sequence: evidence.fallback_sequence,
            observations: evidence.observations,
            retained_receipts: evidence.retained_receipts,
        })
    }

    /// Current immutable evidence without advancing the monitor.
    #[must_use]
    pub fn evidence(&self) -> BocpdSrEvidence {
        let (changepoint_mass, map_run_length, tail_mass) =
            posterior_summary(&self.run_states, self.profile.max_run_length);
        BocpdSrEvidence {
            identity: self.identity.clone(),
            profile: self.profile.clone(),
            through_sequence: self.through_sequence,
            observation_count: canonical_count(self.observation_count),
            sr_statistic: self.sr_statistic,
            changepoint_mass,
            map_run_length,
            tail_mass,
            detection_count: self.detection_count,
            dropped_receipt_count: self.dropped_receipt_count,
            fallback_sequence: self.fallback_sequence,
            observations: self.observations.clone(),
            run_states: self.run_states.clone(),
            retained_receipts: self.retained_receipts.clone(),
        }
    }

    /// Accepts one exact source position.  Every fallible next-state operation
    /// completes in temporaries; an error leaves the monitor byte-identical.
    pub fn observe(
        &mut self,
        input: SequencedBocpdSrObservation,
    ) -> Result<BocpdSrObservationUpdate, BocpdSrError> {
        if input.identity != self.identity {
            return Err(BocpdSrError::IdentityMismatch);
        }
        if input.profile != self.profile {
            return Err(BocpdSrError::ProfileMismatch);
        }
        self.try_advance(input.stream_sequence, input.observation)?;
        Ok(BocpdSrObservationUpdate {
            observation: input,
            evidence: self.evidence(),
        })
    }

    fn try_advance(
        &mut self,
        stream_sequence: u64,
        observation: BocpdSrObservation,
    ) -> Result<(), BocpdSrError> {
        let Some(expected) = self.next_sequence else {
            return Err(BocpdSrError::WindowComplete);
        };
        if self.observation_count >= self.profile.max_observations {
            return Err(BocpdSrError::ObservationLimitReached);
        }
        if stream_sequence != expected {
            return Err(BocpdSrError::UnexpectedSequence {
                expected,
                actual: stream_sequence,
            });
        }

        let next_count =
            self.observation_count
                .checked_add(1)
                .ok_or(BocpdSrError::ArithmeticOverflow {
                    component: "observation-count",
                })?;
        let next_sr = next_sr_statistic(self.sr_statistic, observation, &self.profile)?;
        let next_states = next_bocpd_states(&self.run_states, observation, &self.profile)?;
        let (changepoint_mass, map_run_length, tail_mass) =
            posterior_summary(&next_states, self.profile.max_run_length);
        let crossed = next_sr >= self.profile.sr_threshold
            && changepoint_mass >= self.profile.changepoint_threshold_mass;

        let mut next_receipts = Vec::new();
        next_receipts
            .try_reserve_exact(self.profile.max_retained_receipts)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        next_receipts.extend_from_slice(&self.retained_receipts);
        let mut next_detection_count = self.detection_count;
        let mut next_dropped_count = self.dropped_receipt_count;
        let mut next_fallback = self.fallback_sequence;
        if crossed {
            next_detection_count =
                next_detection_count
                    .checked_add(1)
                    .ok_or(BocpdSrError::ArithmeticOverflow {
                        component: "detection-count",
                    })?;
            let receipt = BocpdSrReceipt {
                stream_sequence,
                sr_statistic: next_sr,
                sr_threshold: self.profile.sr_threshold,
                changepoint_mass,
                changepoint_threshold_mass: self.profile.changepoint_threshold_mass,
                map_run_length,
                tail_mass,
                successor_regime_epoch: self.identity.regime_epoch().checked_add(1).ok_or(
                    BocpdSrError::ArithmeticOverflow {
                        component: "successor-regime-epoch",
                    },
                )?,
                successor_effective_sequence: stream_sequence.checked_add(1).ok_or(
                    BocpdSrError::ArithmeticOverflow {
                        component: "successor-effective-sequence",
                    },
                )?,
            };
            if next_receipts.len() == self.profile.max_retained_receipts {
                next_receipts.rotate_left(1);
                if let Some(last) = next_receipts.last_mut() {
                    *last = receipt;
                }
                next_dropped_count =
                    next_dropped_count
                        .checked_add(1)
                        .ok_or(BocpdSrError::ArithmeticOverflow {
                            component: "dropped-receipt-count",
                        })?;
            } else {
                next_receipts.push(receipt);
            }
            if next_fallback.is_none() {
                next_fallback = Some(stream_sequence);
            }
        }
        let next_sequence = if crossed
            || stream_sequence == self.identity.window().last()
            || next_count >= self.profile.max_observations
        {
            None
        } else {
            Some(
                stream_sequence
                    .checked_add(1)
                    .ok_or(BocpdSrError::ArithmeticOverflow {
                        component: "stream-sequence",
                    })?,
            )
        };

        self.next_sequence = next_sequence;
        self.through_sequence = Some(stream_sequence);
        self.observation_count = next_count;
        self.sr_statistic = next_sr;
        self.run_states = next_states;
        self.detection_count = next_detection_count;
        self.dropped_receipt_count = next_dropped_count;
        self.fallback_sequence = next_fallback;
        self.observations.push(observation);
        self.retained_receipts = next_receipts;
        Ok(())
    }
}

fn next_sr_statistic(
    previous: u128,
    observation: BocpdSrObservation,
    profile: &BocpdSrProfile,
) -> Result<u128, BocpdSrError> {
    let (alternative, null) = match observation {
        BocpdSrObservation::Zero => (
            BOCPD_SR_SCALE - profile.alternative_one_mass,
            BOCPD_SR_SCALE - profile.null_one_mass,
        ),
        BocpdSrObservation::One => (profile.alternative_one_mass, profile.null_one_mass),
    };
    let numerator = previous
        .checked_add(u128::from(BOCPD_SR_SCALE))
        .and_then(|value| value.checked_mul(u128::from(alternative)))
        .ok_or(BocpdSrError::ArithmeticOverflow {
            component: "shiryaev-roberts-recurrence",
        })?;
    Ok(
        round_ratio_u128(numerator, u128::from(null), "shiryaev-roberts-rounding")?
            .min(profile.sr_cap),
    )
}

fn next_bocpd_states(
    previous: &[BocpdSrRunState],
    observation: BocpdSrObservation,
    profile: &BocpdSrProfile,
) -> Result<Vec<BocpdSrRunState>, BocpdSrError> {
    let prior_predictive =
        beta_predictive_mass(profile.beta_alpha, profile.beta_beta, observation)?;
    let total_previous_mass = previous.iter().try_fold(0_u128, |sum, state| {
        sum.checked_add(u128::from(state.posterior_mass))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "posterior-mass-sum",
            })
    })?;
    let cp_raw = total_previous_mass
        .checked_mul(u128::from(profile.hazard_mass))
        .and_then(|value| value.checked_mul(u128::from(prior_predictive)))
        .ok_or(BocpdSrError::ArithmeticOverflow {
            component: "bocpd-changepoint-mass",
        })?;
    let (cp_successes, cp_failures) = match observation {
        BocpdSrObservation::Zero => (0, 1),
        BocpdSrObservation::One => (1, 0),
    };
    let mut candidates = Vec::<RawBocpdState>::new();
    candidates
        .try_reserve_exact(profile.max_run_length + 1)
        .map_err(|_| BocpdSrError::AllocationFailed)?;
    candidates.push(RawBocpdState {
        run_length: 0,
        raw_mass: cp_raw,
        successes: cp_successes,
        failures: cp_failures,
    });
    for state in previous {
        let predictive = beta_predictive_mass(
            profile.beta_alpha.checked_add(state.successes).ok_or(
                BocpdSrError::ArithmeticOverflow {
                    component: "beta-successes",
                },
            )?,
            profile.beta_beta.checked_add(state.failures).ok_or(
                BocpdSrError::ArithmeticOverflow {
                    component: "beta-failures",
                },
            )?,
            observation,
        )?;
        let raw = u128::from(state.posterior_mass)
            .checked_mul(u128::from(BOCPD_SR_SCALE - profile.hazard_mass))
            .and_then(|value| value.checked_mul(u128::from(predictive)))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "bocpd-growth-mass",
            })?;
        let target = usize::try_from(state.run_length)
            .map_err(|_| BocpdSrError::LengthOverflow)?
            .saturating_add(1)
            .min(profile.max_run_length);
        let successes = state
            .successes
            .checked_add(u64::from(observation == BocpdSrObservation::One))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "run-successes",
            })?;
        let failures = state
            .failures
            .checked_add(u64::from(observation == BocpdSrObservation::Zero))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "run-failures",
            })?;
        if let Some(existing) = candidates
            .iter_mut()
            .find(|candidate| usize::try_from(candidate.run_length).ok() == Some(target))
        {
            let combined =
                existing
                    .raw_mass
                    .checked_add(raw)
                    .ok_or(BocpdSrError::ArithmeticOverflow {
                        component: "bocpd-tail-mass",
                    })?;
            existing.successes = weighted_round(
                existing.successes,
                existing.raw_mass,
                successes,
                raw,
                combined,
            )?;
            existing.failures = weighted_round(
                existing.failures,
                existing.raw_mass,
                failures,
                raw,
                combined,
            )?;
            existing.raw_mass = combined;
        } else {
            candidates.push(RawBocpdState {
                run_length: u32::try_from(target).map_err(|_| BocpdSrError::LengthOverflow)?,
                raw_mass: raw,
                successes,
                failures,
            });
        }
    }
    normalize_bocpd_states(candidates)
}

#[derive(Clone, Copy, Debug)]
struct RawBocpdState {
    run_length: u32,
    raw_mass: u128,
    successes: u64,
    failures: u64,
}

fn beta_predictive_mass(
    successes: u64,
    failures: u64,
    observation: BocpdSrObservation,
) -> Result<u64, BocpdSrError> {
    let numerator = match observation {
        BocpdSrObservation::Zero => failures,
        BocpdSrObservation::One => successes,
    };
    let denominator = successes
        .checked_add(failures)
        .ok_or(BocpdSrError::ArithmeticOverflow {
            component: "beta-denominator",
        })?;
    let scaled = u128::from(numerator)
        .checked_mul(u128::from(BOCPD_SR_SCALE))
        .ok_or(BocpdSrError::ArithmeticOverflow {
            component: "beta-predictive",
        })?;
    u64::try_from(round_ratio_u128(
        scaled,
        u128::from(denominator),
        "beta-predictive-rounding",
    )?)
    .map_err(|_| BocpdSrError::ArithmeticOverflow {
        component: "beta-predictive-conversion",
    })
}

fn weighted_round(
    left: u64,
    left_weight: u128,
    right: u64,
    right_weight: u128,
    total_weight: u128,
) -> Result<u64, BocpdSrError> {
    let numerator = u128::from(left)
        .checked_mul(left_weight)
        .and_then(|value| {
            u128::from(right)
                .checked_mul(right_weight)
                .and_then(|rhs| value.checked_add(rhs))
        })
        .ok_or(BocpdSrError::ArithmeticOverflow {
            component: "tail-sufficient-statistic",
        })?;
    u64::try_from(round_ratio_u128(
        numerator,
        total_weight,
        "tail-sufficient-statistic-rounding",
    )?)
    .map_err(|_| BocpdSrError::ArithmeticOverflow {
        component: "tail-sufficient-statistic-conversion",
    })
}

fn round_ratio_u128(
    numerator: u128,
    denominator: u128,
    component: &'static str,
) -> Result<u128, BocpdSrError> {
    if denominator == 0 {
        return Err(BocpdSrError::InvalidState {
            reason: "zero arithmetic denominator",
        });
    }
    numerator
        .checked_add(denominator / 2)
        .ok_or(BocpdSrError::ArithmeticOverflow { component })
        .map(|rounded| rounded / denominator)
}

fn normalize_bocpd_states(
    candidates: Vec<RawBocpdState>,
) -> Result<Vec<BocpdSrRunState>, BocpdSrError> {
    let total = candidates.iter().try_fold(0_u128, |sum, candidate| {
        sum.checked_add(candidate.raw_mass)
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "bocpd-normalizer",
            })
    })?;
    if total == 0 {
        return Err(BocpdSrError::InvalidState {
            reason: "zero BOCPD normalizer",
        });
    }
    let mut states = Vec::new();
    states
        .try_reserve_exact(candidates.len())
        .map_err(|_| BocpdSrError::AllocationFailed)?;
    let mut remainders = Vec::new();
    remainders
        .try_reserve_exact(candidates.len())
        .map_err(|_| BocpdSrError::AllocationFailed)?;
    let mut assigned = 0_u64;
    for (index, candidate) in candidates.iter().enumerate() {
        let scaled = candidate
            .raw_mass
            .checked_mul(u128::from(BOCPD_SR_SCALE))
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "bocpd-normalization",
            })?;
        let quotient =
            u64::try_from(scaled / total).map_err(|_| BocpdSrError::ArithmeticOverflow {
                component: "bocpd-normalized-mass",
            })?;
        assigned = assigned
            .checked_add(quotient)
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "bocpd-assigned-mass",
            })?;
        states.push(BocpdSrRunState {
            run_length: candidate.run_length,
            posterior_mass: quotient,
            successes: candidate.successes,
            failures: candidate.failures,
        });
        remainders.push((scaled % total, candidate.run_length, index));
    }
    remainders.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let missing = BOCPD_SR_SCALE
        .checked_sub(assigned)
        .ok_or(BocpdSrError::InvalidState {
            reason: "normalization assigned more than one",
        })?;
    let missing = usize::try_from(missing).map_err(|_| BocpdSrError::LengthOverflow)?;
    if missing > remainders.len() {
        return Err(BocpdSrError::InvalidState {
            reason: "largest-remainder correction exceeds state count",
        });
    }
    for (_, _, index) in remainders.into_iter().take(missing) {
        states[index].posterior_mass = states[index].posterior_mass.checked_add(1).ok_or(
            BocpdSrError::ArithmeticOverflow {
                component: "largest-remainder-correction",
            },
        )?;
    }
    Ok(states)
}

fn posterior_summary(states: &[BocpdSrRunState], max_run_length: usize) -> (u64, u32, u64) {
    let changepoint_mass = states.first().map_or(0, |state| state.posterior_mass);
    let map_run_length = states
        .iter()
        .max_by(|left, right| {
            left.posterior_mass
                .cmp(&right.posterior_mass)
                .then_with(|| right.run_length.cmp(&left.run_length))
        })
        .map_or(0, |state| state.run_length);
    let tail_mass = states
        .iter()
        .find(|state| usize::try_from(state.run_length).ok() == Some(max_run_length))
        .map_or(0, |state| state.posterior_mass);
    (changepoint_mass, map_run_length, tail_mass)
}

fn validate_bocpd_sr_evidence(evidence: &BocpdSrEvidence) -> Result<(), BocpdSrError> {
    if evidence.identity.detector_id() != BOCPD_SR_REGIME_SIGNAL_ID
        || evidence.identity.detector_version() != BOCPD_SR_REGIME_SIGNAL_VERSION
    {
        return Err(BocpdSrError::WrongDetectorContract);
    }
    if evidence.identity.detector_profile_oid() != evidence.profile.detector_profile_oid {
        return Err(BocpdSrError::IdentityProfileOidMismatch);
    }
    if u64::try_from(evidence.observations.len()).map_err(|_| BocpdSrError::LengthOverflow)?
        != evidence.observation_count
    {
        return Err(BocpdSrError::InvalidState {
            reason: "observation transcript length does not match the observation count",
        });
    }
    if evidence.observation_count
        > u64::try_from(evidence.profile.max_observations)
            .map_err(|_| BocpdSrError::LengthOverflow)?
    {
        return Err(BocpdSrError::InvalidState {
            reason: "observation count exceeds the profile bound",
        });
    }
    if evidence.run_states.is_empty()
        || evidence.run_states.len() > evidence.profile.max_run_length + 1
    {
        return Err(BocpdSrError::InvalidState {
            reason: "posterior inventory is empty or over bound",
        });
    }
    let mut mass_sum = 0_u64;
    for (index, state) in evidence.run_states.iter().enumerate() {
        if usize::try_from(state.run_length).ok() != Some(index) {
            return Err(BocpdSrError::InvalidState {
                reason: "posterior run lengths are not contiguous",
            });
        }
        mass_sum =
            mass_sum
                .checked_add(state.posterior_mass)
                .ok_or(BocpdSrError::ArithmeticOverflow {
                    component: "decoded-posterior-mass",
                })?;
        if state
            .successes
            .checked_add(state.failures)
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "decoded-sufficient-statistics",
            })?
            > evidence.observation_count
        {
            return Err(BocpdSrError::InvalidState {
                reason: "posterior sufficient statistics exceed the observation prefix",
            });
        }
    }
    if mass_sum != BOCPD_SR_SCALE {
        return Err(BocpdSrError::InvalidState {
            reason: "posterior masses do not sum to the fixed scale",
        });
    }
    let (cp, map, tail) = posterior_summary(&evidence.run_states, evidence.profile.max_run_length);
    if (cp, map, tail)
        != (
            evidence.changepoint_mass,
            evidence.map_run_length,
            evidence.tail_mass,
        )
    {
        return Err(BocpdSrError::InvalidState {
            reason: "posterior summary does not match the encoded inventory",
        });
    }
    let expected_through = if evidence.observation_count == 0 {
        None
    } else {
        Some(
            evidence
                .identity
                .window()
                .first()
                .checked_add(evidence.observation_count - 1)
                .ok_or(BocpdSrError::ArithmeticOverflow {
                    component: "decoded-through-sequence",
                })?,
        )
    };
    if evidence.through_sequence != expected_through {
        return Err(BocpdSrError::InvalidState {
            reason: "through sequence does not match the observation count",
        });
    }
    let retained = u64::try_from(evidence.retained_receipts.len())
        .map_err(|_| BocpdSrError::LengthOverflow)?;
    if evidence.dropped_receipt_count.checked_add(retained).ok_or(
        BocpdSrError::ArithmeticOverflow {
            component: "decoded-detection-count",
        },
    )? != evidence.detection_count
    {
        return Err(BocpdSrError::InvalidState {
            reason: "receipt accounting does not match detection count",
        });
    }
    if evidence.fallback_sequence.is_some() != (evidence.detection_count != 0) {
        return Err(BocpdSrError::InvalidState {
            reason: "fallback presence does not match detection count",
        });
    }
    let mut prior_sequence = None;
    for receipt in &evidence.retained_receipts {
        if receipt.sr_threshold != evidence.profile.sr_threshold
            || receipt.changepoint_threshold_mass != evidence.profile.changepoint_threshold_mass
            || receipt.sr_statistic < receipt.sr_threshold
            || receipt.changepoint_mass < receipt.changepoint_threshold_mass
        {
            return Err(BocpdSrError::InvalidState {
                reason: "receipt did not encode a same-sample joint crossing",
            });
        }
        let expected_successor_epoch = evidence.identity.regime_epoch().checked_add(1).ok_or(
            BocpdSrError::ArithmeticOverflow {
                component: "decoded-successor-regime-epoch",
            },
        )?;
        let expected_effective =
            receipt
                .stream_sequence
                .checked_add(1)
                .ok_or(BocpdSrError::ArithmeticOverflow {
                    component: "decoded-successor-effective-sequence",
                })?;
        if receipt.successor_regime_epoch != expected_successor_epoch
            || receipt.successor_effective_sequence != expected_effective
        {
            return Err(BocpdSrError::InvalidState {
                reason: "receipt successor epoch boundary is inconsistent",
            });
        }
        if prior_sequence.is_some_and(|prior| prior >= receipt.stream_sequence) {
            return Err(BocpdSrError::InvalidState {
                reason: "receipt sequences are not strictly increasing",
            });
        }
        prior_sequence = Some(receipt.stream_sequence);
    }

    // A persisted prefix is authoritative only when every separately encoded
    // derived field is exactly the state reached by replaying that prefix from
    // the empty version-1 monitor.  Shape checks alone admit forged resume
    // states (for example a quiet prefix paired with an already-crossed SR
    // statistic), so the codec deliberately pays this bounded verification
    // cost at its trust boundary.
    let mut replayed =
        BocpdSrMonitor::try_new(evidence.identity.clone(), evidence.profile.clone())?;
    for (offset, observation) in evidence.observations.iter().copied().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| BocpdSrError::LengthOverflow)?;
        let sequence = evidence
            .identity
            .window()
            .first()
            .checked_add(offset)
            .ok_or(BocpdSrError::ArithmeticOverflow {
                component: "evidence-replay-sequence",
            })?;
        replayed.try_advance(sequence, observation)?;
    }
    if replayed.evidence() != *evidence {
        return Err(BocpdSrError::InvalidState {
            reason: "derived evidence does not match replay of the binary prefix",
        });
    }
    Ok(())
}

struct BocpdSrEncoder {
    bytes: Vec<u8>,
}

impl BocpdSrEncoder {
    fn try_with_capacity(capacity: usize) -> Result<Self, BocpdSrError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(capacity)
            .map_err(|_| BocpdSrError::AllocationFailed)?;
        Ok(Self { bytes })
    }
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
    fn write_bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }
    fn write_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }
    fn write_u16(&mut self, value: u16) {
        self.write_bytes(&value.to_le_bytes());
    }
    fn write_u32(&mut self, value: u32) {
        self.write_bytes(&value.to_le_bytes());
    }
    fn write_u64(&mut self, value: u64) {
        self.write_bytes(&value.to_le_bytes());
    }
    fn write_u128(&mut self, value: u128) {
        self.write_bytes(&value.to_le_bytes());
    }
    fn write_oid(&mut self, value: ObjectId) {
        self.write_bytes(&value.0);
    }
    fn write_optional_u64(&mut self, value: Option<u64>) {
        self.write_u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.write_u64(value);
        }
    }
}

fn bocpd_sr_encoded_len(evidence: &BocpdSrEvidence) -> Result<usize, BocpdSrError> {
    fn add(total: &mut usize, value: usize) -> Result<(), BocpdSrError> {
        *total = total
            .checked_add(value)
            .ok_or(BocpdSrError::LengthOverflow)?;
        Ok(())
    }

    let mut total = 0_usize;
    // Domain length + domain + version.
    add(&mut total, 2)?;
    add(&mut total, BOCPD_SR_EVIDENCE_ENCODING_DOMAIN.len())?;
    add(&mut total, 2)?;
    // Identity: three OIDs, detector string, version, window, epoch, and two
    // policy OIDs.
    add(&mut total, 3 * 32 + 2)?;
    add(&mut total, evidence.identity.detector_id().len())?;
    add(&mut total, 4 + 3 * 8 + 2 * 32)?;
    // Profile: two OIDs, five u64 parameters, the SR threshold and cap, one
    // u64 threshold, and the three bounded counts.
    add(&mut total, 2 * 32 + 5 * 8 + 2 * 16 + 8 + 4 + 8 + 4)?;
    // Dynamic evidence header. Optional u64 values use one presence byte.
    add(
        &mut total,
        1 + usize::from(evidence.through_sequence.is_some()) * 8,
    )?;
    add(&mut total, 8 + 16 + 8 + 4 + 8 + 8 + 8)?;
    add(
        &mut total,
        1 + usize::from(evidence.fallback_sequence.is_some()) * 8,
    )?;
    add(&mut total, 4)?;
    add(&mut total, evidence.observations.len())?;
    add(&mut total, 4)?;
    add(
        &mut total,
        evidence
            .run_states
            .len()
            .checked_mul(4 + 3 * 8)
            .ok_or(BocpdSrError::LengthOverflow)?,
    )?;
    add(&mut total, 4)?;
    add(
        &mut total,
        evidence
            .retained_receipts
            .len()
            .checked_mul(8 + 2 * 16 + 2 * 8 + 4 + 3 * 8)
            .ok_or(BocpdSrError::LengthOverflow)?,
    )?;
    Ok(total)
}

struct BocpdSrDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BocpdSrDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }
    fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], BocpdSrError> {
        let remaining = self.remaining();
        if remaining < length {
            return Err(BocpdSrError::Truncated {
                offset: self.offset,
                needed: length,
                remaining,
            });
        }
        let end = self
            .offset
            .checked_add(length)
            .ok_or(BocpdSrError::LengthOverflow)?;
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }
    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], BocpdSrError> {
        let mut value = [0; N];
        value.copy_from_slice(self.read_bytes(N)?);
        Ok(value)
    }
    fn read_u8(&mut self) -> Result<u8, BocpdSrError> {
        Ok(self.read_array::<1>()?[0])
    }
    fn read_u16(&mut self) -> Result<u16, BocpdSrError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }
    fn read_u32(&mut self) -> Result<u32, BocpdSrError> {
        Ok(u32::from_le_bytes(self.read_array()?))
    }
    fn read_u64(&mut self) -> Result<u64, BocpdSrError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }
    fn read_u128(&mut self) -> Result<u128, BocpdSrError> {
        Ok(u128::from_le_bytes(self.read_array()?))
    }
    fn read_oid(&mut self) -> Result<ObjectId, BocpdSrError> {
        Ok(ObjectId(self.read_array()?))
    }
    fn read_optional_u64(&mut self, component: &'static str) -> Result<Option<u64>, BocpdSrError> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_u64()?)),
            actual => Err(BocpdSrError::InvalidTag { component, actual }),
        }
    }
}

fn encode_bocpd_sr_identity(
    encoder: &mut BocpdSrEncoder,
    identity: &RegimeSignalIdentity,
) -> Result<(), BocpdSrError> {
    encoder.write_oid(identity.signal_oid());
    encoder.write_oid(identity.metric_stream_oid());
    encoder.write_oid(identity.detector_profile_oid());
    encoder.write_u16(
        u16::try_from(identity.detector_id().len()).map_err(|_| BocpdSrError::LengthOverflow)?,
    );
    encoder.write_bytes(identity.detector_id().as_bytes());
    encoder.write_u32(identity.detector_version());
    encoder.write_u64(identity.window().first());
    encoder.write_u64(identity.window().last());
    encoder.write_u64(identity.regime_epoch());
    encoder.write_oid(identity.candidate_decision_oid());
    encoder.write_oid(identity.pinned_fallback_oid());
    Ok(())
}

fn decode_bocpd_sr_identity(
    decoder: &mut BocpdSrDecoder<'_>,
) -> Result<RegimeSignalIdentity, BocpdSrError> {
    let signal_oid = decoder.read_oid()?;
    let stream_oid = decoder.read_oid()?;
    let profile_oid = decoder.read_oid()?;
    let id_length = usize::from(decoder.read_u16()?);
    if id_length > MAX_DETECTOR_ID_BYTES {
        return Err(BocpdSrError::InvalidState {
            reason: "detector ID exceeds its bound",
        });
    }
    let detector_id = core::str::from_utf8(decoder.read_bytes(id_length)?).map_err(|_| {
        BocpdSrError::InvalidState {
            reason: "detector ID is not UTF-8",
        }
    })?;
    let detector_version = decoder.read_u32()?;
    let window = RegimeSequenceWindow::try_new(decoder.read_u64()?, decoder.read_u64()?)?;
    let identity = RegimeSignalIdentity::try_new(
        signal_oid,
        stream_oid,
        profile_oid,
        detector_id,
        detector_version,
        window,
        decoder.read_u64()?,
        decoder.read_oid()?,
        decoder.read_oid()?,
    )?;
    if identity.detector_id() != BOCPD_SR_REGIME_SIGNAL_ID
        || identity.detector_version() != BOCPD_SR_REGIME_SIGNAL_VERSION
    {
        return Err(BocpdSrError::WrongDetectorContract);
    }
    Ok(identity)
}

fn encode_bocpd_sr_profile(
    encoder: &mut BocpdSrEncoder,
    profile: &BocpdSrProfile,
) -> Result<(), BocpdSrError> {
    encoder.write_oid(profile.detector_profile_oid);
    encoder.write_oid(profile.binary_input_contract_oid);
    encoder.write_u64(profile.null_one_mass);
    encoder.write_u64(profile.alternative_one_mass);
    encoder.write_u64(profile.beta_alpha);
    encoder.write_u64(profile.beta_beta);
    encoder.write_u64(profile.hazard_mass);
    encoder.write_u128(profile.sr_threshold);
    encoder.write_u128(profile.sr_cap);
    encoder.write_u64(profile.changepoint_threshold_mass);
    encoder.write_u32(
        u32::try_from(profile.max_run_length).map_err(|_| BocpdSrError::LengthOverflow)?,
    );
    encoder.write_u64(
        u64::try_from(profile.max_observations).map_err(|_| BocpdSrError::LengthOverflow)?,
    );
    encoder.write_u32(
        u32::try_from(profile.max_retained_receipts).map_err(|_| BocpdSrError::LengthOverflow)?,
    );
    Ok(())
}

fn decode_bocpd_sr_profile(
    decoder: &mut BocpdSrDecoder<'_>,
) -> Result<BocpdSrProfile, BocpdSrError> {
    BocpdSrProfile::try_new(
        decoder.read_oid()?,
        decoder.read_oid()?,
        decoder.read_u64()?,
        decoder.read_u64()?,
        decoder.read_u64()?,
        decoder.read_u64()?,
        decoder.read_u64()?,
        decoder.read_u128()?,
        decoder.read_u128()?,
        decoder.read_u64()?,
        usize::try_from(decoder.read_u32()?).map_err(|_| BocpdSrError::LengthOverflow)?,
        usize::try_from(decoder.read_u64()?).map_err(|_| BocpdSrError::LengthOverflow)?,
        usize::try_from(decoder.read_u32()?).map_err(|_| BocpdSrError::LengthOverflow)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn window() -> Result<RegimeSequenceWindow, RegimeBuildError> {
        RegimeSequenceWindow::try_new(100, 111)
    }

    fn identity_with(
        signal_oid: ObjectId,
        profile_oid: ObjectId,
        detector_id: &str,
        detector_version: u32,
    ) -> Result<RegimeSignalIdentity, RegimeBuildError> {
        RegimeSignalIdentity::try_new(
            signal_oid,
            oid(2),
            profile_oid,
            detector_id,
            detector_version,
            window()?,
            7,
            oid(4),
            oid(5),
        )
    }

    fn identity() -> Result<RegimeSignalIdentity, RegimeBuildError> {
        identity_with(
            oid(1),
            oid(3),
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
        )
    }

    fn page_hinkley(threshold_units: i64) -> PageHinkleyConfig {
        PageHinkleyConfig {
            tolerance: MetricSample::from_micro_units(0),
            threshold: threshold_units.saturating_mul(MetricSample::SCALE),
            reset_after_detection: true,
        }
    }

    fn upward_cusum(threshold_units: i64) -> CusumConfig {
        CusumConfig {
            baseline: MetricSample::from_units(10),
            drift: MetricSample::from_micro_units(0),
            threshold: threshold_units.saturating_mul(MetricSample::SCALE),
            direction: FoundationDirection::Increase,
            reset_after_detection: true,
        }
    }

    fn downward_cusum(threshold_units: i64) -> CusumConfig {
        CusumConfig {
            baseline: MetricSample::from_units(10),
            drift: MetricSample::from_micro_units(0),
            threshold: threshold_units.saturating_mul(MetricSample::SCALE),
            direction: FoundationDirection::Decrease,
            reset_after_detection: true,
        }
    }

    fn profile_with(
        profile_oid: ObjectId,
        max_observations: usize,
        max_receipts: usize,
    ) -> Result<RegimeSignalProfile, RegimeBuildError> {
        RegimeSignalProfile::try_new(
            profile_oid,
            RuntimeMetricSeries::Custom(17),
            page_hinkley(10),
            upward_cusum(100),
            downward_cusum(100),
            max_observations,
            max_receipts,
        )
    }

    fn profile() -> Result<RegimeSignalProfile, RegimeBuildError> {
        profile_with(oid(3), 12, 4)
    }

    fn monitor() -> Result<RegimeSignalMonitor, RegimeBuildError> {
        RegimeSignalMonitor::try_new(identity()?, profile()?)
    }

    fn input(sequence: u64, units: i64) -> Result<SequencedRegimeSample, RegimeBuildError> {
        Ok(SequencedRegimeSample::new(
            identity()?,
            profile()?,
            sequence,
            MetricSample::from_units(units),
        ))
    }

    #[test]
    fn stable_stream_keeps_candidate_and_exact_three_detector_state() -> TestResult {
        let mut value = monitor()?;
        for sequence in 100..=104 {
            let update = value.observe(input(sequence, 10)?)?;
            assert_eq!(
                update.evidence.selection(),
                RegimePolicySelection::CandidateDecision
            );
            assert_eq!(
                update.evidence.status(),
                RegimeSignalStatus::NoChangeDetected
            );
            assert!(update.evidence.retained_receipts().is_empty());
            assert!(!update.evidence.is_ground_truth());
        }

        let evidence = value.evidence();
        assert_eq!(evidence.observation_count(), 5);
        assert_eq!(evidence.detection_count(), 0);
        assert_eq!(evidence.detector_snapshots().len(), COMBINED_DETECTOR_COUNT);
        assert!(
            evidence
                .detector_snapshots()
                .iter()
                .zip(COMBINED_DETECTOR_SLOTS)
                .all(|(snapshot, expected_slot)| snapshot.detector() == expected_slot)
        );
        assert!(
            evidence
                .detector_snapshots()
                .iter()
                .all(|snapshot| snapshot.sample_count() == 5)
        );
        assert_eq!(evidence.selected_policy_oid(), oid(4));
        Ok(())
    }

    #[test]
    fn detected_change_transitions_to_pinned_fallback() -> TestResult {
        let mut value = monitor()?;
        for (offset, units) in [10, 10, 10, 10, 10, 30].into_iter().enumerate() {
            let offset = u64::try_from(offset)?;
            value.observe(input(100 + offset, units)?)?;
        }

        let evidence = value.evidence();
        assert_eq!(evidence.status(), RegimeSignalStatus::ChangeDetected);
        assert_eq!(evidence.selection(), RegimePolicySelection::PinnedFallback);
        assert_eq!(evidence.selected_policy_oid(), oid(5));
        assert_eq!(evidence.fallback_sequence(), Some(105));
        assert_eq!(evidence.detection_count(), 1);
        assert_eq!(evidence.retained_receipts().len(), 1);
        let receipt = evidence.retained_receipts()[0];
        assert_eq!(receipt.stream_sequence(), 105);
        assert_eq!(receipt.series(), RuntimeMetricSeries::Custom(17));
        assert_eq!(receipt.detector(), RegimeDetectorKind::PageHinkley);
        assert_eq!(receipt.direction(), RegimeDirection::Increase);
        assert!(receipt.statistic() >= receipt.threshold());
        assert!(!receipt.is_ground_truth());
        assert!(!evidence.status().is_ground_truth());
        Ok(())
    }

    #[test]
    fn downward_shift_fires_the_downward_cusum_and_not_its_opposite() -> TestResult {
        // The downward detector is one of the three the combined signal
        // declares, and a collapsing metric — throughput falling off, not
        // spiking — is exactly the regime it exists to catch. Its opposite must
        // stay silent, otherwise the direction carried on every receipt is
        // decoration rather than information.
        let mut value = monitor()?;
        for (offset, units) in [10, 10, 10, -40, -40, -40].into_iter().enumerate() {
            value.observe(input(100 + u64::try_from(offset)?, units)?)?;
        }

        let evidence = value.evidence();
        let receipts = evidence.retained_receipts();
        assert!(
            !receipts.is_empty(),
            "a sustained downward excursion produced no detection at all"
        );

        let downward = receipts
            .iter()
            .find(|receipt| receipt.detector() == RegimeDetectorKind::DownwardCusum)
            .ok_or("no downward-cusum receipt for a sustained downward excursion")?;
        assert_eq!(downward.direction(), RegimeDirection::Decrease);
        assert_eq!(downward.series(), RuntimeMetricSeries::Custom(17));
        assert!(downward.statistic() >= downward.threshold());

        assert!(
            receipts
                .iter()
                .all(|receipt| receipt.detector() != RegimeDetectorKind::UpwardCusum),
            "the upward detector fired on a downward excursion"
        );
        // The signal is advisory evidence under its named model, never a
        // safety fact — no receipt may claim otherwise.
        assert!(receipts.iter().all(|receipt| !receipt.is_ground_truth()));
        assert!(!evidence.status().is_ground_truth());
        Ok(())
    }

    #[test]
    fn each_reset_flag_survives_the_canonical_round_trip_independently() -> TestResult {
        // Every existing fixture sets all three reset flags to `true`, so the
        // canonical codec has never had to distinguish them: a decoder that
        // read the three booleans in the wrong order, or read one twice, would
        // round-trip an all-true profile perfectly. Distinct values are what
        // make the field positions load-bearing.
        let profile_oid = oid(9);
        let mut page_hinkley = page_hinkley(10);
        page_hinkley.reset_after_detection = true;
        let mut upward = upward_cusum(100);
        upward.reset_after_detection = false;
        let mut downward = downward_cusum(100);
        downward.reset_after_detection = true;

        let profile = RegimeSignalProfile::try_new(
            profile_oid,
            RuntimeMetricSeries::Custom(17),
            page_hinkley,
            upward,
            downward,
            12,
            4,
        )?;
        let identity = identity_with(
            oid(1),
            profile_oid,
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
        )?;
        assert!(profile.page_hinkley_resets_after_detection());
        assert!(!profile.upward_cusum_resets_after_detection());
        assert!(profile.downward_cusum_resets_after_detection());

        let mut value = RegimeSignalMonitor::try_new(identity.clone(), profile.clone())?;
        for (offset, units) in [10, 10, 10, 30].into_iter().enumerate() {
            value.observe(SequencedRegimeSample::new(
                identity.clone(),
                profile.clone(),
                100 + u64::try_from(offset)?,
                MetricSample::from_units(units),
            ))?;
        }

        let evidence = value.evidence();
        let encoded = evidence.try_to_canonical_bytes()?;
        let decoded = RegimeSignalEvidence::try_from_canonical_bytes(&encoded)?;
        assert_eq!(decoded, evidence);

        let decoded_profile = decoded.profile();
        assert!(decoded_profile.page_hinkley_resets_after_detection());
        assert!(!decoded_profile.upward_cusum_resets_after_detection());
        assert!(decoded_profile.downward_cusum_resets_after_detection());
        assert_eq!(decoded.try_to_canonical_bytes()?, encoded);
        Ok(())
    }

    #[test]
    fn replay_produces_identical_integer_evidence() -> TestResult {
        fn run() -> TestResult<Vec<RegimeSignalEvidence>> {
            let mut value = monitor()?;
            let mut evidence = Vec::new();
            evidence.try_reserve_exact(10)?;
            for (offset, units) in [10, 10, 10, 10, 10, 30, 30, 8, 8, 8]
                .into_iter()
                .enumerate()
            {
                let offset = u64::try_from(offset)?;
                evidence.push(value.observe(input(100 + offset, units)?)?.evidence);
            }
            Ok(evidence)
        }

        assert_eq!(run()?, run()?);
        Ok(())
    }

    #[test]
    fn canonical_evidence_round_trips_and_rejects_noncanonical_state() -> TestResult {
        let mut value = monitor()?;
        for (offset, units) in [10, 10, 10, 10, 10, 30].into_iter().enumerate() {
            value.observe(input(100 + u64::try_from(offset)?, units)?)?;
        }
        let evidence = value.evidence();
        let encoded = evidence.try_to_canonical_bytes()?;
        assert_eq!(
            RegimeSignalEvidence::try_from_canonical_bytes(&encoded)?,
            evidence
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(matches!(
            RegimeSignalEvidence::try_from_canonical_bytes(&trailing),
            Err(RegimeSignalEvidenceCodecError::TrailingBytes { remaining: 1 })
        ));

        let version_offset = 2 + REGIME_SIGNAL_EVIDENCE_ENCODING_DOMAIN.len();
        let mut wrong_version = encoded.clone();
        wrong_version[version_offset..version_offset + 2].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            RegimeSignalEvidence::try_from_canonical_bytes(&wrong_version),
            Err(RegimeSignalEvidenceCodecError::UnsupportedVersion { actual: 2 })
        );

        let mut decoder = RegimeEvidenceDecoder::new(&encoded);
        let domain_len = usize::from(decoder.read_u16()?);
        let _ = decoder.read_bytes(domain_len)?;
        let _ = decoder.read_u16()?;
        let _ = decode_regime_identity(&mut decoder)?;
        let _ = decode_regime_profile(&mut decoder)?;
        let _ = decoder.read_optional_u64("through-sequence")?;
        let _ = decoder.read_u64()?;
        let _ = decoder.read_u64()?;
        let _ = decoder.read_u64()?;
        let _ = decoder.read_optional_u64("fallback-sequence")?;
        let status_offset = decoder.offset;
        let mut inconsistent_status = encoded;
        inconsistent_status[status_offset] =
            encode_regime_status(RegimeSignalStatus::NoChangeDetected);
        assert!(matches!(
            RegimeSignalEvidence::try_from_canonical_bytes(&inconsistent_status),
            Err(RegimeSignalEvidenceCodecError::InvalidState { .. })
        ));
        Ok(())
    }

    #[test]
    fn canonical_evidence_rejects_truncation_at_every_boundary() -> TestResult {
        let evidence = monitor()?.evidence();
        let encoded = evidence.try_to_canonical_bytes()?;
        for length in 0..encoded.len() {
            assert!(
                RegimeSignalEvidence::try_from_canonical_bytes(&encoded[..length]).is_err(),
                "accepted truncated canonical evidence at length {length}"
            );
        }
        Ok(())
    }

    #[test]
    fn identity_profile_and_sequence_rejections_are_atomic() -> TestResult {
        let mut value = monitor()?;
        let before = value.evidence();

        let wrong_identity = identity_with(
            oid(9),
            oid(3),
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
        )?;
        let wrong_identity_input = SequencedRegimeSample::new(
            wrong_identity,
            profile()?,
            100,
            MetricSample::from_units(10),
        );
        assert_eq!(
            value.observe(wrong_identity_input),
            Err(RegimeObserveError::IdentityMismatch)
        );
        assert_eq!(value.evidence(), before);

        let wrong_profile = RegimeSignalProfile::try_new(
            oid(3),
            RuntimeMetricSeries::Custom(17),
            page_hinkley(11),
            upward_cusum(100),
            downward_cusum(100),
            12,
            4,
        )?;
        let wrong_profile_input = SequencedRegimeSample::new(
            identity()?,
            wrong_profile,
            100,
            MetricSample::from_units(10),
        );
        assert_eq!(
            value.observe(wrong_profile_input),
            Err(RegimeObserveError::ProfileMismatch)
        );
        assert_eq!(value.evidence(), before);

        assert_eq!(
            value.observe(input(101, 10)?),
            Err(RegimeObserveError::UnexpectedSequence {
                expected: 100,
                actual: 101,
            })
        );
        assert_eq!(value.evidence(), before);
        Ok(())
    }

    #[test]
    fn construction_rejects_identity_profile_and_resource_mismatches() -> TestResult {
        assert_eq!(
            RegimeSequenceWindow::try_new(9, 8),
            Err(RegimeBuildError::ReversedWindow { first: 9, last: 8 })
        );
        assert_eq!(
            identity_with(oid(1), oid(3), COMBINED_REGIME_SIGNAL_ID, 0),
            Err(RegimeBuildError::ZeroDetectorVersion)
        );
        assert_eq!(
            RegimeSignalIdentity::try_new(
                oid(1),
                oid(2),
                oid(3),
                COMBINED_REGIME_SIGNAL_ID,
                COMBINED_REGIME_SIGNAL_VERSION,
                window()?,
                7,
                oid(4),
                oid(4),
            ),
            Err(RegimeBuildError::CandidateEqualsFallback)
        );
        assert!(matches!(
            RegimeSignalMonitor::try_new(
                identity_with(
                    oid(1),
                    oid(8),
                    COMBINED_REGIME_SIGNAL_ID,
                    COMBINED_REGIME_SIGNAL_VERSION,
                )?,
                profile()?,
            ),
            Err(RegimeBuildError::IdentityProfileOidMismatch { .. })
        ));
        assert_eq!(
            profile_with(oid(3), 1, 2),
            Err(RegimeBuildError::ReceiptLimitExceedsObservationLimit {
                receipts: 2,
                observations: 1,
            })
        );

        let short_identity = RegimeSignalIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
            RegimeSequenceWindow::try_new(100, 101)?,
            7,
            oid(4),
            oid(5),
        )?;
        assert!(matches!(
            RegimeSignalMonitor::try_new(short_identity, profile()?),
            Err(RegimeBuildError::ObservationLimitExceedsWindow {
                maximum: 12,
                window: 2,
            })
        ));
        Ok(())
    }

    #[test]
    fn invalid_foundation_slots_are_rejected_before_monitor_creation() {
        let invalid_upward = CusumConfig {
            direction: FoundationDirection::Decrease,
            ..upward_cusum(10)
        };
        assert_eq!(
            RegimeSignalProfile::try_new(
                oid(3),
                RuntimeMetricSeries::Custom(17),
                page_hinkley(10),
                invalid_upward,
                downward_cusum(10),
                4,
                2,
            ),
            Err(RegimeBuildError::InvalidUpwardCusumDirection)
        );

        let invalid_page_hinkley = PageHinkleyConfig {
            threshold: 0,
            ..page_hinkley(10)
        };
        assert_eq!(
            RegimeSignalProfile::try_new(
                oid(3),
                RuntimeMetricSeries::Custom(17),
                invalid_page_hinkley,
                upward_cusum(10),
                downward_cusum(10),
                4,
                2,
            ),
            Err(RegimeBuildError::NonPositivePageHinkleyThreshold { threshold: 0 })
        );
    }

    #[test]
    fn observation_and_receipt_state_remain_bounded() -> TestResult {
        let bounded_profile = RegimeSignalProfile::try_new(
            oid(3),
            RuntimeMetricSeries::Custom(17),
            page_hinkley(i64::MAX / MetricSample::SCALE),
            upward_cusum(1),
            downward_cusum(i64::MAX / MetricSample::SCALE),
            3,
            1,
        )?;
        let bounded_identity = RegimeSignalIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
            RegimeSequenceWindow::try_new(100, 103)?,
            7,
            oid(4),
            oid(5),
        )?;
        let mut value =
            RegimeSignalMonitor::try_new(bounded_identity.clone(), bounded_profile.clone())?;
        for sequence in 100..=102 {
            value.observe(SequencedRegimeSample::new(
                bounded_identity.clone(),
                bounded_profile.clone(),
                sequence,
                MetricSample::from_units(20),
            ))?;
        }

        let evidence = value.evidence();
        assert_eq!(evidence.observation_count(), 3);
        assert_eq!(evidence.detection_count(), 3);
        assert_eq!(evidence.dropped_receipt_count(), 2);
        assert_eq!(evidence.retained_receipts().len(), 1);
        assert_eq!(evidence.retained_receipts()[0].stream_sequence(), 102);
        assert_eq!(
            evidence.retained_receipts()[0].detector(),
            RegimeDetectorKind::UpwardCusum
        );
        assert_eq!(evidence.fallback_sequence(), Some(100));
        assert_eq!(value.next_sequence(), None);
        let before = evidence;
        assert_eq!(
            value.observe(SequencedRegimeSample::new(
                bounded_identity,
                bounded_profile,
                103,
                MetricSample::from_units(20),
            )),
            Err(RegimeObserveError::ObservationLimitReached { maximum: 3 })
        );
        assert_eq!(value.evidence(), before);
        Ok(())
    }

    #[test]
    fn fallback_is_sticky_after_later_quiet_samples() -> TestResult {
        let mut value = monitor()?;
        for (offset, units) in [10, 10, 10, 10, 10, 30, 30, 30].into_iter().enumerate() {
            let offset = u64::try_from(offset)?;
            value.observe(input(100 + offset, units)?)?;
        }

        let evidence = value.evidence();
        assert_eq!(evidence.selection(), RegimePolicySelection::PinnedFallback);
        assert_eq!(evidence.fallback_sequence(), Some(105));
        assert_eq!(evidence.selected_policy_oid(), oid(5));
        assert!(!evidence.is_ground_truth());
        Ok(())
    }

    fn bocpd_sr_profile(
        sr_threshold: u128,
        changepoint_threshold_mass: u64,
        max_run_length: usize,
    ) -> Result<BocpdSrProfile, BocpdSrError> {
        BocpdSrProfile::try_new(
            oid(31),
            oid(32),
            250_000,
            750_000,
            1,
            1,
            200_000,
            sr_threshold,
            100_000_000,
            changepoint_threshold_mass,
            max_run_length,
            12,
            4,
        )
    }

    fn bocpd_sr_identity() -> Result<RegimeSignalIdentity, BocpdSrError> {
        Ok(RegimeSignalIdentity::try_new(
            oid(30),
            oid(33),
            oid(31),
            BOCPD_SR_REGIME_SIGNAL_ID,
            BOCPD_SR_REGIME_SIGNAL_VERSION,
            RegimeSequenceWindow::try_new(100, 111)?,
            7,
            oid(4),
            oid(5),
        )?)
    }

    fn observe_bocpd_sr(
        monitor: &mut BocpdSrMonitor,
        identity: &RegimeSignalIdentity,
        profile: &BocpdSrProfile,
        sequence: u64,
        observation: BocpdSrObservation,
    ) -> Result<BocpdSrEvidence, BocpdSrError> {
        Ok(monitor
            .observe(SequencedBocpdSrObservation::new(
                identity.clone(),
                profile.clone(),
                sequence,
                observation,
            ))?
            .evidence)
    }

    #[test]
    fn bocpd_sr_hand_vector_pins_every_cell_and_sr_recurrence() -> TestResult {
        let identity = bocpd_sr_identity()?;
        let profile = bocpd_sr_profile(u128::MAX, 900_000, 8)?;
        let mut monitor = BocpdSrMonitor::try_new(identity.clone(), profile.clone())?;
        let expected = [
            (3_000_000_u128, &[(200_000, 1, 0), (800_000, 1, 0)][..]),
            (
                12_000_000,
                &[(157_895, 1, 0), (168_421, 2, 0), (673_684, 2, 0)][..],
            ),
            (
                4_333_333,
                &[
                    (322_034, 0, 1),
                    (135_593, 1, 1),
                    (108_475, 2, 1),
                    (433_898, 2, 1),
                ][..],
            ),
            (
                15_999_999,
                &[
                    (199_819, 1, 0),
                    (171_596, 1, 1),
                    (108_377, 2, 1),
                    (104_042, 3, 1),
                    (416_166, 3, 1),
                ][..],
            ),
            (
                50_999_997,
                &[
                    (165_378, 1, 0),
                    (176_244, 2, 0),
                    (113_513, 2, 1),
                    (86_032, 3, 1),
                    (91_767, 4, 1),
                    (367_066, 4, 1),
                ][..],
            ),
        ];
        for (offset, observation) in [
            BocpdSrObservation::One,
            BocpdSrObservation::One,
            BocpdSrObservation::Zero,
            BocpdSrObservation::One,
            BocpdSrObservation::One,
        ]
        .into_iter()
        .enumerate()
        {
            let sequence = 100 + u64::try_from(offset)?;
            let evidence =
                observe_bocpd_sr(&mut monitor, &identity, &profile, sequence, observation)?;
            let (expected_sr, expected_states) = expected[offset];
            assert_eq!(evidence.sr_statistic(), expected_sr);
            assert_eq!(evidence.run_states().len(), expected_states.len());
            for (run_length, (state, (mass, successes, failures))) in evidence
                .run_states()
                .iter()
                .zip(expected_states.iter())
                .enumerate()
            {
                assert_eq!(usize::try_from(state.run_length())?, run_length);
                assert_eq!(state.posterior_mass(), *mass);
                assert_eq!(state.successes(), *successes);
                assert_eq!(state.failures(), *failures);
            }
            assert_eq!(
                evidence
                    .run_states()
                    .iter()
                    .map(|state| state.posterior_mass())
                    .sum::<u64>(),
                BOCPD_SR_SCALE
            );
            assert_eq!(evidence.status(), RegimeSignalStatus::NoChangeDetected);
        }
        Ok(())
    }

    #[test]
    fn bocpd_sr_requires_both_crossings_and_emits_an_exact_epoch_boundary() -> TestResult {
        let identity = bocpd_sr_identity()?;
        let profile = bocpd_sr_profile(4_000_000, 300_000, 8)?;
        let mut monitor = BocpdSrMonitor::try_new(identity.clone(), profile.clone())?;
        let mut terminal = None;
        for (offset, observation) in [
            BocpdSrObservation::Zero,
            BocpdSrObservation::Zero,
            BocpdSrObservation::Zero,
            BocpdSrObservation::Zero,
            BocpdSrObservation::Zero,
            BocpdSrObservation::One,
        ]
        .into_iter()
        .enumerate()
        {
            terminal = Some(observe_bocpd_sr(
                &mut monitor,
                &identity,
                &profile,
                100 + u64::try_from(offset)?,
                observation,
            )?);
        }
        let terminal = terminal.ok_or("missing terminal evidence")?;
        assert_eq!(terminal.fallback_sequence(), Some(105));
        assert_eq!(terminal.status(), RegimeSignalStatus::ChangeDetected);
        assert_eq!(terminal.selection(), RegimePolicySelection::PinnedFallback);
        assert_eq!(terminal.selected_policy_oid(), oid(5));
        assert_eq!(terminal.retained_receipts().len(), 1);
        let receipt = terminal.retained_receipts()[0];
        assert_eq!(receipt.stream_sequence(), 105);
        assert_eq!(receipt.successor_regime_epoch(), 8);
        assert_eq!(receipt.successor_effective_sequence(), 106);
        assert!(!receipt.is_ground_truth());
        assert_eq!(
            monitor.observe(SequencedBocpdSrObservation::new(
                identity,
                profile,
                106,
                BocpdSrObservation::One,
            )),
            Err(BocpdSrError::WindowComplete)
        );

        for (sr_threshold, cp_threshold) in [(4_000_000, 900_000), (u128::MAX, 300_000)] {
            let identity = bocpd_sr_identity()?;
            let profile = bocpd_sr_profile(sr_threshold, cp_threshold, 8)?;
            let mut one_component = BocpdSrMonitor::try_new(identity.clone(), profile.clone())?;
            for (offset, observation) in [
                BocpdSrObservation::Zero,
                BocpdSrObservation::Zero,
                BocpdSrObservation::Zero,
                BocpdSrObservation::Zero,
                BocpdSrObservation::Zero,
                BocpdSrObservation::One,
            ]
            .into_iter()
            .enumerate()
            {
                observe_bocpd_sr(
                    &mut one_component,
                    &identity,
                    &profile,
                    100 + u64::try_from(offset)?,
                    observation,
                )?;
            }
            assert_eq!(
                one_component.evidence().status(),
                RegimeSignalStatus::NoChangeDetected,
                "one detector crossing must not trigger the combined signal"
            );
        }
        Ok(())
    }

    #[test]
    fn bocpd_sr_tail_codec_resume_and_rejections_are_fail_closed() -> TestResult {
        let identity = bocpd_sr_identity()?;
        let profile = bocpd_sr_profile(u128::MAX, 900_000, 2)?;
        let mut first = BocpdSrMonitor::try_new(identity.clone(), profile.clone())?;
        for (offset, observation) in [
            BocpdSrObservation::One,
            BocpdSrObservation::Zero,
            BocpdSrObservation::One,
            BocpdSrObservation::One,
        ]
        .into_iter()
        .enumerate()
        {
            observe_bocpd_sr(
                &mut first,
                &identity,
                &profile,
                100 + u64::try_from(offset)?,
                observation,
            )?;
        }
        let evidence = first.evidence();
        assert_eq!(evidence.run_states().len(), 3);
        assert!(evidence.tail_mass() > 0);
        let encoded = evidence.try_to_canonical_bytes()?;
        assert_eq!(
            BocpdSrEvidence::try_from_canonical_bytes(&encoded)?,
            evidence
        );
        for length in 0..encoded.len() {
            assert!(
                BocpdSrEvidence::try_from_canonical_bytes(&encoded[..length]).is_err(),
                "truncation at {length} was accepted"
            );
        }
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            BocpdSrEvidence::try_from_canonical_bytes(&trailing),
            Err(BocpdSrError::TrailingBytes { remaining: 1 })
        );
        let mut wrong_domain = encoded.clone();
        wrong_domain[2] ^= 1;
        assert!(BocpdSrEvidence::try_from_canonical_bytes(&wrong_domain).is_err());

        let restored = BocpdSrEvidence::try_from_canonical_bytes(&encoded)?;
        let mut resumed = BocpdSrMonitor::try_resume_from_evidence(restored)?;
        let resumed_terminal = observe_bocpd_sr(
            &mut resumed,
            &identity,
            &profile,
            104,
            BocpdSrObservation::Zero,
        )?;
        let direct_terminal = observe_bocpd_sr(
            &mut first,
            &identity,
            &profile,
            104,
            BocpdSrObservation::Zero,
        )?;
        assert_eq!(
            resumed_terminal.try_to_canonical_bytes()?,
            direct_terminal.try_to_canonical_bytes()?
        );

        let before = first.evidence().try_to_canonical_bytes()?;
        assert_eq!(
            first.observe(SequencedBocpdSrObservation::new(
                identity.clone(),
                profile.clone(),
                106,
                BocpdSrObservation::One,
            )),
            Err(BocpdSrError::UnexpectedSequence {
                expected: 105,
                actual: 106,
            })
        );
        assert_eq!(first.evidence().try_to_canonical_bytes()?, before);

        first.sr_statistic = u128::MAX;
        let before_overflow = first.evidence().try_to_canonical_bytes()?;
        assert!(matches!(
            first.observe(SequencedBocpdSrObservation::new(
                identity,
                profile,
                105,
                BocpdSrObservation::One,
            )),
            Err(BocpdSrError::ArithmeticOverflow { .. })
        ));
        assert_eq!(first.evidence().try_to_canonical_bytes()?, before_overflow);
        Ok(())
    }
}
