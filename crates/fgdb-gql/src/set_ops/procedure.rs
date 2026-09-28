//! Appendix C `ProcedureCall`: a registered procedure's result relation as a
//! read-pipeline source (`CALL ns.name(args) YIELD out [AS alias], ...`).
//!
//! Loom only names, types and positions the call. The host resolves and runs
//! it (Prism for `fnx.*`) through [`GraphSetSource::procedure`], so this crate
//! never depends on a procedure implementation. A host that supplies no
//! procedures refuses with `ProcedureUnavailable`; an unknown name or bad
//! argument is the host's typed refusal. Yield columns are the Any domain, as
//! UNWIND columns are: their values are checked where they are used.

use crate::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use crate::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetValue};

/// A prepared, bound procedure invocation. Arguments are constant values
/// (literals or bound parameters), evaluated once per execution.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedProcedureCall {
    pub(super) namespace: String,
    pub(super) name: String,
    pub(super) arguments: Vec<GraphSetValue>,
    /// The procedure's own output names, in YIELD order.
    pub(super) outputs: Vec<String>,
}

impl PreparedProcedureCall {
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The procedure output columns the host must return, in this order.
    #[must_use]
    pub fn outputs(&self) -> &[String] {
        &self.outputs
    }
    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        for text in [&self.namespace, &self.name] {
            bytes.extend_from_slice(&(text.len() as u64).to_be_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        bytes.extend_from_slice(&(self.arguments.len() as u64).to_be_bytes());
        for argument in &self.arguments {
            super::projection::append_value_transcript(argument, bytes);
        }
        bytes.extend_from_slice(&(self.outputs.len() as u64).to_be_bytes());
        for output in &self.outputs {
            bytes.extend_from_slice(&(output.len() as u64).to_be_bytes());
            bytes.extend_from_slice(output.as_bytes());
        }
    }
}

/// The trusted row sources one set execution may read: graph patterns and,
/// when the host supplies them, procedure calls. Every existing closure over
/// patterns is a source with no procedures.
pub trait GraphSetSource<E, C> {
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>;

    /// `None` means this host executes no procedures. Rows must carry exactly
    /// `call.outputs()` columns, in that order.
    fn procedure(
        &mut self,
        _call: &PreparedProcedureCall,
        _arguments: &[GraphValue],
        _policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        None
    }
}

impl<E, C, F> GraphSetSource<E, C> for F
where
    F: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        self(pattern, policy)
    }
}

/// A pattern source plus a procedure host.
pub struct WithProcedures<S, P> {
    pub patterns: S,
    pub procedures: P,
}

impl<E, C, S, P> GraphSetSource<E, C> for WithProcedures<S, P>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    P: FnMut(
        &PreparedProcedureCall,
        &[GraphValue],
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
        (self.patterns)(pattern, policy)
    }
    fn procedure(
        &mut self,
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>> {
        Some((self.procedures)(call, arguments, policy))
    }
}
