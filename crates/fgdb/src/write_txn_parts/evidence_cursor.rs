fn decode_evidence_cursor_checkpoint<E>(
    bytes: &[u8],
) -> Result<
    fgdb_gql::GqlEvidencePageToken,
    fgdb_gql::GqlEvidencePageAuditError<E>,
> {
    fgdb_gql::GqlEvidencePageToken::from_bytes(bytes).map_err(|source| {
        fgdb_gql::GqlEvidencePageAuditError::Page(
            fgdb_gql::GqlEvidencePageError::TokenDecode(source),
        )
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Audit one durable prepared-result envelope under the default untrusted
    /// policy and open a linear owned cursor over the reproduced exact result.
    ///
    /// Audit and historical replay happen once at cursor creation. Subsequent
    /// pages advance over the owned materialized artifact without reopening the
    /// snapshot or re-executing the query.
    pub fn open_untrusted_prepared_query_artifact_cursor(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.open_prepared_query_artifact_cursor_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit one durable prepared-result envelope under caller-supplied
    /// admission limits and open an owned cursor over the exact replayed rows.
    pub fn open_prepared_query_artifact_cursor_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map(fgdb_gql::GqlEvidenceCursor::from_prepared_artifact)
    }

    /// Strictly decode a checkpoint, audit and historically replay the durable
    /// artifact once, then resume a linear cursor at the token's exact offset.
    pub fn resume_untrusted_prepared_query_artifact_cursor(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        self.resume_prepared_query_artifact_cursor_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            checkpoint,
        )
    }

    /// Resume a durable cursor under caller-supplied artifact admission limits.
    /// Token syntax is checked before artifact replay; token-to-result binding is
    /// checked only after the artifact has passed its complete audit.
    pub fn resume_prepared_query_artifact_cursor_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        let checkpoint = decode_evidence_cursor_checkpoint(checkpoint)?;
        let artifact = self
            .audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        fgdb_gql::GqlEvidenceCursor::resume_prepared_artifact(
            artifact,
            &checkpoint,
        )
        .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}

impl crate::EmbeddedReadView {
    /// Audit one durable prepared-result envelope under the default untrusted
    /// policy and open a linear cursor bound to this view's snapshot authority.
    pub fn open_untrusted_prepared_query_artifact_cursor(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.open_prepared_query_artifact_cursor_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit one durable prepared-result envelope under caller-supplied
    /// admission limits and open a cursor over the exact replayed rows.
    pub fn open_prepared_query_artifact_cursor_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map(fgdb_gql::GqlEvidenceCursor::from_prepared_artifact)
    }

    /// Strictly decode a checkpoint, audit against this immutable view once,
    /// and resume at the token's exact result-bound offset.
    pub fn resume_untrusted_prepared_query_artifact_cursor(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        self.resume_prepared_query_artifact_cursor_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            checkpoint,
        )
    }

    /// Resume against this immutable view under caller-supplied admission
    /// limits. A sequence outside the retained view preserves the existing audit
    /// refusal before token-to-result binding.
    pub fn resume_prepared_query_artifact_cursor_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        let checkpoint = decode_evidence_cursor_checkpoint(checkpoint)?;
        let artifact = self
            .audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        fgdb_gql::GqlEvidenceCursor::resume_prepared_artifact(
            artifact,
            &checkpoint,
        )
        .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}

impl WriteTxn {
    /// Audit one staged-overlay result envelope under the default untrusted
    /// policy and open an owned cursor over the exact result at open time.
    ///
    /// The returned cursor owns that audited materialized result. Later writes
    /// to this transaction do not mutate or revalidate the already-open cursor;
    /// opening another cursor from the old bytes after such a write still
    /// refuses through the staged-effect audit.
    pub fn open_untrusted_prepared_query_overlay_artifact_cursor<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<WriteTxnError>,
    > {
        self.open_prepared_query_overlay_artifact_cursor_with_limits(
            database,
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit one staged-overlay result envelope under caller-supplied admission
    /// limits and open an owned cursor over the exact result at open time.
    pub fn open_prepared_query_overlay_artifact_cursor_with_limits<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidenceLimitedAuditError<WriteTxnError>,
    > {
        self.audit_prepared_query_overlay_artifact_with_limits(
            database, query, bytes, limits,
        )
        .map(fgdb_gql::GqlEvidenceCursor::from_overlay_artifact)
    }

    /// Strictly decode a checkpoint, audit the staged artifact against this
    /// transaction's current basis and canonical staged effect once, then resume
    /// a cursor at the token's exact offset.
    pub fn resume_untrusted_prepared_query_overlay_artifact_cursor<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<WriteTxnError>,
    > {
        self.resume_prepared_query_overlay_artifact_cursor_with_limits(
            database,
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            checkpoint,
        )
    }

    /// Resume a staged cursor under caller-supplied artifact limits. Token syntax
    /// is checked before overlay audit; staged-effect and row replay still precede
    /// token-to-result binding.
    pub fn resume_prepared_query_overlay_artifact_cursor_with_limits<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        checkpoint: &[u8],
    ) -> Result<
        fgdb_gql::GqlEvidenceCursor,
        fgdb_gql::GqlEvidencePageAuditError<WriteTxnError>,
    > {
        let checkpoint = decode_evidence_cursor_checkpoint(checkpoint)?;
        let artifact = self
            .audit_prepared_query_overlay_artifact_with_limits(
                database, query, bytes, limits,
            )
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        fgdb_gql::GqlEvidenceCursor::resume_overlay_artifact(
            artifact,
            &checkpoint,
        )
        .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}
