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
use fgdb_gql::{
    GqlMapArgumentText, GqlParameterType, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    GraphWriteTemplateStatement,
};
use fgdb_warden::PlannerPredicates;

/// Borrow a session's trusted catalog without reducing it to a symbol-only
/// closure. Computed map keys and metadata need the same pinned reverse names
/// and time-zone bundle as ordinary native preparation.
struct BorrowedCatalog<'a, R: ?Sized>(&'a mut R);

impl<R: GraphSymbolResolver + ?Sized> GraphSymbolResolver for BorrowedCatalog<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.0.resolve_symbol(kind, name)
    }

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        self.0.time_zones()
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        self.0.reverse_catalog()
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.0.reverse_label(id)
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.0.reverse_relation(id)
    }
}

/// Preserve the existing before/after lookup controls and first refusal while
/// forwarding the host's immutable catalog, as the UNWIND metered resolver does.
struct ControlledCatalog<'a, R, F> {
    resolver: &'a mut R,
    control: &'a mut F,
    failure: &'a mut Option<WriteTxnError>,
}

impl<R, F> GraphSymbolResolver for ControlledCatalog<'_, R, F>
where
    R: GraphSymbolResolver,
    F: FnMut() -> Result<(), WriteTxnError>,
{
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if self.failure.is_some() {
            return None;
        }
        if let Err(error) = (self.control)() {
            *self.failure = Some(error);
            return None;
        }
        let symbol = self.resolver.resolve_symbol(kind, name);
        if let Err(error) = (self.control)() {
            *self.failure = Some(error);
            return None;
        }
        symbol
    }

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        self.failure
            .is_none()
            .then(|| self.resolver.time_zones())
            .flatten()
    }

    fn reverse_catalog(&self) -> Option<fgdb_gql::ReverseSymbolCatalog> {
        self.failure
            .is_none()
            .then(|| self.resolver.reverse_catalog())
            .flatten()
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.failure
            .is_none()
            .then(|| self.resolver.reverse_label(id))
            .flatten()
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.failure
            .is_none()
            .then(|| self.resolver.reverse_relation(id))
            .flatten()
    }
}

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
    execution.work(bytes).map_err(admission)?;
    // Engine identities are not masked, so a capability cannot observe them
    // through id()/elementId() (fgdb-j687q, owner ruling 2026-09-30).
    if fgdb_gql::reads_element_identity(text) {
        return Err(admission(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    Ok(())
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
    resolver: impl GraphSymbolResolver,
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
    mut resolver: impl GraphSymbolResolver,
) -> Result<PreparedGraphWriteScript, Fault> {
    let mut declarations = Vec::new();
    for (name, kind) in params.parameter_types() {
        checkpoint(cx, execution).map_err(admission)?;
        // Keep the native query_write inference convention. Integer and
        // pagination roles are inferred by the shared typed parser.
        if matches!(
            kind,
            GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
        ) {
            declarations.push((name, kind));
        }
    }
    // The parser's resolver protocol returns Option, not a control error.
    // Retain a refusal separately: it must not become UnknownSymbol, nor
    // cause later catalog callbacks or a retry through a privileged API.
    let mut failure = None;
    let prepared = {
        let mut control = || checkpoint(cx, execution);
        PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
            text,
            relation,
            &declarations,
            &mut ControlledCatalog {
                resolver: &mut resolver,
                control: &mut control,
                failure: &mut failure,
            },
        )
    };
    if let Some(error) = failure {
        return Err(admission(error));
    }
    // Recheck even syntax-only failures and source-free statements. No
    // prepared definition, pin or identity escapes a late control refusal.
    checkpoint(cx, execution).map_err(admission)?;
    prepared.map_err(Fault::Binding)
}

/// Bind one native chain without lowering whole-map values or replacing the
/// live preparation permit. Pattern-map arguments keep their original offsets.
#[allow(clippy::result_large_err)]
fn bind_chain_admitted<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
    params: &GqlParameters,
    relation: RelationId,
    mut resolver: impl GraphSymbolResolver,
) -> Result<fgdb_gql::PreparedGraphWriteChain, Fault> {
    cx.with_restriction(|| {
        let lowered = GqlMapArgumentText::lower_fields(text, params);
        let (text, params) = lowered
            .as_ref()
            .map_or((text, params), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
        let mut declarations = Vec::new();
        for (name, kind) in params.parameter_types() {
            checkpoint(cx, execution).map_err(admission)?;
            if matches!(
                kind,
                GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
            ) {
                declarations.push((name, kind));
            }
        }
        let mut failure = None;
        let prepared = {
            let mut control = || checkpoint(cx, execution);
            fgdb_gql::PreparedGraphWriteChainText::prepare_with_parameter_types(
                text,
                relation,
                &declarations,
                ControlledCatalog {
                    resolver: &mut resolver,
                    control: &mut control,
                    failure: &mut failure,
                },
            )
        };
        if let Some(error) = failure {
            return Err(admission(error));
        }
        checkpoint(cx, execution).map_err(admission)?;
        let prepared = prepared.map_err(|error| restored(lowered, Fault::ChainBinding(error)))?;
        // A chain is one native statement. Its clauses and input occurrences
        // spend their cumulative execution allowance in the existing owner.
        execution.work(1).map_err(admission)?;
        let bound = prepared.bind_parameters(params);
        checkpoint(cx, execution).map_err(admission)?;
        bound.map_err(|error| restored(lowered, Fault::ChainBinding(error)))
    })
}

/// Build the public representation while the chain still owns its rollback
/// workspace. No allocation or fallible value conversion follows publication.
fn chain_result(
    chain: &fgdb_gql::PreparedGraphWriteChain,
    stats: fgdb_gql::GraphWriteProgramStats,
    rows: &[fgdb_gql::algebra::GraphValueRow],
    work: &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
) -> Result<QueryResult, WriteTxnError> {
    work(1)?;
    if !chain.returns_rows() {
        // Private identities are not an implicit RETURN. The public receipt
        // reports the complete statement's measured effects and creation counts.
        return Ok(QueryResult::Write {
            receipt: GraphWriteProgramReceipt::new(stats, Vec::new()),
            completion: None,
        });
    }
    let mut columns = Vec::new();
    for name in chain.columns() {
        let bytes = u64::try_from(name.len())
            .map_err(|_| WriteTxnError::Authorization(Error::TooLarge))?;
        work(1)?;
        work(bytes)?;
        columns.push(name.clone());
    }
    let mut output = Vec::new();
    for row in rows {
        work(1)?;
        let mut cells = Vec::new();
        for value in row.values() {
            work(1)?;
            let copied = value.copy_with_control(&mut |_| work(1))?;
            cells.push(crate::QueryValue::Value(copied));
        }
        output.push(cells);
    }
    Ok(QueryResult::Rows {
        columns,
        rows: output,
    })
}

/// A script fault of a lowered statement, at the caller's bytes.
#[allow(clippy::result_large_err)]
fn restored(lowered: Option<&GqlMapArgumentText>, mut fault: Fault) -> Fault {
    let Some(lowered) = lowered else {
        return fault;
    };
    match &mut fault {
        Fault::Binding(error)
        | Fault::BatchBinding(fgdb_gql::GraphWriteScriptBatchError::Arguments {
            source: error,
            ..
        }) => lowered.restore(error),
        Fault::ChainBinding(error) => lowered.restore(error),
        _ => {}
    }
    fault
}

/// An argument set whose map arguments do not fit the statement lowered with
/// the first set's keys (fgdb-0s2pq): a script-wide argument error, so no
/// statement and byte zero.
fn map_argument_fault(argument_set: usize, error: &fgdb_gql::GqlMapArgumentError) -> Fault {
    use fgdb_gql::GqlMapArgumentError as E;
    let expected = match error {
        E::ExpectedMap { .. } | E::KeysDiffer { .. } => {
            "a map argument with the keys of the first argument set"
        }
        E::UnsupportedValue { .. } => "a map argument whose values are properties",
        E::RowsDiffer { .. } => "UNWIND rows with the keys of the first argument set",
        E::Parameter(_) => "map argument fields within the parameter limits",
    };
    Fault::BatchBinding(fgdb_gql::GraphWriteScriptBatchError::Arguments {
        argument_set,
        source: fgdb_gql::GraphWriteScriptError {
            statement: None,
            offset: 0,
            kind: fgdb_gql::GraphWriteScriptErrorKind::Syntax(
                fgdb_gql::GraphPatternTextErrorKind::Expected(expected),
            ),
        },
    })
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
    resolver: impl GraphSymbolResolver,
) -> Result<super::Bound<'static>, Fault> {
    cx.with_restriction(|| {
        reserve_text(cx, execution, text)?;
        bind_native_admitted(
            cx,
            execution,
            text,
            params,
            relation,
            scope,
            max_statements,
            resolver,
        )
    })
}

