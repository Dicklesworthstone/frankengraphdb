use crate::{
    GqlEvidenceArtifactKind, GqlEvidenceAuditError, GqlEvidenceDecodeError,
    GqlOverlayResultArtifact, GqlPreparedResultArtifact,
};

const MAGIC: [u8; 8] = *b"FGQEVID1";
const VERSION_MAJOR: u16 = 1;
const VERSION_MINOR: u16 = 0;
const HEADER_LEN: usize = 16;
const PREPARED_ROW_COUNT_OFFSET: usize = HEADER_LEN + 8 + (32 * 3);
const OVERLAY_ROW_COUNT_OFFSET: usize = HEADER_LEN + 8 + (32 * 4);
const PREPARED_FIXED_LEN: u64 = 160;
const OVERLAY_FIXED_LEN: u64 = 192;
const ROW_LEN: u64 = 8;

/// Conservative resource limits for decoding or encoding untrusted GQL
/// evidence envelopes.
///
/// The limits are policy, not part of the v1 canonical byte transcript. Callers
/// that need larger artifacts can supply an explicit value while retaining the
/// same strict decoder and transcript semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlEvidenceLimits {
    max_encoded_bytes: u64,
    max_rows: u64,
}

impl GqlEvidenceLimits {
    /// Default admission policy for raw bytes crossing an untrusted boundary.
    ///
    /// The row ceiling fits within the byte ceiling for both current artifact
    /// kinds. Neither value is a product SLO or a format maximum.
    pub const DEFAULT_UNTRUSTED: Self = Self::new(16 * 1024 * 1024, 1_000_000);

    #[must_use]
    pub const fn new(max_encoded_bytes: u64, max_rows: u64) -> Self {
        Self {
            max_encoded_bytes,
            max_rows,
        }
    }

    #[must_use]
    pub const fn max_encoded_bytes(self) -> u64 {
        self.max_encoded_bytes
    }

    #[must_use]
    pub const fn max_rows(self) -> u64 {
        self.max_rows
    }

    /// Screen one prepared-result envelope before the decoder allocates rows.
    pub fn preflight_prepared(
        self,
        bytes: &[u8],
    ) -> Result<(), GqlEvidenceLimitExceeded> {
        self.check_encoded_bytes(len_as_u64(bytes.len()))?;
        if let Some(row_count) = declared_row_count(
            bytes,
            GqlEvidenceArtifactKind::PreparedResult,
            PREPARED_ROW_COUNT_OFFSET,
        ) {
            self.check_rows(row_count)?;
        }
        Ok(())
    }

    /// Screen one staged-overlay envelope before the decoder allocates rows.
    pub fn preflight_overlay(
        self,
        bytes: &[u8],
    ) -> Result<(), GqlEvidenceLimitExceeded> {
        self.check_encoded_bytes(len_as_u64(bytes.len()))?;
        if let Some(row_count) = declared_row_count(
            bytes,
            GqlEvidenceArtifactKind::StagedOverlayResult,
            OVERLAY_ROW_COUNT_OFFSET,
        ) {
            self.check_rows(row_count)?;
        }
        Ok(())
    }

    fn check_encoded_bytes(
        self,
        observed: u64,
    ) -> Result<(), GqlEvidenceLimitExceeded> {
        check_limit(
            GqlEvidenceLimitDimension::EncodedBytes,
            self.max_encoded_bytes,
            observed,
        )
    }

    fn check_rows(self, observed: u64) -> Result<(), GqlEvidenceLimitExceeded> {
        check_limit(
            GqlEvidenceLimitDimension::Rows,
            self.max_rows,
            observed,
        )
    }
}

impl Default for GqlEvidenceLimits {
    fn default() -> Self {
        Self::DEFAULT_UNTRUSTED
    }
}

/// The resource dimension that refused an evidence envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlEvidenceLimitDimension {
    EncodedBytes,
    Rows,
}

/// Typed refusal emitted before an evidence decoder performs unbounded work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlEvidenceLimitExceeded {
    pub dimension: GqlEvidenceLimitDimension,
    pub limit: u64,
    pub observed: u64,
}

