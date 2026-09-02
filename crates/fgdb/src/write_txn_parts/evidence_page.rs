impl<V: Vfs + Clone> Database<V> {
    /// Resource-safe audit plus deterministic paging for one durable prepared-
    /// result envelope under the default untrusted-input policy.
    ///
    /// The complete artifact is decoded and historically replayed before the
    /// requested slice is returned. This is not a streaming cursor.
    pub fn audit_untrusted_prepared_query_artifact_page(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_page_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            page_size,
            after,
        )
    }

    /// Resource-safe audit plus deterministic paging under caller-supplied
    /// artifact limits.
    pub fn audit_prepared_query_artifact_page_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        let artifact = self
            .audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        artifact
            .page_from_token_bytes(page_size, after)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}

impl crate::EmbeddedReadView {
    /// Resource-safe audit plus deterministic paging against this immutable
    /// view's snapshot authority.
    pub fn audit_untrusted_prepared_query_artifact_page(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_page_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            page_size,
            after,
        )
    }

    /// Resource-safe audit plus deterministic paging against this immutable
    /// view under caller-supplied artifact limits.
    pub fn audit_prepared_query_artifact_page_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<GqlError>,
    > {
        let artifact = self
            .audit_prepared_query_artifact_with_limits(query, bytes, limits)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        artifact
            .page_from_token_bytes(page_size, after)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}

impl WriteTxn {
    /// Resource-safe staged-overlay audit plus deterministic paging under the
    /// default untrusted-input policy.
    ///
    /// The entire artifact is decoded and the current overlay is re-executed
    /// before paging. A later staged effect therefore refuses before any page is
    /// returned.
    pub fn audit_untrusted_prepared_query_overlay_artifact_page<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<WriteTxnError>,
    > {
        self.audit_prepared_query_overlay_artifact_page_with_limits(
            database,
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
            page_size,
            after,
        )
    }

    /// Resource-safe staged-overlay audit plus deterministic paging under
    /// caller-supplied artifact limits.
    pub fn audit_prepared_query_overlay_artifact_page_with_limits<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        page_size: u64,
        after: Option<&[u8]>,
    ) -> Result<
        fgdb_gql::GqlEvidencePage,
        fgdb_gql::GqlEvidencePageAuditError<WriteTxnError>,
    > {
        let artifact = self
            .audit_prepared_query_overlay_artifact_with_limits(
                database, query, bytes, limits,
            )
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Audit)?;
        artifact
            .page_from_token_bytes(page_size, after)
            .map_err(fgdb_gql::GqlEvidencePageAuditError::Page)
    }
}
