//! Native text writes enter the existing capability-scoped program, not the
//! privileged query_write path. Parsing and binding share its live allowance.

use super::{
    Authority, CapabilityToken, CommitCx, Database, Error, Execution, Fault, GqlParameters,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, Input, PreparedGraphWriteScript, QueryCx,
    TxnCx, Vfs, WriteTxnError, admission,
};
use crate::QueryResult;
use fgdb_delta_types::RelationId;
use fgdb_gql::unwind_write::{
    GraphUnwindBindError, GraphUnwindBindEvent, GraphUnwindWriteError, GraphUnwindWriteText,
};
use fgdb_gql::{GqlParameterType, GraphSymbol, GraphSymbolKind, GraphWriteTemplateStatement};
use fgdb_warden::PlannerPredicates;

fn checkpoint<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
    execution.checkpoint()
}

// The host ceiling counts native statements, including every expanded row.
// Ordinary scripts are checked before binding, not just before execution.
#[allow(clippy::result_large_err)]
fn statement_limit(script: &PreparedGraphWriteScript, limit: usize) -> Result<(), Fault> {
    let observed = script.statements().len() as u128;
    if observed > limit as u128 {
        return Err(Fault::BatchBinding(
            fgdb_gql::GraphWriteScriptBatchError::TooManyStatements { limit, observed },
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn reserve_text<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
) -> Result<(), Fault> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(admission)?;
    let bytes = u64::try_from(text.len())
        .map_err(|_| admission(WriteTxnError::Authorization(Error::TooLarge)))?;
    // Reserve original UTF-8 bytes BEFORE lexical classification, parsing or
    // declaration allocation. This is an ingress price, not compiler telemetry.
    execution.work(bytes).map_err(admission)
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
    resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<PreparedGraphWriteScript, Fault> {
    cx.with_restriction(|| {
        reserve_text(cx, execution, text)?;
        prepare_admitted(cx, execution, text, params, relation, resolver)
    })
}

// Both callers have reserved the original text under the same live permit.
// Prepared sessions retain precisely their existing script preparation costs.
#[allow(clippy::result_large_err)]
fn prepare_admitted<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
    params: &GqlParameters,
    relation: RelationId,
    mut resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<PreparedGraphWriteScript, Fault> {
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
}

// The only outputs are the existing OWNED program/batch variants. No borrow of
// a temporary script, unchecked raw writer or independent permit can escape.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn bind_native<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
    params: &GqlParameters,
    relation: RelationId,
    scope: &PlannerPredicates,
    max_statements: usize,
    resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<super::Bound<'static>, Fault> {
    cx.with_restriction(|| {
        reserve_text(cx, execution, text)?;
        let parsed = match GraphUnwindWriteText::parse_if_supported(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                checkpoint(cx, execution).map_err(admission)?;
                return Err(match error {
                    GraphUnwindWriteError::Syntax(source) => Fault::Binding(source),
                    error => Fault::UnwindBinding(error),
                });
            }
        };
        let Some(parsed) = parsed else {
            // CREATE/INSERT UNWIND and ordinary scripts keep their original
            // compiler, preflight and binding protocol, with no second byte bill.
            let script = prepare_admitted(cx, execution, text, params, relation, resolver)?;
            statement_limit(&script, max_statements)?;
            let super::Bound::Script(program) =
                Input::Script(&script, params).bind(cx, scope, execution)?
            else {
                unreachable!("script input always returns an owned script program")
            };
            return Ok(super::Bound::Script(program));
        };
        checkpoint(cx, execution).map_err(admission)?;
        // Every supported UNWIND mutation contains MERGE or MATCH. Refuse a
        // write-only grant BEFORE row inspection, including empty/malformed rows.
        if !scope.rights().can_read() {
            return Err(admission(WriteTxnError::Authorization(Error::PermissionDenied)));
        }
        let batch = parsed.bind_with_limit_controlled(
            params,
            relation,
            max_statements,
            resolver,
            |event| {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                match event {
                    GraphUnwindBindEvent::Work(units) => execution.work(units),
                    GraphUnwindBindEvent::Definition(script) => {
                        // The compiled shape is admitted before native record
                        // binding, as on Input::Batch. The ordinary program
                        // preflight remains authoritative before any graph access.
                        for statement in script.statements() {
                            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.poll()?;
                            if matches!(
                                statement,
                                GraphWriteTemplateStatement::EdgeMerge(_)
                                    | GraphWriteTemplateStatement::EdgeUpsert(_)
                            ) && !scope.allows_relation(statement.relation())
                            {
                                return Err(WriteTxnError::Authorization(Error::ScopeDenied));
                            }
                        }
                        Ok(())
                    }
                }
            },
        );
        match batch {
            Ok(batch) => Ok(super::Bound::Batch(batch)),
            Err(GraphUnwindBindError::Interrupted(error)) => Err(admission(error)),
            Err(GraphUnwindBindError::Binding(error)) => {
                // Expiry/cancellation cannot be hidden behind a late value or
                // syntax failure. Never poll again after a control has refused.
                checkpoint(cx, execution).map_err(admission)?;
                Err(Fault::UnwindBinding(error))
            }
        }
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Parse, bind and atomically execute native write text under one capability.
    ///
    /// Namespace, signature, branch and Write rights are verified BEFORE syntax,
    /// parameter inspection or catalog callbacks. A single live permit covers
    /// preparation, binding, every statement, receipt admission and completion.
    /// Ordinary syntax/parameter errors retain Binding and original script
    /// coordinates. UNWIND admission uses UnwindBinding; executed batch failures
    /// retain BatchProgram's input coordinates and native publication cause.
    /// Control failures take precedence over a refused lookup's parser error.
    ///
    /// Preparation reserves one signed work unit per original UTF-8 byte, then
    /// one per parameter and each pre/post resolver/final checkpoint. Binding
    /// retains the prepared-script statement-instance price. These are ingress
    /// prices, not measured compiler operations, allocator-byte limits or
    /// preemption inside parsing or a trusted host callback. Native syntax limits
    /// still apply. No stage receives another permit or resets an allowance.
    ///
    /// Bounded UNWIND MERGE/MATCH mutations admit at most 64 rows as ONE program.
    /// They require ReadWrite before inspecting rows, and additionally charge
    /// the controlled binder's row/field, metadata, payload and repeated-global
    /// work, lowered-text compilation and expanded native binding to the SAME
    /// permit. Every row binds before graph observation or identity allocation.
    /// Later rows see earlier checked effects; any precommit failure discards
    /// the entire batch. Repeated identity receipts consume repeated signed rows.
    /// CREATE/INSERT forms keep their existing compiler. Explicit larger bound
    /// batches and the prepared-session API retain their existing contracts.
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
                let bound = bind_native(
                    query_cx,
                    &mut execution,
                    text,
                    params,
                    relation,
                    verified.predicates(),
                    fgdb_gql::MAX_GRAPH_MUTATION_STATEMENTS,
                    resolver,
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
pub use session::{AuthorizedBoundWriteBatch, AuthorizedPreparedWrite, AuthorizedWriteSession};
