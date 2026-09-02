use crate::{
    GqlEvidenceArtifactKind, GqlEvidenceLimitedAuditError, GqlOverlayResultArtifact,
    GqlPreparedResultArtifact,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::{CommitSeq, VId};

const TOKEN_MAGIC: [u8; 8] = *b"FGQPAGE1";
const TOKEN_VERSION_MAJOR: u16 = 1;
const TOKEN_VERSION_MINOR: u16 = 0;
const TOKEN_RESERVED_LEN: usize = 3;
const TOKEN_HEADER_LEN: usize = 8 + 2 + 2 + 1 + TOKEN_RESERVED_LEN;
const TOKEN_DIGEST_LEN: usize = 32;
/// Exact encoded width of a v1 evidence-page token.
pub const GQL_EVIDENCE_PAGE_TOKEN_LEN: usize =
    TOKEN_HEADER_LEN + 8 + TOKEN_DIGEST_LEN + 8 + TOKEN_DIGEST_LEN;
const TOKEN_DOMAIN_V1: &[u8] = b"fgdb:gql-evidence-page-token:v1";

/// A self-checking continuation token for a materialized evidence artifact.
///
/// The token binds the artifact kind, snapshot sequence or transaction basis,
/// complete ordered-result digest, and next row offset. Its checksum detects
/// accidental or unsophisticated mutation; it is not a MAC, capability, or
/// publisher-authenticity proof.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GqlEvidencePageToken {
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    next_offset: u64,
    checksum: Digest,
}

impl GqlEvidencePageToken {
    fn new(
        kind: GqlEvidenceArtifactKind,
        sequence: CommitSeq,
        result_digest: Digest,
        next_offset: u64,
    ) -> Self {
        let checksum = token_checksum(kind, sequence, result_digest, next_offset);
        Self {
            kind,
            sequence,
            result_digest,
            next_offset,
            checksum,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> GqlEvidenceArtifactKind {
        self.kind
    }

    /// Snapshot sequence for durable artifacts, transaction basis for staged
    /// artifacts.
    #[must_use]
    pub const fn sequence(&self) -> CommitSeq {
        self.sequence
    }

    #[must_use]
    pub const fn result_digest(&self) -> Digest {
        self.result_digest
    }

    #[must_use]
    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }

    #[must_use]
    pub const fn canonical_encoded_len() -> usize {
        GQL_EVIDENCE_PAGE_TOKEN_LEN
    }

    #[must_use]
    pub fn verifies_checksum(&self) -> bool {
        digest_eq(
            self.checksum,
            token_checksum(
                self.kind,
                self.sequence,
                self.result_digest,
                self.next_offset,
            ),
        )
    }

    /// Encode the exact fixed-width v1 token.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; GQL_EVIDENCE_PAGE_TOKEN_LEN] {
        let mut bytes = [0_u8; GQL_EVIDENCE_PAGE_TOKEN_LEN];
        bytes[..8].copy_from_slice(&TOKEN_MAGIC);
        bytes[8..10].copy_from_slice(&TOKEN_VERSION_MAJOR.to_be_bytes());
        bytes[10..12].copy_from_slice(&TOKEN_VERSION_MINOR.to_be_bytes());
        bytes[12] = self.kind as u8;
        bytes[13..13 + TOKEN_RESERVED_LEN].fill(0);
        bytes[TOKEN_HEADER_LEN..TOKEN_HEADER_LEN + 8]
            .copy_from_slice(&self.sequence.0.to_be_bytes());
        let result_start = TOKEN_HEADER_LEN + 8;
        bytes[result_start..result_start + TOKEN_DIGEST_LEN].copy_from_slice(&self.result_digest.0);
        let offset_start = result_start + TOKEN_DIGEST_LEN;
        bytes[offset_start..offset_start + 8].copy_from_slice(&self.next_offset.to_be_bytes());
        let checksum_start = offset_start + 8;
        bytes[checksum_start..].copy_from_slice(&self.checksum.0);
        bytes
    }

