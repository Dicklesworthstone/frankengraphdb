//! No-RETURN autocommit writes over Bolt's existing capability boundary.
//!
//! RUN retains one bounded request, never a transaction or database lock.
//! PULL and DISCARD consume it before awaiting the native atomic writer;
//! RESET, GOODBYE and disconnect before consumption abandon it without effects.
//! A lost terminal response is not evidence of rollback and is not replayed.

use super::*;
use fgdb_gql::{BoundNativeGraphWrite, GraphSymbol, PreparedGraphWriteChainText};
use fgdb_protocol::body::{Body, Execute, ExecuteMode, Outcome};
use fgdb_warden::{PlannerPredicates, Rights};

#[cfg(test)]
mod tests;

/// The request owns values, not interpolated source or client-chosen identities.
/// There is deliberately no Clone or payload-bearing Debug implementation.
pub(super) struct DeferredWrite {
    statement: Execute,
    minimum_seq: CommitSeq,
}

/// Validate metadata whose meaning matters to a write before retaining it.
/// A local bookmark is a lower bound, checked under the native write lock.
/// Foreign bookmarks, impersonation and unsupported timeouts never silently
/// turn into a write with weaker semantics.
fn write_options(extra: &Map, database: &str) -> Result<CommitSeq, Failure> {
    let options = causal::options(extra, database)?;
    if options.mode == causal::AccessMode::Read {
        return Err(read_only());
    }
    Ok(options.minimum_seq)
}

/// The old keyword detector is only a hint, never execution authority. A
/// valid native read wins even when an alias, label or comment spells CREATE.
/// Branch selectors use the same native owner as normal read preparation.
fn is_native_read(db: &Served, text: &str, parameters: &GqlParameters) -> bool {
    let Ok(selector) = PreparedGraphBranchText::prepare(text) else {
        return false;
    };
    let Ok(selected) = selector.bind_parameters(parameters) else {
        return false;
    };
    PreparedNativeRead::prepare(
        selected.statement(),
        selected.parameters(),
        db.symbols.clone(),
    )
    .is_ok()
}

