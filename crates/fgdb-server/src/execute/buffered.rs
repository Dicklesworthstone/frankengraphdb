//! Authenticated cold source selection under the existing served-generation owner.

use super::*;
use asupersync::fs::UnixVfs;
use fgdb::{
    AuthorizedBufferedError, AuthorizedBufferedReadSource, BufferedOpenError, BufferedReadError,
};
use fgdb_delta_types::catalog::SymbolCatalog;
use fgdb_gql::GqlParameters;
use fgdb_strata::tiered::buffer::BufferError;
use fgdb_types::QueryCx;
use std::sync::Arc;

pub(crate) struct BufferedSession<'a> {
    native: fgdb::AuthorizedReadSession<
        'a,
        crate::Symbols,
        fn() -> u64,
        AuthorizedBufferedReadSource<UnixVfs>,
    >,
    generation: Generation,
    // Native owners retire before another source may occupy this slot.
    _lease: crate::buffered::BufferedLease,
}

/// One writer publication and its exact catalog binding. Both source streaming
/// and external ordering consume this owner; neither can reopen a newer root
/// after the writer lock has been released.
pub(super) struct PreparedBufferedPublication {
    pub(super) publication: fgdb::BufferedReadPublication<UnixVfs>,
    pub(super) selected: fgdb_types::CommitSeq,
    pub(super) definition: PreparedRead,
    pub(super) lease: crate::buffered::BufferedLease,
}

pub(super) fn resolve_publication_catalog(
    db: &Served,
    definition: &PreparedRead,
    catalog: &SymbolCatalog,
) -> Result<crate::Symbols, String> {
    let symbols = db
        .symbols
        .with_catalog(catalog)
        .map_err(|_| "configured bindings conflict with the publication catalog".to_owned())?;
    if !symbols.matches_catalog(catalog) || !same_symbols(&definition.symbols, &symbols) {
        return Err("prepared bindings do not match the exact publication catalog".to_owned());
    }
    Ok(symbols)
}

impl BufferedSession<'_> {
    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }

    pub(crate) fn stream_prepared<'q>(
        &'q mut self,
        query: &'q QueryCx,
        prepared: &fgdb::AuthorizedPreparedRead,
        parameters: &GqlParameters,
    ) -> Result<ReadCursor<'q>, Refusal> {
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        let cursor = self.native.stream(query, prepared, parameters);
        generation.check().map_err(Refusal::from)?;
        Ok(ReadCursor {
            native: ReadSource::Buffered(cursor.map_err(refusal)?),
            generation,
        })
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) async fn read_buffered_session_for<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
    text: &str,
    parameters: &GqlParameters,
) -> Result<(BufferedSession<'a>, fgdb::AuthorizedPreparedRead), Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let PreparedBufferedPublication {
        publication,
        selected,
        definition,
        lease,
    } = capture_prepared_publication_for(cx, db, token, prepared, &generation, text, parameters)
        .await?;
    // The capture's read lock is gone. Recovery may fence any Pending I/O;
    // every poll and final result retains the original generation checks.
    let (native, prepared) = ordered::in_generation(&generation, async {
        publication
            .into_authorized_prepared_read_session_with_resolver(
                &contexts.commit(),
                &query,
                &db.authority,
                token,
                TRUNK,
                selected,
                db.query_policy,
                unix_millis as fn() -> u64,
                |catalog| resolve_publication_catalog(db, &definition, &catalog),
                Arc::clone(&definition.selector),
                Arc::clone(&definition.native),
                definition.lowered.clone(),
            )
            .await
            .map_err(refusal)
    })
    .await?;
    Ok((
        BufferedSession {
            native,
            generation,
            _lease: lease,
        },
        prepared,
    ))
}

