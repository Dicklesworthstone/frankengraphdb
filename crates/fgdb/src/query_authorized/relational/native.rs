//! Native text and reusable templates over one Warden-scoped execution.
//! Dispatch sees only the admitted generation and permit, not a Database, so
//! no branch can fall back to privileged readers or reset an input's authority.

#[path = "native/session.rs"]
mod session;
pub use session::{
    AuthorizedAggregateCursor, AuthorizedAsyncAggregateCursor, AuthorizedBeaconIndex,
    AuthorizedDifferenceCursor, AuthorizedDifferenceExecution, AuthorizedOrderExecution,
    AuthorizedOrderedReadSession, AuthorizedPreparedFnxCall, AuthorizedPreparedRead,
    AuthorizedReadSession, AuthorizedRowCursor, AuthorizedSealedReadSession,
    AuthorizedSendAggregateCursor, AuthorizedSendRowCursor, AuthorizedSortLimits,
    AuthorizedSpoolCursor,
};

use super::*;
use crate::query::{aggregates, values};
use crate::{PreparedNativeRead, QueryResult, QueryValue};
use fgdb_gql::{GqlParameters, GraphSymbolResolver, PreparedGraphBranchText};

// Only metadata and complete result rows leave the dispatcher. The outer
// authorized owner still performs the sole signed delivery admission.
fn rows_of(
    result: QueryResult,
    columns: &mut Vec<String>,
) -> Result<Vec<Vec<QueryValue>>, QueryError> {
    match result {
        QueryResult::Rows {
            columns: names,
            rows,
        } => {
            *columns = names;
            Ok(rows)
        }
        QueryResult::Write { .. } => Err(QueryError::Unsupported {
            diagnostics: vec!["authorized native execution covers reads only".to_owned()],
        }),
    }
}

// Engine identities are not masked, so a capability cannot observe them
// through id()/elementId() (fgdb-j687q, owner ruling 2026-09-30). Checked on
// the selected statement text before preparation calls the catalog.
fn refuse_element_identity(statement: &str) -> Result<(), QueryError> {
    if fgdb_gql::reads_element_identity(statement) {
        return Err(QueryError::Authorization(fgdb_warden::Error::ScopeDenied));
    }
    Ok(())
}

// EXPLAIN on an authorized surface (fgdb-ooiik). The listing derives from the
// statement text, its parameters and the caller's catalog, never from records,
// so nothing hidden from the capability can change it. The certificate form
// binds the snapshot sequence, which hidden commits advance, so it refuses.
// Invalidation is checked after preparation even when preparation fails.
fn explain_authorized(
    explain: Result<(&str, bool), QueryError>,
    params: &GqlParameters,
    resolver: impl GraphSymbolResolver,
    checkpoint: impl FnOnce() -> Result<(), QueryError>,
) -> Result<QueryResult, QueryError> {
    let (statement, certificate) = explain?;
    if certificate {
        return Err(QueryError::Unsupported {
            diagnostics: vec![
                "EXPLAIN (CERTIFICATE) is not available under a capability".to_owned(),
            ],
        });
    }
    let prepared = PreparedNativeRead::prepare(statement, params, resolver);
    checkpoint()?;
    let listing = crate::query::explain::explain_rows(&prepared?);
    crate::query::explain::explain_result(listing, None)
}

