use crate::{
    GqlEvidenceArtifactKind, GqlEvidencePage, GqlEvidencePageError, GqlEvidencePageToken,
    GqlOverlayResultArtifact, GqlPreparedResultArtifact,
};
use fgdb_crypto::Digest;
use fgdb_types::CommitSeq;

/// Explicit lifecycle state for one owned materialized evidence cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlEvidenceCursorState {
    Open,
    Exhausted,
    Closed,
}

/// Deterministic cursor-consumption dimension governed by
/// [`GqlEvidenceCursorLimits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlEvidenceCursorLimitDimension {
    /// Number of successful page calls made by this cursor instance.
    Pages,
    /// Number of rows one page would return.
    PageRows,
    /// Total rows successfully returned by this cursor instance.
    EmittedRows,
}

/// Typed refusal when an owned cursor would exceed its configured consumption
/// policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlEvidenceCursorLimitExceeded {
    pub dimension: GqlEvidenceCursorLimitDimension,
    pub limit: u64,
    pub observed: u64,
}

impl core::fmt::Display for GqlEvidenceCursorLimitExceeded {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "GQL evidence cursor {:?} limit exceeded: observed {}, limit {}",
            self.dimension, self.observed, self.limit
        )
    }
}

impl core::error::Error for GqlEvidenceCursorLimitExceeded {}

/// Deterministic lifetime and per-page limits for one owned materialized-result
/// cursor.
///
/// These limits govern rows returned by the cursor after it has already passed
/// artifact admission and exact replay. They are not query-execution budgets,
/// storage admission, memory accounting, streaming backpressure, or lease
/// policy. A resumed cursor starts a new consumption lifetime at its checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlEvidenceCursorLimits {
    max_pages: Option<u64>,
    max_page_rows: Option<u64>,
    max_emitted_rows: Option<u64>,
}

impl GqlEvidenceCursorLimits {
    /// No cursor-consumption limits.
    pub const UNLIMITED: Self = Self {
        max_pages: None,
        max_page_rows: None,
        max_emitted_rows: None,
    };

    /// Bound successful pages, rows in any one page, and total emitted rows.
    /// Zero is a valid fail-closed bound for every dimension.
    #[must_use]
    pub const fn new(max_pages: u64, max_page_rows: u64, max_emitted_rows: u64) -> Self {
        Self {
            max_pages: Some(max_pages),
            max_page_rows: Some(max_page_rows),
            max_emitted_rows: Some(max_emitted_rows),
        }
    }

    /// Bound successful page count while leaving row dimensions unlimited.
    #[must_use]
    pub const fn pages(max_pages: u64) -> Self {
        Self {
            max_pages: Some(max_pages),
            max_page_rows: None,
            max_emitted_rows: None,
        }
    }

    /// Bound rows in each successful page while leaving lifetime totals
    /// unlimited.
    #[must_use]
    pub const fn page_rows(max_page_rows: u64) -> Self {
        Self {
            max_pages: None,
            max_page_rows: Some(max_page_rows),
            max_emitted_rows: None,
        }
    }

    /// Bound total rows emitted by one cursor instance while leaving other
    /// dimensions unlimited.
    #[must_use]
    pub const fn emitted_rows(max_emitted_rows: u64) -> Self {
        Self {
            max_pages: None,
            max_page_rows: None,
            max_emitted_rows: Some(max_emitted_rows),
        }
    }

    #[must_use]
    pub const fn max_pages(self) -> Option<u64> {
        self.max_pages
    }

    #[must_use]
    pub const fn max_page_rows(self) -> Option<u64> {
        self.max_page_rows
    }

    #[must_use]
    pub const fn max_emitted_rows(self) -> Option<u64> {
        self.max_emitted_rows
    }

    fn check(
        self,
        pages_emitted: u64,
        rows_emitted: u64,
        page_rows: u64,
    ) -> Result<(), GqlEvidenceCursorLimitExceeded> {
        check_limit(
            GqlEvidenceCursorLimitDimension::PageRows,
            self.max_page_rows,
            page_rows,
        )?;
        check_limit(
            GqlEvidenceCursorLimitDimension::Pages,
            self.max_pages,
            pages_emitted.saturating_add(1),
        )?;
        check_limit(
            GqlEvidenceCursorLimitDimension::EmittedRows,
            self.max_emitted_rows,
            rows_emitted.saturating_add(page_rows),
        )
    }
}

