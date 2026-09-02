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
            query,
            as_of,
            plan_certificate.digest,
            rows,
        ))
    }

    /// Decode and audit one prepared-result envelope by reopening its exact
    /// historical sequence and re-executing the retained plan.
    ///
    /// Format, input, plan, execution, and row mismatches remain distinct typed
    /// refusals. Successful audit returns the decoded artifact whose rows were
    /// reproduced exactly.
    pub fn audit_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceAuditError<GqlError>,
    > {
        let artifact =
            fgdb_gql::GqlPreparedResultArtifact::from_bytes(bytes)
                .map_err(fgdb_gql::GqlEvidenceAuditError::Decode)?;
        if !artifact.verifies_input(query) {
            return Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch);
        }
        let plan_certificate =
            crate::gql_cert::certify(query.plan(), artifact.snapshot_seq());
        if !artifact.verifies_plan(plan_certificate.digest) {
            return Err(fgdb_gql::GqlEvidenceAuditError::PlanMismatch);
        }
        let replay = self
            .execute_prepared_query_at(query, artifact.snapshot_seq())
            .map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
        if replay.as_slice() != artifact.rows() {
            return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
        }
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

    /// Execute one coherent prepared definition at a sequence retained by this
    /// immutable view and package its exact evidence.
    pub fn execute_prepared_query_artifact_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<fgdb_gql::GqlPreparedResultArtifact, GqlError> {
        let rows = self.execute_prepared_query_at(query, as_of)?;
        let plan_certificate = crate::gql_cert::certify(query.plan(), as_of);
        Ok(fgdb_gql::GqlPreparedResultArtifact::new(
            query,
            as_of,
            plan_certificate.digest,
            rows,
        ))
    }

    /// Decode and audit an envelope against this immutable view. A sequence
    /// above the view's frontier preserves the existing typed read refusal.
    pub fn audit_prepared_query_artifact(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlPreparedResultArtifact,
        fgdb_gql::GqlEvidenceAuditError<GqlError>,
    > {
        let artifact =
            fgdb_gql::GqlPreparedResultArtifact::from_bytes(bytes)
                .map_err(fgdb_gql::GqlEvidenceAuditError::Decode)?;
        if !artifact.verifies_input(query) {
            return Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch);
        }
        let plan_certificate =
            crate::gql_cert::certify(query.plan(), artifact.snapshot_seq());
        if !artifact.verifies_plan(plan_certificate.digest) {
            return Err(fgdb_gql::GqlEvidenceAuditError::PlanMismatch);
        }
        let replay = self
            .execute_prepared_query_at(query, artifact.snapshot_seq())
            .map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
        if replay.as_slice() != artifact.rows() {
            return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
        }
        Ok(artifact)
    }
}

impl WriteTxn {
    /// Execute one coherent prepared definition over the current staged overlay
    /// and return a canonical identity-and-row envelope.
    ///
    /// The artifact remains an in-process audit aid: it does not carry the
    /// durable snapshot, staged template bytes, read set, or conflict state
    /// needed for standalone transaction replay.
    pub fn execute_prepared_query_overlay_artifact<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<fgdb_gql::GqlOverlayResultArtifact, WriteTxnError> {
        let rows = self.execute_prepared_query(database, query)?;
        let plan_certificate = self.prepared_query_plan_certificate(query)?;
        let staged_effect_digest = self.staged_effect_digest()?;
        Ok(fgdb_gql::GqlOverlayResultArtifact::new(
            query,
            self.basis,
            plan_certificate.digest,
            staged_effect_digest,
            rows,
        ))
    }

    /// Decode and audit one staged-overlay artifact against this transaction's
    /// current basis, plan, canonical staged effect, and exact re-executed rows.
    ///
    /// Staging another mutation after artifact issuance causes a typed
    /// `StagedEffectMismatch`; a finished transaction preserves its existing
    /// lifecycle refusal through `Execution`.
    pub fn audit_prepared_query_overlay_artifact<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        bytes: &[u8],
    ) -> Result<
        fgdb_gql::GqlOverlayResultArtifact,
        fgdb_gql::GqlEvidenceAuditError<WriteTxnError>,
    > {
        let artifact =
            fgdb_gql::GqlOverlayResultArtifact::from_bytes(bytes)
                .map_err(fgdb_gql::GqlEvidenceAuditError::Decode)?;
        if artifact.basis() != self.basis {
            return Err(fgdb_gql::GqlEvidenceAuditError::SnapshotMismatch);
        }
        if !artifact.verifies_input(query) {
            return Err(fgdb_gql::GqlEvidenceAuditError::InputMismatch);
        }
        let plan_certificate = self
            .prepared_query_plan_certificate(query)
            .map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
        if !artifact.verifies_plan(plan_certificate.digest) {
            return Err(fgdb_gql::GqlEvidenceAuditError::PlanMismatch);
        }
        let staged_effect_digest = self
            .staged_effect_digest()
            .map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
        if !artifact.verifies_staged_effect(staged_effect_digest) {
            return Err(
                fgdb_gql::GqlEvidenceAuditError::StagedEffectMismatch,
            );
        }
        let replay = self
            .execute_prepared_query(database, query)
            .map_err(fgdb_gql::GqlEvidenceAuditError::Execution)?;
        if replay.as_slice() != artifact.rows() {
            return Err(fgdb_gql::GqlEvidenceAuditError::ResultMismatch);
        }
        Ok(artifact)
    }
}