/// Capture catalog and root while borrowing the same healthy writer, then
/// release that borrow before cold admission. No resident Snapshot enters the
/// returned owner, and no path or independently reopened root is selected.
#[allow(clippy::too_many_arguments)]
pub(super) async fn capture_prepared_publication_for(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: &Generation,
    text: &str,
    parameters: &GqlParameters,
) -> Result<PreparedBufferedPublication, Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut admission = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    if let Some(prepared) = prepared {
        prepared.generation.check().map_err(Refusal::from)?;
    }
    admission
        .charge_work_at(
            unix_millis(),
            (if prepared.is_some() {
                0
            } else {
                text.len() as u64
            })
            .saturating_add(parameters.canonical_byte_len() as u64),
        )
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    let cut = query.with_restriction(|| match prepared {
        Some(prepared) => prepared.catalog_cut(parameters),
        None => statement_catalog_cut(text, parameters),
    });
    admission
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    let cut = cut?;
    let host = db.buffered_reads.as_ref().ok_or_else(|| {
        Refusal::new(
            ErrorCode::Statement,
            "buffered source reads are not enabled for this database",
        )
    })?;
    let (pool, limits, lease) = host.admit()?;
    let (publication, selected, definition) = ordered::in_generation(generation, async {
        let guard = db.db.read(cx).await.map_err(Refusal::from)?;
        admission.checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query.checkpoint().map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let publication = guard.buffered_read_publication(&query, pool, limits)
            .map_err(|error| refusal(AuthorizedBufferedError::Open(error)))?;
        let selected = cut.unwrap_or(publication.frontier());
        let definition = query.with_restriction(|| {
            let catalog = guard.symbol_catalog_at(selected).map_err(|_| Refusal::new(
                ErrorCode::Statement, "the requested buffered snapshot catalog is unavailable",
            ))?;
            let symbols = db.symbols.with_catalog(&catalog).map_err(|_| Refusal::new(
                ErrorCode::Conflict, "configured symbol bindings conflict with the selected catalog",
            ))?;
            if !symbols.matches_catalog(&catalog) {
                return Err(Refusal::new(ErrorCode::Statement,
                    "buffered source reads require durable names at the selected snapshot"));
            }
            match prepared {
                Some(prepared) => {
                    if !same_symbols(&prepared.symbols, &symbols) {
                        return Err(Refusal::new(ErrorCode::Conflict,
                            "the prepared statement's symbol catalog changed; prepare it again at the selected snapshot"));
                    }
                    Ok(prepared.clone())
                }
                None => super::prepared::prepare_read_with_symbols(
                    generation.clone(), text, parameters, |_, _| Ok(symbols),
                ),
            }
        });
        admission.checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query.checkpoint().map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        guard.generation().check().map_err(Refusal::from)?;
        Ok((publication, selected, definition?))
    }).await?;
    Ok(PreparedBufferedPublication {
        publication,
        selected,
        definition,
        lease,
    })
}

/// Physical failures keep a stable public class and never disclose a path,
/// object id, corrupt hidden vertex id or unscoped storage diagnostic.
pub(crate) fn refusal(error: AuthorizedBufferedError) -> Refusal {
    let code = match error {
        AuthorizedBufferedError::Query(error) => return query_refusal(error),
        AuthorizedBufferedError::DifferentCut { .. } => ErrorCode::Statement,
        AuthorizedBufferedError::CatalogBinding(_) => ErrorCode::Conflict,
        AuthorizedBufferedError::Read(ref error) => read_code(error),
        AuthorizedBufferedError::Open(BufferedOpenError::Read(ref error)) => read_code(error),
        AuthorizedBufferedError::Open(BufferedOpenError::Memory(_)) => ErrorCode::Budget,
        AuthorizedBufferedError::Open(BufferedOpenError::RecoveryRequired) => {
            ErrorCode::DatabaseRecovering
        }
        AuthorizedBufferedError::Open(BufferedOpenError::Open(ref error)) => open_code(error),
        AuthorizedBufferedError::Catalog(fgdb::SymbolCatalogError::Storage(ref error)) => {
            rebuild_code(error)
        }
        AuthorizedBufferedError::Catalog(_) => ErrorCode::Execution,
    };
    Refusal::new(
        code,
        match code {
            ErrorCode::Budget => "buffered source physical allowance exceeded",
            ErrorCode::Cancelled => "buffered source read cancelled",
            ErrorCode::Statement => "buffered source cannot use the requested snapshot",
            ErrorCode::Conflict => "buffered source bindings do not match the selected catalog",
            ErrorCode::DatabaseRecovering => "database publication requires recovery",
            _ => "buffered source read failed",
        },
    )
}

