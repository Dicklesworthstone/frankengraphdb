/// One durable audit contract, with the replay strategy supplied by the public
/// adapter. Governed audit must not call ordinary audit and then replay again:
/// that would perform unbounded work before enforcing its execution policy.
fn audit_durable_with<E>(
    query: &fgdb_gql::PreparedGqlQuery,
    bytes: &[u8],
    replay: impl FnOnce(CommitSeq) -> Result<Vec<VId>, E>,
) -> Result<fgdb_gql::GqlPreparedResultArtifact, fgdb_gql::GqlEvidenceAuditError<E>> {
    let artifact = fgdb_gql::GqlPreparedResultArtifact::from_bytes(bytes)
        .map_err(fgdb_gql::GqlEvidenceAuditError::Decode)?;
    if !artifact.verifies_input(query) {
        return Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch);
    }
    let plan_certificate = crate::gql_cert::certify(query.plan(), artifact.snapshot_seq());
    if !artifact.verifies_plan(plan_certificate.digest) {
        return Err(fgdb_gql::GqlEvidenceAuditError::PlanMismatch);
    }
    if !plan_certificate.verifies_result_digest(artifact.rows(), artifact.result_digest()) {
        return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
    }
    let rows = replay(artifact.snapshot_seq()).map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
    if rows.as_slice() != artifact.rows() {
        return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
    }
    Ok(artifact)
}

/// Preserve the existing overlay refusal order and certificate authority while
/// changing only how the final replay is executed. Lazy identity getters keep
/// lifecycle failures in the same position as ordinary audit's prior behavior.
fn audit_overlay_with<E>(
    query: &fgdb_gql::PreparedGqlQuery,
    bytes: &[u8],
    basis: CommitSeq,
    plan_digest: impl FnOnce() -> Result<fgdb_crypto::Digest, E>,
    staged_digest: impl FnOnce() -> Result<fgdb_crypto::Digest, E>,
    replay: impl FnOnce() -> Result<Vec<VId>, E>,
) -> Result<fgdb_gql::GqlOverlayResultArtifact, fgdb_gql::GqlEvidenceAuditError<E>> {
    let artifact = fgdb_gql::GqlOverlayResultArtifact::from_bytes(bytes)
        .map_err(fgdb_gql::GqlEvidenceAuditError::Decode)?;
    if artifact.basis() != basis {
        return Err(fgdb_gql::GqlEvidenceAuditError::SnapshotMismatch);
    }
    if !artifact.verifies_input(query) {
        return Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch);
    }
    let digest = plan_digest().map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
    if !artifact.verifies_plan(digest) {
        return Err(fgdb_gql::GqlEvidenceAuditError::PlanMismatch);
    }
    let staged = staged_digest().map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
    if !artifact.verifies_staged_effect(staged) {
        return Err(fgdb_gql::GqlEvidenceAuditError::StagedEffectMismatch);
    }
    let rows = replay().map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
    if rows.as_slice() != artifact.rows() {
        return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
    }
    Ok(artifact)
}

fn governed_audit_execution_error<E>(
    error: fgdb_gql::GqlQueryError<E, Box<asupersync::error::Error>>,
) -> fgdb_gql::GqlEvidenceLimitedAuditError<
    fgdb_gql::GqlQueryError<E, Box<asupersync::error::Error>>,
> {
    fgdb_gql::GqlEvidenceLimitedAuditError::Audit(
        fgdb_gql::GqlEvidenceAuditError::Execution(error),
    )
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute one coherent prepared definition at the live frontier and return
    /// a canonical self-contained evidence envelope.
    ///
    /// The v1 envelope is an unreleased application artifact. It is not a
    /// Chronicle object, FGP frame, or compatibility commitment.
    pub fn execute_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_query_artifact_at(query, as_of)
    }

    /// Execute one coherent prepared definition at an exact retained sequence
    /// and package its exact inputs, plan identity, and ordered rows.
    pub fn execute_prepared_query_artifact_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, GqlError> {
        let rows = self.execute_prepared_query_at(query, as_of)?;
        let plan_certificate = crate::gql_cert::certify(query.plan(), as_of);
        Ok(fgdb_gql::GqlPreparedResultArtifact::new(
            query, as_of, plan_certificate.digest, rows,
        ))
    }

    /// Decode, verify, and reproduce the exact retained historical result.
    /// Format, input, plan, execution and row mismatches remain distinct.
    pub fn audit_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, fgdb_gql::GqlEvidenceAuditError<GqlError>> {
        audit_durable_with(query, bytes, |as_of| self.execute_prepared_query_at(query, as_of))
    }

    /// Admit untrusted artifact bytes under `limits`, then replay once under
    /// all four query-policy dimensions and QueryCx interruption. Identity and
    /// result checks are identical to ordinary audit. No partially audited
    /// artifact escapes when replay fails. This does not preempt source-table
    /// materialization or turn the artifact into an authorization credential.
    pub fn audit_prepared_query_artifact_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, fgdb_gql::GqlEvidenceLimitedAuditError<
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    >> {
        self.ensure_readable().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Source(GqlError::Read(source)),
        ))?;
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        limits.preflight_prepared(bytes).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        let artifact = audit_durable_with(query, bytes, |as_of| {
            self.execute_prepared_query_governed_at(cx, query, as_of, policy).map(|run| run.value)
        }).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)?;
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        Ok(artifact)
    }
}