    /// Decode one exact fixed-width v1 token and verify its checksum.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GqlEvidencePageTokenDecodeError> {
        if bytes.len() < GQL_EVIDENCE_PAGE_TOKEN_LEN {
            return Err(GqlEvidencePageTokenDecodeError::Truncated {
                needed: GQL_EVIDENCE_PAGE_TOKEN_LEN,
                remaining: bytes.len(),
            });
        }
        if bytes.len() > GQL_EVIDENCE_PAGE_TOKEN_LEN {
            return Err(GqlEvidencePageTokenDecodeError::TrailingBytes {
                count: bytes.len() - GQL_EVIDENCE_PAGE_TOKEN_LEN,
            });
        }
        if bytes[..8] != TOKEN_MAGIC {
            return Err(GqlEvidencePageTokenDecodeError::InvalidMagic);
        }
        let major = u16::from_be_bytes([bytes[8], bytes[9]]);
        let minor = u16::from_be_bytes([bytes[10], bytes[11]]);
        if major != TOKEN_VERSION_MAJOR || minor != TOKEN_VERSION_MINOR {
            return Err(GqlEvidencePageTokenDecodeError::UnsupportedVersion { major, minor });
        }
        let kind = match bytes[12] {
            1 => GqlEvidenceArtifactKind::PreparedResult,
            2 => GqlEvidenceArtifactKind::StagedOverlayResult,
            found => return Err(GqlEvidencePageTokenDecodeError::UnknownKind { found }),
        };
        if bytes[13..13 + TOKEN_RESERVED_LEN]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(GqlEvidencePageTokenDecodeError::NonZeroReserved);
        }

        let sequence = CommitSeq(u64::from_be_bytes(
            bytes[TOKEN_HEADER_LEN..TOKEN_HEADER_LEN + 8]
                .try_into()
                .expect("fixed token slice has exact sequence width"),
        ));
        let result_start = TOKEN_HEADER_LEN + 8;
        let result_digest = Digest(
            bytes[result_start..result_start + TOKEN_DIGEST_LEN]
                .try_into()
                .expect("fixed token slice has exact digest width"),
        );
        let offset_start = result_start + TOKEN_DIGEST_LEN;
        let next_offset = u64::from_be_bytes(
            bytes[offset_start..offset_start + 8]
                .try_into()
                .expect("fixed token slice has exact offset width"),
        );
        let checksum_start = offset_start + 8;
        let checksum = Digest(
            bytes[checksum_start..]
                .try_into()
                .expect("fixed token slice has exact checksum width"),
        );
        let token = Self {
            kind,
            sequence,
            result_digest,
            next_offset,
            checksum,
        };
        if !token.verifies_checksum() {
            return Err(GqlEvidencePageTokenDecodeError::ChecksumMismatch);
        }
        Ok(token)
    }
}

impl core::fmt::Debug for GqlEvidencePageToken {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GqlEvidencePageToken")
            .field("kind", &self.kind)
            .field("sequence", &self.sequence)
            .field("result_digest", &self.result_digest)
            .field("next_offset", &self.next_offset)
            .field("checksum", &self.checksum)
            .finish()
    }
}

/// Strict fixed-token decoding failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidencePageTokenDecodeError {
    Truncated { needed: usize, remaining: usize },
    TrailingBytes { count: usize },
    InvalidMagic,
    UnsupportedVersion { major: u16, minor: u16 },
    UnknownKind { found: u8 },
    NonZeroReserved,
    ChecksumMismatch,
}

impl core::fmt::Display for GqlEvidencePageTokenDecodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated { needed, remaining } => write!(
                formatter,
                "GQL evidence page token truncated: needed {needed}, remaining {remaining}"
            ),
            Self::TrailingBytes { count } => {
                write!(
                    formatter,
                    "GQL evidence page token has {count} trailing bytes"
                )
            }
            Self::InvalidMagic => formatter.write_str("invalid GQL evidence page-token magic"),
            Self::UnsupportedVersion { major, minor } => write!(
                formatter,
                "unsupported GQL evidence page-token version {major}.{minor}"
            ),
            Self::UnknownKind { found } => {
                write!(formatter, "unknown GQL evidence page-token kind {found}")
            }
            Self::NonZeroReserved => {
                formatter.write_str("GQL evidence page-token reserved bytes must be zero")
            }
            Self::ChecksumMismatch => {
                formatter.write_str("GQL evidence page-token checksum mismatch")
            }
        }
    }
}