fn native_at<Clock: FnMut() -> u64>(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<QueryResult, QueryError> {
    execution.borrow_mut().checkpoint()?;
    // Privileged preparation grants no exception to the text ingress rule.
    // Keep refusal inside the completion check and before binding or reads.
    let result = refuse_element_identity(prepared.statement()).and_then(|()| match prepared {
        PreparedNativeRead::Pattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            let rows = pattern_at(snapshot, at, &query, scope, execution, policy)
                .map_err(query_error)?
                .value;
            Ok(values(query.columns().to_vec(), rows))
        }
        PreparedNativeRead::Aggregate(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            let rows = graph::graph_at(snapshot, at, &query, scope, execution, policy)?;
            Ok(aggregates(
                prepared.columns().to_vec(),
                prepared.output_slots(),
                rows,
            ))
        }
        PreparedNativeRead::PipelineAggregate(prepared) => {
            // Zero/multiple graph leaves require the complete relation. Every
            // leaf retains this same scope, permit and snapshot coordinate.
            let rows = if prepared.requires_relational_input() {
                let query = prepared
                    .bind_relation_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                aggregate_at(snapshot, at, &query, scope, execution, policy)?
            } else {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                graph::graph_at(snapshot, at, &query, scope, execution, policy)?
            };
            Ok(aggregates(
                prepared.columns().to_vec(),
                prepared.output_slots(),
                rows,
            ))
        }
        PreparedNativeRead::Set(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::SetText)?;
            let rows = set_at(snapshot, at, &query, scope, execution, policy)?;
            Ok(values(prepared.columns().to_vec(), rows))
        }
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            snapshot
                .check_frontier(query.as_of())
                .map_err(QueryError::Read)?;
            let rows = pattern_at(
                snapshot,
                query.as_of(),
                query.pattern(),
                scope,
                execution,
                policy,
            )
            .map_err(query_error)?
            .value;
            Ok(values(query.pattern().columns().to_vec(), rows))
        }
        PreparedNativeRead::TemporalAggregate(prepared) => {
            let query = prepared
                .bind_source_parameters(params)
                .map_err(QueryError::TemporalText)?;
            snapshot
                .check_frontier(query.as_of())
                .map_err(QueryError::Read)?;
            // Every historical source retains the same masked projection,
            // live permit and cumulative allowance as an ordinary pipeline.
            let rows = match query.aggregate() {
                fgdb_gql::TemporalAggregatePlan::Graph(aggregate) => {
                    graph::graph_at(snapshot, query.as_of(), aggregate, scope, execution, policy)?
                }
                fgdb_gql::TemporalAggregatePlan::Relation(aggregate) => {
                    aggregate_at(snapshot, query.as_of(), aggregate, scope, execution, policy)?
                }
            };
            Ok(aggregates(
                prepared.columns().to_vec(),
                prepared.output_slots(),
                rows,
            ))
        }
        PreparedNativeRead::TemporalSet(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            snapshot
                .check_frontier(query.as_of())
                .map_err(QueryError::Read)?;
            let rows = set_at(
                snapshot,
                query.as_of(),
                query.query(),
                scope,
                execution,
                policy,
            )?;
            Ok(values(prepared.columns().to_vec(), rows))
        }
    });
    // Binding and result-shaping are inside the live boundary too. A late
    // credential invalidation wins before returning even a binding error.
    execution.borrow_mut().checkpoint()?;
    result
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute native GQL text under the host-selected Warden capability.
    /// Authentication, rights and database namespace checks precede parsing,
    /// parameter binding and all symbol-resolver callbacks. The existing native
    /// classifier selects exactly one facade; a failed bind/source/execution
    /// never retries an unscoped query or a different engine.
    ///
    /// Pattern, aggregate, row-pipeline, set and temporal native reads use the
    /// same scoped graph source. Temporal selectors bind one exact cut for all
    /// leaves and cannot bypass current credential validity. One live permit
    /// covers preparation boundaries, all input work and final result delivery.
    /// The result is the ordinary lossless QueryResult, with the same aliases,
    /// aggregate output slots, NULLs, exact numeric values and row ordering.
    ///
    /// AT BRANCH may name only the exact branch already selected by the trusted
    /// host. It does not resolve another view or grant access. Literal/parameter
    /// selectors use the existing branch parser; branch-only arguments are
    /// consumed there, while all other arguments retain native validation.
    ///
    /// Keep the issuer, raw Database, catalog mapping and monotone clock in the
    /// trusted host. Ordinary Database methods are still privileged. EXPLAIN
    /// lists the text-derived plan; writes, EXPLAIN (CERTIFICATE), certified
    /// replay and unsupported classes refuse. This adds no server session,
    /// streaming delivery, spill or physical noninterference.
    #[allow(clippy::too_many_arguments)]
    pub fn query_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<QueryResult, QueryError> {
        let mut columns = Vec::new();
        let rows = authorized(
            self,
            cx,
            authority,
            token,
            branch,
            None,
            clock,
            |snapshot, at, scope, execution| {
                let selector_error =
                    |error: fgdb_gql::GraphBranchTextError| QueryError::Unsupported {
                        diagnostics: vec![error.to_string()],
                    };
                let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
                let selected = selector.bind_parameters(params).map_err(selector_error)?;
                if selected.branch().is_some_and(|name| name != branch) {
                    return Err(QueryError::Authorization(fgdb_warden::Error::ScopeDenied));
                }
                refuse_element_identity(selected.statement())?;
                execution.borrow_mut().checkpoint()?;
                if let Some(explain) = crate::query::explain::explain_prefix(selected.statement()) {
                    return rows_of(
                        explain_authorized(explain, selected.parameters(), resolver, || {
                            execution.borrow_mut().checkpoint()
                        })?,
                        &mut columns,
                    );
                }
                let prepared = PreparedNativeRead::prepare(
                    selected.statement(),
                    selected.parameters(),
                    resolver,
                );
                // No RefCell borrow spans caller-controlled resolver code. Check
                // invalidation even when preparation itself reports an error.
                execution.borrow_mut().checkpoint()?;
                let prepared = prepared?;
                rows_of(
                    native_at(
                        &prepared,
                        selected.parameters(),
                        snapshot,
                        at,
                        scope,
                        execution,
                        policy,
                    )?,
                    &mut columns,
                )
            },
        )?;
        Ok(QueryResult::Rows { columns, rows })
    }
}

