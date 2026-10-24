//! The robot `error` event's stable `code`: what a failure asks of its
//! caller. A code comes from the typed error the CLI still holds, never from
//! message text; a failure it cannot classify keeps its class's default.

use super::Failure;
use fgdb_gql::{GqlQueryError, GqlRefusalClass};
use std::error::Error;

/// A closed set, documented once in `fgdb robot schema` (`errors.codes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    /// Exit 2: the command line is wrong.
    Usage,
    /// The statement does not parse; `offset` is where to fix it.
    Syntax,
    /// A recognized construct that is not supported yet.
    Unsupported,
    /// A label, relation or property name has no binding.
    NotFound,
    /// A parameter or value is missing, unexpected, mistyped or out of range.
    InvalidValue,
    /// The statement parses, but its definition is refused.
    Statement,
    /// An execution limit was exceeded; a larger limit may succeed.
    Budget,
    /// The command or capability may not do this (a write on read-only `query`).
    Permission,
    /// A concurrent commit won; nothing was committed and a retry may succeed.
    Conflict,
    /// Refused while executing or delivering; nothing was committed.
    Execution,
    /// Exit 4: the database or key could not be opened.
    Open,
    /// Exit 5: storage or the operating system failed.
    Io,
}

impl Code {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Syntax => "syntax",
            Self::Unsupported => "unsupported",
            Self::NotFound => "not_found",
            Self::InvalidValue => "invalid_value",
            Self::Statement => "statement",
            Self::Budget => "budget",
            Self::Permission => "permission",
            Self::Conflict => "conflict",
            Self::Execution => "execution",
            Self::Open => "open",
            Self::Io => "io",
        }
    }
}

impl From<GqlRefusalClass> for Code {
    fn from(class: GqlRefusalClass) -> Self {
        match class {
            GqlRefusalClass::Syntax => Self::Syntax,
            GqlRefusalClass::Unsupported => Self::Unsupported,
            GqlRefusalClass::NotFound => Self::NotFound,
            GqlRefusalClass::InvalidValue => Self::InvalidValue,
            GqlRefusalClass::Statement => Self::Statement,
        }
    }
}

type Cancel = Box<asupersync::error::Error>;
/// The one write-program instantiation every embedded write path executes.
type WriteProgramError =
    fgdb_gql::GraphWriteProgramError<fgdb::WriteTxnError, fgdb::WriteTxnError, Cancel>;

/// The code and statement byte offset of the first error in `error`'s source
/// chain that names one.
pub(crate) fn classify(error: &(dyn Error + 'static)) -> Option<(Code, Option<usize>)> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(found) = classify_one(error) {
            return Some(found);
        }
        current = error.source();
    }
    None
}

/// A statement's text error: its class and its byte.
macro_rules! text_errors {
    ($error:ident, $($ty:ty),+ $(,)?) => {
        $(if let Some(text) = $error.downcast_ref::<$ty>() {
            return Some((text.class().into(), Some(text.offset)));
        })+
    };
}

