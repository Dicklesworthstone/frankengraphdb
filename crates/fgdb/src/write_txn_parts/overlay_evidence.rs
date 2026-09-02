const STAGED_EFFECT_DOMAIN_V1: &[u8] = b"fgdb:write-txn-staged-effect:v1";

impl WriteTxn {
    /// Hash the canonical semantic effect currently staged over this
    /// transaction's durable basis.
    ///
    /// The transcript binds the basis and either an explicit empty-overlay tag
    /// or the complete canonical `LogicalDeltaTemplate` retained by the
    /// prepared write. It identifies the staged net effect, not the caller's
    /// sequence of API calls that produced the same canonical effect.
    pub fn staged_effect_digest(&self) -> Result<fgdb_crypto::Digest, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(STAGED_EFFECT_DOMAIN_V1);
        hasher.update(&self.basis.0.to_be_bytes());
        match &self.prepared {
            None => {
                hasher.update(&[0]);
            }
            Some(prepared) => {
                debug_assert_eq!(prepared.basis(), self.basis);
                let bytes = prepared
                    .template
                    .canonical_bytes()
                    .map_err(|error| WriteTxnError::Write(WriteError::Canonical(error)))?;
                hasher.update(&[1]);
                hasher.update(&count_as_u64(bytes.len()).to_be_bytes());
                hasher.update(&bytes);
            }
        }
        Ok(hasher.finalize())
    }

    /// Execute one coherent prepared query over the current staged overlay and
    /// certify the exact ordered rows.
    ///
    /// Evidence is minted only after successful execution. The plan certificate
    /// binds the durable basis and concrete `BoundPlan`; the overlay result
    /// certificate additionally binds the canonical staged net effect and every
    /// returned row in order.
    pub fn execute_prepared_query_with_overlay_result_certificate<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<
        (
            Vec<VId>,
            crate::GqlPlanCertificate,
            fgdb_gql::GqlOverlayResultCertificate,
        ),
        WriteTxnError,
    > {
        let rows = self.execute_prepared_query(database, query)?;
        let plan_certificate = self.prepared_query_plan_certificate(query)?;
        let staged_effect_digest = self.staged_effect_digest()?;
        let result_certificate = fgdb_gql::GqlOverlayResultCertificate::new(
            self.basis,
            plan_certificate.digest,
            staged_effect_digest,
            &rows,
        );
        Ok((rows, plan_certificate, result_certificate))
    }

    /// Verify exact staged-overlay result evidence against this transaction's
    /// current basis, canonical staged effect, and the supplied prepared query.
    ///
    /// Staging another mutation after issuance changes the staged-effect digest
    /// and makes the old certificate fail. A finished transaction refuses the
    /// verification because its overlay authority no longer exists.
    pub fn verifies_prepared_query_overlay_result(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        rows: &[VId],
        certificate: &fgdb_gql::GqlOverlayResultCertificate,
    ) -> Result<bool, WriteTxnError> {
        let plan_certificate = self.prepared_query_plan_certificate(query)?;
        let staged_effect_digest = self.staged_effect_digest()?;
        Ok(certificate.verifies(
            self.basis,
            plan_certificate.digest,
            staged_effect_digest,
            rows,
        ))
    }
}
