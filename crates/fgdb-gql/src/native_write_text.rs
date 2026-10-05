//! Native no-RETURN text binding shared by embedded and command-line writers.
//!
//! The existing compilers own syntax, types and lowering. This boundary only
//! selects the bounded UNWIND mutation adapter or the ordinary write script,
//! and retains input coordinates until execution/completion has succeeded.

use crate::unwind_write::{GraphUnwindWriteError, GraphUnwindWriteText};
use crate::{
    BoundGraphWriteScriptBatch, GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramError, GraphWriteScriptError, GraphWriteScriptExecutionError,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_delta_types::RelationId;

#[derive(Debug)]
pub enum NativeGraphWriteBindError {
    ScriptPreparation(GraphWriteScriptError),
    ScriptBinding(GraphWriteScriptError),
    Unwind(GraphUnwindWriteError),
}
impl core::fmt::Display for NativeGraphWriteBindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ScriptPreparation(error) | Self::ScriptBinding(error) => error.fmt(f),
            Self::Unwind(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for NativeGraphWriteBindError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ScriptPreparation(error) | Self::ScriptBinding(error) => Some(error),
            Self::Unwind(error) => Some(error),
        }
    }
}

enum BoundInput {
    Script(PreparedGraphWriteProgram),
    Unwind(BoundGraphWriteScriptBatch),
}

/// One fully bound native no-RETURN write. This owns no transaction or authority.
///
/// A script retains its ordinary statement coordinates. A bounded UNWIND
/// mutation additionally retains its input-row coordinates; do not discard
/// this object before translating an execution or publication failure.
/// All parameters and all UNWIND rows bind before an executable program escapes.
/// CREATE/INSERT RETURN is a separate native result-query contract and must be
/// dispatched by that existing compiler before calling this no-RETURN binder.
/// The statement/parameter size caps of the underlying compilers still apply.
pub struct BoundNativeGraphWrite(BoundInput);

impl core::fmt::Debug for BoundNativeGraphWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundNativeGraphWrite")
            .field("statements", &self.program().statements().len())
            .field("input_records", &self.input_records())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl BoundNativeGraphWrite {
    /// Bind ordinary scripts or one `UNWIND $rows AS row` MERGE/MATCH mutation.
    ///
    /// Recognition uses the existing native lexer and inspects no values or
    /// catalog state. An admitted UNWIND is never retried as another statement
    /// kind after a bind error. Ordinary UNWIND CREATE/INSERT remains on its
    /// existing compiler, including its list/composite expression semantics.
    /// UNWIND expansion uses the existing atomic-batch hard ceiling (65,536
    /// statements), not the smaller limit on statements in a script definition.
    /// Execution budgets are separate and belong to the entire returned program.
    #[allow(clippy::result_large_err)] // one terminal native binding diagnostic
    pub fn bind(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, NativeGraphWriteBindError> {
        if let Some(unwind) = GraphUnwindWriteText::parse_if_supported(text)
            .map_err(NativeGraphWriteBindError::Unwind)?
        {
            let batch = unwind
                .bind_with_limit(
                    arguments,
                    relation,
                    PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
                    &mut resolve,
                )
                .map_err(NativeGraphWriteBindError::Unwind)?;
            return Ok(Self(BoundInput::Unwind(batch)));
        }
        // Preserve native script inference: integer/pagination/list roles are
        // resolved by the existing parser; canonical scalar kinds are explicit.
        let declarations: Vec<_> = arguments
            .parameter_types()
            .filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_)))
            .collect();
        let script = PreparedGraphWriteScript::prepare_with_parameter_types(
            text,
            relation,
            &declarations,
            resolve,
        )
        .map_err(NativeGraphWriteBindError::ScriptPreparation)?;
        let program = script
            .bind_parameters(arguments)
            .map_err(NativeGraphWriteBindError::ScriptBinding)?;
        Ok(Self(BoundInput::Script(program)))
    }

    #[must_use]
    pub fn program(&self) -> &PreparedGraphWriteProgram {
        match &self.0 {
            BoundInput::Script(program) => program,
            BoundInput::Unwind(batch) => batch.program(),
        }
    }

    /// Some identifies a record-expanded UNWIND; None is an ordinary script.
    #[must_use]
    pub fn input_records(&self) -> Option<usize> {
        match &self.0 {
            BoundInput::Script(_) => None,
            BoundInput::Unwind(batch) => Some(batch.argument_sets()),
        }
    }

    /// Preserve the native program cause, adding row coordinates only when the
    /// batch can locate a failing executed step. Completion errors keep their
    /// original committed/unknown meaning and have no invented record location.
    #[must_use]
    pub fn execution_error<E, A, C>(
        &self,
        source: GraphWriteProgramError<E, A, C>,
    ) -> GraphWriteScriptExecutionError<E, A, C> {
        match &self.0 {
            BoundInput::Script(_) => GraphWriteScriptExecutionError::Program(source),
            BoundInput::Unwind(batch) => batch.execution_error(source),
        }
    }
}
