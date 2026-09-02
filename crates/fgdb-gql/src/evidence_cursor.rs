use crate::{
    GqlEvidenceArtifactKind, GqlEvidencePage, GqlEvidencePageError,
    GqlEvidencePageToken, GqlOverlayResultArtifact, GqlPreparedResultArtifact,
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

/// Lifecycle or page-construction refusal for an owned evidence cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlEvidenceCursorError {
    Closed,
    Exhausted,
    Page(GqlEvidencePageError),
}

impl core::fmt::Display for GqlEvidenceCursorError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("GQL evidence cursor is closed"),
            Self::Exhausted => formatter.write_str("GQL evidence cursor is exhausted"),
            Self::Page(source) => core::fmt::Display::fmt(source, formatter),
        }
    }
}

impl core::error::Error for GqlEvidenceCursorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
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
    Open {
        artifact: CursorArtifact,
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
/// terminal page releases the retained artifact and enters [`Exhausted`];
/// [`GqlEvidenceCursor::close`] releases it early and enters [`Closed`].
///
/// Direct constructors do not audit their artifact. Raw or untrusted bytes
/// should enter through the `fgdb` product adapters that audit and replay before
/// returning this cursor.
///
/// This is not a streaming executor, server cursor, lease, authorization
/// capability, backpressure protocol, or larger-than-memory result path.
#[must_use = "an evidence cursor has no effect until it is advanced or closed"]
pub struct GqlEvidenceCursor {
    kind: GqlEvidenceArtifactKind,
    sequence: CommitSeq,
    result_digest: Digest,
    total_rows: u64,
    position: u64,
    lifecycle: CursorLifecycle,
}

impl GqlEvidenceCursor {
    /// Construct a cursor over an already materialized durable-result artifact.
    ///
    /// This constructor performs no database replay. Use the corresponding
    /// `fgdb::Database` or `fgdb::EmbeddedReadView` open method for untrusted
    /// artifact bytes.
    #[must_use]
    pub fn from_prepared_artifact(artifact: GqlPreparedResultArtifact) -> Self {
        Self::new(CursorArtifact::Prepared(artifact))
    }

    /// Construct a cursor over an already materialized staged-overlay artifact.
    ///
    /// This constructor performs no transaction audit. Use the corresponding
    /// `fgdb::WriteTxn` open method for untrusted artifact bytes.
    #[must_use]
    pub fn from_overlay_artifact(artifact: GqlOverlayResultArtifact) -> Self {
        Self::new(CursorArtifact::Overlay(artifact))
    }

    fn new(artifact: CursorArtifact) -> Self {
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
            lifecycle: CursorLifecycle::Open {
                artifact,
                next_token: None,
            },
        }
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

    /// Offset of the next unread row.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    #[must_use]
    pub const fn remaining_rows(&self) -> u64 {
        self.total_rows.saturating_sub(self.position)
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
    /// A successful terminal page transitions the cursor to `Exhausted` and
    /// releases the retained full artifact. Refusals leave position and
    /// lifecycle unchanged.
    pub fn next_page(
        &mut self,
        page_size: u64,
    ) -> Result<GqlEvidencePage, GqlEvidenceCursorError> {
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
            CursorLifecycle::Exhausted => {
                return Err(GqlEvidenceCursorError::Exhausted);
            }
            CursorLifecycle::Closed => {
                return Err(GqlEvidenceCursorError::Closed);
            }
        };

        self.position = page.end_offset();
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
            .field("checkpoint_token", &self.checkpoint_token())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GqlEvidenceCursor, GqlEvidenceCursorError, GqlEvidenceCursorState,
    };
    use crate::{
        GqlOverlayResultArtifact, GqlPreparedResultArtifact, PreparedGqlQuery,
        RelationBind,
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
        GqlPreparedResultArtifact::new(
            &query(),
            CommitSeq(11),
            Digest([0x31; 32]),
            rows,
        )
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
        assert!(cursor.checkpoint_token().is_none());

        let first = cursor.next_page(2).expect("first page succeeds");
        assert_eq!(first.rows(), &[VId(1), VId(2)]);
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.remaining_rows(), 3);
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
        assert_eq!(cursor.remaining_rows(), 0);
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
    fn zero_page_size_does_not_advance_or_close() {
        let mut cursor =
            GqlEvidenceCursor::from_prepared_artifact(prepared(vec![VId(1)]));
        assert!(matches!(
            cursor.next_page(0),
            Err(GqlEvidenceCursorError::Page(
                crate::GqlEvidencePageError::ZeroPageSize
            ))
        ));
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Open);
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.remaining_rows(), 1);
    }

    #[test]
    fn empty_result_yields_one_terminal_page_then_exhaustion() {
        let mut cursor =
            GqlEvidenceCursor::from_prepared_artifact(prepared(Vec::new()));
        let page = cursor.next_page(8).expect("empty terminal page succeeds");
        assert!(page.rows().is_empty());
        assert!(page.is_terminal());
        assert!(cursor.is_exhausted());
        assert!(matches!(
            cursor.next_page(8),
            Err(GqlEvidenceCursorError::Exhausted)
        ));
    }

    #[test]
    fn overlay_cursor_preserves_kind_and_redacts_rows() {
        let mut cursor = GqlEvidenceCursor::from_overlay_artifact(overlay(vec![
            VId(0xfeed_face),
        ]));
        assert_eq!(
            cursor.kind(),
            crate::GqlEvidenceArtifactKind::StagedOverlayResult
        );
        let debug = format!("{cursor:?}");
        assert!(!debug.contains("4277009102"));
        assert!(cursor.next_page(1).expect("page succeeds").is_terminal());
    }
}
