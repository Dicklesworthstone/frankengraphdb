//! Credit-driven native source reads, with one retained session and permit.
//! A frame and at most one row awaiting byte credit are the adapter's only row
//! buffers. Native aggregates retain their governed group/support state and
//! consume the input cooperatively on first demand, then move completed groups
//! through this same delivery path without rescanning the source graph. The
//! metadata chunk announces the child before any input is demanded, so its
//! cancellation can be serviced during accumulation.

use super::*;
use crate::execute::{PreparedRead, read_prepared_session, read_session};
use fgdb_types::PurposeContexts;

pub(super) enum SourceStatement<'a> {
    Text(&'a str),
    Prepared(&'a PreparedRead),
}

pub(super) struct Chunks {
    columns: Option<Vec<String>>,
    pending: Option<(Vec<WireValue>, usize)>,
    ended: bool,
}

impl Chunks {
    pub(super) fn new(columns: Vec<String>) -> Self {
        Self {
            columns: Some(columns),
            pending: None,
            ended: false,
        }
    }

    pub(super) fn done(&self) -> bool {
        self.ended && self.pending.is_none() && self.columns.is_none()
    }

    /// Pull only when there is credit for a row and its smallest possible
    /// encoding. Exact byte length is known after native projection, so one
    /// row may wait for byte credit. It is never pulled again or requeries.
    pub(super) fn next(
        &mut self,
        byte_room: usize,
        row_room: usize,
        frame_budget: usize,
        mut pull: impl FnMut() -> Result<(Option<Vec<WireValue>>, bool), Refusal>,
    ) -> Result<Option<ResultChunk>, Refusal> {
        let mut size = self.columns.as_ref().map_or(0, |names| {
            4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
        });
        if size > frame_budget {
            return Err(Refusal::new(
                ErrorCode::Execution,
                "the column list exceeds the negotiated frame limit",
            ));
        }
        if size > byte_room {
            return Ok(None);
        }
        let mut rows = Vec::new();
        while rows.len() < row_room {
            if self.pending.is_none() {
                // The row's column count alone requires four bytes. A header
                // or column-only grant must not drive graph candidates.
                if self.ended || size.saturating_add(4) > byte_room {
                    break;
                }
                let (row, ended) = pull()?;
                let empty = row.is_none();
                self.accept(row, ended || empty, frame_budget)?;
            }
            let Some((_, length)) = &self.pending else {
                break;
            };
            if size.saturating_add(*length) > byte_room {
                break;
            }
            size += *length;
            rows.push(self.pending.take().expect("measured pending row").0);
        }
        if rows.is_empty() && self.columns.is_none() {
            return Ok(None);
        }
        Ok(Some(ResultChunk {
            columns: self.columns.take(),
            rows,
        }))
    }

    /// Native reads can arrive asynchronously. Admit exactly one pending row
    /// outside the synchronous chunk assembler, then share its measuring and
    /// credit rules. No row future starts without room for its minimum frame.
    pub(super) fn needs_row(&self, byte_room: usize, row_room: usize) -> bool {
        let metadata = self.columns.as_ref().map_or(0, |names| {
            4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
        });
        self.pending.is_none()
            && !self.ended
            && row_room > 0
            && metadata.saturating_add(4) <= byte_room
    }

    pub(super) fn accept(
        &mut self,
        row: Option<Vec<WireValue>>,
        ended: bool,
        frame_budget: usize,
    ) -> Result<(), Refusal> {
        // row_len validates/encodes recursively. Measure once, never again
        // for a ping or insufficient credit grant.
        self.pending = match row {
            Some(row) => {
                let length = ResultChunk::row_len(&row).map_err(|_| {
                    Refusal::new(
                        ErrorCode::Execution,
                        "a result value exceeds the wire bounds",
                    )
                })?;
                if length > frame_budget {
                    return Err(Refusal::new(
                        ErrorCode::Execution,
                        "a result row exceeds the negotiated frame limit",
                    ));
                }
                Some((row, length))
            }
            None => None,
        };
        self.ended = ended || self.pending.is_none();
        Ok(())
    }
}

impl Lane {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn source_read(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        stream: StreamId,
        binding: Binding,
        statement: SourceStatement<'_>,
        parameters: &[(String, WireValue)],
    ) -> Result<(), Stop> {
        let parameters = match crate::convert::parameters(parameters, None) {
            Ok(parameters) => parameters,
            Err(error) => {
                return self
                    .refuse(
                        cx,
                        request,
                        stream,
                        binding,
                        ErrorCode::Statement,
                        &error.to_string(),
                    )
                    .await;
            }
        };
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        let opened = match statement {
            SourceStatement::Text(text) => match read_session(cx, db, token).await {
                Ok((mut session, _)) => {
                    self.send_generation = Some(session.generation());
                    session
                        .prepare(&query, text, &parameters)
                        .map(|prepared| (session, prepared))
                }
                Err(error) => Err(error),
            },
            SourceStatement::Prepared(prepared) => {
                read_prepared_session(cx, db, token, prepared).await
            }
        };
        let (mut session, prepared) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        self.send_generation = Some(session.generation());
        let mut source = match session.stream_prepared(&query, &prepared, &parameters) {
            Ok(source) => source,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        let outcome = Outcome::Rows {
            seq: source.snapshot_seq(),
        };
        let mut chunks = Chunks::new(source.columns().to_vec());
        let mut window = FlowWindow::new(self.initial_window, self.maximum_window)
            .map_err(|_| Stop::Transport)?;
        let framing = binding.header_len() + CHUNK_OVERHEAD;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let mut delivered = 0u64;
        while !chunks.done() {
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            // Announce the real stream identity before polling an aggregate.
            // Metadata uses byte credit but spends no input or output rows.
            let announcing = chunks.columns.is_some();
            if source.is_aggregate() && !announcing && chunks.needs_row(byte_room, row_room) {
                let row = match self
                    .read_progress(
                        cx,
                        waiter,
                        &mut window,
                        stream,
                        request,
                        binding,
                        source.next(),
                    )
                    .await?
                {
                    Ok(row) => row,
                    Err(error) => {
                        return self
                            .refuse(cx, request, stream, binding, error.code, &error.message)
                            .await;
                    }
                };
                if let Err(error) = chunks.accept(row, source.exhausted(), frame_budget) {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            }
            // Credit received while the retained row future was Pending is
            // already available. Never wait for a redundant second grant.
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = if announcing {
                0
            } else {
                usize::try_from(available.rows).unwrap_or(usize::MAX)
            };
            let next = if source.is_aggregate() {
                chunks.next(byte_room, row_room.min(1), frame_budget, || {
                    Err(Refusal::new(
                        ErrorCode::Execution,
                        "native aggregate row was not admitted",
                    ))
                })
            } else {
                chunks.next(byte_room, row_room, frame_budget, || {
                    let row = source.next_immediate()?;
                    Ok((row, source.exhausted()))
                })
            };
            let chunk = match next {
                Ok(Some(chunk)) => chunk,
                Ok(None) if chunks.done() => break,
                Ok(None) => {
                    self.await_credit(cx, waiter, &mut window, stream, request, binding)
                        .await?;
                    continue;
                }
                Err(error) => {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            };
            let count = chunk.rows.len() as u64;
            let payload = match chunk.encode() {
                Ok(payload) => payload,
                Err(_) => {
                    return self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Execution,
                            "a result value exceeds the wire bounds",
                        )
                        .await;
                }
            };
            let frame = Frame::new(
                FrameKind::SnapshotResultChunk,
                request,
                stream,
                binding,
                payload,
                self.send_limits,
            )
            .map_err(|_| Stop::Transport)?;
            // Existing output guards recheck live capability and recovery at
            // each physical write/flush, including after suspended byte credit.
            self.credited(cx, waiter, &mut window, stream, &frame, count)
                .await?;
            delivered += count;
        }
        let end = ResultEnd {
            outcome,
            rows: delivered,
        };
        let frame = Frame::new(
            FrameKind::SnapshotResultEnd,
            request,
            stream,
            binding,
            end.encode().map_err(|_| Stop::Transport)?,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, &mut window, stream, &frame, 0)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolKind};
    use fgdb_types::{EId, VId};
    use fgdb_warden::{Grant, QueryLimits, Scope};

    /// A read-only grant that sees every label. `Grant::read_only` scopes
    /// labels to `Only(∅)`, whose one clause no vertex satisfies, an unlabeled
    /// one included: the input would be invisible and never charge a node.
    fn visible_read(limits: QueryLimits) -> Grant {
        Grant {
            labels: Scope::All,
            ..Grant::read_only(crate::TRUNK, u64::MAX / 2, limits)
        }
    }

    fn accepted<T>(result: Result<T, Refusal>) -> T {
        result.unwrap_or_else(|error| panic!("unexpected refusal: {}", error.message))
    }

    #[test]
    fn zero_row_or_payload_credit_never_pulls_and_byte_wait_retains_one_row() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let metadata = accepted(chunks.next(9, 0, 128, || panic!("no row credit"))).unwrap();
        assert_eq!(metadata.columns, Some(vec!["n".into()]));
        assert!(metadata.rows.is_empty());
        assert!(accepted(chunks.next(0, 4, 128, || panic!("no payload credit"))).is_none());
        assert!(accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).is_none());

        let mut pulls = 0;
        let row = vec![WireValue::Text("waiting for byte credit".into())];
        let size = ResultChunk::row_len(&row).unwrap();
        assert!(
            accepted(chunks.next(4, 1, 128, || {
                pulls += 1;
                Ok((Some(row.clone()), false))
            }))
            .is_none()
        );
        assert_eq!(pulls, 1);
        for bytes in [0, 4, size - 1] {
            assert!(
                accepted(chunks.next(bytes, 10, 128, || panic!("pending row was repulled")))
                    .is_none()
            );
        }
        let ready =
            accepted(chunks.next(size, 1, 128, || panic!("pending row was repulled"))).unwrap();
        assert_eq!(ready.rows, [row]);
        assert!(!chunks.done());
        assert!(
            accepted(chunks.next(128, 1, 128, || {
                pulls += 1;
                Ok((None, true))
            }))
            .is_none()
        );
        assert!(chunks.done());
        assert_eq!(pulls, 2);
    }

    #[test]
    fn exact_row_credit_never_reads_a_suffix_and_native_limit_needs_no_eof_pull() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let mut pulls = 0;
        let first = accepted(chunks.next(128, 1, 128, || {
            pulls += 1;
            Ok((Some(vec![WireValue::Int(pulls)]), false))
        }))
        .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Int(1)]]);
        assert_eq!(pulls, 1);
        assert!(accepted(chunks.next(128, 0, 128, || panic!("exhausted row credit"))).is_none());
        let last = accepted(chunks.next(128, 1, 128, || {
            pulls += 1;
            Ok((Some(vec![WireValue::Int(pulls)]), true))
        }))
        .unwrap();
        assert_eq!(last.rows, [vec![WireValue::Int(2)]]);
        assert!(chunks.done());
        assert_eq!(pulls, 2);
        assert!(accepted(chunks.next(128, 1, 128, || panic!("LIMIT drained source"))).is_none());
        // Dropping a cancelled connection's framing state has no source work.
        drop(chunks);
        assert_eq!(pulls, 2);
    }

    #[test]
    fn a_late_native_refusal_is_not_eof_or_an_oversized_private_chunk() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let first =
            accepted(chunks.next(128, 1, 128, || Ok((Some(vec![WireValue::Int(1)]), false))))
                .unwrap();
        assert_eq!(first.rows.len(), 1);
        let error = chunks
            .next(128, 1, 128, || {
                Err(Refusal::new(
                    ErrorCode::Budget,
                    "retained native allowance exhausted",
                ))
            })
            .expect_err("native refusal");
        assert_eq!(error.code, ErrorCode::Budget);
        assert!(!chunks.done());

        let error = chunks
            .next(128, 1, 128, || {
                Ok((Some(vec![WireValue::Text("x".repeat(256))]), true))
            })
            .expect_err("oversized row refusal");
        assert_eq!(error.code, ErrorCode::Execution);
        assert!(!chunks.done());
    }

    #[test]
    fn aggregate_open_and_zero_credit_do_not_consume_the_native_input() {
        let ((), report) = run_async_under_lab(0x79a0_0511, |root| async move {
            let (server, _, _) = served(&root, "aggregate-zero-credit").await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(1))
                .await
                .unwrap();
            let token = db
                .authority
                .issue_at(
                    &visible_read(QueryLimits {
                        max_nodes: 0,
                        max_work: 100_000,
                        max_rows: 1,
                    }),
                    crate::unix_millis(),
                )
                .unwrap();
            let (mut session, _) = accepted(read_session(&root, db, &token).await);
            let query = contexts.query();
            let mut source = accepted(session.stream(
                &query,
                "MATCH (n) RETURN count(*) AS n",
                &GqlParameters::new(),
            ));
            let mut chunks = Chunks::new(source.columns().to_vec());
            let metadata = accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).unwrap();
            assert_eq!(metadata.columns, Some(vec!["n".into()]));
            assert!(metadata.rows.is_empty());
            assert!(accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).is_none());
            assert!(accepted(chunks.next(0, 1, 128, || panic!("no byte credit"))).is_none());
            assert!(!chunks.needs_row(128, 0));
            assert!(!chunks.needs_row(0, 1));
            assert!(chunks.needs_row(128, 1));
            let error = source.next().await.err().unwrap();
            assert_eq!(error.code, ErrorCode::Budget);
            assert!(
                !chunks.done(),
                "a denied input must not become aggregate EOF"
            );
            drop(source);
            drop(session);
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn prepared_source_open_waits_for_credit_and_refuses_unsupported_profiles_before_scan() {
        let ((), report) = run_async_under_lab(0x79a0_0514, |root| async move {
            let (server, _, _) = served(&root, "prepared-zero-credit").await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(1))
                .await
                .unwrap();
            let token = db
                .authority
                .issue_at(
                    &visible_read(QueryLimits {
                        max_nodes: 0,
                        max_work: 100_000,
                        max_rows: 4,
                    }),
                    crate::unix_millis(),
                )
                .unwrap();
            let query = contexts.query();
            let parameters = GqlParameters::new();
            for text in ["MATCH (n) RETURN n", "MATCH (n) RETURN count(*) AS n"] {
                let definition = accepted(
                    crate::execute::prepare_read(
                        &root,
                        db,
                        &token,
                        &Prepare {
                            mode: fgdb_protocol::body::PrepareMode::Read,
                            statement: text.into(),
                            parameters: vec![],
                        },
                    )
                    .await,
                );
                let (mut session, prepared) =
                    accepted(read_prepared_session(&root, db, &token, &definition).await);
                let mut source = accepted(session.stream_prepared(&query, &prepared, &parameters));
                let mut chunks = Chunks::new(source.columns().to_vec());
                let metadata =
                    accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).unwrap();
                assert!(metadata.rows.is_empty());
                assert!(accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).is_none());
                assert!(accepted(chunks.next(0, 1, 128, || panic!("no byte credit"))).is_none());
                assert!(!chunks.needs_row(128, 0));
                assert!(!chunks.needs_row(0, 1));
                assert!(chunks.needs_row(128, 1));
                assert_eq!(
                    source.next().await.err().unwrap().code,
                    ErrorCode::Budget,
                    "{text}"
                );
                assert!(!chunks.done());
            }
            let definition = accepted(
                crate::execute::prepare_read(
                    &root,
                    db,
                    &token,
                    &Prepare {
                        mode: fgdb_protocol::body::PrepareMode::Read,
                        statement: "MATCH (n) RETURN 1 AS value".into(),
                        parameters: vec![],
                    },
                )
                .await,
            );
            let (mut session, prepared) =
                accepted(read_prepared_session(&root, db, &token, &definition).await);
            assert_eq!(
                session
                    .stream_prepared(&query, &prepared, &parameters)
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Statement,
                "profile refusal must precede the zero-node execution limit"
            );
            drop(session);
            for text in ["MATCH (n) RETURN id(n)", "MATCH (n) RETURN elementId(n)"] {
                assert_eq!(
                    crate::execute::prepare_read(
                        &root,
                        db,
                        &token,
                        &Prepare {
                            mode: fgdb_protocol::body::PrepareMode::Read,
                            statement: text.into(),
                            parameters: vec![],
                        },
                    )
                    .await
                    .err()
                    .unwrap()
                    .code,
                    ErrorCode::PermissionDenied
                );
            }
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn native_aggregate_work_and_scratch_failures_keep_their_budget_class() {
        let ((), report) = run_async_under_lab(0x79a0_0512, |root| async move {
            let (mut server, token, _) = served(&root, "aggregate-native-limits").await;
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut batch = vertex(1);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(1), VId(1), VId(2), vec![]);
            server.databases["test"]
                .db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), batch)
                .await
                .unwrap();
            Arc::get_mut(server.databases.get_mut("test").unwrap())
                .unwrap()
                .symbols
                .bind(GraphSymbolKind::Relation, "R", 1)
                .unwrap();
            let query = contexts.query();
            for policy in [
                GqlQueryPolicy::new(1000, 1000, 0, 1_000_000),
                GqlQueryPolicy::new(1000, 1000, 1_000_000, 0),
            ] {
                Arc::get_mut(server.databases.get_mut("test").unwrap())
                    .unwrap()
                    .query_policy = policy;
                let db = &server.databases["test"];
                for text in [
                    "MATCH (n) RETURN count(*) AS n",
                    "MATCH (a)-[:R]->(b) RETURN count(*) AS n",
                ] {
                    let (mut session, _) = accepted(read_session(&root, db, &token).await);
                    let mut source = accepted(session.stream(&query, text, &GqlParameters::new()));
                    let error = source
                        .next()
                        .await
                        .expect_err("native allowance must refuse");
                    assert_eq!(error.code, ErrorCode::Budget, "{text}: {}", error.message);
                }
            }
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn completed_aggregate_groups_remain_fenced_by_recovery_and_live_authority() {
        let ((), report) = run_async_under_lab(0x79a0_0513, |root| async move {
            let (server, token, _) = served(&root, "aggregate-generation-fence").await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut batch = vertex(1);
            batch.create_vertex(VId(2), vec![], vec![]);
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), batch)
                .await
                .unwrap();
            let query = contexts.query();
            let text = "MATCH (n) RETURN n AS key, count(*) AS n GROUP BY n";
            let parameters = GqlParameters::new();
            let (mut session, _) = accepted(read_session(&root, db, &token).await);
            let mut source = accepted(session.stream(&query, text, &parameters));
            assert_eq!(source.snapshot_seq(), 1);
            assert_eq!(
                accepted(source.next().await),
                Some(vec![WireValue::Vertex(1), WireValue::Count(1)])
            );
            fail_after_marker(&root, db, 3).await;
            assert_eq!(
                settled(db).await,
                crate::DatabaseStatus::Ready { generation: 2 }
            );
            assert_eq!(
                source.next().await.err().unwrap().code,
                ErrorCode::DatabaseRecovering
            );
            drop(source);
            drop(session);

            let (mut session, _) = accepted(read_session(&root, db, &token).await);
            let mut source = accepted(session.stream(&query, text, &parameters));
            assert_eq!(source.snapshot_seq(), 2);
            assert!(accepted(source.next().await).is_some());
            assert!(db.authority.retire());
            assert_eq!(
                source.next().await.err().unwrap().code,
                ErrorCode::PermissionDenied
            );
            assert!(accepted(source.next().await).is_none());
            drop(source);
            drop(session);
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
