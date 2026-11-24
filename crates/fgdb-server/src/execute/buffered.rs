//! Authenticated cold source selection under the existing served-generation owner.

use super::*;
use asupersync::fs::UnixVfs;
use fgdb::{
    AuthorizedBufferedError, AuthorizedBufferedReadSource, BufferedOpenError, BufferedReadError,
};
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

/// Capture catalog and root while borrowing the same healthy writer, then
/// release that borrow before cold admission. No resident Snapshot enters the
/// returned session, and no path or independently reopened root is selected.
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
    let (publication, selected, definition) = ordered::in_generation(&generation, async {
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
                |catalog| {
                    let symbols = db.symbols.with_catalog(&catalog).map_err(|_| {
                        "configured bindings conflict with the publication catalog".to_owned()
                    })?;
                    if !symbols.matches_catalog(&catalog)
                        || !same_symbols(&definition.symbols, &symbols)
                    {
                        return Err(
                            "prepared bindings do not match the exact publication catalog"
                                .to_owned(),
                        );
                    }
                    Ok(symbols)
                },
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

/// Physical failures keep a stable public class and never disclose a path,
/// object id, corrupt hidden vertex id or unscoped storage diagnostic.
pub(crate) fn refusal(error: AuthorizedBufferedError) -> Refusal {
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
            BufferedReadError::BeyondPublication { .. } => ErrorCode::Statement,
            _ => ErrorCode::Execution,
        }
    }
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
        AuthorizedBufferedError::Catalog(_) | AuthorizedBufferedError::Open(_) => {
            ErrorCode::Execution
        }
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
