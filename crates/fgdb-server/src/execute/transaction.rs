//! One explicit native write owner shared with the FGP transport.
//! Statements retain the canonical workspace and the original signed permit.
//! No database lock survives a request, and every error or cancelled operation
//! discards the whole unpublished prefix. Only commit may publish effects.

use super::{Answer, Created, Refusal, authorize_write, convert, edge_upsert_output, write_refusal};
use crate::recovery::Generation;
use crate::{Served, TRUNK, unix_millis};
use asupersync::Cx;
use fgdb::AuthorizedWriteTransaction;
use fgdb_gql::{
    GqlMapArgumentText, GqlParameters, GqlTextOffsets, GraphAggregateTextSlot, GraphSymbol,
    GraphSymbolKind, GraphSymbolResolver, PreparedGraphAggregateText,
    PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate, PreparedGraphSetText,
    PreparedGraphWriteChainText, ReverseSymbolCatalog,
};
use fgdb_protocol::body::{Body, ErrorCode, Execute, ExecuteMode, Outcome};
use fgdb_types::{CommitSeq, EmbeddedTxnCompletion, PurposeContexts};
use fgdb_warden::CapabilityToken;
use std::cell::{Cell, RefCell};

/// Connection-scoped ownership of the existing native transaction. This does
/// not add a durable transaction lease, reattachment, or outcome lookup.
pub(crate) struct NativeWriteTransaction<'s> {
    db: &'s Served,
    token: CapabilityToken,
    owner: Option<Box<AuthorizedWriteTransaction<'s, fn() -> u64>>>,
    generation: Generation,
    basis: CommitSeq,
}

