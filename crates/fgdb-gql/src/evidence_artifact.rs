use crate::{GqlOverlayResultCertificate, PreparedGqlQuery, RelationBind};
use fgdb_crypto::{Digest, Hasher, hash};
use fgdb_types::{CommitSeq, VId};

const MAGIC: [u8; 8] = *b"FGQEVID1";
const VERSION_MAJOR: u16 = 1;
const VERSION_MINOR: u16 = 0;
const KIND_PREPARED_RESULT: u8 = 1;
const KIND_STAGED_OVERLAY_RESULT: u8 = 2;
const RESERVED_LEN: usize = 3;
const HEADER_LEN: usize = 16;
const DIGEST_LEN: usize = 32;
/// Exact encoded width of one result row: a `VId` is a `u128`, written
/// big-endian. This is a v1 format decision shared with the digest transcript
/// and with `evidence_limits::ROW_LEN`; the decoder, the encoder, and the byte
/// arithmetic must all agree on it.
const ROW_LEN: usize = 16;
const GQL_RESULT_DIGEST_DOMAIN_V1: &[u8] = b"fgdb:gql-ordered-result-digest:v1";

/// Closed artifact kinds in the first evidence-envelope version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GqlEvidenceArtifactKind {
    PreparedResult = KIND_PREPARED_RESULT,
    StagedOverlayResult = KIND_STAGED_OVERLAY_RESULT,
}

/// Fail-closed decoding errors for canonical GQL evidence envelopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidenceDecodeError {
    Truncated {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    InvalidMagic,
    UnsupportedVersion {
        major: u16,
        minor: u16,
    },
    UnexpectedKind {
        expected: GqlEvidenceArtifactKind,
        found: u8,
    },
    NonZeroReserved,
    RowCountOverflow {
        row_count: u64,
    },
    LengthOverflow,
    TrailingBytes {
        count: usize,
    },
    ResultDigestMismatch,
}

impl core::fmt::Display for GqlEvidenceDecodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "GQL evidence truncated at byte {offset}: needed {needed}, remaining {remaining}"
            ),
            Self::InvalidMagic => formatter.write_str("invalid GQL evidence magic"),
            Self::UnsupportedVersion { major, minor } => write!(
                formatter,
                "unsupported GQL evidence version {major}.{minor}"
            ),
            Self::UnexpectedKind { expected, found } => write!(
                formatter,
                "unexpected GQL evidence kind {found}, expected {expected:?}"
            ),
            Self::NonZeroReserved => {
                formatter.write_str("GQL evidence reserved bytes must be zero")
            }
            Self::RowCountOverflow { row_count } => write!(
                formatter,
                "GQL evidence row count {row_count} does not fit this platform"
            ),
            Self::LengthOverflow => formatter.write_str("GQL evidence encoded length overflows"),
            Self::TrailingBytes { count } => {
                write!(formatter, "GQL evidence has {count} trailing bytes")
            }
            Self::ResultDigestMismatch => {
                formatter.write_str("GQL evidence result digest mismatch")
            }
        }
    }
}

impl core::error::Error for GqlEvidenceDecodeError {}

/// Audit errors preserve format, definition, execution, and replay mismatches as
/// separate refusal classes.
#[derive(Debug)]
pub enum GqlEvidenceAuditError<E> {
    Decode(GqlEvidenceDecodeError),
    SnapshotMismatch,
    InputMismatch,
    PlanMismatch,
    StagedEffectMismatch,
    Execution(E),
    ResultMismatch,
}