impl core::error::Error for GqlEvidencePageTokenDecodeError {}

/// Paging failures remain distinct from evidence admission and replay failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidencePageError {
    ZeroPageSize,
    TokenDecode(GqlEvidencePageTokenDecodeError),
    TokenKindMismatch {
        expected: GqlEvidenceArtifactKind,
        found: GqlEvidenceArtifactKind,
    },
    TokenSequenceMismatch {
        expected: CommitSeq,
        found: CommitSeq,
    },
    TokenResultMismatch,
    OffsetPastEnd {
        offset: u64,
        row_count: u64,
    },
    OffsetDoesNotFitPlatform {
        offset: u64,
    },
}

impl core::fmt::Display for GqlEvidencePageError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroPageSize => formatter.write_str("GQL evidence page size must be positive"),
            Self::TokenDecode(source) => core::fmt::Display::fmt(source, formatter),
            Self::TokenKindMismatch { expected, found } => write!(
                formatter,
                "GQL evidence page-token kind mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::TokenSequenceMismatch { expected, found } => write!(
                formatter,
                "GQL evidence page-token sequence mismatch: expected {expected:?}, found {found:?}"
            ),
            Self::TokenResultMismatch => {
                formatter.write_str("GQL evidence page-token result mismatch")
            }
            Self::OffsetPastEnd { offset, row_count } => write!(
                formatter,
                "GQL evidence page-token offset {offset} is past row count {row_count}"
            ),
            Self::OffsetDoesNotFitPlatform { offset } => write!(
                formatter,
                "GQL evidence page-token offset {offset} does not fit this platform"
            ),
        }
    }
}

impl core::error::Error for GqlEvidencePageError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::TokenDecode(source) => Some(source),
            Self::ZeroPageSize
            | Self::TokenKindMismatch { .. }
            | Self::TokenSequenceMismatch { .. }
            | Self::TokenResultMismatch
            | Self::OffsetPastEnd { .. }
            | Self::OffsetDoesNotFitPlatform { .. } => None,
        }
    }
}

/// One deterministic contiguous slice of an already materialized exact result.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlEvidencePage {
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    start_offset: u64,
    end_offset: u64,
    total_rows: u64,
    rows: Vec<VId>,
    next_token: Option<GqlEvidencePageToken>,
}

impl GqlEvidencePage {
    #[must_use]
    pub const fn kind(&self) -> GqlEvidenceArtifactKind {
        self.kind
    }

    #[must_use]
    pub const fn sequence(&self) -> CommitSeq {
        self.sequence
    }

    #[must_use]
    pub const fn result_digest(&self) -> Digest {
        self.result_digest
    }

    #[must_use]
    pub const fn start_offset(&self) -> u64 {
        self.start_offset
    }

    #[must_use]
    pub const fn end_offset(&self) -> u64 {
        self.end_offset
    }

    #[must_use]
    pub const fn total_rows(&self) -> u64 {
        self.total_rows
    }

    #[must_use]
    pub const fn remaining_rows(&self) -> u64 {
        self.total_rows - self.end_offset
    }

    #[must_use]
    pub fn rows(&self) -> &[VId] {
        &self.rows
    }

    #[must_use]
    pub fn next_token(&self) -> Option<&GqlEvidencePageToken> {
        self.next_token.as_ref()
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.next_token.is_none()
    }
}

impl core::fmt::Debug for GqlEvidencePage {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GqlEvidencePage")
            .field("kind", &self.kind)
            .field("sequence", &self.sequence)
            .field("result_digest", &self.result_digest)
            .field("start_offset", &self.start_offset)
            .field("end_offset", &self.end_offset)
            .field("total_rows", &self.total_rows)
            .field("remaining_rows", &self.remaining_rows())
            .field("page_row_count", &self.rows.len())
            .field("rows", &"[REDACTED]")
            .field("next_token", &self.next_token)
            .finish()
    }
}