impl core::fmt::Display for GqlEvidenceLimitExceeded {
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        write!(
            formatter,
            "GQL evidence {:?} limit exceeded: observed {}, limit {}",
            self.dimension, self.observed, self.limit
        )
    }
}

impl core::error::Error for GqlEvidenceLimitExceeded {}

/// Resource-aware decoding keeps policy refusal distinct from byte-format
/// refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidenceLimitedDecodeError {
    Limit(GqlEvidenceLimitExceeded),
    Decode(GqlEvidenceDecodeError),
}

impl core::fmt::Display for GqlEvidenceLimitedDecodeError {
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match self {
            Self::Limit(source) => core::fmt::Display::fmt(source, formatter),
            Self::Decode(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl core::error::Error for GqlEvidenceLimitedDecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Limit(source) => Some(source),
            Self::Decode(source) => Some(source),
        }
    }
}

/// Resource-aware product audit keeps admission refusal distinct from every
/// existing syntax, identity, execution, and replay refusal.
#[derive(Debug)]
pub enum GqlEvidenceLimitedAuditError<E> {
    Limit(GqlEvidenceLimitExceeded),
    Audit(GqlEvidenceAuditError<E>),
}

impl<E: core::fmt::Display> core::fmt::Display
    for GqlEvidenceLimitedAuditError<E>
{
    fn fmt(
        &self,
        formatter: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        match self {
            Self::Limit(source) => core::fmt::Display::fmt(source, formatter),
            Self::Audit(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error
    for GqlEvidenceLimitedAuditError<E>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Limit(source) => Some(source),
            Self::Audit(source) => Some(source),
        }
    }
}

impl GqlPreparedResultArtifact {
    /// Strictly decode bytes under the default untrusted-input policy.
    pub fn from_untrusted_bytes(
        bytes: &[u8],
    ) -> Result<Self, GqlEvidenceLimitedDecodeError> {
        Self::from_bytes_with_limits(bytes, GqlEvidenceLimits::DEFAULT_UNTRUSTED)
    }

    /// Strictly decode bytes after screening total size and the declared row
    /// count before row allocation.
    pub fn from_bytes_with_limits(
        bytes: &[u8],
        limits: GqlEvidenceLimits,
    ) -> Result<Self, GqlEvidenceLimitedDecodeError> {
        limits
            .preflight_prepared(bytes)
            .map_err(GqlEvidenceLimitedDecodeError::Limit)?;
        let artifact =
            Self::from_bytes(bytes).map_err(GqlEvidenceLimitedDecodeError::Decode)?;
        limits
            .check_rows(len_as_u64(artifact.rows().len()))
            .map_err(GqlEvidenceLimitedDecodeError::Limit)?;
        Ok(artifact)
    }

    /// Encode only when both the exact row count and canonical byte length fit
    /// the supplied policy.
    pub fn to_bytes_with_limits(
        &self,
        limits: GqlEvidenceLimits,
    ) -> Result<Vec<u8>, GqlEvidenceLimitExceeded> {
        let rows = len_as_u64(self.rows().len());
        limits.check_rows(rows)?;
        limits.check_encoded_bytes(encoded_len(PREPARED_FIXED_LEN, rows))?;
        Ok(self.to_bytes())
    }

    #[must_use]
    pub fn canonical_encoded_len(&self) -> u64 {
        encoded_len(
            PREPARED_FIXED_LEN,
            len_as_u64(self.rows().len()),
        )
    }
}

impl GqlOverlayResultArtifact {
    /// Strictly decode staged-overlay bytes under the default untrusted policy.
    pub fn from_untrusted_bytes(
        bytes: &[u8],
    ) -> Result<Self, GqlEvidenceLimitedDecodeError> {
        Self::from_bytes_with_limits(bytes, GqlEvidenceLimits::DEFAULT_UNTRUSTED)
    }

    /// Strictly decode staged-overlay bytes after screening total size and the
    /// declared row count before row allocation.
    pub fn from_bytes_with_limits(
        bytes: &[u8],
        limits: GqlEvidenceLimits,
    ) -> Result<Self, GqlEvidenceLimitedDecodeError> {
        limits
            .preflight_overlay(bytes)
            .map_err(GqlEvidenceLimitedDecodeError::Limit)?;
        let artifact =
            Self::from_bytes(bytes).map_err(GqlEvidenceLimitedDecodeError::Decode)?;
        limits
            .check_rows(len_as_u64(artifact.rows().len()))
            .map_err(GqlEvidenceLimitedDecodeError::Limit)?;
        Ok(artifact)
    }

    /// Encode only when both the exact row count and canonical byte length fit
    /// the supplied policy.
    pub fn to_bytes_with_limits(
        &self,
        limits: GqlEvidenceLimits,
    ) -> Result<Vec<u8>, GqlEvidenceLimitExceeded> {
        let rows = len_as_u64(self.rows().len());
        limits.check_rows(rows)?;
        limits.check_encoded_bytes(encoded_len(OVERLAY_FIXED_LEN, rows))?;
        Ok(self.to_bytes())
    }

    #[must_use]
    pub fn canonical_encoded_len(&self) -> u64 {
        encoded_len(
            OVERLAY_FIXED_LEN,
            len_as_u64(self.rows().len()),
        )
    }
}

fn check_limit(
    dimension: GqlEvidenceLimitDimension,
    limit: u64,
    observed: u64,
) -> Result<(), GqlEvidenceLimitExceeded> {
    if observed > limit {
        Err(GqlEvidenceLimitExceeded {
            dimension,
            limit,
            observed,
        })
    } else {
        Ok(())
    }
}

fn declared_row_count(
    bytes: &[u8],
    expected_kind: GqlEvidenceArtifactKind,
    offset: usize,
) -> Option<u64> {
    if !has_expected_header(bytes, expected_kind) {
        return None;
    }
    let raw: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_be_bytes(raw))
}