impl Default for GqlEvidenceCursorLimits {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

fn check_limit(
    dimension: GqlEvidenceCursorLimitDimension,
    limit: Option<u64>,
    observed: u64,
) -> Result<(), GqlEvidenceCursorLimitExceeded> {
    match limit {
        Some(limit) if observed > limit => Err(GqlEvidenceCursorLimitExceeded {
            dimension,
            limit,
            observed,
        }),
        Some(_) | None => Ok(()),
    }
}

/// Lifecycle, consumption-policy, or page-construction refusal for an owned
/// evidence cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidenceCursorError {
    Closed,
    Exhausted,
    Limit(GqlEvidenceCursorLimitExceeded),
    Page(GqlEvidencePageError),
}

impl core::fmt::Display for GqlEvidenceCursorError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("GQL evidence cursor is closed"),
            Self::Exhausted => formatter.write_str("GQL evidence cursor is exhausted"),
            Self::Limit(source) => core::fmt::Display::fmt(source, formatter),
            Self::Page(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl core::error::Error for GqlEvidenceCursorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Limit(source) => Some(source),
            Self::Page(source) => Some(source),
            Self::Closed | Self::Exhausted => None,
        }
    }
}

enum CursorArtifact {
    Prepared(GqlPreparedResultArtifact),
    Overlay(GqlOverlayResultArtifact),
}

impl CursorArtifact {
    fn kind(&self) -> GqlEvidenceArtifactKind {
        match self {
            Self::Prepared(_) => GqlEvidenceArtifactKind::PreparedResult,
            Self::Overlay(_) => GqlEvidenceArtifactKind::StagedOverlayResult,
        }
    }

    fn sequence(&self) -> CommitSeq {
        match self {
            Self::Prepared(artifact) => artifact.snapshot_seq(),
            Self::Overlay(artifact) => artifact.basis(),
        }
    }

    fn result_digest(&self) -> Digest {
        match self {
            Self::Prepared(artifact) => artifact.result_digest(),
            Self::Overlay(artifact) => artifact.result_digest(),
        }
    }

    fn total_rows(&self) -> u64 {
        let rows = match self {
            Self::Prepared(artifact) => artifact.rows().len(),
            Self::Overlay(artifact) => artifact.rows().len(),
        };
        u64::try_from(rows).unwrap_or(u64::MAX)
    }

    fn page(
        &self,
        page_size: u64,
        after: Option<&GqlEvidencePageToken>,
    ) -> Result<GqlEvidencePage, GqlEvidencePageError> {
        match self {
            Self::Prepared(artifact) => artifact.page(page_size, after),
            Self::Overlay(artifact) => artifact.page(page_size, after),
        }
    }
}

enum CursorLifecycle {
    /// The retained artifact is boxed so the two unit variants do not pay for
    /// the open variant's size (clippy `large_enum_variant`); a cursor is a
    /// single owned object, so the one allocation is not on any hot path.
    Open {
        artifact: Box<CursorArtifact>,
        next_token: Option<GqlEvidencePageToken>,
    },
    Exhausted,
    Closed,
}

/// A linear, owned cursor over one already materialized evidence artifact.
///
/// The cursor avoids decoding and replaying the same artifact for every page:
/// callers first obtain or audit an artifact, construct the cursor once, and
/// then advance monotonically through its exact ordered rows. Reaching the
/// terminal page releases the retained artifact and enters
/// [`GqlEvidenceCursorState::Exhausted`]; [`GqlEvidenceCursor::close`] releases
/// it early and enters [`GqlEvidenceCursorState::Closed`].
///
/// Direct constructors do not audit their artifact. Raw or untrusted bytes
/// should enter through the `fgdb` product adapters that audit and replay before
/// returning this cursor.
///
/// Cursor limits apply only to pages emitted by this cursor instance. They do
/// not attest or constrain the query execution that produced the materialized
/// artifact. This remains distinct from a streaming executor, server cursor,
/// lease, authorization capability, backpressure protocol, or larger-than-
/// memory result path.
#[must_use = "an evidence cursor has no effect until it is advanced or closed"]
pub struct GqlEvidenceCursor {
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    total_rows: u64,
    position: u64,
    limits: GqlEvidenceCursorLimits,
    pages_emitted: u64,
    rows_emitted: u64,
    lifecycle: CursorLifecycle,
}