/// One-call product audit failures: the artifact must first pass resource-safe
/// decode and exact replay, then the continuation token must match that result.
#[derive(Debug)]
pub enum GqlEvidencePageAuditError<E> {
    Audit(GqlEvidenceLimitedAuditError<E>),
    Page(GqlEvidencePageError),
}

impl<E: core::fmt::Display> core::fmt::Display for GqlEvidencePageAuditError<E> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Audit(source) => core::fmt::Display::fmt(source, formatter),
            Self::Page(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for GqlEvidencePageAuditError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Audit(source) => Some(source),
            Self::Page(source) => Some(source),
        }
    }
}

impl GqlPreparedResultArtifact {
    /// Return one deterministic page of this already materialized result.
    pub fn page(
        &self,
        page_size: u64,
        after: Option<&GqlEvidencePageToken>,
    ) -> Result<GqlEvidencePage, GqlEvidencePageError> {
        page_rows(
            GqlEvidenceArtifactKind::PreparedResult,
            self.snapshot_seq(),
            self.result_digest(),
            self.rows(),
            page_size,
            after,
        )
    }

    /// Decode an optional canonical token and return one deterministic page.
    pub fn page_from_token_bytes(
        &self,
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<GqlEvidencePage, GqlEvidencePageError> {
        page_from_token_bytes(self, page_size, after)
    }
}

impl GqlOverlayResultArtifact {
    /// Return one deterministic page of this already materialized staged result.
    pub fn page(
        &self,
        page_size: u64,
        after: Option<&GqlEvidencePageToken>,
    ) -> Result<GqlEvidencePage, GqlEvidencePageError> {
        page_rows(
            GqlEvidenceArtifactKind::StagedOverlayResult,
            self.basis(),
            self.result_digest(),
            self.rows(),
            page_size,
            after,
        )
    }