impl crate::EmbeddedReadView {
    /// Execute one coherent prepared definition at this view's pinned frontier
    /// and return a canonical evidence envelope.
    pub fn execute_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, GqlError> {
        self.execute_prepared_query_artifact_at(query, self.frontier())
    }

    /// Execute at a sequence retained by this immutable view.
    pub fn execute_prepared_query_artifact_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, GqlError> {
        let rows = self.execute_prepared_query_at(query, as_of)?;
        let plan_certificate = crate::gql_cert::certify(query.plan(), as_of);
        Ok(fgdb_gql::GqlPreparedResultArtifact::new(
            query, as_of, plan_certificate.digest, rows,
        ))
    }

    /// Audit against this immutable view without observing later generations.
    pub fn audit_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, fgdb_gql::GqlEvidenceAuditError<GqlError>> {
        audit_durable_with(query, bytes, |as_of| self.execute_prepared_query_at(query, as_of))
    }

    /// The same bounded artifact replay at this view's immutable generation.
    pub fn audit_prepared_query_artifact_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, fgdb_gql::GqlEvidenceLimitedAuditError<
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    >> {
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        limits.preflight_prepared(bytes).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        let artifact = audit_durable_with(query, bytes, |as_of| {
            self.execute_prepared_query_governed_at(cx, query, as_of, policy).map(|run| run.value)
        }).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)?;
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        Ok(artifact)
    }
}

impl WriteTxn {
    /// Execute the current staged overlay and package its exact rows. This is
    /// an in-process audit aid, not a payload-complete standalone replay bundle.
    pub fn execute_prepared_query_overlay_artifact<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<fgdb_gql::GqlOverlayResultArtifact, WriteTxnError> {
        let rows = self.execute_prepared_query(database, query)?;
        let plan_certificate = self.prepared_query_plan_certificate(query)?;
        let staged_effect_digest = self.staged_effect_digest()?;
        Ok(fgdb_gql::GqlOverlayResultArtifact::new(
            query, self.basis, plan_certificate.digest, staged_effect_digest, rows,
        ))
    }

    /// Audit the current basis, plan, staged net effect and exact replayed rows.
    /// Lazy identity checks preserve the ordinary lifecycle refusal ordering.
    pub fn audit_prepared_query_overlay_artifact<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<fgdb_gql::GqlOverlayResultArtifact, fgdb_gql::GqlEvidenceAuditError<WriteTxnError>> {
        audit_overlay_with(query, bytes, self.basis,
            || self.prepared_query_plan_certificate(query).map(|certificate| certificate.digest),
            || self.staged_effect_digest(),
            || self.execute_prepared_query(database, query),
        )
    }

    /// Bound both artifact admission and the actual overlay replay. A refused
    /// replay retains its admitted transaction witnesses but never returns the
    /// merely decoded artifact as though its rows had been verified.
    pub fn audit_prepared_query_overlay_artifact_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
        limits: fgdb_gql::GqlEvidenceLimits,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlOverlayResultArtifact, fgdb_gql::GqlEvidenceLimitedAuditError<
        fgdb_gql::GqlQueryError<WriteTxnError, Box<asupersync::error::Error>>,
    >> {
        self.ensure_database(database).map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Source(source),
        ))?;
        database.ensure_readable().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Source(WriteTxnError::Read(source)),
        ))?;
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        limits.preflight_overlay(bytes).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Limit)?;
        let artifact = audit_overlay_with(query, bytes, self.basis,
            || self.prepared_query_plan_certificate(query).map(|certificate| certificate.digest)
                .map_err(fgdb_gql::GqlQueryError::Source),
            || self.staged_effect_digest().map_err(fgdb_gql::GqlQueryError::Source),
            || self.execute_prepared_query_governed(database, cx, query, policy).map(|run| run.value),
        ).map_err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit)?;
        cx.checkpoint().map_err(|source| governed_audit_execution_error(
            fgdb_gql::GqlQueryError::Interrupted(source),
        ))?;
        Ok(artifact)
    }
}

#[cfg(test)]
mod governed_audit_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn common_verifier_calls_replay_once_and_never_for_wrong_input() {
        let query = fgdb_gql::PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(1))).unwrap();
        let seq = CommitSeq(7);
        let certificate = crate::gql_cert::certify(query.plan(), seq);
        let artifact = fgdb_gql::GqlPreparedResultArtifact::new(&query, seq, certificate.digest, vec![VId(2)]);
        let calls = Cell::new(0);
        let bytes = artifact.to_bytes();
        let result = audit_durable_with(&query, &bytes, |at| {
            assert_eq!(at, seq);
            calls.set(calls.get() + 1);
            Ok::<_, ()>(vec![VId(2)])
        }).unwrap();
        assert_eq!(result.rows(), &[VId(2)]);
        assert_eq!(calls.get(), 1);
        let other = fgdb_gql::PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN a",
            &RelationBind::new().with_relation("R", RelationId(1))).unwrap();
        assert!(matches!(audit_durable_with(&other, &bytes, |_| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(vec![VId(2)])
        }), Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch)));
        assert_eq!(calls.get(), 1);
        assert!(matches!(audit_durable_with(&query, &bytes, |_| Err::<Vec<VId>, _>("cancelled")),
            Err(fgdb_gql::GqlEvidenceAuditError::Execution("cancelled"))));
    }
}