impl<E: core::fmt::Display> core::fmt::Display for GqlEvidenceAuditError<E> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Decode(source) => core::fmt::Display::fmt(source, formatter),
            Self::SnapshotMismatch => {
                formatter.write_str("GQL evidence snapshot or transaction basis mismatch")
            }
            Self::InputMismatch => formatter.write_str("GQL evidence prepared-input mismatch"),
            Self::PlanMismatch => formatter.write_str("GQL evidence plan mismatch"),
            Self::StagedEffectMismatch => {
                formatter.write_str("GQL evidence staged-effect mismatch")
            }
            Self::Execution(source) => core::fmt::Display::fmt(source, formatter),
            Self::ResultMismatch => formatter.write_str("GQL evidence rows differ from replay"),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for GqlEvidenceAuditError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Decode(source) => Some(source),
            Self::Execution(source) => Some(source),
            Self::SnapshotMismatch
            | Self::InputMismatch
            | Self::PlanMismatch
            | Self::StagedEffectMismatch
            | Self::ResultMismatch => None,
        }
    }
}

/// Canonical evidence envelope for one exact prepared query result over a
/// durable snapshot.
///
/// The envelope is self-contained for verification of its own bytes and input
/// identity. Product-level audit still reopens `snapshot_seq`, recomputes the
/// plan certificate, and re-executes the query before accepting the rows.
///
/// Version 1 is an unreleased application artifact, not a Chronicle object,
/// FGP frame, or compatibility commitment.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlPreparedResultArtifact {
    snapshot_seq: CommitSeq,
    statement_digest: Digest,
    bind_digest: Digest,
    plan_digest: Digest,
    rows: Vec<VId>,
    result_digest: Digest,
}

impl GqlPreparedResultArtifact {
    #[must_use]
    pub fn new(
        query: &PreparedGqlQuery,
        snapshot_seq: CommitSeq,
        plan_digest: Digest,
        rows: Vec<VId>,
    ) -> Self {
        let result_digest = digest_prepared_result(snapshot_seq, plan_digest, &rows);
        Self {
            snapshot_seq,
            statement_digest: digest_statement(query.statement()),
            bind_digest: digest_bind(query.bind()),
            plan_digest,
            rows,
            result_digest,
        }
    }

    #[must_use]
    pub const fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot_seq
    }

    #[must_use]
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }

    #[must_use]
    pub fn rows(&self) -> &[VId] {
        &self.rows
    }

    #[must_use]
    pub const fn result_digest(&self) -> Digest {
        self.result_digest
    }

    #[must_use]
    pub fn verifies_input(&self, query: &PreparedGqlQuery) -> bool {
        digest_eq(self.statement_digest, digest_statement(query.statement()))
            && digest_eq(self.bind_digest, digest_bind(query.bind()))
    }

    #[must_use]
    pub fn verifies_plan(&self, plan_digest: Digest) -> bool {
        digest_eq(self.plan_digest, plan_digest)
    }

    #[must_use]
    pub fn verifies_rows(&self) -> bool {
        digest_eq(
            self.result_digest,
            digest_prepared_result(self.snapshot_seq, self.plan_digest, &self.rows),
        )
    }

    /// Encode the exact canonical v1 envelope.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + 8 + (DIGEST_LEN * 4) + 8 + (self.rows.len() * ROW_LEN));
        encode_header(GqlEvidenceArtifactKind::PreparedResult, &mut bytes);
        bytes.extend_from_slice(&self.snapshot_seq.0.to_be_bytes());
        bytes.extend_from_slice(&self.statement_digest.0);
        bytes.extend_from_slice(&self.bind_digest.0);
        bytes.extend_from_slice(&self.plan_digest.0);
        bytes.extend_from_slice(&count_as_u64(self.rows.len()).to_be_bytes());
        for row in &self.rows {
            bytes.extend_from_slice(&row.0.to_be_bytes());
        }
        bytes.extend_from_slice(&self.result_digest.0);
        bytes
    }

    /// Decode one exact canonical v1 prepared-result envelope.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GqlEvidenceDecodeError> {
        let mut decoder = Decoder::new(bytes);
        decoder.header(GqlEvidenceArtifactKind::PreparedResult)?;
        let snapshot_seq = CommitSeq(decoder.u64()?);
        let statement_digest = decoder.digest()?;
        let bind_digest = decoder.digest()?;
        let plan_digest = decoder.digest()?;
        let rows = decoder.rows_and_leave_digest()?;
        let result_digest = decoder.digest()?;
        decoder.finish()?;

        let artifact = Self {
            snapshot_seq,
            statement_digest,
            bind_digest,
            plan_digest,
            rows,
            result_digest,
        };
        if !artifact.verifies_rows() {
            return Err(GqlEvidenceDecodeError::ResultDigestMismatch);
        }
        Ok(artifact)
    }
}

