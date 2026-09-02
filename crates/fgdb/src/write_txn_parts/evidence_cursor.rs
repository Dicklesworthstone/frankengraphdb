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
}
