//! Native text writes enter the existing capability-scoped program, not the
//! privileged query_write path. Parsing and binding share its live allowance.

use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, Fault, GqlParameters,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, Input, PreparedGraphWriteScript, QueryCx,
    TxnCx, Vfs, WriteTxnError, admission,
};
use crate::QueryResult;
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlParameterType, GraphSymbol, GraphSymbolKind};

fn checkpoint<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
    execution.checkpoint()
}

// The trusted host still owns catalog semantics. This wrapper controls when
// that host can be called, not the content of a tenant's symbol catalog.
#[allow(clippy::result_large_err)] // one terminal native script diagnostic
fn prepare<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
    params: &GqlParameters,
    relation: RelationId,
    mut resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<PreparedGraphWriteScript, Fault> {
    cx.with_restriction(|| {
        cx.checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(admission)?;
        let bytes = u64::try_from(text.len())
            .map_err(|_| admission(WriteTxnError::Authorization(Error::TooLarge)))?;
        // One signed work unit per original UTF-8 byte, including whitespace,
        // reserved BEFORE parsing or declaration allocation. This is an ingress
        // admission price, not a claim to count exact compiler operations.
        execution.work(bytes).map_err(admission)?;
        let mut declarations = Vec::new();
        for (name, kind) in params.parameter_types() {
            checkpoint(cx, execution).map_err(admission)?;
            // Keep the native query_write inference convention. Integer and
            // pagination roles are inferred by the shared typed parser.
            if matches!(kind, GqlParameterType::Scalar(_)) {
                declarations.push((name, kind));
            }
        }
        // The parser's resolver protocol returns Option, not a control error.
        // Retain a refusal separately: it must not become UnknownSymbol, nor
        // cause later catalog callbacks or a retry through a privileged API.
        let mut failure = None;
        let prepared = PreparedGraphWriteScript::prepare_with_parameter_types(
            text,
            relation,
            &declarations,
            |kind, name| {
                if failure.is_some() {
                    return None;
                }
                if let Err(error) = checkpoint(cx, execution) {
                    failure = Some(error);
                    return None;
                }
                let symbol = resolver(kind, name);
                if let Err(error) = checkpoint(cx, execution) {
                    failure = Some(error);
                    return None;
                }
                symbol
            },
        );
        if let Some(error) = failure {
            return Err(admission(error));
        }
        // Recheck even syntax-only failures and source-free statements. No
        // prepared definition, pin or identity escapes a late control refusal.
        checkpoint(cx, execution).map_err(admission)?;
        prepared.map_err(Fault::Binding)
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Parse, bind and atomically execute native write text under one capability.
    ///
    /// Namespace, signature, branch and Write rights are verified BEFORE syntax,
    /// parameter inspection or catalog callbacks. A single live permit covers
    /// preparation, binding, every statement, receipt admission and completion.
    /// Native syntax and parameter errors are returned in the Binding arm with
    /// original script coordinates. Control failures retain their original cause
    /// and take precedence over a parser error caused by a refused lookup.
    ///
    /// Preparation reserves one signed work unit per original UTF-8 byte, then
    /// one per parameter and each pre/post resolver/final checkpoint. Binding
    /// retains the prepared-script statement-instance price. These are ingress
    /// prices, not measured compiler operations, allocator-byte limits or
    /// preemption inside parsing or a trusted host callback. Native syntax limits
    /// still apply. No stage receives another permit or resets an allowance.
    ///
    /// The existing typed program checks ReadWrite requirements, relation scope,
    /// masked selection, original attempted fields and before/after images.
    /// Engine-owned identity allocation and one Chronicle completion remain the
    /// sole write path. No caller allocator, raw transaction, fallback or retry
    /// is exposed. The complete returning receipt is admitted and built before
    /// publication; only a fixed QueryResult wrapper is constructed afterward.
    /// Unknown/recovery completion errors are never relabeled as rollback.
    ///
    /// The host MUST supply its trusted issuer, branch routing, monotone clock
    /// and capability-safe symbol catalog. Write-only holders can resolve names
    /// needed for creation; this API does not authorize arbitrary catalog
    /// discovery. Raw Database APIs remain privileged. Resident preparation,
    /// sequential-ID metadata, durable audit/revocation and full SSI retain the
    /// existing authorized-program limitations.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // preserve original script/commit diagnostics
    pub async fn query_write_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        policy: GraphWriteProgramPolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<QueryResult, Fault> {
        let refusal = |error| admission(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        let mut execution = Execution {
            cx: commit_cx,
            permit,
            clock,
        };
        commit_cx
            .with_restriction_async(async {
                let script = prepare(query_cx, &mut execution, text, params, relation, resolver)?;
                let bound = Input::Script(&script, params).bind(
                    query_cx,
                    verified.predicates(),
                    &mut execution,
                )?;
                let (receipt, completion) = self
                    .complete_authorized_program(
                        txn_cx,
                        query_cx,
                        commit_cx,
                        bound.program(),
                        policy,
                        verified.predicates(),
                        &mut execution,
                        true,
                        GraphWriteProgramReceipt::new,
                    )
                    .await
                    .map_err(|error| bound.error(error))?;
                Ok(QueryResult::Write {
                    receipt,
                    completion: Some(completion),
                })
            })
            .await
    }
}

#[cfg(test)]
#[path = "native_write_tests.rs"]
mod tests;

#[path = "write_session.rs"]
mod session;