// Native dispatch has already paid for the original bytes. Prepared sessions
// continue through bind_native and reserve their own text exactly once.
#[allow(clippy::too_many_arguments, clippy::result_large_err)]
fn bind_native_admitted<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    text: &str,
    params: &GqlParameters,
    relation: RelationId,
    scope: &PlannerPredicates,
    max_statements: usize,
    resolver: impl GraphSymbolResolver,
) -> Result<super::Bound<'static>, Fault> {
    cx.with_restriction(|| {
        let classified =
            GraphUnwindWriteText::requires_computed_map_input(text).and_then(|computed_maps| {
                if computed_maps {
                    Ok((None, true))
                } else {
                    GraphUnwindWriteText::parse_if_supported(text).map(|parsed| (parsed, false))
                }
            });
        let (parsed, computed_maps) = match classified {
            Ok(parsed) => parsed,
            Err(error) => {
                checkpoint(cx, execution).map_err(admission)?;
                return Err(match error {
                    GraphUnwindWriteError::Syntax(source) => Fault::Binding(source),
                    error => Fault::UnwindBinding(error),
                });
            }
        };
        if computed_maps {
            // Like bounded MATCH ingress, admit reading before arguments or
            // catalog callbacks. The full mutation then freezes one relation;
            // it never retries a refused row binder or weakens its profiles.
            checkpoint(cx, execution).map_err(admission)?;
            if !scope.rights().can_read() {
                return Err(admission(WriteTxnError::Authorization(
                    Error::PermissionDenied,
                )));
            }
        }
        let Some(parsed) = parsed else {
            // CREATE/INSERT UNWIND and ordinary scripts keep their original
            // compiler, preflight and binding protocol, with no second byte bill.
            // A map argument standing for a property map (`SET n += $props`)
            // binds as the literal map of its keys (fgdb-0s2pq), and its
            // diagnostics keep the caller's bytes. Each spliced key is one more
            // declaration, charged as every declaration is.
            let lowered = if computed_maps {
                // Keep whole-map values in the row language. Pattern fields
                // retain their usual argument lowering and source coordinates.
                GqlMapArgumentText::lower_fields(text, params)
            } else {
                GqlMapArgumentText::lower(text, params)
            };
            let (text, params) = lowered
                .as_ref()
                .map_or((text, params), |(lowered, derived)| {
                    (lowered.text(), derived)
                });
            let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
            let script = prepare_admitted(cx, execution, text, params, relation, resolver)
                .map_err(|fault| restored(lowered, fault))?;
            statement_limit(&script, max_statements)?;
            let super::Bound::Script(program) = Input::Script(&script, params)
                .bind(cx, scope, execution)
                .map_err(|fault| restored(lowered, fault))?
            else {
                unreachable!("script input always returns an owned script program")
            };
            return Ok(super::Bound::Script(program));
        };
        checkpoint(cx, execution).map_err(admission)?;
        // Every supported UNWIND mutation contains MERGE or MATCH. Refuse a
        // write-only grant BEFORE row inspection, including empty/malformed rows.
        if !scope.rights().can_read() {
            return Err(admission(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        let batch = parsed.bind_with_limit_controlled_resolver(
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
    /// Admit immutable native write-script metadata for a trusted host cache.
    ///
    /// This is preparation only: no snapshot, graph read, identity allocation,
    /// transaction or mutation is retained. Representative operands are checked
    /// and discarded. Namespace, live Write authority, cancellation, signed work
    /// and the host statement ceiling govern the same compiler/binder used by
    /// authorized write sessions. The returned script grants no authority;
    /// execute it through `execute_graph_write_script_authorized` with a fresh
    /// capability check and a complete new operand map on every invocation.
    ///
    /// The host owns the catalog, branch route, clock, cache bounds and metadata
    /// invalidation. This privileged host API does not export a session-owned
    /// prepared handle or turn it into a transferable capability. Native RETURN
    /// and the separate dynamic UNWIND mutation profile remain unsupported.
    /// Template admission is not effect admission: the executor still checks
    /// every attempted field and before/after image. As on native text writes,
    /// symbol-existence diagnostics belong to the supplied trusted host catalog;
    /// this method is not a catalog discovery or catalog masking boundary.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    pub fn prepare_graph_write_script_authorized(
        &self,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        relation: RelationId,
        max_statements: usize,
        mut clock: impl FnMut() -> u64,
    ) -> Result<PreparedGraphWriteScript, Fault> {
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
        let script = prepare(query_cx, &mut execution, text, params, relation, resolver)?;
        statement_limit(&script, max_statements)?;
        let bound =
            Input::Script(&script, params).bind(query_cx, verified.predicates(), &mut execution);
        checkpoint(query_cx, &mut execution).map_err(admission)?;
        drop(bound?);
        Ok(script)
    }

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
    /// MERGE and disjoint mutations see earlier checked effects. Vertex updates
    /// that can change their own MATCH first freeze every row's selection and
    /// expressions against the entry overlay, then authorize and apply each
    /// original field in input order. Any precommit failure discards the entire
    /// batch. Repeated identity receipts consume repeated signed rows.
    /// CREATE/INSERT forms keep their existing compiler. Explicit larger bound
    /// batches and the prepared-session API retain their existing contracts.
    /// Computed whole-map MATCH updates use one complete relational mutation:
    /// row-specific keys and all RHS values are frozen before any staging.
    /// Their one statement shares the ordinary query/effect limits and signed
    /// permit, and every actual map key passes native field admission. Native
    /// write chains use the same live permit for preparation, typed chain
    /// admission and completion. Explicit RETURN produces admitted rows;
    /// no-RETURN chains expose a statistics receipt without private identities.
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
        resolver: impl GraphSymbolResolver,
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
                let chain = query_cx.with_restriction(|| {
                    reserve_text(query_cx, &mut execution, text)?;
                    let classified = fgdb_gql::PreparedGraphWriteChainText::is_chain(text);
                    checkpoint(query_cx, &mut execution).map_err(admission)?;
                    classified.map_err(|error| {
                        Fault::ChainBinding(fgdb_gql::GraphWriteChainTextError::Insert(error))
                    })
                })?;
                if chain {
                    let chain = bind_chain_admitted(
                        query_cx,
                        &mut execution,
                        text,
                        params,
                        relation,
                        resolver,
                    )?;
                    let mut result = None;
                    let (_, _, completion) = self
                        .complete_authorized_chain(
                            txn_cx,
                            query_cx,
                            commit_cx,
                            &chain,
                            None,
                            policy.insertion_policy(),
                            verified.predicates(),
                            execution,
                            |stats, rows, work| {
                                result = Some(chain_result(&chain, stats, rows, work)?);
                                Ok(())
                            },
                        )
                        .await
                        .map_err(Fault::Chain)?;
                    let mut result =
                        result.expect("chain representation was admitted before completion");
                    if let QueryResult::Write {
                        completion: written,
                        ..
                    } = &mut result
                    {
                        *written = Some(completion);
                    }
                    return Ok(result);
                }
                let bound = bind_native_admitted(
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