impl<'s> NativeWriteTransaction<'s> {
    pub(crate) async fn begin(
        cx: &Cx,
        db: &'s Served,
        token: &CapabilityToken,
    ) -> Result<Self, Refusal> {
        authorize_write(db, token)?;
        let generation = db.db.generation().map_err(Refusal::from)?;
        let _operation = generation.enter().map_err(Refusal::from)?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
        generation.check().map_err(Refusal::from)?;
        authorize_write(db, token)?;
        let owner = guard
            .begin_authorized_write(
                &contexts.txn(),
                &contexts.commit(),
                &db.authority,
                token,
                TRUNK,
                db.write_policy.insertion_policy(),
                db.max_statements,
                unix_millis as fn() -> u64,
            )
            .map_err(|error| write_refusal(&error))?;
        let basis = owner.basis();
        generation.check().map_err(Refusal::from)?;
        drop(guard);
        Ok(Self {
            db,
            token: token.clone(),
            owner: Some(Box::new(owner)),
            generation,
            basis,
        })
    }

    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }

    pub(crate) const fn basis(&self) -> CommitSeq {
        self.basis
    }

    /// Internal Rows metadata names only the pinned basis. FGP must use its
    /// explicit provisional result class, never an autocommit terminal, when
    /// delivering this answer. The negotiated row/schema budget excludes the
    /// ordinary chunk overhead and is independent of current flow credit.
    pub(crate) async fn execute_framed(
        &mut self,
        cx: &Cx,
        request: &Execute,
        frame_budget: usize,
    ) -> Result<Answer, Refusal> {
        // Take before every fallible operation and before the first await.
        // Failed conversion, preparation, output admission or cancellation
        // cannot leave an earlier successful prefix available for COMMIT.
        let mut owner = self.owner.take().ok_or_else(finished)?;
        authorize_write(self.db, &self.token)?;
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        if !matches!(request.mode, ExecuteMode::Read | ExecuteMode::Write) {
            return Err(Refusal::new(
                ErrorCode::Statement,
                "explicit transactions support buffered Read or Write statements",
            ));
        }
        super::refuse_element_identity(&request.statement)?;
        request
            .encode()
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let parameters = query.with_restriction(|| {
            convert::parameters(&request.parameters, None)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
        })?;
        let mut guard = self.db.db.write(cx).await.map_err(Refusal::from)?;
        generation.check().map_err(Refusal::from)?;
        authorize_write(self.db, &self.token)?;
        let mut preparation_refusal = None;
        let statement = owner
            .prepare_statement(
                &guard,
                &query,
                &request.statement,
                &parameters,
                |parameters, scope, work| {
                    match prepare_transaction_statement(
                        self.db,
                        &request.statement,
                        parameters,
                        scope,
                        work,
                    ) {
                        Ok(statement) => Ok(statement),
                        Err(error) => {
                            preparation_refusal = Some(error);
                            Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                        }
                    }
                },
            )
            .map_err(|error| {
                if matches!(&error, fgdb::WriteTxnError::AuthorizedMutationRefused)
                    && let Some(refusal) = preparation_refusal
                {
                    return refusal;
                }
                write_refusal(&error)
            })?;
        if request.mode == ExecuteMode::Read
            && matches!(&statement, TransactionStatement::Write(_))
        {
            return Err(Refusal::new(
                ErrorCode::PermissionDenied,
                "a Read request cannot execute a write in an explicit transaction",
            ));
        }
        let columns = statement.columns().to_vec();
        let before = owner.statistics().ok_or_else(finished)?;
        let mut admitted = None;
        let mut output_refusal = None;
        let mut collect = |result| match result {
            Ok(rows) => {
                admitted = Some(rows);
                Ok(())
            }
            Err(error) => {
                output_refusal = Some(error);
                Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
            }
        };
        let result = match &statement {
            TransactionStatement::Read(statement) => owner
                .read(&mut guard, &query, statement, |rows, work, _| {
                    collect(edge_upsert_output::collect_with_frame_budget(
                        &columns,
                        rows,
                        work,
                        Some(frame_budget),
                    ))
                })
                .map(|_| ()),
            TransactionStatement::Aggregate {
                statement, slots, ..
            } => owner
                .read_aggregate(&mut guard, &query, statement, |rows, work, _| {
                    collect(edge_upsert_output::collect_aggregate_with_frame_budget(
                        &columns,
                        slots,
                        rows,
                        work,
                        Some(frame_budget),
                    ))
                })
                .map(|_| ()),
            TransactionStatement::Write(statement) => owner
                .stage_chain(&mut guard, &query, statement, |rows, work, _| {
                    collect(edge_upsert_output::collect_with_frame_budget(
                        &columns,
                        rows,
                        work,
                        Some(frame_budget),
                    ))
                })
                .map(|_| ()),
        };
        drop(guard);
        result.map_err(|error| output_refusal.unwrap_or_else(|| write_refusal(&error)))?;
        generation.check().map_err(Refusal::from)?;
        let after = owner.statistics().ok_or_else(finished)?;
        let created = Created {
            vertices: after.created_vertices - before.created_vertices,
            edges: after.created_edges - before.created_edges,
        };
        let rows = admitted.expect("native transaction output admission ran");
        self.owner = Some(owner);
        Ok(Answer {
            columns,
            rows,
            outcome: Outcome::Rows { seq: self.basis.0 },
            generation,
            created,
        })
    }

    /// Consume ownership before publication. A dropped/failed completion
    /// cannot be replayed through this handle, including an unknown outcome.
    pub(crate) async fn commit(mut self, cx: &Cx) -> Result<Answer, Refusal> {
        let owner = self.owner.take().ok_or_else(finished)?;
        let stats = owner.statistics().ok_or_else(finished)?;
        authorize_write(self.db, &self.token)?;
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        let mut guard = self.db.db.write(cx).await.map_err(Refusal::from)?;
        generation.check().map_err(Refusal::from)?;
        authorize_write(self.db, &self.token)?;
        let completion = (*owner)
            .commit(&mut guard)
            .await
            .map_err(|error| write_refusal(&error))?;
        drop(guard);
        let statements = stats.completed_statements as u64;
        let outcome = match completion {
            EmbeddedTxnCompletion::WriteCommitted { commit_seq } => {
                self.db.commits.committed();
                Outcome::WriteCommitted {
                    seq: commit_seq.0,
                    statements,
                }
            }
            EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
                seq: snapshot_seq.0,
                statements,
            },
        };
        // There is deliberately no post-commit retryable generation check.
        // The transport's existing guard fences physical output if recovery
        // starts after the native completion has decided the write outcome.
        Ok(Answer {
            columns: Vec::new(),
            rows: Vec::new(),
            outcome,
            generation,
            created: Created {
                vertices: stats.created_vertices,
                edges: stats.created_edges,
            },
        })
    }

    pub(crate) fn rollback(mut self) {
        if let Some(owner) = self.owner.take() {
            (*owner).rollback();
        }
    }
}

fn finished() -> Refusal {
    Refusal::new(ErrorCode::Statement, "the explicit transaction is no longer active")
}