// OpenError, RebuildError and ExtentError do not expose a
// generic source() chain. Inspect their typed wrappers explicitly so a nested
// physical allowance or interruption cannot become an execution failure.
fn open_code(error: &fgdb::OpenError) -> ErrorCode {
    match error {
        fgdb::OpenError::Io(error) => io_code(error),
        fgdb::OpenError::Store(error) => store_code(error),
        fgdb::OpenError::Rebuild(error) => rebuild_code(error),
        _ => ErrorCode::Execution,
    }
}

fn rebuild_code(error: &fgdb::RebuildError) -> ErrorCode {
    match error {
        fgdb::RebuildError::Interrupted(_) => ErrorCode::Cancelled,
        fgdb::RebuildError::Store(error) => store_code(error),
        _ => ErrorCode::Execution,
    }
}

fn io_code(error: &std::io::Error) -> ErrorCode {
    if error.kind() == std::io::ErrorKind::Interrupted {
        ErrorCode::Cancelled
    } else {
        ErrorCode::Execution
    }
}

fn read_code(error: &BufferedReadError) -> ErrorCode {
    match error {
        BufferedReadError::Interrupted(_) => ErrorCode::Cancelled,
        BufferedReadError::Memory(_)
        | BufferedReadError::Limit { .. }
        | BufferedReadError::SizeOverflow => ErrorCode::Budget,
        BufferedReadError::Buffer(BufferError::Cancelled(_)) => ErrorCode::Cancelled,
        BufferedReadError::Buffer(
            BufferError::Memory(_)
            | BufferError::CacheSlotsExhausted { .. }
            | BufferError::ExtentTooLarge { .. },
        ) => ErrorCode::Budget,
        BufferedReadError::Buffer(BufferError::Load(error)) => io_code(error),
        BufferedReadError::Store(error) => store_code(error),
        BufferedReadError::BeyondPublication { .. } => ErrorCode::Statement,
        _ => ErrorCode::Execution,
    }
}

fn store_code(error: &fgdb_strata::store::StoreError) -> ErrorCode {
    use fgdb_strata::store::{ExtentError, StoreError};
    use fgdb_strata::tiered::sealed::AdjRunError;
    match error {
        StoreError::Io(error) => io_code(error),
        StoreError::ObjectTooLarge { .. }
        | StoreError::RootReferenceLimit { .. }
        | StoreError::Extent(ExtentError::Memory(_) | ExtentError::PlaintextTooLarge { .. }) => {
            ErrorCode::Budget
        }
        StoreError::RootBlockLoad { error, .. }
        | StoreError::RootPatchLoad { error, .. }
        | StoreError::ManifestRootLoad { error, .. }
        | StoreError::BlockPatchLoad { error, .. } => store_code(error),
        StoreError::MalformedAdjRun(error) => match error.as_ref() {
            AdjRunError::Sealed(error) => sealed_code(error),
            _ => ErrorCode::Execution,
        },
        _ => ErrorCode::Execution,
    }
}