/// Admission probes native definitions, not expanded rows. The resolver
/// exposes no catalog name outside the signed scopes. The complete native
/// binder runs again at consumption, under its cooperative execution budget.
fn is_native_write(
    db: &Served,
    text: &str,
    parameters: &GqlParameters,
    scope: &PlannerPredicates,
) -> bool {
    if is_native_read(db, text, parameters) {
        return false;
    }
    let resolve = |kind, name: &str| {
        db.symbols
            .resolve(kind, name)
            .filter(|symbol| match symbol {
                GraphSymbol::Label(label) => scope.allows_label(*label),
                GraphSymbol::Relation(relation) => scope.allows_relation(*relation),
                GraphSymbol::Property(property) => scope.allows_property(*property),
            })
    };
    if PreparedGraphWriteChainText::is_chain(text).unwrap_or(false) {
        // Preparing the definition does not expand parameter rows or allocate
        // identities. PULL/DISCARD later binds the retained definition.
        // Execution uses one signed permit and the queued generation fence.
        // Map arguments splice as the executor's ReturningTemplate splices
        // them (fgdb-0s2pq), so both classify the same statement.
        let lowered = fgdb_gql::GqlMapArgumentText::lower(text, parameters);
        let (text, parameters) = lowered
            .as_ref()
            .map_or((text, parameters), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let declarations: Vec<_> = parameters.parameter_types().collect();
        return PreparedGraphWriteChainText::prepare_with_parameter_types(
            text,
            db.write_relation,
            &declarations,
            resolve,
        )
        .is_ok_and(|chain| !chain.returns_rows());
    }
    BoundNativeGraphWrite::probe_definition(text, parameters, db.write_relation, resolve).is_ok()
}

impl<'s> Connection<'s> {
    /// No graph access, identity allocation, expansion of argument rows or
    /// transaction opening occurs here. Definition probes retain no expanded
    /// rows; the shared executor admits the full input and signed budgets at
    /// consumption. In particular, write RETURN never reaches a returning owner.
    pub(super) fn queue_autocommit_write(
        &mut self,
        query: &str,
        arguments: &Map,
        parameters: &GqlParameters,
        extra: &Map,
    ) -> Result<Option<Map>, Failure> {
        if self.transaction.is_some() || !looks_like_write(query) {
            return Ok(None);
        }
        let (name, db) = self.database(extra)?;
        self.output_database = Some(db);
        let token = self.token.as_ref().expect("authenticated");
        let capability = db
            .authority
            .verify_at(token, crate::TRUNK, crate::unix_millis())
            .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
        if !is_native_write(db, query, parameters, capability.predicates()) {
            return Ok(None);
        }
        let minimum_seq = write_options(extra, &name)?;
        if !matches!(
            capability.predicates().rights(),
            Rights::Write | Rights::ReadWrite
        ) {
            return Err(read_only());
        }
        // The ordinary body encoder enforces its complete size/name/value
        // contract even though Bolt does not put this request on an FGP wire.
        let statement = Execute {
            mode: ExecuteMode::Write,
            statement: query.to_owned(),
            parameters: wire_parameters(arguments)?,
        };
        statement
            .encode()
            .map_err(|error| Failure::invalid(error.to_string()))?;
        let generation = db
            .db
            .generation()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        self.output_generation = Some(generation.clone());
        self.pending = Some(Pending {
            rows: VecDeque::new(),
            database: name,
            db,
            seq: None,
            generation,
            write: Some(DeferredWrite {
                statement,
                minimum_seq,
            }),
        });
        Ok(Some(vec![
            ("fields".to_owned(), Value::List(Vec::new())),
            ("t_first".to_owned(), Value::Int(0)),
        ]))
    }

    /// The owner is removed before the first await: an error, cancellation or
    /// lost response cannot leave a request that a second PULL can execute.
    pub(super) async fn finish_autocommit_write(&mut self, cx: &Cx) -> Result<Map, Failure> {
        let pending = self.pending.take().expect("pending write checked by PULL");
        let deferred = pending.write.expect("pending write checked by PULL");
        self.output_database = Some(pending.db);
        self.output_generation = Some(pending.generation.clone());
        let answer = crate::execute::write_no_return_at(
            cx,
            pending.db,
            self.token.as_ref().expect("authenticated"),
            &deferred.statement,
            &pending.generation,
            deferred.minimum_seq,
        )
        .await
        .map_err(Failure::from_refusal)?;
        self.output_generation = Some(answer.generation);
        let committed = matches!(answer.outcome, Outcome::WriteCommitted { .. });
        let (seq, statements) = match answer.outcome {
            Outcome::WriteCommitted { seq, statements }
            | Outcome::ReadClosed { seq, statements } => (CommitSeq(seq), statements),
            _ => {
                // Never translate an unexpected post-execution answer into a
                // retryable refusal or suggest that a commit rolled back.
                return Err(Failure::new(
                    "Neo.DatabaseError.Statement.ExecutionFailed",
                    "unexpected native write completion; do not replay automatically",
                ));
            }
        };
        let unrepresentable = || {
            Failure::new(
                "Neo.DatabaseError.Statement.ExecutionFailed",
                "write completion cannot be represented; do not replay automatically",
            )
        };
        let statements = i64::try_from(statements).map_err(|_| unrepresentable())?;
        // Drivers read their counters from `stats` (fgdb-xwmqs). The program
        // knows exactly what it created and whether it published anything;
        // its other effects are one count over every kind, so no
        // per-kind key is invented for them: contains-updates carries them.
        let mut stats = vec![("contains-updates".to_owned(), Value::Bool(committed))];
        for (key, count) in [
            ("nodes-created", answer.created.vertices),
            ("relationships-created", answer.created.edges),
        ] {
            if count != 0 {
                let count = i64::try_from(count).map_err(|_| unrepresentable())?;
                stats.push((key.to_owned(), Value::Int(count)));
            }
        }
        Ok(vec![
            ("type".to_owned(), Value::string("w")),
            ("t_last".to_owned(), Value::Int(0)),
            ("db".to_owned(), Value::string(pending.database.clone())),
            (
                "bookmark".to_owned(),
                Value::string(bookmark(&pending.database, seq)),
            ),
            ("stats".to_owned(), Value::Map(stats)),
            ("fgdb_statements".to_owned(), Value::Int(statements)),
        ])
    }
}