impl GqlEvidenceCursor {
    /// Construct an unlimited cursor over an already materialized durable-result
    /// artifact. This constructor performs no database replay.
    #[must_use]
    pub fn from_prepared_artifact(artifact: GqlPreparedResultArtifact) -> Self {
        Self::from_prepared_artifact_with_limits(artifact, GqlEvidenceCursorLimits::UNLIMITED)
    }

    /// Construct a consumption-bounded cursor over an already materialized
    /// durable-result artifact. This constructor performs no database replay.
    #[must_use]
    pub fn from_prepared_artifact_with_limits(
        artifact: GqlPreparedResultArtifact,
        limits: GqlEvidenceCursorLimits,
    ) -> Self {
        Self::new(CursorArtifact::Prepared(artifact), limits)
    }

    /// Construct an unlimited cursor over an already materialized staged-result
    /// artifact. This constructor performs no transaction audit.
    #[must_use]
    pub fn from_overlay_artifact(artifact: GqlOverlayResultArtifact) -> Self {
        Self::from_overlay_artifact_with_limits(artifact, GqlEvidenceCursorLimits::UNLIMITED)
    }

    /// Construct a consumption-bounded cursor over an already materialized
    /// staged-result artifact. This constructor performs no transaction audit.
    #[must_use]
    pub fn from_overlay_artifact_with_limits(
        artifact: GqlOverlayResultArtifact,
        limits: GqlEvidenceCursorLimits,
    ) -> Self {
        Self::new(CursorArtifact::Overlay(artifact), limits)
    }

    /// Resume an unlimited materialized durable result from a result-bound
    /// checkpoint. This validates the token but performs no database replay.
    pub fn resume_prepared_artifact(
        artifact: GqlPreparedResultArtifact,
        checkpoint: &GqlEvidencePageToken,
    ) -> Result<Self, GqlEvidencePageError> {
        Self::resume_prepared_artifact_with_limits(
            artifact,
            checkpoint,
            GqlEvidenceCursorLimits::UNLIMITED,
        )
    }

    /// Resume a consumption-bounded durable result from a result-bound
    /// checkpoint. The new cursor's consumption counters start at zero.
    pub fn resume_prepared_artifact_with_limits(
        artifact: GqlPreparedResultArtifact,
        checkpoint: &GqlEvidencePageToken,
        limits: GqlEvidenceCursorLimits,
    ) -> Result<Self, GqlEvidencePageError> {
        Self::resume(CursorArtifact::Prepared(artifact), checkpoint, limits)
    }

    /// Resume an unlimited materialized staged result from a result-bound
    /// checkpoint. This validates the token but performs no transaction audit.
    pub fn resume_overlay_artifact(
        artifact: GqlOverlayResultArtifact,
        checkpoint: &GqlEvidencePageToken,
    ) -> Result<Self, GqlEvidencePageError> {
        Self::resume_overlay_artifact_with_limits(
            artifact,
            checkpoint,
            GqlEvidenceCursorLimits::UNLIMITED,
        )
    }

    /// Resume a consumption-bounded staged result from a result-bound
    /// checkpoint. The new cursor's consumption counters start at zero.
    pub fn resume_overlay_artifact_with_limits(
        artifact: GqlOverlayResultArtifact,
        checkpoint: &GqlEvidencePageToken,
        limits: GqlEvidenceCursorLimits,
    ) -> Result<Self, GqlEvidencePageError> {
        Self::resume(CursorArtifact::Overlay(artifact), checkpoint, limits)
    }

    fn new(artifact: CursorArtifact, limits: GqlEvidenceCursorLimits) -> Self {
        let kind = artifact.kind();
        let sequence = artifact.sequence();
        let result_digest = artifact.result_digest();
        let total_rows = artifact.total_rows();
        Self {
            kind,
            sequence,
            result_digest,
            total_rows,
            position: 0,
            limits,
            pages_emitted: 0,
            rows_emitted: 0,
            lifecycle: CursorLifecycle::Open {
                artifact: Box::new(artifact),
                next_token: None,
            },
        }
    }