impl core::fmt::Debug for GqlPreparedResultArtifact {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GqlPreparedResultArtifact")
            .field("snapshot_seq", &self.snapshot_seq)
            .field("statement_digest", &self.statement_digest)
            .field("bind_digest", &self.bind_digest)
            .field("plan_digest", &self.plan_digest)
            .field("row_count", &self.rows.len())
            .field("rows", &"[REDACTED]")
            .field("result_digest", &self.result_digest)
            .finish()
    }
}

/// Canonical evidence envelope for one exact result over a staged transaction
/// overlay.
///
/// Version 1 carries identities and rows. It does not carry the durable snapshot
/// contents, staged template bytes, read set, or conflict state, so standalone
/// transaction replay remains outside its claim.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlOverlayResultArtifact {
    basis: CommitSeq,
    statement_digest: Digest,
    bind_digest: Digest,
    plan_digest: Digest,
    staged_effect_digest: Digest,
    rows: Vec<VId>,
    result_digest: Digest,
}

impl GqlOverlayResultArtifact {
    #[must_use]
    pub fn new(
        query: &PreparedGqlQuery,
        basis: CommitSeq,
        plan_digest: Digest,
        staged_effect_digest: Digest,
        rows: Vec<VId>,
    ) -> Self {
        let result_digest =
            GqlOverlayResultCertificate::new(basis, plan_digest, staged_effect_digest, &rows)
                .result_digest;
        Self {
            basis,
            statement_digest: digest_statement(query.statement()),
            bind_digest: digest_bind(query.bind()),
            plan_digest,
            staged_effect_digest,
            rows,
            result_digest,
        }
    }

    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    #[must_use]
    pub const fn plan_digest(&self) -> Digest {
        self.plan_digest
    }

    #[must_use]
    pub const fn staged_effect_digest(&self) -> Digest {
        self.staged_effect_digest
    }

    #[must_use]
    pub fn rows(&self) -> &[VId] {
        &self.rows
    }

    #[must_use]
    pub const fn result_digest(&self) -> Digest {
        self.result_digest
    }

    #[must_use]
    pub fn verifies_input(&self, query: &PreparedGqlQuery) -> bool {
        digest_eq(self.statement_digest, digest_statement(query.statement()))
            && digest_eq(self.bind_digest, digest_bind(query.bind()))
    }

    #[must_use]
    pub fn verifies_plan(&self, plan_digest: Digest) -> bool {
        digest_eq(self.plan_digest, plan_digest)
    }

    #[must_use]
    pub fn verifies_staged_effect(&self, staged_effect_digest: Digest) -> bool {
        digest_eq(self.staged_effect_digest, staged_effect_digest)
    }