pub(crate) enum TransactionStatement {
    Read(Box<fgdb_gql::PreparedGraphSet>),
    Aggregate {
        statement: Box<PreparedGraphSetAggregate>,
        columns: Vec<String>,
        slots: Vec<GraphAggregateTextSlot>,
    },
    Write(Box<fgdb_gql::PreparedGraphWriteChain>),
}

impl TransactionStatement {
    pub(crate) fn columns(&self) -> &[String] {
        match self {
            Self::Read(statement) => statement.columns(),
            Self::Aggregate { columns, .. } => columns,
            Self::Write(statement) => statement.columns(),
        }
    }
}

fn transaction_statement_error<E: GqlTextOffsets + core::fmt::Display>(
    lowered: Option<&GqlMapArgumentText>,
    mut error: E,
) -> Refusal {
    if let Some(lowered) = lowered {
        lowered.restore(&mut error);
    }
    Refusal::new(ErrorCode::Statement, error.to_string())
}

/// The native owner's meter latches its first refusal. Resolver methods have
/// an Option return contract, so no failed lookup may continue observing the
/// catalog or turn the original control refusal into a syntax diagnostic.
/// Reverse names are copied only after admission and retain the signed scope.
pub(crate) struct TransactionCatalog<'a> {
    symbols: &'a crate::Symbols,
    scope: &'a fgdb_warden::PlannerPredicates,
    work: RefCell<&'a mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>>,
    failed: Cell<bool>,
    denied: Cell<bool>,
}

impl<'a> TransactionCatalog<'a> {
    pub(crate) fn new(
        symbols: &'a crate::Symbols,
        scope: &'a fgdb_warden::PlannerPredicates,
        work: &'a mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
    ) -> Self {
        Self {
            symbols,
            scope,
            work: RefCell::new(work),
            failed: Cell::new(false),
            denied: Cell::new(false),
        }
    }

    fn charge(&self, units: u64) -> Result<(), fgdb::WriteTxnError> {
        if self.failed.get() {
            return Err(fgdb::WriteTxnError::AuthorizedMutationRefused);
        }
        let result = (self.work.borrow_mut())(units);
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }

    pub(crate) fn check(&self) -> Result<(), Refusal> {
        self.charge(0).map_err(|error| write_refusal(&error))?;
        if self.denied.get() {
            return Err(write_refusal(
                &fgdb::WriteTxnError::Authorization(fgdb_warden::Error::ScopeDenied),
            ));
        }
        Ok(())
    }

    fn admits(&self, symbol: GraphSymbol) -> bool {
        match symbol {
            GraphSymbol::Label(id) => self.scope.allows_label(id),
            GraphSymbol::Relation(id) => self.scope.allows_relation(id),
            GraphSymbol::Property(id) => self.scope.allows_property(id),
        }
    }

    fn copy_name(&self, name: &str) -> Option<String> {
        self.charge((name.len().div_ceil(64) as u64).saturating_add(1))
            .ok()?;
        Some(name.to_owned())
    }
}

impl GraphSymbolResolver for &TransactionCatalog<'_> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if self.denied.get() || self.failed.get() {
            return None;
        }
        self.charge((name.len().div_ceil(64) as u64).saturating_add(1))
            .ok()?;
        let symbol = self.symbols.resolve(kind, name);
        self.charge(0).ok()?;
        if symbol.is_some_and(|symbol| !self.admits(symbol)) {
            self.denied.set(true);
            return None;
        }
        symbol
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        if self.denied.get() || self.failed.get() {
            return None;
        }
        self.charge(1).ok()?;
        let source = self.symbols.borrowed_reverse_catalog();
        let mut catalog = ReverseSymbolCatalog::new();
        for (id, name) in &source.labels {
            // Hidden catalog growth must not change the signed work bill.
            // Poll live controls during traversal; charge only visible copies.
            self.charge(0).ok()?;
            if self.scope.allows_label(*id) {
                catalog.labels.insert(*id, self.copy_name(name)?);
            }
        }
        for (id, name) in &source.relations {
            self.charge(0).ok()?;
            if self.scope.allows_relation(*id) {
                catalog.relations.insert(*id, self.copy_name(name)?);
            }
        }
        for (id, name) in &source.properties {
            self.charge(0).ok()?;
            if self.scope.allows_property(*id) {
                catalog.properties.insert(*id, self.copy_name(name)?);
            }
        }
        Some(catalog)
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        if self.denied.get() || self.failed.get() || !self.scope.allows_label(id) {
            return None;
        }
        self.charge(1).ok()?;
        let name = self.symbols.borrowed_reverse_catalog().labels.get(&id)?;
        self.copy_name(name)
    }

    fn reverse_relation(&self, id: fgdb_delta_types::RelationId) -> Option<String> {
        if self.denied.get() || self.failed.get() || !self.scope.allows_relation(id) {
            return None;
        }
        self.charge(1).ok()?;
        let name = self.symbols.borrowed_reverse_catalog().relations.get(&id)?;
        self.copy_name(name)
    }
}

