//! An opt-in retained compressed source using the existing session boundary.

use super::*;
use crate::SealedReadView;
use fgdb_prism::FnxMemoryLimits;

/// A capability-scoped read session at one sealed owner's selected cut.
///
/// Obtain it from [`SealedReadView::authorized_read_session`]. The existing
/// session owns admission, prepared-handle identity, fresh execution permits,
/// final delivery, expiry, retirement and pin release. This wrapper only fixes
/// the cut and makes the already authenticated compressed image available to
/// compatible analytics calls. It never lends a privileged view or certificate.
///
/// Calls selecting the retained relation use compressed cursors only when the
/// capability admits every incidence, label and property and the registered
/// kernel supports the requested projection. Restricted capabilities and other
/// calls retain the existing mandatory-mask decoded source at this same cut.
/// This is resident execution; it does not turn the derived image into durable
/// Tier-R authority or establish descriptor-I/O or timing noninterference.
pub struct AuthorizedSealedReadSession<'a, Resolver, Clock> {
    inner: AuthorizedReadSession<'a, Resolver, Clock>,
}

impl<R, C> core::fmt::Debug for AuthorizedSealedReadSession<'_, R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedSealedReadSession")
            .field("closed", &self.is_closed())
            .field("authority_and_generation", &"[REDACTED]")
            .finish()
    }
}

impl<R, C> AuthorizedSealedReadSession<'_, R, C> {
    /// Release both retained representations and authority without callbacks.
    pub fn close(&mut self) {
        self.inner.close();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedSealedReadSession<'_, R, C> {
    /// Execute every native read class at the retained cut. An explicit
    /// different AS OF refuses before any graph source is consumed.
    pub fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.inner.query(cx, text, params)
    }

    /// Rows and selected sequence cross the same fresh final delivery gate.
    pub fn query_with_snapshot(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<(QueryResult, CommitSeq), QueryError> {
        self.inner.query_with_snapshot(cx, text, params)
    }

    /// Retain an owner-bound template using the fixed trusted catalog.
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> Result<AuthorizedPreparedRead, QueryError> {
        self.inner.prepare(cx, text, params)
    }

    /// Rebind arguments and execute under a new permit from this exact owner.
    pub fn execute(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.inner.execute(cx, prepared, params)
    }

    pub fn execute_with_snapshot(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<(QueryResult, CommitSeq), QueryError> {
        self.inner.execute_with_snapshot(cx, prepared, params)
    }
}

impl SealedReadView {
    /// Narrow this exact retained generation into the existing Warden session
    /// boundary. Signature, namespace, rights and current validity are checked
    /// before cloning the compressed source. The host fixes the catalog,
    /// branch, memory policy and monotone clock; query code cannot replace them.
    ///
    /// The source image is reused across statements. Projection construction,
    /// kernel execution and completed cursor pulls still spend each statement's
    /// fresh signed and host allowances. No raw anchor, source root, certificate
    /// or privileged session is returned to the capability holder.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_read_session<'a, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        memory: FnxMemoryLimits,
        clock: C,
    ) -> Result<AuthorizedSealedReadSession<'a, R, C>, QueryError> {
        let (view, partition, at, relation) = self.authorized_parts();
        let mut inner =
            view.authorized_read_session(cx, authority, token, branch, resolver, policy, clock)?;
        // The ordinary constructor has performed admission and final validity
        // checks. Installing a private source grants no execution permission.
        let state = inner.state.as_mut().ok_or(QueryError::Authorization(
            AuthorizationError::ExecutionStopped,
        ))?;
        state.sealed = Some(Arc::new(
            crate::query::authorized::analytics::sealed::Source::new(
                view, partition, at, relation, memory,
            ),
        ));
        Ok(AuthorizedSealedReadSession { inner })
    }
}

pub(super) fn selected_cut(view: &EmbeddedReadView, execution: &Live<'_, '_, '_>) -> CommitSeq {
    execution
        .borrow()
        .sealed
        .as_ref()
        .map_or_else(|| view.frontier(), |source| source.as_of())
}

pub(super) fn check_selected_cut(
    execution: &Live<'_, '_, '_>,
    selected: CommitSeq,
) -> Result<(), QueryError> {
    if execution
        .borrow()
        .sealed
        .as_ref()
        .is_some_and(|source| source.as_of() != selected)
    {
        return Err(QueryError::Unsupported {
            diagnostics: vec![
                "query selects a different cut from this retained read session".to_owned(),
            ],
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "sealed/tests.rs"]
mod tests;