    #[must_use]
    pub fn verifies_rows(&self) -> bool {
        GqlOverlayResultCertificate {
            basis: self.basis,
            plan_digest: self.plan_digest,
            staged_effect_digest: self.staged_effect_digest,
            row_count: count_as_u64(self.rows.len()),
            result_digest: self.result_digest,
        }
        .verifies(
            self.basis,
            self.plan_digest,
            self.staged_effect_digest,
            &self.rows,
        )
    }

    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(HEADER_LEN + 8 + (DIGEST_LEN * 5) + 8 + (self.rows.len() * ROW_LEN));
        encode_header(GqlEvidenceArtifactKind::StagedOverlayResult, &mut bytes);
        bytes.extend_from_slice(&self.basis.0.to_be_bytes());
        bytes.extend_from_slice(&self.statement_digest.0);
        bytes.extend_from_slice(&self.bind_digest.0);
        bytes.extend_from_slice(&self.plan_digest.0);
        bytes.extend_from_slice(&self.staged_effect_digest.0);
        bytes.extend_from_slice(&count_as_u64(self.rows.len()).to_be_bytes());
        for row in &self.rows {
            bytes.extend_from_slice(&row.0.to_be_bytes());
        }
        bytes.extend_from_slice(&self.result_digest.0);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, GqlEvidenceDecodeError> {
        let mut decoder = Decoder::new(bytes);
        decoder.header(GqlEvidenceArtifactKind::StagedOverlayResult)?;
        let basis = CommitSeq(decoder.u64()?);
        let statement_digest = decoder.digest()?;
        let bind_digest = decoder.digest()?;
        let plan_digest = decoder.digest()?;
        let staged_effect_digest = decoder.digest()?;
        let rows = decoder.rows_and_leave_digest()?;
        let result_digest = decoder.digest()?;
        decoder.finish()?;

        let artifact = Self {
            basis,
            statement_digest,
            bind_digest,
            plan_digest,
            staged_effect_digest,
            rows,
            result_digest,
        };
        if !artifact.verifies_rows() {
            return Err(GqlEvidenceDecodeError::ResultDigestMismatch);
        }
        Ok(artifact)
    }
}

impl core::fmt::Debug for GqlOverlayResultArtifact {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GqlOverlayResultArtifact")
            .field("basis", &self.basis)
            .field("statement_digest", &self.statement_digest)
            .field("bind_digest", &self.bind_digest)
            .field("plan_digest", &self.plan_digest)
            .field("staged_effect_digest", &self.staged_effect_digest)
            .field("row_count", &self.rows.len())
            .field("rows", &"[REDACTED]")
            .field("result_digest", &self.result_digest)
            .finish()
    }
}

fn encode_header(kind: GqlEvidenceArtifactKind, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION_MAJOR.to_be_bytes());
    bytes.extend_from_slice(&VERSION_MINOR.to_be_bytes());
    bytes.push(kind as u8);
    bytes.extend_from_slice(&[0; RESERVED_LEN]);
}

fn digest_statement(statement: &str) -> Digest {
    hash(statement.as_bytes())
}

fn digest_bind(bind: &RelationBind) -> Digest {
    hash(&bind.canonical_bytes())
}

fn digest_prepared_result(snapshot_seq: CommitSeq, plan_digest: Digest, rows: &[VId]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(GQL_RESULT_DIGEST_DOMAIN_V1);
    hasher.update(&plan_digest.0);
    hasher.update(&snapshot_seq.0.to_be_bytes());
    hasher.update(&count_as_u64(rows.len()).to_be_bytes());
    for row in rows {
        hasher.update(&row.0.to_be_bytes());
    }
    hasher.finalize()
}