    fn resume(
        artifact: CursorArtifact,
        checkpoint: &GqlEvidencePageToken,
        limits: GqlEvidenceCursorLimits,
    ) -> Result<Self, GqlEvidencePageError> {
        artifact.page(1, Some(checkpoint))?;
        let kind = artifact.kind();
        let sequence = artifact.sequence();
        let result_digest = artifact.result_digest();
        let total_rows = artifact.total_rows();
        let position = checkpoint.next_offset();
        let lifecycle = if position == total_rows {
            CursorLifecycle::Exhausted
        } else {
            CursorLifecycle::Open {
                artifact: Box::new(artifact),
                next_token: Some(*checkpoint),
            }
        };
        Ok(Self {
            kind,
            sequence,
            result_digest,
            total_rows,
            position,
            limits,
            pages_emitted: 0,
            rows_emitted: 0,
            lifecycle,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> GqlEvidenceArtifactKind {
        self.kind
    }

    /// Snapshot sequence for durable results; transaction basis for staged
    /// results.
    #[must_use]
    pub const fn sequence(&self) -> CommitSeq {
        self.sequence
    }

    #[must_use]
    pub const fn result_digest(&self) -> Digest {
        self.result_digest
    }

    #[must_use]
    pub const fn total_rows(&self) -> u64 {
        self.total_rows
    }

    /// Offset of the next unread row in the complete certified result.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    #[must_use]
    pub const fn remaining_rows(&self) -> u64 {
        self.total_rows.saturating_sub(self.position)
    }

    #[must_use]
    pub const fn limits(&self) -> GqlEvidenceCursorLimits {
        self.limits
    }

    #[must_use]
    pub const fn pages_emitted(&self) -> u64 {
        self.pages_emitted
    }

    #[must_use]
    pub const fn rows_emitted(&self) -> u64 {
        self.rows_emitted
    }

    #[must_use]
    pub fn remaining_page_budget(&self) -> Option<u64> {
        self.limits
            .max_pages()
            .map(|limit| limit.saturating_sub(self.pages_emitted))
    }

    #[must_use]
    pub fn remaining_emitted_row_budget(&self) -> Option<u64> {
        self.limits
            .max_emitted_rows()
            .map(|limit| limit.saturating_sub(self.rows_emitted))
    }

    #[must_use]
    pub fn state(&self) -> GqlEvidenceCursorState {
        match &self.lifecycle {
            CursorLifecycle::Open { .. } => GqlEvidenceCursorState::Open,
            CursorLifecycle::Exhausted => GqlEvidenceCursorState::Exhausted,
            CursorLifecycle::Closed => GqlEvidenceCursorState::Closed,
        }
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(&self.lifecycle, CursorLifecycle::Open { .. })
    }

    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        matches!(&self.lifecycle, CursorLifecycle::Exhausted)
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(&self.lifecycle, CursorLifecycle::Closed)
    }

    /// Portable checkpoint for the next unread row while more rows remain.
    #[must_use]
    pub fn checkpoint_token(&self) -> Option<GqlEvidencePageToken> {
        match &self.lifecycle {
            CursorLifecycle::Open { next_token, .. } => *next_token,
            CursorLifecycle::Exhausted | CursorLifecycle::Closed => None,
        }
    }

    /// Advance once and return the next contiguous page.
    ///
    /// Request syntax and cursor-consumption limits are checked before slicing.
    /// A successful terminal page transitions to `Exhausted` and releases the
    /// retained artifact. Every refusal leaves position, counters, and lifecycle
    /// unchanged.
    pub fn next_page(&mut self, page_size: u64) -> Result<GqlEvidencePage, GqlEvidenceCursorError> {
        match &self.lifecycle {
            CursorLifecycle::Exhausted => {
                return Err(GqlEvidenceCursorError::Exhausted);
            }
            CursorLifecycle::Closed => {
                return Err(GqlEvidenceCursorError::Closed);
            }
            CursorLifecycle::Open { .. } => {}
        }
        if page_size == 0 {
            return Err(GqlEvidenceCursorError::Page(
                GqlEvidencePageError::ZeroPageSize,
            ));
        }

        let page_rows = self.remaining_rows().min(page_size);
        self.limits
            .check(self.pages_emitted, self.rows_emitted, page_rows)
            .map_err(GqlEvidenceCursorError::Limit)?;

        let page = match &mut self.lifecycle {
            CursorLifecycle::Open {
                artifact,
                next_token,
            } => {
                let page = artifact
                    .page(page_size, next_token.as_ref())
                    .map_err(GqlEvidenceCursorError::Page)?;
                *next_token = page.next_token().copied();
                page
            }
            CursorLifecycle::Exhausted | CursorLifecycle::Closed => {
                unreachable!("cursor lifecycle was checked before page construction")
            }
        };

        self.position = page.end_offset();
        self.pages_emitted = self.pages_emitted.saturating_add(1);
        self.rows_emitted = self
            .rows_emitted
            .saturating_add(u64::try_from(page.rows().len()).unwrap_or(u64::MAX));
        if page.is_terminal() {
            self.lifecycle = CursorLifecycle::Exhausted;
        }
        Ok(page)
    }

    /// Release the retained artifact and permanently close the cursor.
    ///
    /// Returns `true` only for the first transition into `Closed`.
    pub fn close(&mut self) -> bool {
        if self.is_closed() {
            return false;
        }
        self.lifecycle = CursorLifecycle::Closed;
        true
    }
}

impl core::fmt::Debug for GqlEvidenceCursor {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GqlEvidenceCursor")
            .field("kind", &self.kind)
            .field("sequence", &self.sequence)
            .field("result_digest", &self.result_digest)
            .field("state", &self.state())
            .field("position", &self.position)
            .field("total_rows", &self.total_rows)
            .field("remaining_rows", &self.remaining_rows())
            .field("limits", &self.limits)
            .field("pages_emitted", &self.pages_emitted)
            .field("rows_emitted", &self.rows_emitted)
            .field("remaining_page_budget", &self.remaining_page_budget())
            .field(
                "remaining_emitted_row_budget",
                &self.remaining_emitted_row_budget(),
            )
            .field("checkpoint_token", &self.checkpoint_token())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GqlEvidenceCursor, GqlEvidenceCursorError, GqlEvidenceCursorLimitDimension,
        GqlEvidenceCursorLimits, GqlEvidenceCursorState,
    };
    use crate::{
        GQL_EVIDENCE_PAGE_TOKEN_LEN, GqlEvidenceArtifactKind, GqlEvidencePageToken,
        GqlOverlayResultArtifact, GqlPreparedResultArtifact, PreparedGqlQuery, RelationBind,
    };
    use fgdb_crypto::{Digest, Hasher};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CommitSeq, VId};