/// Native read preparation wins over keyword hints: an alias or literal
/// spelling CREATE cannot turn a read into a write. Every compiler and binding
/// spends the retained owner allowance. Parameters already carry that owner's
/// trusted once-per-statement clock; no adapter clock is captured again.
pub(crate) fn prepare_transaction_statement(
    db: &Served,
    text: &str,
    parameters: &GqlParameters,
    scope: &fgdb_warden::PlannerPredicates,
    work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
) -> Result<TransactionStatement, Refusal> {
    let catalog = TransactionCatalog::new(&db.symbols, scope, work);
    let charge = |units| catalog.charge(units).map_err(|error| write_refusal(&error));
    charge(1)?;
    let lowered_read = GqlMapArgumentText::lower_fields(text, parameters);
    let (read_text, read_parameters) = lowered_read
        .as_ref()
        .map_or((text, parameters), |(lowered, derived)| {
            (lowered.text(), derived)
        });
    let recipe = lowered_read.as_ref().map(|(lowered, _)| lowered);
    let mut declarations = Vec::new();
    for declaration in read_parameters.parameter_types() {
        charge(1)?;
        declarations.push(declaration);
    }
    charge(read_text.len().saturating_sub(text.len()) as u64)?;
    let read = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        read_text,
        &declarations,
        &mut &catalog,
    );
    catalog.check()?;
    let read_error = match read {
        Ok(template) => {
            charge(1)?;
            return template
                .bind_parameters(read_parameters)
                .map(|statement| TransactionStatement::Read(Box::new(statement)))
                .map_err(|error| transaction_statement_error(recipe, error));
        }
        Err(error) => error,
    };
    let mut offset = read_error.offset;
    let mut read_error = transaction_statement_error(recipe, read_error);
    charge(1)?;
    let aggregate = PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
        read_text,
        &declarations,
        &catalog,
    );
    catalog.check()?;
    match aggregate {
        Ok(template) => {
            charge(1)?;
            let statement = template
                .bind_relation_parameters(read_parameters)
                .map_err(|error| transaction_statement_error(recipe, error))?;
            return Ok(TransactionStatement::Aggregate {
                statement: Box::new(statement),
                columns: template.columns().to_vec(),
                slots: template.output_slots().to_vec(),
            });
        }
        Err(error) if error.offset > offset => {
            offset = error.offset;
            read_error = transaction_statement_error(recipe, error);
        }
        Err(_) => {}
    }
    charge(1)?;
    let aggregate = PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
        read_text,
        &declarations,
        &catalog,
    );
    catalog.check()?;
    match aggregate {
        Ok(template) => {
            charge(1)?;
            let statement = template
                .bind_parameters(read_parameters)
                .map_err(|error| transaction_statement_error(recipe, error))?;
            let statement =
                PreparedGraphSetAggregate::from_complete_input(statement).ok_or_else(|| {
                    Refusal::new(
                        ErrorCode::Statement,
                        "aggregate input is unsupported in an explicit write transaction",
                    )
                })?;
            return Ok(TransactionStatement::Aggregate {
                statement: Box::new(statement),
                columns: template.columns().to_vec(),
                slots: template.output_slots().to_vec(),
            });
        }
        Err(error) if error.offset > offset => {
            read_error = transaction_statement_error(recipe, error);
        }
        Err(_) => {}
    }
    if !looks_like_write(text) {
        return Err(read_error);
    }
    charge(1)?;
    let lowered = GqlMapArgumentText::lower(text, parameters);
    let (text, parameters) = lowered
        .as_ref()
        .map_or((text, parameters), |(lowered, derived)| {
            (lowered.text(), derived)
        });
    let recipe = lowered.as_ref().map(|(lowered, _)| lowered);
    let mut declarations = Vec::new();
    for declaration in parameters.parameter_types() {
        charge(1)?;
        declarations.push(declaration);
    }
    let template = PreparedGraphWriteChainText::prepare_statement_with_parameter_types(
        text,
        db.write_relation,
        &declarations,
        &catalog,
    );
    catalog.check()?;
    let template = template.map_err(|error| transaction_statement_error(recipe, error))?;
    charge(1)?;
    template
        .bind_parameters(parameters)
        .map(|statement| TransactionStatement::Write(Box::new(statement)))
        .map_err(|error| transaction_statement_error(recipe, error))
}