fn count_as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
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

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn header(&mut self, expected: GqlEvidenceArtifactKind) -> Result<(), GqlEvidenceDecodeError> {
        if self.take::<8>()? != MAGIC {
            return Err(GqlEvidenceDecodeError::InvalidMagic);
        }
        let major = self.u16()?;
        let minor = self.u16()?;
        if major != VERSION_MAJOR || minor != VERSION_MINOR {
            return Err(GqlEvidenceDecodeError::UnsupportedVersion { major, minor });
        }
        let found = self.u8()?;
        if found != expected as u8 {
            return Err(GqlEvidenceDecodeError::UnexpectedKind { expected, found });
        }
        if self.take::<RESERVED_LEN>()? != [0; RESERVED_LEN] {
            return Err(GqlEvidenceDecodeError::NonZeroReserved);
        }
        Ok(())
    }

    fn rows_and_leave_digest(&mut self) -> Result<Vec<VId>, GqlEvidenceDecodeError> {
        let row_count = self.u64()?;
        let row_count = usize::try_from(row_count)
            .map_err(|_| GqlEvidenceDecodeError::RowCountOverflow { row_count })?;
        let row_bytes = row_count
            .checked_mul(ROW_LEN)
            .ok_or(GqlEvidenceDecodeError::LengthOverflow)?;
        let required = row_bytes
            .checked_add(DIGEST_LEN)
            .ok_or(GqlEvidenceDecodeError::LengthOverflow)?;
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if remaining < required {
            return Err(GqlEvidenceDecodeError::Truncated {
                offset: self.offset,
                needed: required,
                remaining,
            });
        }

        let mut rows = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            rows.push(VId(self.u128()?));
        }
        Ok(rows)
    }

    fn u128(&mut self) -> Result<u128, GqlEvidenceDecodeError> {
        Ok(u128::from_be_bytes(self.take::<ROW_LEN>()?))
    }

    fn digest(&mut self) -> Result<Digest, GqlEvidenceDecodeError> {
        Ok(Digest(self.take::<DIGEST_LEN>()?))
    }

    fn u8(&mut self) -> Result<u8, GqlEvidenceDecodeError> {
        Ok(self.take::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, GqlEvidenceDecodeError> {
        Ok(u16::from_be_bytes(self.take::<2>()?))
    }

    fn u64(&mut self) -> Result<u64, GqlEvidenceDecodeError> {
        Ok(u64::from_be_bytes(self.take::<8>()?))
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], GqlEvidenceDecodeError> {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if remaining < N {
            return Err(GqlEvidenceDecodeError::Truncated {
                offset: self.offset,
                needed: N,
                remaining,
            });
        }
        let end = self.offset + N;
        let mut value = [0; N];
        value.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(value)
    }

    fn finish(self) -> Result<(), GqlEvidenceDecodeError> {
        let trailing = self.bytes.len().saturating_sub(self.offset);
        if trailing == 0 {
            Ok(())
        } else {
            Err(GqlEvidenceDecodeError::TrailingBytes { count: trailing })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GqlEvidenceArtifactKind, GqlEvidenceDecodeError, GqlOverlayResultArtifact,
        GqlPreparedResultArtifact,
    };
    use crate::{PreparedGqlQuery, RelationBind};
    use fgdb_crypto::Digest;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, VId};

    fn query() -> PreparedGqlQuery {
        PreparedGqlQuery::prepare(
            "MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(7)),
        )
        .expect("query binds")
    }

    fn digest(byte: u8) -> Digest {
        Digest([byte; 32])
    }

    #[test]
    fn prepared_artifact_round_trips_and_rejects_every_truncation() {
        let artifact = GqlPreparedResultArtifact::new(
            &query(),
            CommitSeq(11),
            digest(0x31),
            vec![VId(2), VId(9)],
        );
        let bytes = artifact.to_bytes();
        assert_eq!(
            GqlPreparedResultArtifact::from_bytes(&bytes).expect("canonical bytes decode"),
            artifact
        );

        for end in 0..bytes.len() {
            assert!(
                GqlPreparedResultArtifact::from_bytes(&bytes[..end]).is_err(),
                "prefix {end} must refuse"
            );
        }
    }

    #[test]
    fn rows_are_sixteen_byte_ids_and_full_width_survives_round_trip() {
        // The format decision under test: a `VId` is a `u128` and occupies
        // exactly 16 bytes per row. A decoder that read 8 bytes would either
        // fail to compile against `VId` (the 2026-09-02 defect) or, worse,
        // decode a truncated id, so both halves are pinned here.
        let rows = vec![VId(u128::MAX), VId(0), VId(1 << 64), VId(u64::MAX as u128)];
        let artifact =
            GqlPreparedResultArtifact::new(&query(), CommitSeq(11), digest(0x31), rows.clone());
        let bytes = artifact.to_bytes();
        let fixed = 16 + 8 + (32 * 3) + 8 + 32;
        assert_eq!(bytes.len(), fixed + rows.len() * 16);
        let decoded = GqlPreparedResultArtifact::from_bytes(&bytes).expect("full-width ids decode");
        assert_eq!(decoded.rows(), rows.as_slice());
        assert_eq!(decoded, artifact);

        let overlay = GqlOverlayResultArtifact::new(
            &query(),
            CommitSeq(11),
            digest(0x31),
            digest(0x41),
            rows.clone(),
        );
        let bytes = overlay.to_bytes();
        let fixed = 16 + 8 + (32 * 4) + 8 + 32;
        assert_eq!(bytes.len(), fixed + rows.len() * 16);
        assert_eq!(
            GqlOverlayResultArtifact::from_bytes(&bytes)
                .expect("full-width overlay ids decode")
                .rows(),
            rows.as_slice()
        );
    }

    #[test]
    fn prepared_decoder_rejects_header_trailing_and_result_mutations() {
        let artifact =
            GqlPreparedResultArtifact::new(&query(), CommitSeq(11), digest(0x31), vec![VId(2)]);
        let bytes = artifact.to_bytes();

        let mut wrong_magic = bytes.clone();
        wrong_magic[0] ^= 1;
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&wrong_magic),
            Err(GqlEvidenceDecodeError::InvalidMagic)
        ));

        let mut wrong_version = bytes.clone();
        wrong_version[9] = 2;
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&wrong_version),
            Err(GqlEvidenceDecodeError::UnsupportedVersion { .. })
        ));

        let mut wrong_kind = bytes.clone();
        wrong_kind[12] = GqlEvidenceArtifactKind::StagedOverlayResult as u8;
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&wrong_kind),
            Err(GqlEvidenceDecodeError::UnexpectedKind { .. })
        ));

        let mut reserved = bytes.clone();
        reserved[13] = 1;
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&reserved),
            Err(GqlEvidenceDecodeError::NonZeroReserved)
        ));

        let mut row = bytes.clone();
        row[128] ^= 1;
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&row),
            Err(GqlEvidenceDecodeError::ResultDigestMismatch)
        ));

        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            GqlPreparedResultArtifact::from_bytes(&trailing),
            Err(GqlEvidenceDecodeError::TrailingBytes { count: 1 })
        ));
    }

    #[test]
    fn prepared_artifact_binds_input_plan_snapshot_and_rows() {
        let query = query();
        let artifact = GqlPreparedResultArtifact::new(
            &query,
            CommitSeq(11),
            digest(0x31),
            vec![VId(2), VId(9)],
        );
        assert!(artifact.verifies_input(&query));
        assert!(artifact.verifies_plan(digest(0x31)));
        assert!(artifact.verifies_rows());
        assert!(!artifact.verifies_plan(digest(0x32)));

        let other = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN a", query.bind())
            .expect("other query binds");
        assert!(!artifact.verifies_input(&other));

        let debug = format!("{artifact:?}");
        assert!(!debug.contains("VId(2)"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn overlay_artifact_round_trips_and_rejects_context_mutations() {
        let query = query();
        let artifact = GqlOverlayResultArtifact::new(
            &query,
            CommitSeq(11),
            digest(0x31),
            digest(0x41),
            vec![VId(2), VId(9)],
        );
        let bytes = artifact.to_bytes();
        assert_eq!(
            GqlOverlayResultArtifact::from_bytes(&bytes).expect("canonical bytes decode"),
            artifact
        );
        assert!(artifact.verifies_input(&query));
        assert!(artifact.verifies_plan(digest(0x31)));
        assert!(artifact.verifies_staged_effect(digest(0x41)));
        assert!(artifact.verifies_rows());
        assert!(!artifact.verifies_staged_effect(digest(0x42)));

        for end in 0..bytes.len() {
            assert!(
                GqlOverlayResultArtifact::from_bytes(&bytes[..end]).is_err(),
                "prefix {end} must refuse"
            );
        }

        let mut row = bytes;
        row[160] ^= 1;
        assert!(matches!(
            GqlOverlayResultArtifact::from_bytes(&row),
            Err(GqlEvidenceDecodeError::ResultDigestMismatch)
        ));
    }
}