    const STATEMENT: &str = "MATCH (a)-[:R]->(b) RETURN b";
    const TOKEN_HEADER_LEN: usize = 8 + 2 + 2 + 1 + 3;

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

    fn valid_token(
        kind: GqlEvidenceArtifactKind,
        sequence: CommitSeq,
        result_digest: Digest,
        next_offset: u64,
    ) -> GqlEvidencePageToken {
        let kind_tag = match kind {
            GqlEvidenceArtifactKind::PreparedResult => 1,
            GqlEvidenceArtifactKind::StagedOverlayResult => 2,
        };
        let mut hasher = Hasher::new();
        hasher.update(b"fgdb:gql-evidence-page-token:v1");
        hasher.update(&[kind_tag]);
        hasher.update(&sequence.0.to_be_bytes());
        hasher.update(&result_digest.0);
        hasher.update(&next_offset.to_be_bytes());
        let checksum = hasher.finalize();

        let mut bytes = [0_u8; GQL_EVIDENCE_PAGE_TOKEN_LEN];
        bytes[..8].copy_from_slice(b"FGQPAGE1");
        bytes[8..10].copy_from_slice(&1_u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&0_u16.to_be_bytes());
        bytes[12] = kind_tag;
        bytes[TOKEN_HEADER_LEN..TOKEN_HEADER_LEN + 8].copy_from_slice(&sequence.0.to_be_bytes());
        let result_start = TOKEN_HEADER_LEN + 8;
        bytes[result_start..result_start + 32].copy_from_slice(&result_digest.0);
        let offset_start = result_start + 32;
        bytes[offset_start..offset_start + 8].copy_from_slice(&next_offset.to_be_bytes());
        bytes[offset_start + 8..].copy_from_slice(&checksum.0);
        GqlEvidencePageToken::from_bytes(&bytes).expect("independent valid token fixture decodes")
    }

