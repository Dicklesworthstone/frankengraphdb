//! Atomic parameterized and heterogeneous reads over one authorized, pinned
//! native session. Every result retains its selected historical cut. Neither
//! argument adaptation nor response adaptation can expose a successful prefix.

use super::*;
use core::future::Future;
use core::pin::Pin;

pub(crate) struct ReadBatchResult {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<WireValue>>,
    pub(crate) seq: u64,
}

pub(crate) struct ReadBatchAnswer {
    pub(crate) results: Vec<ReadBatchResult>,
    pub(crate) generation: Generation,
}

#[derive(Clone, Copy)]
enum ReadBatchInput<'a> {
    Parameterized(&'a ExecuteBatch),
    Statements(&'a [Execute]),
}

impl<'a> ReadBatchInput<'a> {
    fn len(self) -> usize {
        match self {
            Self::Parameterized(batch) => batch.argument_sets.len(),
            Self::Statements(statements) => statements.len(),
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Parameterized(_) => "argument sets",
            Self::Statements(_) => "statements",
        }
    }

    fn statement(self, index: usize) -> Result<(&'a str, &'a [(String, WireValue)]), Refusal> {
        match self {
            Self::Parameterized(batch) => Ok((&batch.statement, &batch.argument_sets[index])),
            Self::Statements(statements) => {
                let statement = &statements[index];
                if statement.mode != fgdb_protocol::body::ExecuteMode::Read {
                    return Err(Refusal::new(
                        ErrorCode::Statement,
                        "a read batch accepts only read statements",
                    ));
                }
                Ok((&statement.statement, &statement.parameters))
            }
        }
    }
}

pub(crate) fn read_batch<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a ExecuteBatch,
) -> Pin<Box<dyn Future<Output = Result<ReadBatchAnswer, Refusal>> + Send + 'a>> {
    Box::pin(read_batch_inner(
        cx,
        db,
        token,
        ReadBatchInput::Parameterized(statement),
    ))
}

/// Distinct statements share the same native batch owner and cumulative permit
/// as repeated parameter sets. This never loops over independent read sessions.
pub(crate) fn read_statement_batch<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statements: &'a [Execute],
) -> Pin<Box<dyn Future<Output = Result<ReadBatchAnswer, Refusal>> + Send + 'a>> {
    Box::pin(read_batch_inner(
        cx,
        db,
        token,
        ReadBatchInput::Statements(statements),
    ))
}

async fn read_batch_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    input: ReadBatchInput<'_>,
) -> Result<ReadBatchAnswer, Refusal> {
    // This unspent admission permit checks live authority during adaptation.
    // The native session below owns the single cumulative execution budget.
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut admission = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let generation = db.db.generation().map_err(Refusal::from)?;
    let maximum = fgdb::AuthorizedReadSession::<crate::Symbols, fn() -> u64>::MAX_BATCH_STATEMENTS;
    let count = input.len();
    if !(1..=maximum).contains(&count) {
        return Err(Refusal::new(
            ErrorCode::Statement,
            format!(
                "a read batch requires between 1 and {maximum} {}",
                input.unit()
            ),
        ));
    }
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let mut parameters = Vec::with_capacity(count);
    for index in 0..count {
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        admission
            .checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        generation.check().map_err(Refusal::from)?;
        let (text, arguments) = input.statement(index)?;
        parameters.push((
            text,
            convert::parameters(arguments, None)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?,
        ));
    }
    let (mut session, _) = ordered::in_generation(&generation, read_session(cx, db, token)).await?;
    let _operation = generation.enter().map_err(Refusal::from)?;
    let statements = parameters
        .iter()
        .map(|(text, parameters)| (*text, parameters))
        .collect::<Vec<_>>();
    let results = session
        .native
        .query_batch_with_snapshots(&query, &statements);
    generation.check().map_err(Refusal::from)?;
    let results = results.map_err(query_refusal)?;
    let mut converted = Vec::with_capacity(results.len());
    for (result, snapshot) in results {
        let QueryResult::Rows { columns, rows } = result else {
            return Err(Refusal::new(
                ErrorCode::Statement,
                "a write statement cannot execute as a read",
            ));
        };
        let mut wire_rows = Vec::with_capacity(rows.len());
        for row in rows {
            query
                .checkpoint()
                .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
            admission
                .checkpoint_at(unix_millis())
                .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
            generation.check().map_err(Refusal::from)?;
            wire_rows.push(row.iter().map(convert::cell).collect());
        }
        converted.push(ReadBatchResult {
            columns,
            rows: wire_rows,
            seq: snapshot.0,
        });
    }
    admission
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    Ok(ReadBatchAnswer {
        results: converted,
        generation,
    })
}
