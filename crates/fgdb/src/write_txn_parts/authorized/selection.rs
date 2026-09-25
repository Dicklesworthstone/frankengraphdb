//! One masked graph source for prepared authorized writes. The caller owns the
//! already verified write permit; selection never issues a second allowance.

use super::{Database, Error, Execution, Vfs, WriteTxnError};
use crate::query::QueryError;
use fgdb_gql::algebra::{GlaOutput, PreparedGraphPattern};
use fgdb_gql::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy};
use fgdb_types::QueryCx;
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Selected<Row> = Result<GqlQueryExecution<Row>, GqlQueryError<WriteTxnError, WriteTxnError>>;

fn query_control(error: WriteTxnError) -> QueryError {
    match error {
        WriteTxnError::Authorization(error) => QueryError::Authorization(error),
        WriteTxnError::Interrupted(error) => QueryError::Pattern(GqlQueryError::Interrupted(error)),
        // Controls only produce the two errors above. Never expose a future
        // native mutation error through the selection's interruption carrier.
        _ => QueryError::Authorization(Error::ScopeDenied),
    }
}

fn selection_error(
    error: GqlQueryError<crate::ReadError, QueryError>,
) -> GqlQueryError<WriteTxnError, WriteTxnError> {
    match error {
        GqlQueryError::Source(error) => GqlQueryError::Source(WriteTxnError::from(error)),
        GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
        GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
        GqlQueryError::Interrupted(QueryError::Authorization(error)) => {
            GqlQueryError::Interrupted(WriteTxnError::Authorization(error))
        }
        GqlQueryError::Interrupted(QueryError::Pattern(GqlQueryError::Interrupted(error))) => {
            GqlQueryError::Interrupted(WriteTxnError::Interrupted(error))
        }
        GqlQueryError::Interrupted(_) => {
            GqlQueryError::Source(WriteTxnError::AuthorizedMutationRefused)
        }
    }
}

/// Call only before staging in an exclusively borrowed private transaction.
/// Its owner checks ReadWrite rights before any database access. Hidden history
/// polls cancellation without charging a signed or native resource allowance;
/// admitted data uses the same source and GLA as ordinary authorized reads.
pub(super) fn select<V: Vfs + Clone, Row: GlaOutput, Clock: FnMut() -> u64>(
    database: &Database<V>,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Selected<Row> {
    database
        .select_for_authorized_insert(
            cx,
            pattern,
            scope,
            policy,
            || {
                cx.checkpoint()
                    .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
                let mut borrowed = execution.borrow_mut();
                let execution = &mut **borrowed;
                execution.checkpoint().map_err(query_control)?;
                let now = (execution.clock)();
                execution
                    .permit
                    .charge_nodes_at(now, 1)
                    .map_err(QueryError::Authorization)
            },
            || {
                cx.checkpoint()
                    .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
                execution.borrow_mut().poll().map_err(query_control)
            },
            || {
                cx.checkpoint()
                    .map_err(|error| query_control(WriteTxnError::Interrupted(error)))?;
                execution.borrow_mut().checkpoint().map_err(query_control)
            },
        )
        .map_err(selection_error)
}