impl PreparedNativeRead {
    /// Rebind this complete native template against a freshly authenticated
    /// capability, without changing its class, symbol mapping or definition.
    /// Reuse never freezes permissions: each invocation checks the current
    /// host authority and token before binding parameters or reading a source.
    /// A narrower token masks all leaves before evaluation, including an old
    /// temporal cut; retired/expired credentials cannot reuse prepared access.
    ///
    /// The template itself grants nothing. Its catalog must match the trusted
    /// host's graph/branch mapping. All ordinary authorized-source limitations
    /// apply. Use Database::query_authorized for textual AT BRANCH selectors;
    /// a PreparedNativeRead already contains the branch-free native template.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_authorized<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<QueryResult, QueryError> {
        let mut columns = Vec::new();
        let rows = authorized(
            database,
            cx,
            authority,
            token,
            branch,
            None,
            clock,
            |snapshot, at, scope, execution| {
                rows_of(
                    native_at(self, params, snapshot, at, scope, execution, policy)?,
                    &mut columns,
                )
            },
        )?;
        Ok(QueryResult::Rows { columns, rows })
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::{LabelId, SchemaEpoch};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use fgdb_warden::{Error as AuthorizationError, Grant, QueryLimits, Scope};

    const BRANCH: &str = "prepared-identity";
    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);

    fn grant(nodes: u64) -> Grant {
        let mut grant = Grant::read_only(
            BRANCH,
            1000,
            QueryLimits {
                max_nodes: nodes,
                max_work: 1_000_000,
                max_rows: nodes,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.properties = Scope::only([PropertyKeyId(1)]);
        grant
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
    }
    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p" | "id") => {
                Some(GraphSymbol::Property(PropertyKeyId(1)))
            }
            _ => None,
        }
    }
    async fn database(cx: &fgdb_types::CommitCx) -> Database<crate::MemVfs> {
        let mut db =
            Database::open_memory(cx, crate::DatabaseKeys::new([0x37; 32], NS, [0x95; 32]))
                .await
                .unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for (id, label, value) in [(1, 1, 10), (2, 99, 20), (3, 1, 30)] {
            batch.create_vertex(
                VId(id),
                vec![LabelId(label)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        db.write(cx, batch).await.unwrap();
        db
    }

    #[test]
    fn privileged_templates_cannot_bypass_identity_admission_even_without_source_budget() {
        let ((), report) = run_async_under_lab(0x5ec0_6901, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let authority =
                Authority::new(AuthKey::from_seed(901), NS, "graph", SchemaEpoch(0), 1).unwrap();
            let token = authority.issue_at(&grant(0), 100).unwrap();
            let params = GqlParameters::new();
            let invalid = GqlParameters::new().with_int64("unexpected", 1).unwrap();
            for text in [
                "MATCH (n:L) RETURN id(n) AS key",
                "MATCH (n:L) RETURN elementId(n) AS key",
                "MATCH (n:L) RETURN id /* comment */ (n) AS key LIMIT 0",
                "MATCH (n:L) WITH elementId(n) AS key RETURN key",
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN id(n) AS key",
                "MATCH (n:L) RETURN id(n) AS key UNION ALL MATCH (m:L) RETURN id(m) AS key",
            ] {
                // Ordinary preparation is privileged and must remain legal.
                let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
                for arguments in [&params, &invalid] {
                    let result = prepared.execute_authorized(
                        &db,
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        arguments,
                        policy(),
                        || 100,
                    );
                    assert!(
                        matches!(
                            result,
                            Err(QueryError::Authorization(AuthorizationError::ScopeDenied))
                        ),
                        "identity template reached binding/source admission: {text}: {result:?}"
                    );
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordinary_aliases_properties_literals_and_live_credential_precedence_are_preserved() {
        let ((), report) = run_async_under_lab(0x5ec0_6902, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let authority =
                Authority::new(AuthKey::from_seed(902), NS, "graph", SchemaEpoch(0), 1).unwrap();
            let token = authority.issue_at(&grant(1000), 100).unwrap();
            let params = GqlParameters::new();
            for text in [
                "MATCH (n:L) RETURN n.p AS id",
                "MATCH (n:L) RETURN n.id AS elementId",
                "RETURN 'id(n)' AS text",
            ] {
                let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
                let expected = db
                    .query_authorized(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        text,
                        &params,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                assert_eq!(
                    prepared
                        .execute_authorized(
                            &db,
                            &cx,
                            &authority,
                            &token,
                            BRANCH,
                            &params,
                            policy(),
                            || 100,
                        )
                        .unwrap(),
                    expected
                );
            }
            let prepared =
                PreparedNativeRead::prepare("MATCH (n:L) RETURN id(n) AS key", &params, symbols)
                    .unwrap();
            assert!(matches!(
                prepared.execute_authorized(
                    &db,
                    &cx,
                    &authority,
                    &token,
                    BRANCH,
                    &params,
                    policy(),
                    || 1000,
                ),
                Err(QueryError::Authorization(AuthorizationError::Expired))
            ));
            assert!(authority.retire());
            assert!(matches!(
                prepared.execute_authorized(
                    &db,
                    &cx,
                    &authority,
                    &token,
                    BRANCH,
                    &params,
                    policy(),
                    || 100,
                ),
                Err(QueryError::Authorization(
                    AuthorizationError::AuthorityRetired
                ))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
