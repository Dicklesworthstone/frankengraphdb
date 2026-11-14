//! Explicit Bolt reads retain one native pin and signed allowance through
//! preparation, every RUN, graph hydration, result admission and final close.

use super::*;

pub(super) async fn begin<'s>(
    cx: &Cx,
    db: &'s Served,
    token: &CapabilityToken,
    minimum: CommitSeq,
    output_generation: &mut Option<Generation>,
) -> Result<(TransactionSession<'s>, CommitSeq), Failure> {
    let _operation = db
        .db
        .enter()
        .map_err(|error| Failure::from_refusal(error.into()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (view, generation) = db
        .db
        .read_view(cx)
        .await
        .map_err(|error| Failure::from_refusal(error.into()))?;
    // A bookmark refusal depends on the selected view's frontier. Its FAILURE
    // must retain this exact fence even when no transaction owner is returned.
    *output_generation = Some(generation.clone());
    let owner = view
        .begin_authorized_read(
            &crate::query_cx(&contexts),
            &db.authority,
            token,
            crate::TRUNK,
            db.symbols.clone(),
            db.query_policy,
            crate::unix_millis as fn() -> u64,
        )
        .map_err(|error| Failure::from_refusal(query_refusal(error)))?;
    let basis = owner.basis();
    causal::require_frontier(basis, minimum)?;
    generation
        .check()
        .map_err(|error| Failure::from_refusal(error.into()))?;
    Ok((
        TransactionSession::Read {
            owner: Box::new(owner),
            generation,
        },
        basis,
    ))
}

impl<'s> Connection<'s> {
    pub(super) fn run_read_transaction_statement(
        &mut self,
        cx: &Cx,
        mut transaction: Transaction<'s>,
        text: &str,
        arguments: &Map,
        parameters: &GqlParameters,
    ) -> Result<Map, Failure> {
        let db = transaction.db;
        let generation = transaction.session.generation();
        let _operation = generation
            .enter()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        let TransactionSession::Read { owner, .. } = &mut transaction.session else {
            unreachable!("read owner selected");
        };
        if owner.completed_statements() >= db.max_statements as u64 {
            return Err(Failure::from_refusal(Refusal::new(
                ErrorCode::Budget,
                "the explicit transaction statement limit was exceeded",
            )));
        }
        // Preserve the same finite statement/value domains as the other native
        // adapters. Native preparation meters the original canonical input.
        Execute {
            mode: ExecuteMode::Read,
            statement: text.to_owned(),
            parameters: wire_parameters(arguments)?,
        }
        .encode()
        .map_err(|error| Failure::invalid(error.to_string()))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let max_frame_len = self.server.limits.max_frame_len as usize;
        let metadata = owner.query_with_graph_output(
            &crate::query_cx(&contexts),
            text,
            parameters,
            |result, work, metadata| {
                let (columns, rows) = crate::execute::graph_output_read(result, work, None)
                    .map_err(Failure::from_refusal)?;
                let (rows, retained) = autocommit::collect_read(
                    rows,
                    max_frame_len,
                    work,
                    metadata,
                    db.symbols.borrowed_reverse_catalog(),
                )
                .map_err(Failure::from_refusal)?;
                work(transaction.name.len().div_ceil(64) as u64 + 1)
                    .map_err(|error| Failure::from_refusal(query_refusal(error)))?;
                let database = transaction.name.clone();
                let retained = retained
                    .checked_add(std::mem::size_of::<Pending<'_>>())
                    .and_then(|bytes| bytes.checked_add(database.capacity()))
                    .ok_or_else(|| {
                        Failure::from_refusal(Refusal::new(
                            ErrorCode::Budget,
                            "Bolt transaction retained-result budget exceeded",
                        ))
                    })?;
                // The serializer already measured every actual row/container
                // capacity. Admit their transaction-wide ownership before the
                // native callback completes or publishes pending cache data.
                let qid = transaction.cursors.insert(
                    Pending {
                        rows,
                        database,
                        db,
                        seq: Some(transaction.seq),
                        generation: generation.clone(),
                        write: None,
                    },
                    retained,
                )?;
                autocommit::admit_read_metadata(
                    vec![
                        (
                            "fields".to_owned(),
                            Value::List(columns.into_iter().map(Value::String).collect()),
                        ),
                        ("t_first".to_owned(), Value::Int(0)),
                        ("qid".to_owned(), Value::Int(qid)),
                    ],
                    max_frame_len,
                    work,
                )
                .map_err(Failure::from_refusal)
            },
            |error| {
                let refusal = query_refusal(error);
                // Preserve the adapter's access-mode diagnostic only after
                // the native read parser refuses. Never let a text hint mask
                // a real budget, authority, cancellation or source failure.
                if refusal.code == ErrorCode::Statement && looks_like_write(text) {
                    read_only()
                } else {
                    Failure::from_refusal(refusal)
                }
            },
        )?;
        generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        self.transaction = Some(transaction);
        Ok(metadata)
    }
}