    /// Decode an optional canonical token and return one deterministic page.
    pub fn page_from_token_bytes(
        &self,
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<GqlEvidencePage, GqlEvidencePageError> {
        match after {
            None => self.page(page_size, None),
            Some(bytes) => {
                let token = GqlEvidencePageToken::from_bytes(bytes)
                    .map_err(GqlEvidencePageError::TokenDecode)?;
                self.page(page_size, Some(&token))
            }
        }
    }
}

fn page_from_token_bytes(
    artifact: &GqlPreparedResultArtifact,
    page_size: u64,
    after: Option<&[u8]>,
) -> Result<GqlEvidencePage, GqlEvidencePageError> {
    match after {
        None => artifact.page(page_size, None),
        Some(bytes) => {
            let token = GqlEvidencePageToken::from_bytes(bytes)
                .map_err(GqlEvidencePageError::TokenDecode)?;
            artifact.page(page_size, Some(&token))
        }
    }
}

fn page_rows(
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    rows: &[VId],
    page_size: u64,
    after: Option<&GqlEvidencePageToken>,
) -> Result<GqlEvidencePage, GqlEvidencePageError> {
    if page_size == 0 {
        return Err(GqlEvidencePageError::ZeroPageSize);
    }

    let start_offset = match after {
        None => 0,
        Some(token) => {
            if token.kind != kind {
                return Err(GqlEvidencePageError::TokenKindMismatch {
                    expected: kind,
                    found: token.kind,
                });
            }
            if token.sequence != sequence {
                return Err(GqlEvidencePageError::TokenSequenceMismatch {
                    expected: sequence,
                    found: token.sequence,
                });
            }
            if !digest_eq(token.result_digest, result_digest) {
                return Err(GqlEvidencePageError::TokenResultMismatch);
            }
            token.next_offset
        }
    };

    let row_count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    if start_offset > row_count {
        return Err(GqlEvidencePageError::OffsetPastEnd {
            offset: start_offset,
            row_count,
        });
    }
    let end_offset = start_offset.saturating_add(page_size).min(row_count);
    let start = usize::try_from(start_offset).map_err(|_| {
        GqlEvidencePageError::OffsetDoesNotFitPlatform {
            offset: start_offset,
        }
    })?;
    let end = usize::try_from(end_offset)
        .map_err(|_| GqlEvidencePageError::OffsetDoesNotFitPlatform { offset: end_offset })?;
    let next_token = if end_offset < row_count {
        Some(GqlEvidencePageToken::new(
            kind,
            sequence,
            result_digest,
            end_offset,
        ))
    } else {
        None
    };

    Ok(GqlEvidencePage {
        kind,
        sequence,
        result_digest,
        start_offset,
        end_offset,
        total_rows: row_count,
        rows: rows[start..end].to_vec(),
        next_token,
    })
}

fn token_checksum(
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    next_offset: u64,
) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(TOKEN_DOMAIN_V1);
    hasher.update(&[kind as u8]);
    hasher.update(&sequence.0.to_be_bytes());
    hasher.update(&result_digest.0);
    hasher.update(&next_offset.to_be_bytes());
    hasher.finalize()
}

fn digest_eq(left: Digest, right: Digest) -> bool {
    left.0
        .iter()
        .zip(right.0.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (*left ^ *right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::{GqlEvidencePageError, GqlEvidencePageToken, GqlEvidencePageTokenDecodeError};
    use crate::{
        GqlOverlayResultArtifact, GqlPreparedResultArtifact, PreparedGqlQuery, RelationBind,
    };
    use fgdb_crypto::Digest;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, VId};

    const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";

    fn query() -> PreparedGqlQuery {
        PreparedGqlQuery::prepare(
            STATEMENT,
            &RelationBind::new().with_relation("R", RelationId(7)),
        )
        .expect("query prepares")
    }

    fn prepared(rows: Vec<VId>) -> GqlPreparedResultArtifact {
        GqlPreparedResultArtifact::new(&query(), CommitSeq(11), Digest([0x31; 32]), rows)
    }

    fn overlay(rows: Vec<VId>) -> GqlOverlayResultArtifact {
        GqlOverlayResultArtifact::new(
            &query(),
            CommitSeq(11),
            Digest([0x31; 32]),
            Digest([0x42; 32]),
            rows,
        )
    }

    #[test]
    fn pages_are_contiguous_repeatable_and_terminal() {
        let artifact = prepared(vec![VId(1), VId(2), VId(3), VId(4), VId(5)]);
        let first = artifact.page(2, None).expect("first page succeeds");
        assert_eq!(first.start_offset(), 0);
        assert_eq!(first.end_offset(), 2);
        assert_eq!(first.total_rows(), 5);
        assert_eq!(first.remaining_rows(), 3);
        assert_eq!(first.rows(), &[VId(1), VId(2)]);
        let first_token = *first.next_token().expect("more rows remain");
        assert_eq!(first_token.next_offset(), 2);

        let second = artifact
            .page(2, Some(&first_token))
            .expect("second page succeeds");
        assert_eq!(second.start_offset(), 2);
        assert_eq!(second.end_offset(), 4);
        assert_eq!(second.total_rows(), 5);
        assert_eq!(second.remaining_rows(), 1);
        assert_eq!(second.rows(), &[VId(3), VId(4)]);
        let second_token = *second.next_token().expect("one row remains");

        let final_page = artifact
            .page(2, Some(&second_token))
            .expect("final page succeeds");
        assert_eq!(final_page.start_offset(), 4);
        assert_eq!(final_page.end_offset(), 5);
        assert_eq!(final_page.total_rows(), 5);
        assert_eq!(final_page.remaining_rows(), 0);
        assert_eq!(final_page.rows(), &[VId(5)]);
        assert!(final_page.is_terminal());

        let repeated = artifact
            .page(2, Some(&first_token))
            .expect("same token is repeatable");
        assert_eq!(repeated, second);
    }

    #[test]
    fn empty_and_exact_end_pages_are_terminal() {
        let empty = prepared(vec![]);
        let page = empty.page(8, None).expect("empty result has a page");
        assert!(page.rows().is_empty());
        assert!(page.is_terminal());

        let artifact = prepared(vec![VId(1), VId(2)]);
        let end = GqlEvidencePageToken::new(
            artifact.page(1, None).expect("page succeeds").kind(),
            artifact.snapshot_seq(),
            artifact.result_digest(),
            2,
        );
        let page = artifact
            .page(1, Some(&end))
            .expect("exact end offset is an empty terminal page");
        assert!(page.rows().is_empty());
        assert!(page.is_terminal());
    }

    #[test]
    fn tokens_bind_kind_sequence_result_and_offset() {
        let artifact = prepared(vec![VId(1), VId(2), VId(3)]);
        let token = *artifact
            .page(1, None)
            .expect("page succeeds")
            .next_token()
            .expect("token exists");

        let other_kind = overlay(vec![VId(1), VId(2), VId(3)]);
        assert!(matches!(
            other_kind.page(1, Some(&token)),
            Err(GqlEvidencePageError::TokenKindMismatch { .. })
        ));

        let other_sequence = GqlPreparedResultArtifact::new(
            &query(),
            CommitSeq(12),
            Digest([0x31; 32]),
            vec![VId(1), VId(2), VId(3)],
        );
        assert!(matches!(
            other_sequence.page(1, Some(&token)),
            Err(GqlEvidencePageError::TokenSequenceMismatch { .. })
        ));

        let other_result = prepared(vec![VId(1), VId(2), VId(4)]);
        assert!(matches!(
            other_result.page(1, Some(&token)),
            Err(GqlEvidencePageError::TokenResultMismatch)
        ));

        let past_end =
            GqlEvidencePageToken::new(token.kind(), token.sequence(), token.result_digest(), 99);
        assert!(matches!(
            artifact.page(1, Some(&past_end)),
            Err(GqlEvidencePageError::OffsetPastEnd {
                offset: 99,
                row_count: 3
            })
        ));
    }

    #[test]
    fn zero_page_size_is_a_typed_refusal() {
        assert!(matches!(
            prepared(vec![VId(1)]).page(0, None),
            Err(GqlEvidencePageError::ZeroPageSize)
        ));
    }

    #[test]
    fn token_encoding_is_canonical_strict_and_self_checking() {
        let artifact = prepared(vec![VId(1), VId(2)]);
        let token = *artifact
            .page(1, None)
            .expect("page succeeds")
            .next_token()
            .expect("token exists");
        let bytes = token.to_bytes();
        assert_eq!(bytes.len(), GqlEvidencePageToken::canonical_encoded_len());
        assert_eq!(
            GqlEvidencePageToken::from_bytes(&bytes).expect("token decodes"),
            token
        );

        for cut in 0..bytes.len() {
            assert!(matches!(
                GqlEvidencePageToken::from_bytes(&bytes[..cut]),
                Err(GqlEvidencePageTokenDecodeError::Truncated { .. })
            ));
        }

        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&trailing),
            Err(GqlEvidencePageTokenDecodeError::TrailingBytes { count: 1 })
        ));

        let mut bad_magic = bytes;
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&bad_magic),
            Err(GqlEvidencePageTokenDecodeError::InvalidMagic)
        ));

        let mut bad_version = bytes;
        bad_version[9] = 2;
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&bad_version),
            Err(GqlEvidencePageTokenDecodeError::UnsupportedVersion { .. })
        ));

        let mut bad_kind = bytes;
        bad_kind[12] = 99;
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&bad_kind),
            Err(GqlEvidencePageTokenDecodeError::UnknownKind { found: 99 })
        ));

        let mut bad_reserved = bytes;
        bad_reserved[13] = 1;
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&bad_reserved),
            Err(GqlEvidencePageTokenDecodeError::NonZeroReserved)
        ));

        let mut bad_checksum = bytes;
        bad_checksum[40] ^= 1;
        assert!(matches!(
            GqlEvidencePageToken::from_bytes(&bad_checksum),
            Err(GqlEvidencePageTokenDecodeError::ChecksumMismatch)
        ));
    }

    #[test]
    fn page_debug_redacts_rows() {
        let page = prepared(vec![VId(0xfeed_face)])
            .page(8, None)
            .expect("page succeeds");
        let debug = format!("{page:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("4277009102"));
    }
}
