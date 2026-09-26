//! Failure vocabulary for an atomic write program followed by a result query.

use crate::{GqlQueryError, GraphWriteProgramError};

/// Neither arm contains a successful write prefix or result prefix. A result
/// query runs over private staged state BEFORE native transaction completion.
/// `Program` includes native completion failures and therefore does not imply
/// rollback: the embedded owner's committed/unknown/recovery contract applies.
/// `Query` is a prepublication refusal; its query/evaluator/control error remains
/// inspectable rather than being mislabeled as a mutation statement failure.
#[derive(Debug)]
pub enum GraphWriteQueryError<E, A, C> {
    Program(GraphWriteProgramError<E, A, C>),
    Query(GqlQueryError<E, C>),
}

impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphWriteQueryError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Program(error) => error.fmt(f),
            Self::Query(error) => write!(f, "write program result query: {error}"),
        }
    }
}

impl<
    E: core::error::Error + 'static,
    A: core::error::Error + 'static,
    C: core::error::Error + 'static,
> core::error::Error for GraphWriteQueryError<E, A, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Program(error) => Some(error),
            Self::Query(error) => Some(error),
        }
    }
}