/// A dispatch hint and refusal selector, never execution authority. Both
/// valid native reads and native write definitions are checked before queuing;
/// only a native authorized owner can execute a mutation under authority.
pub(crate) fn looks_like_write(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`') => {
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                let mut depth = 1_usize;
                while at < bytes.len() && depth != 0 {
                    if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
                        depth += 1;
                        at += 2;
                    } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
                        depth -= 1;
                        at += 2;
                    } else {
                        at += 1;
                    }
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            byte if byte.is_ascii_alphabetic() => {
                let start = at;
                while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                    at += 1;
                }
                let word = &text[start..at];
                let after_dot = start > 0 && bytes[start - 1] == b'.';
                if !after_dot
                    && [
                        "CREATE", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "INSERT", "FOREACH",
                    ]
                    .iter()
                    .any(|keyword| word.eq_ignore_ascii_case(keyword))
                {
                    return true;
                }
            }
            _ => at += 1,
        }
    }
    false
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::tests::served;
    use asupersync::lab::run_async_under_lab;
    use fgdb_protocol::body::WireValue;

    fn accepted<T>(result: Result<T, Refusal>) -> T {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
    }

    fn request(mode: ExecuteMode, statement: &str) -> Execute {
        Execute {
            mode,
            statement: statement.to_owned(),
            parameters: Vec::new(),
        }
    }

    async fn state(cx: &Cx, db: &Served) -> (CommitSeq, usize) {
        let guard = db.db.read(cx).await.unwrap();
        (guard.frontier().unwrap(), guard.vertices().unwrap().len())
    }

    #[test]
    fn native_owner_retains_provisional_reads_and_publishes_only_at_commit() {
        let ((), report) = run_async_under_lab(0xf6_7a_0001, |cx| async move {
            let (server, token, _) = served(&cx, "fgp-transaction-shared-owner").await;
            let db = &server.databases["test"];
            let before = state(&cx, db).await;
            let mut transaction = accepted(NativeWriteTransaction::begin(&cx, db, &token).await);
            assert_eq!(transaction.basis(), before.0);
            let created = accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Write, "CREATE (n) RETURN 7 AS value"),
                        4096,
                    )
                    .await,
            );
            assert_eq!(created.columns, ["value"]);
            assert_eq!(created.rows, [vec![WireValue::Int(7)]]);
            assert_eq!(created.created.vertices, 1);
            assert_eq!(created.outcome, Outcome::Rows { seq: before.0.0 });
            assert_eq!(state(&cx, db).await, before);

            let own_rows = accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Read, "MATCH (n) RETURN 9 AS value"),
                        4096,
                    )
                    .await,
            );
            assert_eq!(own_rows.rows, [vec![WireValue::Int(9)]]);
            let aggregate = accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Read, "MATCH (n) RETURN count(*) AS total"),
                        4096,
                    )
                    .await,
            );
            assert_eq!(aggregate.rows, [vec![WireValue::Count(1)]]);
            assert_eq!(state(&cx, db).await, before);
            accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Write, "CREATE (n)"),
                        4096,
                    )
                    .await,
            );
            let completed = accepted(transaction.commit(&cx).await);
            assert_eq!(
                completed.outcome,
                Outcome::WriteCommitted {
                    seq: before.0.0 + 1,
                    statements: 4,
                }
            );
            assert!(completed.columns.is_empty());
            assert!(completed.rows.is_empty());
            assert_eq!(completed.created.vertices, 2);
            assert_eq!(state(&cx, db).await, (CommitSeq(before.0.0 + 1), 2));
            server.join_database_workers(&cx).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn native_owner_rolls_back_on_mode_input_and_output_refusals() {
        let ((), report) = run_async_under_lab(0xf6_7a_0002, |cx| async move {
            let (server, token, _) = served(&cx, "fgp-transaction-refusals").await;
            let db = &server.databases["test"];
            let before = state(&cx, db).await;
            let mut malformed = request(ExecuteMode::Read, "MATCH (n) RETURN n");
            malformed.parameters = vec![(
                "map".to_owned(),
                WireValue::Map(vec![
                    ("z".to_owned(), WireValue::Int(1)),
                    ("a".to_owned(), WireValue::Int(2)),
                ]),
            )];
            for (refused, frame_budget, expected) in [
                (
                    request(ExecuteMode::Read, "CREATE (n)"),
                    4096,
                    ErrorCode::PermissionDenied,
                ),
                (
                    request(ExecuteMode::ReadStream, "MATCH (n) RETURN n"),
                    4096,
                    ErrorCode::Statement,
                ),
                (malformed, 4096, ErrorCode::Statement),
                (
                    request(ExecuteMode::Read, "MATCH (n) RETURN n"),
                    1,
                    ErrorCode::Budget,
                ),
                (
                    request(ExecuteMode::Read, "MATCH (n) RETURN count(*) AS total"),
                    1,
                    ErrorCode::Budget,
                ),
                (
                    request(ExecuteMode::Write, "CREATE (n) RETURN 1 AS value"),
                    1,
                    ErrorCode::Budget,
                ),
            ] {
                let mut transaction = accepted(NativeWriteTransaction::begin(&cx, db, &token).await);
                accepted(
                    transaction
                        .execute_framed(
                            &cx,
                            &request(ExecuteMode::Write, "CREATE (n)"),
                            4096,
                        )
                        .await,
                );
                let refusal = transaction
                    .execute_framed(&cx, &refused, frame_budget)
                    .await
                    .err()
                    .expect("second statement must refuse");
                assert_eq!(refusal.code, expected, "{}", refusal.message);
                assert!(transaction.commit(&cx).await.is_err());
                assert_eq!(state(&cx, db).await, before, "no prefix may publish");
            }
            server.join_database_workers(&cx).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn native_owner_rollback_drop_and_read_close_never_publish() {
        let ((), report) = run_async_under_lab(0xf6_7a_0003, |cx| async move {
            let (server, token, _) = served(&cx, "fgp-transaction-rollback").await;
            let db = &server.databases["test"];
            let before = state(&cx, db).await;
            for explicit in [false, true] {
                let mut transaction = accepted(NativeWriteTransaction::begin(&cx, db, &token).await);
                accepted(
                    transaction
                        .execute_framed(
                            &cx,
                            &request(ExecuteMode::Write, "CREATE (n)"),
                            4096,
                        )
                        .await,
                );
                if explicit {
                    transaction.rollback();
                } else {
                    drop(transaction);
                }
                assert_eq!(state(&cx, db).await, before);
            }
            let mut transaction = accepted(NativeWriteTransaction::begin(&cx, db, &token).await);
            let answer = accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Read, "MATCH (n) RETURN count(*) AS total"),
                        4096,
                    )
                    .await,
            );
            assert_eq!(answer.rows, [vec![WireValue::Count(0)]]);
            let closed = accepted(transaction.commit(&cx).await);
            assert_eq!(
                closed.outcome,
                Outcome::ReadClosed { seq: before.0.0, statements: 1 }
            );
            assert_eq!(state(&cx, db).await, before);
            server.join_database_workers(&cx).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn native_owner_refuses_a_new_frontier_without_replaying_staged_work() {
        let ((), report) = run_async_under_lab(0xf6_7a_0004, |cx| async move {
            let (server, token, _) = served(&cx, "fgp-transaction-stale-frontier").await;
            let db = &server.databases["test"];
            let mut transaction = accepted(NativeWriteTransaction::begin(&cx, db, &token).await);
            accepted(
                transaction
                    .execute_framed(
                        &cx,
                        &request(ExecuteMode::Write, "CREATE (n)"),
                        4096,
                    )
                    .await,
            );
            let concurrent = request(ExecuteMode::Write, "CREATE (n)");
            accepted(super::super::write(&cx, db, &token, &concurrent).await);
            let committed = state(&cx, db).await;
            assert_eq!(committed, (CommitSeq(1), 1));
            let error = transaction
                .execute_framed(
                    &cx,
                    &request(ExecuteMode::Read, "MATCH (n) RETURN n"),
                    4096,
                )
                .await
                .err()
                .expect("a retained transaction cannot silently change its basis");
            assert_eq!(error.code, ErrorCode::Execution);
            assert!(transaction.commit(&cx).await.is_err());
            assert_eq!(state(&cx, db).await, committed);
            server.join_database_workers(&cx).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