fn has_expected_header(
    bytes: &[u8],
    expected_kind: GqlEvidenceArtifactKind,
) -> bool {
    bytes.len() >= HEADER_LEN
        && bytes[..8] == MAGIC
        && u16::from_be_bytes([bytes[8], bytes[9]]) == VERSION_MAJOR
        && u16::from_be_bytes([bytes[10], bytes[11]]) == VERSION_MINOR
        && bytes[12] == expected_kind as u8
        && bytes[13..HEADER_LEN].iter().all(|byte| *byte == 0)
}

fn encoded_len(fixed: u64, rows: u64) -> u64 {
    rows
        .checked_mul(ROW_LEN)
        .and_then(|row_bytes| fixed.checked_add(row_bytes))
        .unwrap_or(u64::MAX)
}

fn len_as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        GqlEvidenceLimitDimension, GqlEvidenceLimitedDecodeError,
        GqlEvidenceLimits,
    };
    use crate::{
        GqlEvidenceDecodeError, GqlOverlayResultArtifact,
        GqlPreparedResultArtifact, PreparedGqlQuery, RelationBind,
    };
    use fgdb_crypto::Digest;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, VId};

    const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";

    fn prepared_query() -> PreparedGqlQuery {
        PreparedGqlQuery::prepare(
            STATEMENT,
            &RelationBind::new().with_relation("R", RelationId(7)),
        )
        .expect("query prepares")
    }

    fn prepared_artifact() -> GqlPreparedResultArtifact {
        GqlPreparedResultArtifact::new(
            &prepared_query(),
            CommitSeq(11),
            Digest([0x31; 32]),
            vec![VId(2), VId(9)],
        )
    }

    fn overlay_artifact() -> GqlOverlayResultArtifact {
        GqlOverlayResultArtifact::new(
            &prepared_query(),
            CommitSeq(11),
            Digest([0x31; 32]),
            Digest([0x42; 32]),
            vec![VId(2), VId(9)],
        )
    }

    #[test]
    fn exact_byte_and_row_limits_succeed() {
        let prepared = prepared_artifact();
        let bytes = prepared.to_bytes();
        let limits =
            GqlEvidenceLimits::new(bytes.len() as u64, prepared.rows().len() as u64);

        let decoded = GqlPreparedResultArtifact::from_bytes_with_limits(
            &bytes, limits,
        )
        .expect("exact limits admit the artifact");
        assert_eq!(decoded, prepared);
        assert_eq!(prepared.canonical_encoded_len(), bytes.len() as u64);
        assert_eq!(
            prepared
                .to_bytes_with_limits(limits)
                .expect("exact encoding limits succeed"),
            bytes
        );
    }

    #[test]
    fn one_below_byte_and_row_limits_refuse() {
        let artifact = prepared_artifact();
        let bytes = artifact.to_bytes();

        let byte_error = GqlPreparedResultArtifact::from_bytes_with_limits(
            &bytes,
            GqlEvidenceLimits::new((bytes.len() - 1) as u64, u64::MAX),
        )
        .expect_err("one byte below the exact length refuses");
        assert!(matches!(
            byte_error,
            GqlEvidenceLimitedDecodeError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::EncodedBytes
                    && exceeded.limit == (bytes.len() - 1) as u64
                    && exceeded.observed == bytes.len() as u64
        ));

        let row_error = GqlPreparedResultArtifact::from_bytes_with_limits(
            &bytes,
            GqlEvidenceLimits::new(u64::MAX, 1),
        )
        .expect_err("one row below the declaration refuses");
        assert!(matches!(
            row_error,
            GqlEvidenceLimitedDecodeError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.limit == 1
                    && exceeded.observed == 2
        ));
    }

    #[test]
    fn declared_rows_are_screened_before_structural_decode() {
        let mut bytes = prepared_artifact().to_bytes();
        bytes[120..128].copy_from_slice(&u64::MAX.to_be_bytes());
        bytes.truncate(128);

        let error = GqlPreparedResultArtifact::from_bytes_with_limits(
            &bytes,
            GqlEvidenceLimits::new(1024, 100),
        )
        .expect_err("hostile declared row count refuses before allocation");
        assert!(matches!(
            error,
            GqlEvidenceLimitedDecodeError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.limit == 100
                    && exceeded.observed == u64::MAX
        ));
    }

    #[test]
    fn malformed_headers_preserve_decoder_error_classes() {
        let mut bytes = prepared_artifact().to_bytes();
        bytes[0] ^= 0xff;

        let error = GqlPreparedResultArtifact::from_bytes_with_limits(
            &bytes,
            GqlEvidenceLimits::new(bytes.len() as u64, u64::MAX),
        )
        .expect_err("invalid magic remains a format refusal");
        assert!(matches!(
            error,
            GqlEvidenceLimitedDecodeError::Decode(
                GqlEvidenceDecodeError::InvalidMagic
            )
        ));
    }

    #[test]
    fn overlay_limits_use_the_overlay_row_count_offset() {
        let artifact = overlay_artifact();
        let bytes = artifact.to_bytes();
        let limits =
            GqlEvidenceLimits::new(bytes.len() as u64, artifact.rows().len() as u64);

        let decoded = GqlOverlayResultArtifact::from_bytes_with_limits(
            &bytes, limits,
        )
        .expect("exact overlay limits admit the artifact");
        assert_eq!(decoded, artifact);
        assert_eq!(artifact.canonical_encoded_len(), bytes.len() as u64);

        let mut hostile = bytes;
        hostile[152..160].copy_from_slice(&u64::MAX.to_be_bytes());
        hostile.truncate(160);
        let error = GqlOverlayResultArtifact::from_bytes_with_limits(
            &hostile,
            GqlEvidenceLimits::new(1024, 100),
        )
        .expect_err("hostile overlay row count refuses before allocation");
        assert!(matches!(
            error,
            GqlEvidenceLimitedDecodeError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.observed == u64::MAX
        ));
    }

    #[test]
    fn default_untrusted_policy_accepts_small_canonical_artifacts() {
        let prepared = prepared_artifact();
        let overlay = overlay_artifact();

        assert_eq!(
            GqlPreparedResultArtifact::from_untrusted_bytes(
                &prepared.to_bytes()
            )
            .expect("small prepared artifact passes default policy"),
            prepared
        );
        assert_eq!(
            GqlOverlayResultArtifact::from_untrusted_bytes(
                &overlay.to_bytes()
            )
            .expect("small overlay artifact passes default policy"),
            overlay
        );
    }
}
