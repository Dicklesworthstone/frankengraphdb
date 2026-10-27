//! Complete native result differences across two cuts of one authorized view.
//! Endpoint evaluation, masking and exact consolidation stay in the engine.

use super::*;
use core::future::Future;
use core::pin::Pin;
use fgdb_gql::result_diff::{GraphDiffError, GraphResultChanges};
use fgdb_types::CommitSeq;

pub(crate) struct DifferenceAnswer {
    pub(crate) columns: Vec<String>,
    pub(crate) entries: Vec<(i128, Vec<WireValue>)>,
    pub(crate) before: u64,
    pub(crate) after: u64,
    pub(crate) generation: Generation,
}

enum Definition<'a> {
    Text(&'a str),
    Prepared(&'a PreparedRead),
}

/// Bind one native read and compare its complete results, after minus before.
/// The retained generation also fences later transport delivery.
pub(crate) fn read_difference<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    text: &'a str,
    parameters: &'a [(String, WireValue)],
    before: u64,
    after: u64,
) -> Pin<Box<dyn Future<Output = Result<DifferenceAnswer, Refusal>> + Send + 'a>> {
    Box::pin(difference_inner(
        cx,
        db,
        token,
        Definition::Text(text),
        parameters,
        before,
        after,
    ))
}

/// Reuse catalog bindings, with a fresh authorized session and arguments for
/// both endpoints. A connection template carries no execution authority.
pub(crate) fn read_prepared_difference<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    prepared: &'a PreparedRead,
    parameters: &'a [(String, WireValue)],
    before: u64,
    after: u64,
) -> Pin<Box<dyn Future<Output = Result<DifferenceAnswer, Refusal>> + Send + 'a>> {
    Box::pin(difference_inner(
        cx,
        db,
        token,
        Definition::Prepared(prepared),
        parameters,
        before,
        after,
    ))
}

async fn difference_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    definition: Definition<'_>,
    parameters: &[(String, WireValue)],
    before: u64,
    after: u64,
) -> Result<DifferenceAnswer, Refusal> {
    // Authentication precedes argument conversion, template status, source
    // admission and syntax. Recheck this permit after conversion and delivery
    // adaptation; the native session owns the cumulative execution allowance.
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut admission = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let generation = db.db.generation().map_err(Refusal::from)?;
    if let Definition::Prepared(prepared) = &definition {
        prepared.generation.check().map_err(Refusal::from)?;
    }
    let parameters = convert::parameters(parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()));
    admission
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    let parameters = parameters?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let before = CommitSeq(before);
    let after = CommitSeq(after);
    let changes = match definition {
        Definition::Text(text) => {
            let (mut session, _) =
                ordered::in_generation(&generation, read_session(cx, db, token)).await?;
            let _operation = generation.enter().map_err(Refusal::from)?;
            let result = session
                .native
                .query_diff(&query, text, &parameters, before, after);
            generation.check().map_err(Refusal::from)?;
            result.map_err(difference_refusal)?
        }
        Definition::Prepared(prepared) => {
            let (mut session, template) =
                ordered::in_generation(&generation, read_prepared_session(cx, db, token, prepared))
                    .await?;
            let _operation = generation.enter().map_err(Refusal::from)?;
            let parameters = prepared.arguments(&query, db, token, &parameters)?;
            let result = session
                .native
                .execute_diff(&query, &template, &parameters, before, after);
            generation.check().map_err(Refusal::from)?;
            prepared.generation.check().map_err(Refusal::from)?;
            result.map_err(difference_refusal)?
        }
    };
    let _operation = generation.enter().map_err(Refusal::from)?;
    let entries = wire_entries(&changes);
    admission
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    Ok(DifferenceAnswer {
        columns: changes.columns().to_vec(),
        entries: entries?,
        before: changes.before().0,
        after: changes.after().0,
        generation,
    })
}

fn wire_entries(changes: &GraphResultChanges) -> Result<Vec<(i128, Vec<WireValue>)>, Refusal> {
    changes
        .changes()
        .iter()
        .map(|(row, weight)| {
            Ok((
                wire_weight(weight)?,
                row.iter().map(convert::cell).collect(),
            ))
        })
        .collect()
}

fn wire_weight(weight: &fgdb_delta_types::ZWeight) -> Result<i128, Refusal> {
    weight.to_i128().ok_or_else(|| {
        Refusal::new(
            ErrorCode::Execution,
            "a difference weight exceeds the wire range",
        )
    })
}

fn difference_refusal(error: GqlQueryError<GraphDiffError<QueryError>, QueryError>) -> Refusal {
    let message = error.to_string();
    let code = match error {
        GqlQueryError::Source(
            GraphDiffError::Definition(error) | GraphDiffError::Endpoint { source: error, .. },
        )
        | GqlQueryError::Interrupted(error) => query_refusal(error).code,
        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_) => ErrorCode::Budget,
        GqlQueryError::Source(GraphDiffError::TemporalSelector)
        | GqlQueryError::IdentifiedEdgesRequired => ErrorCode::Statement,
        GqlQueryError::Source(_) | GqlQueryError::Data(_) => ErrorCode::Execution,
    };
    Refusal::new(code, message)
}

#[cfg(test)]
#[path = "difference/tests.rs"]
mod tests;
