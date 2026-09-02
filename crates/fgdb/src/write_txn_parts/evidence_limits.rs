impl<V: Vfs + Clone> Database<V> {
    /// Audit raw prepared-result bytes under the default untrusted-input policy.
    ///
    /// Resource admission happens before the strict format decoder can allocate
    /// the declared row vector. Syntax, identity, execution, and replay
    /// refusals remain nested under the existing audit error vocabulary.
    pub fn audit_untrusted_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit raw prepared-result bytes under caller-supplied resource limits.
    pub fn audit_prepared_query_artifact_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        limits
            .preflight_prepared(bytes)
            .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        self.audit_prepared_query_artifact(query, bytes)
            .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)
    }
}

impl crate::EmbeddedReadView {
    /// Audit raw prepared-result bytes under the default untrusted-input policy
    /// and this view's immutable snapshot authority.
    pub fn audit_untrusted_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        self.audit_prepared_query_artifact_with_limits(
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit raw prepared-result bytes under caller-supplied resource limits
    /// and this view's immutable snapshot authority.
    pub fn audit_prepared_query_artifact_with_limits(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<GqlError>,
    > {
        limits
            .preflight_prepared(bytes)
            .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        self.audit_prepared_query_artifact(query, bytes)
            .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)
    }
}

impl WriteTxn {
    /// Audit raw staged-overlay bytes under the default untrusted-input policy
    /// and this transaction's current basis and staged-effect authority.
    pub fn audit_untrusted_prepared_query_overlay_artifact<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlOverlayResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<WriteTxnError>,
    > {
        self.audit_prepared_query_overlay_artifact_with_limits(
            database,
            query,
            bytes,
            fgdb_gql::GqlEvidenceLimits::DEFAULT_UNTRUSTED,
        )
    }

    /// Audit raw staged-overlay bytes under caller-supplied resource limits and
    /// this transaction's current basis and staged-effect authority.
    pub fn audit_prepared_query_overlay_artifact_with_limits<
        V: Vfs + Clone,
    >(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
    ) -> Result<
        fgdb_gql::GqlOverlayResultArtifact,
        fgdb_gql::GqlEvidenceLimitedAuditError<WriteTxnError>,
    > {
        limits
            .preflight_overlay(bytes)
            .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        self.audit_prepared_query_overlay_artifact(
            database, query, bytes,
        )
        .map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)
    }
}