fn classify_one(error: &(dyn Error + 'static)) -> Option<(Code, Option<usize>)> {
    text_errors!(
        error,
        fgdb_gql::GqlUnsupportedConstruct,
        fgdb_gql::GraphWriteScriptError,
        fgdb_gql::GraphPatternTextError,
        fgdb_gql::GraphSetTextError,
        fgdb_gql::GraphPipelineAggregateTextError,
        fgdb_gql::GraphTemporalTextError,
        fgdb_gql::GraphTemporalSetTextError,
        fgdb_gql::GraphInsertTextError,
        fgdb_gql::GraphMutationTextError,
        fgdb_gql::GraphDeleteTextError,
        fgdb_gql::GraphVertexMergeTextError,
        fgdb_gql::GraphEdgeMergeTextError,
        fgdb_gql::GraphVertexUpsertTextError,
        fgdb_gql::GraphEdgeUpsertTextError,
        fgdb_gql::GraphCheapestPathTextError,
        fgdb_gql::GraphBranchTextError,
    );
    if let Some(error) = error.downcast_ref::<fgdb::QueryError>() {
        return query(error);
    }
    if error.is::<fgdb_gql::GqlBudgetExceeded>() || error.is::<fgdb_gql::GlaLimitExceeded>() {
        return Some((Code::Budget, None));
    }
    // A data exception (division by zero, overflow, a collection where a
    // scalar is read) is the value's, not the statement's.
    if error.is::<fgdb_gql::GraphIntegerError>() {
        return Some((Code::InvalidValue, None));
    }
    if let Some(error) = error.downcast_ref::<WriteProgramError>()
        && matches!(
            error,
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Budget { .. }
            ) | fgdb_gql::GraphWriteProgramError::CreationBudget { .. }
        )
    {
        return Some((Code::Budget, None));
    }
    if let Some(error) = error.downcast_ref::<fgdb_gql::GraphWriteScriptBatchError>() {
        use fgdb_gql::GraphWriteScriptBatchError as B;
        return match error {
            B::TooManyStatements { .. } => Some((Code::Budget, None)),
            B::Empty => Some((Code::InvalidValue, None)),
            B::Definition(_) => Some((Code::Statement, None)),
            B::Arguments { .. } => None,
        };
    }
    if let Some(error) = error.downcast_ref::<fgdb_gql::unwind_write::GraphUnwindWriteError>() {
        use fgdb_gql::unwind_write::GraphUnwindWriteError as U;
        return match error {
            U::TooManyRows { .. } | U::ExpandedParametersTooLarge { .. } => {
                Some((Code::Budget, None))
            }
            U::RowFeedsLaterMatch { .. } => Some((Code::Unsupported, None)),
            U::Row { offset, .. } | U::Expansion { offset, .. } => {
                Some((Code::InvalidValue, Some(*offset)))
            }
            U::SourceParameter | U::ArgumentNames | U::Empty | U::Parameter { .. } => {
                Some((Code::InvalidValue, None))
            }
            U::Syntax(_) | U::Definition(_) | U::Binding(_) => None,
        };
    }
    if let Some(error) = error.downcast_ref::<fgdb_prism::FnxBindError>() {
        return Some((procedure(&error.kind), Some(error.at)));
    }
    if let Some(error) = error.downcast_ref::<fgdb_prism::FnxCallError>() {
        return Some((procedure(&error.kind), None));
    }
    if let Some(error) = error.downcast_ref::<fgdb::WriteError>()
        && matches!(error, fgdb::WriteError::FirstCommitterWins { .. })
    {
        return Some((Code::Conflict, None));
    }
    if let Some(error) = error.downcast_ref::<fgdb::WriteTxnError>()
        && matches!(
            error,
            fgdb::WriteTxnError::Authorization(_)
                | fgdb::WriteTxnError::AuthorizedMutationRefused
                | fgdb::WriteTxnError::AuthorizedClientIdentity
        )
    {
        return Some((Code::Permission, None));
    }
    None
}

fn query(error: &fgdb::QueryError) -> Option<(Code, Option<usize>)> {
    use fgdb::QueryError as Q;
    match error {
        Q::Refused { source, .. } => query(source),
        Q::PatternText(text) => Some((text.class().into(), Some(text.offset))),
        Q::SetText(text) => Some((text.class().into(), Some(text.offset))),
        Q::PipelineText(text) => Some((text.class().into(), Some(text.offset))),
        Q::TemporalText(text) => Some((text.class().into(), Some(text.offset))),
        Q::TemporalSetText(text) => Some((text.class().into(), Some(text.offset))),
        Q::Unsupported { .. }
        | Q::StreamingUnsupported { .. }
        | Q::TemporalTransactionUnsupported { .. }
        | Q::AggregateStreamPlan(_)
        | Q::EdgeAggregateStreamPlan(_) => Some((Code::Unsupported, None)),
        Q::Authorization(_) => Some((Code::Permission, None)),
        Q::Pattern(error) => execution(error),
        Q::Aggregate(error) => execution(error),
        Q::Set(error) => execution(error),
        Q::Stream(error) => execution(error),
        Q::EdgeStream(error) => execution(error),
        Q::AggregateStream(error) => execution(error),
        Q::EdgeAggregateStream(error) => execution(error),
        Q::TransactionPattern(error) => execution(error),
        Q::TransactionAggregate(error) => execution(error),
        Q::TransactionSet(error) => execution(error),
        Q::Read(_) | Q::Transaction(_) => None,
    }
}

fn execution<E, C>(error: &GqlQueryError<E, C>) -> Option<(Code, Option<usize>)> {
    match error {
        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_) => Some((Code::Budget, None)),
        GqlQueryError::Data(_) => Some((Code::InvalidValue, None)),
        GqlQueryError::Source(_)
        | GqlQueryError::Interrupted(_)
        | GqlQueryError::IdentifiedEdgesRequired => None,
    }
}

fn procedure(kind: &fgdb_prism::FnxBindErrorKind) -> Code {
    use fgdb_prism::FnxBindErrorKind as K;
    match kind {
        K::Expected(_) | K::TrailingInput | K::DuplicateYield => Code::Syntax,
        K::UnknownProcedure | K::UnknownYield => Code::NotFound,
        K::MissingParameter
        | K::MissingArgument(_)
        | K::InvalidArgument(_)
        | K::TooManyArguments => Code::InvalidValue,
        K::TextTooLong => Code::Statement,
    }
}

/// Name a construct no native compiler implements before one of them refuses
/// it at a byte that points elsewhere. The embedded CLI has no branch catalog,
/// so `AT BRANCH` is one too.
pub(crate) fn unsupported_construct(text: &str) -> Result<(), Failure> {
    match fgdb_gql::GqlUnsupportedConstruct::find(text, false) {
        Some(construct) => Err(Failure::statement(construct)),
        None => Ok(()),
    }
}