fn sealed_code(error: &fgdb_strata::tiered::sealed::SealedError) -> ErrorCode {
    use fgdb_strata::tiered::sealed::SealedError;
    if error.is_admission_limit() {
        return ErrorCode::Budget;
    }
    match error {
        SealedError::Interrupted(_) => ErrorCode::Cancelled,
        SealedError::Store(error) => store_code(error),
        SealedError::BufferedSource(error) => read_code(error),
        _ => ErrorCode::Execution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::tests::{fail_after_marker, served_with_config, settled, vertex};
    use asupersync::lab::run_async_under_lab;

    fn accepted<T>(result: Result<T, Refusal>) -> T {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
    }

    fn refused<T>(result: Result<T, Refusal>) -> Refusal {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected refusal"),
        }
    }

    #[test]
    fn wrapped_storage_refusals_preserve_resource_classes_and_hide_diagnostics() {
        use fgdb_strata::store::{ExtentError, StoreError};
        use fgdb_strata::tiered::sealed::{AdjRunError, SealedError};

        let open = |error| AuthorizedBufferedError::Open(BufferedOpenError::Open(error));
        let catalog = |error| {
            AuthorizedBufferedError::Catalog(fgdb::SymbolCatalogError::Storage(Box::new(error)))
        };
        let memory = fgdb::MemoryError::ResourceExhausted {
            requested: 4096,
            available: 0,
            limit: 1024,
        };
        let private = "private-object-path-and-hidden-identity";
        let cases = [
            (
                open(fgdb::OpenError::Store(StoreError::Extent(
                    ExtentError::Memory(memory),
                ))),
                ErrorCode::Budget,
            ),
            (
                open(fgdb::OpenError::Rebuild(fgdb::RebuildError::Store(
                    StoreError::ObjectTooLarge {
                        limit: 8,
                        observed: 9,
                    },
                ))),
                ErrorCode::Budget,
            ),
            (
                catalog(fgdb::RebuildError::Store(StoreError::RootPatchLoad {
                    at: 99,
                    error: Box::new(StoreError::RootReferenceLimit {
                        resource: private,
                        requested: 2,
                        limit: 1,
                    }),
                })),
                ErrorCode::Budget,
            ),
            (
                AuthorizedBufferedError::Read(BufferedReadError::Store(Box::new(
                    StoreError::MalformedAdjRun(Box::new(AdjRunError::Sealed(SealedError::Store(
                        Box::new(StoreError::Extent(ExtentError::Memory(memory))),
                    )))),
                ))),
                ErrorCode::Budget,
            ),
            (
                open(fgdb::OpenError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    private,
                ))),
                ErrorCode::Cancelled,
            ),
            (
                catalog(fgdb::RebuildError::Store(StoreError::RootBlockLoad {
                    at: 99,
                    error: Box::new(StoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        private,
                    ))),
                })),
                ErrorCode::Cancelled,
            ),
            (
                AuthorizedBufferedError::Read(BufferedReadError::Buffer(BufferError::Load(
                    std::io::Error::new(std::io::ErrorKind::Interrupted, private),
                ))),
                ErrorCode::Cancelled,
            ),
            (
                AuthorizedBufferedError::Read(BufferedReadError::VertexHistoryConflict {
                    vid: fgdb_types::VId(0xdead_beef),
                }),
                ErrorCode::Execution,
            ),
            (
                open(fgdb::OpenError::Store(StoreError::Io(
                    std::io::Error::other(private),
                ))),
                ErrorCode::Execution,
            ),
            (
                AuthorizedBufferedError::CatalogBinding(private.to_owned()),
                ErrorCode::Conflict,
            ),
        ];
        for (error, code) in cases {
            let refusal = refusal(error);
            assert_eq!(refusal.code, code);
            assert_eq!(
                refusal.message,
                match code {
                    ErrorCode::Budget => "buffered source physical allowance exceeded",
                    ErrorCode::Cancelled => "buffered source read cancelled",
                    ErrorCode::Conflict =>
                        "buffered source bindings do not match the selected catalog",
                    _ => "buffered source read failed",
                }
            );
        }
    }

    #[test]
    fn buffered_pool_refusals_refund_slots_and_authentication_precedes_stale_generations() {
        let ((), report) = run_async_under_lab(0x79a0_b003, |root| async move {
            let (server, token, _) = served_with_config(&root, "buffered-pool-auth", |config| {
                let mut buffered = crate::BufferedReadConfig::new(8 * 1024 * 1024);
                buffered.max_concurrent = 1;
                config.buffered_reads = Some(buffered);
            })
            .await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let query = crate::query_cx(&contexts);
            let parameters = GqlParameters::new();
            let statement = "MATCH (n) RETURN n";
            let pool = db.buffered_reads.as_ref().unwrap().pool();
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(1))
                .await
                .unwrap();
            let generation = accepted(ordered::ordered_generation(db, &token));
            let held = pool.reserve(&query, pool.available()).unwrap();
            let error = refused(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    generation.clone(),
                    statement,
                    &parameters,
                )
                .await,
            );
            assert_eq!(error.code, ErrorCode::Budget);
            assert_eq!(error.message, "buffered source physical allowance exceeded");
            assert_eq!(pool.used(), pool.limit());
            drop(held);
            assert_eq!(pool.used(), 0);
            let opened = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    generation.clone(),
                    statement,
                    &parameters,
                )
                .await,
            );
            assert!(pool.used() > 0);
            drop(opened);
            assert_eq!(pool.used(), 0);

            fail_after_marker(&root, db, 2).await;
            assert!(matches!(
                settled(db).await,
                crate::DatabaseStatus::Ready { .. }
            ));
            let expired = db
                .authority
                .issue_at(
                    &fgdb_warden::Grant::read_only(
                        TRUNK,
                        1,
                        fgdb_warden::QueryLimits {
                            max_nodes: 100,
                            max_work: 100,
                            max_rows: 100,
                        },
                    ),
                    0,
                )
                .unwrap();
            assert_eq!(
                refused(
                    read_buffered_session_for(
                        &root,
                        db,
                        &expired,
                        None,
                        generation.clone(),
                        statement,
                        &parameters,
                    )
                    .await
                )
                .code,
                ErrorCode::PermissionDenied
            );
            assert_eq!(
                refused(
                    read_buffered_session_for(
                        &root,
                        db,
                        &token,
                        None,
                        generation,
                        statement,
                        &parameters,
                    )
                    .await
                )
                .code,
                ErrorCode::DatabaseRecovering
            );
            let opened = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    accepted(ordered::ordered_generation(db, &token)),
                    statement,
                    &parameters,
                )
                .await,
            );
            drop(opened);
            assert_eq!(pool.used(), 0);
            server.finish_shutdown(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn buffered_source_pins_publication_releases_admission_and_obeys_recovery() {
        let ((), report) = run_async_under_lab(0xb0ff_2026, |root| async move {
            let (server, token, _) = served_with_config(&root, "buffered-source-owner", |config| {
                let mut buffered = crate::BufferedReadConfig::new(64 * 1024 * 1024);
                buffered.max_concurrent = 1;
                config.buffered_reads = Some(buffered);
            })
            .await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let query = crate::query_cx(&contexts);
            let parameters = GqlParameters::new();
            let statement = "MATCH (n) RETURN n ORDER BY n";
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(1))
                .await
                .unwrap();
            let generation = accepted(ordered::ordered_generation(db, &token));
            let (mut session, prepared) = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    generation,
                    statement,
                    &parameters,
                )
                .await,
            );
            let mut source = accepted(session.stream_prepared(&query, &prepared, &parameters));
            // The source retains no writer lock and no resident Snapshot.
            // Advancing the writer before the first pull preserves cut 1.
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(2))
                .await
                .unwrap();
            assert_eq!(source.snapshot_seq(), 1);
            assert_eq!(
                accepted(source.next().await),
                Some(vec![WireValue::Vertex(1)])
            );
            assert_eq!(accepted(source.next().await), None);
            let busy = refused(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    accepted(ordered::ordered_generation(db, &token)),
                    statement,
                    &parameters,
                )
                .await,
            );
            assert_eq!(busy.code, ErrorCode::Budget);
            drop(source);
            drop(session);

            let (mut current, prepared) = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    accepted(ordered::ordered_generation(db, &token)),
                    statement,
                    &parameters,
                )
                .await,
            );
            let mut source = accepted(current.stream_prepared(&query, &prepared, &parameters));
            assert_eq!(source.snapshot_seq(), 2);
            assert_eq!(
                accepted(source.next().await),
                Some(vec![WireValue::Vertex(1)])
            );
            assert_eq!(
                accepted(source.next().await),
                Some(vec![WireValue::Vertex(2)])
            );
            drop(source);
            drop(current);

            let (mut fenced, prepared) = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    accepted(ordered::ordered_generation(db, &token)),
                    statement,
                    &parameters,
                )
                .await,
            );
            let mut source = accepted(fenced.stream_prepared(&query, &prepared, &parameters));
            fail_after_marker(&root, db, 3).await;
            assert_eq!(
                refused(source.next().await).code,
                ErrorCode::DatabaseRecovering
            );
            drop(source);
            drop(fenced);
            assert!(matches!(
                settled(db).await,
                crate::DatabaseStatus::Ready { .. }
            ));
            let (mut recovered, prepared) = accepted(
                read_buffered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    accepted(ordered::ordered_generation(db, &token)),
                    statement,
                    &parameters,
                )
                .await,
            );
            let mut source = accepted(recovered.stream_prepared(&query, &prepared, &parameters));
            assert_eq!(source.snapshot_seq(), 3);
            let mut rows = Vec::new();
            while let Some(row) = accepted(source.next().await) {
                rows.push(row);
            }
            assert_eq!(
                rows,
                [
                    vec![WireValue::Vertex(1)],
                    vec![WireValue::Vertex(2)],
                    vec![WireValue::Vertex(3)]
                ]
            );
            drop(source);
            drop(recovered);
            server.finish_shutdown(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
