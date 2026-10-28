//! Native no-RETURN text binding shared by embedded and command-line writers.
//!
//! The existing compilers own syntax, types and lowering. This boundary only
//! selects the bounded UNWIND mutation adapter or the ordinary write script,
//! splices a script's map arguments that stand for property maps
//! (GqlMapArgumentText), and retains input coordinates until
//! execution/completion has succeeded.

use crate::unwind_write::{GraphUnwindWriteError, GraphUnwindWriteText};
use crate::{
    BoundGraphWriteScriptBatch, GqlMapArgumentText, GqlParameterType, GqlParameters, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramError, GraphWriteScriptError, GraphWriteScriptExecutionError,
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
    /// Probe a native no-RETURN definition without expanding argument rows.
    ///
    /// This is a dispatch probe, not a bound program or authority. Ordinary
    /// scripts, including computed-map UNWIND updates, are prepared but not
    /// bound; row-expanded UNWIND admission is lexical only. Values, row shapes,
    /// downstream types and execution budgets still
    /// require the ordinary binder. Collection contents are never inspected.
    /// Native reads should be admitted by their own compiler before this probe.
    /// Map arguments standing for property maps splice as `bind` splices
    /// them, so a statement the probe admits is one every native write
    /// executor binds (fgdb-0s2pq).
    #[allow(clippy::result_large_err)]
    pub fn probe_definition(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<(), NativeGraphWriteBindError> {
        let computed_maps = GraphUnwindWriteText::requires_computed_map_input(text)
            .map_err(NativeGraphWriteBindError::Unwind)?;
        if !computed_maps
            && GraphUnwindWriteText::parse_if_supported(text)
                .map_err(NativeGraphWriteBindError::Unwind)?
                .is_some()
        {
            return Ok(());
        }
        let lowered = if computed_maps {
            GqlMapArgumentText::lower_fields(text, arguments)
        } else {
            GqlMapArgumentText::lower(text, arguments)
        };
        let (text, arguments) = lowered
            .as_ref()
            .map_or((text, arguments), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
        Self::prepare_script(text, arguments, relation, resolve)
            .map(|_| ())
            .map_err(|error| NativeGraphWriteBindError::ScriptPreparation(restored(lowered, error)))
    }

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
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, NativeGraphWriteBindError> {
        Self::bind_with_resolver(text, arguments, relation, resolve)
    }

    /// As [`Self::bind`], with the host's own resolver rather than a closure
    /// over it, so the statements see its pinned tzdb (fgdb-txbg9).
    #[allow(clippy::result_large_err)]
    pub fn bind_with_resolver(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        mut resolve: impl crate::GraphSymbolResolver,
    ) -> Result<Self, NativeGraphWriteBindError> {
        let computed_maps = GraphUnwindWriteText::requires_computed_map_input(text)
            .map_err(NativeGraphWriteBindError::Unwind)?;
        if !computed_maps
            && let Some(unwind) = GraphUnwindWriteText::parse_if_supported(text)
                .map_err(NativeGraphWriteBindError::Unwind)?
        {
            let batch = unwind
                .bind_with_limit_resolver(
                    arguments,
                    relation,
                    PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
                    crate::graph_text::LentResolver(&mut resolve),
                )
                .map_err(NativeGraphWriteBindError::Unwind)?;
            return Ok(Self(BoundInput::Unwind(batch)));
        }
        // Computed whole-map UNWIND updates keep one relational MATCH bag,
        // including heterogeneous maps and rekeys. As on authorized ingress,
        // only pattern fields are lowered; whole map operands stay values.
        // Other scripts retain their ordinary literal-map argument lowering.
        let lowered = if computed_maps {
            GqlMapArgumentText::lower_fields(text, arguments)
        } else {
            GqlMapArgumentText::lower(text, arguments)
        };
        let (text, arguments) = lowered
            .as_ref()
            .map_or((text, arguments), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
        let script = Self::prepare_script(text, arguments, relation, resolve).map_err(|error| {
            NativeGraphWriteBindError::ScriptPreparation(restored(lowered, error))
        })?;
        let program = script
            .bind_parameters(arguments)
            .map_err(|error| NativeGraphWriteBindError::ScriptBinding(restored(lowered, error)))?;
        Ok(Self(BoundInput::Script(program)))
    }

    #[allow(clippy::result_large_err)]
    fn prepare_script(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        mut resolve: impl crate::GraphSymbolResolver,
    ) -> Result<PreparedGraphWriteScript, GraphWriteScriptError> {
        // Preserve numeric inference while declaring collection/scalar roles.
        // Collections may occur outside UNWIND, which otherwise infers List.
        let declarations: Vec<_> = arguments
            .parameter_types()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
                )
            })
            .collect();
        PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
            text,
            relation,
            &declarations,
            &mut resolve,
        )
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

/// A script error of a lowered statement, at the caller's bytes.
fn restored(
    lowered: Option<&GqlMapArgumentText>,
    mut error: GraphWriteScriptError,
) -> GraphWriteScriptError {
    if let Some(lowered) = lowered {
        lowered.restore(&mut error);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_probe_and_binder_share_ordinary_script_preparation() {
        let arguments = GqlParameters::new();
        let text = "CREATE (n); CREATE (m)";
        BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| None)
            .unwrap();
        let bound =
            BoundNativeGraphWrite::bind(text, &arguments, RelationId(1), |_, _| None).unwrap();
        assert_eq!(bound.program().statements().len(), 2);
        assert_eq!(bound.input_records(), None);
    }

    #[test]
    fn definition_probe_does_not_bind_or_expand_unwind_rows() {
        let text = "UNWIND $rows AS row MERGE (n:Item {key: row.key})";
        for arguments in [
            GqlParameters::new(),
            GqlParameters::new().with_list("rows", Vec::new()).unwrap(),
        ] {
            BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| {
                panic!("lexical UNWIND probing must not resolve row-dependent symbols")
            })
            .unwrap();
            // A successful probe is not a bound program: missing and empty
            // sources must still fail the ordinary all-rows binding boundary.
            assert!(matches!(
                BoundNativeGraphWrite::bind(text, &arguments, RelationId(1), |_, _| None),
                Err(NativeGraphWriteBindError::Unwind(_))
            ));
        }
    }

    #[test]
    fn definition_probe_refuses_reads_returning_writes_and_broken_scripts() {
        let arguments = GqlParameters::new();
        for text in ["RETURN 1", "CREATE (n) RETURN n", "CREATE ("] {
            assert!(
                BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| {
                    None
                },)
                .is_err(),
                "{text}",
            );
        }
    }
}