    #[test]
    fn cursor_advances_monotonically_and_exhausts_once() {
        let mut cursor = GqlEvidenceCursor::from_prepared_artifact(prepared(vec![
            VId(1),
            VId(2),
            VId(3),
            VId(4),
            VId(5),
        ]));
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Open);
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.total_rows(), 5);
        assert_eq!(cursor.remaining_rows(), 5);
        assert_eq!(cursor.pages_emitted(), 0);
        assert_eq!(cursor.rows_emitted(), 0);
        assert!(cursor.checkpoint_token().is_none());

        let first = cursor.next_page(2).expect("first page succeeds");
        assert_eq!(first.rows(), &[VId(1), VId(2)]);
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.pages_emitted(), 1);
        assert_eq!(cursor.rows_emitted(), 2);
        let checkpoint = cursor.checkpoint_token().expect("more rows remain");
        assert_eq!(checkpoint.next_offset(), 2);

        let second = cursor.next_page(2).expect("second page succeeds");
        assert_eq!(second.rows(), &[VId(3), VId(4)]);
        assert_eq!(cursor.position(), 4);

        let terminal = cursor.next_page(2).expect("terminal page succeeds");
        assert_eq!(terminal.rows(), &[VId(5)]);
        assert!(terminal.is_terminal());
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Exhausted);
        assert_eq!(cursor.position(), 5);
        assert_eq!(cursor.pages_emitted(), 3);
        assert_eq!(cursor.rows_emitted(), 5);
        assert!(cursor.checkpoint_token().is_none());
        assert!(matches!(
            cursor.next_page(2),
            Err(GqlEvidenceCursorError::Exhausted)
        ));

        assert!(cursor.close());
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Closed);
        assert!(!cursor.close());
        assert!(matches!(
            cursor.next_page(2),
            Err(GqlEvidenceCursorError::Closed)
        ));
    }

    #[test]
    fn cursor_limits_are_exact_and_refusals_do_not_advance() {
        let limits = GqlEvidenceCursorLimits::new(2, 2, 3);
        let mut cursor = GqlEvidenceCursor::from_prepared_artifact_with_limits(
            prepared(vec![VId(1), VId(2), VId(3), VId(4), VId(5)]),
            limits,
        );
        assert_eq!(
            cursor.next_page(2).expect("exact page bound").rows(),
            &[VId(1), VId(2)]
        );

        let row_refusal = cursor
            .next_page(2)
            .expect_err("lifetime row total would exceed three");
        assert!(matches!(
            row_refusal,
            GqlEvidenceCursorError::Limit(exceeded)
                if exceeded.dimension
                    == GqlEvidenceCursorLimitDimension::EmittedRows
                    && exceeded.limit == 3
                    && exceeded.observed == 4
        ));
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.pages_emitted(), 1);
        assert_eq!(cursor.rows_emitted(), 2);

        assert_eq!(
            cursor.next_page(1).expect("exact lifetime total").rows(),
            &[VId(3)]
        );
        assert_eq!(cursor.remaining_page_budget(), Some(0));
        assert_eq!(cursor.remaining_emitted_row_budget(), Some(0));
        let page_refusal = cursor
            .next_page(1)
            .expect_err("third successful page is forbidden");
        assert!(matches!(
            page_refusal,
            GqlEvidenceCursorError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceCursorLimitDimension::Pages
                    && exceeded.limit == 2
                    && exceeded.observed == 3
        ));
        assert_eq!(cursor.position(), 3);
        assert_eq!(cursor.pages_emitted(), 2);
        assert_eq!(cursor.rows_emitted(), 3);

        let mut per_page = GqlEvidenceCursor::from_prepared_artifact_with_limits(
            prepared(vec![VId(1), VId(2)]),
            GqlEvidenceCursorLimits::page_rows(1),
        );
        let per_page_refusal = per_page
            .next_page(2)
            .expect_err("two returned rows exceed per-page limit one");
        assert!(matches!(
            per_page_refusal,
            GqlEvidenceCursorError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceCursorLimitDimension::PageRows
                    && exceeded.limit == 1
                    && exceeded.observed == 2
        ));
        assert_eq!(per_page.position(), 0);
        assert_eq!(per_page.pages_emitted(), 0);
    }

    #[test]
    fn checkpoint_resume_validates_position_and_starts_new_budget_lifetime() {
        let artifact = prepared(vec![VId(1), VId(2), VId(3), VId(4)]);
        let checkpoint = *artifact
            .page(2, None)
            .expect("first page succeeds")
            .next_token()
            .expect("checkpoint exists");
        let mut cursor = GqlEvidenceCursor::resume_prepared_artifact_with_limits(
            artifact.clone(),
            &checkpoint,
            GqlEvidenceCursorLimits::new(1, 2, 2),
        )
        .expect("matching checkpoint resumes");
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.pages_emitted(), 0);
        assert_eq!(cursor.rows_emitted(), 0);
        assert_eq!(
            cursor.next_page(8).expect("remaining page succeeds").rows(),
            &[VId(3), VId(4)]
        );
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.pages_emitted(), 1);
        assert_eq!(cursor.rows_emitted(), 2);

        let wrong = overlay(vec![VId(1), VId(2), VId(3), VId(4)]);
        assert!(GqlEvidenceCursor::resume_overlay_artifact(wrong, &checkpoint).is_err());
    }

    #[test]
    fn valid_exact_end_checkpoint_resumes_exhausted() {
        let artifact = prepared(vec![VId(1), VId(2)]);
        let checkpoint = valid_token(
            GqlEvidenceArtifactKind::PreparedResult,
            artifact.snapshot_seq(),
            artifact.result_digest(),
            2,
        );
        let cursor = GqlEvidenceCursor::resume_prepared_artifact(artifact, &checkpoint)
            .expect("valid exact-end checkpoint resumes");
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.remaining_rows(), 0);
        assert!(cursor.checkpoint_token().is_none());
        assert_eq!(cursor.pages_emitted(), 0);
        assert_eq!(cursor.rows_emitted(), 0);
    }

    #[test]
    fn zero_page_size_does_not_advance_or_consume_budget() {
        let mut cursor = GqlEvidenceCursor::from_prepared_artifact_with_limits(
            prepared(vec![VId(1)]),
            GqlEvidenceCursorLimits::new(1, 1, 1),
        );
        assert!(matches!(
            cursor.next_page(0),
            Err(GqlEvidenceCursorError::Page(
                crate::GqlEvidencePageError::ZeroPageSize
            ))
        ));
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Open);
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.pages_emitted(), 0);
        assert_eq!(cursor.rows_emitted(), 0);
    }

    #[test]
    fn empty_result_yields_one_terminal_page_then_exhaustion() {
        let mut cursor = GqlEvidenceCursor::from_prepared_artifact_with_limits(
            prepared(Vec::new()),
            GqlEvidenceCursorLimits::new(1, 0, 0),
        );
        let page = cursor.next_page(8).expect("empty terminal page succeeds");
        assert!(page.rows().is_empty());
        assert!(page.is_terminal());
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.pages_emitted(), 1);
        assert_eq!(cursor.rows_emitted(), 0);
        assert!(matches!(
            cursor.next_page(8),
            Err(GqlEvidenceCursorError::Exhausted)
        ));
    }

    #[test]
    fn overlay_cursor_preserves_kind_and_redacts_rows() {
        let mut cursor = GqlEvidenceCursor::from_overlay_artifact(overlay(vec![VId(0xfeed_face)]));
        assert_eq!(cursor.kind(), GqlEvidenceArtifactKind::StagedOverlayResult);
        let debug = format!("{cursor:?}");
        assert!(!debug.contains("4277009102"));
        assert!(cursor.next_page(1).expect("page succeeds").is_terminal());
    }
}
