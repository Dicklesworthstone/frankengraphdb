//! One UNWIND binder, with admission before owned expansion and native binding.

use super::*;
use crate::{GqlParameterType, GqlParameterValue, GraphWriteScriptBatchBindError};
use fgdb_types::CanonicalScalarKind;

/// Definition work, not graph-query work or a byte-memory quota. A host may
/// charge these units to the SAME live permit used for subsequent execution.
#[derive(Clone, Copy, Debug)]
pub enum GraphUnwindBindEvent<'a> {
    /// Reserve before the corresponding allocation, copy or bounded operation.
    /// Payloads are billed per occurrence even when immutable storage is shared.
    Work(u64),
    /// Inspect every compiled operation class and relation before binding the
    /// expanded program. This immutable definition grants no graph authority.
    Definition(&'a PreparedGraphWriteScript),
}

/// No statement has executed. A control refusal is not a malformed input and
/// carries neither a partial program nor an invented executed-row coordinate.
#[derive(Debug)]
pub enum GraphUnwindBindError<C> {
    Binding(GraphUnwindWriteError),
    Interrupted(C),
}
impl<C> From<GraphUnwindWriteError> for GraphUnwindBindError<C> {
    fn from(error: GraphUnwindWriteError) -> Self {
        Self::Binding(error)
    }
}
impl<C: core::fmt::Display> core::fmt::Display for GraphUnwindBindError<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Binding(error) => error.fmt(f),
            Self::Interrupted(error) => error.fmt(f),
        }
    }
}
impl<C: core::error::Error + 'static> core::error::Error for GraphUnwindBindError<C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::Interrupted(error) => Some(error),
        }
    }
}

fn work<C>(
    control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    units: u64,
) -> Result<(), GraphUnwindBindError<C>> {
    control(GraphUnwindBindEvent::Work(units)).map_err(GraphUnwindBindError::Interrupted)
}

fn reserve_scalar<C>(
    value: Option<&CanonicalScalar>,
    control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
) -> Result<(), GraphUnwindBindError<C>> {
    work(control, 1)?;
    let sizes = match value {
        Some(CanonicalScalar::Text(value)) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        Some(CanonicalScalar::Bytes(value)) => [value.as_slice().len(), 0],
        Some(CanonicalScalar::Timestamp(value)) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
        _ => [0, 0],
    };
    for bytes in sizes {
        if bytes != 0 {
            // The caller's checked list bounds each scalar. No encoding or
            // cloning is needed to admit its variable-size payload first.
            work(control, bytes as u64)?;
        }
    }
    Ok(())
}

impl GraphUnwindWriteText {
    /// Bind through the ordinary UNWIND compiler with cooperative admission.
    ///
    /// The host receives one unit per bounded lookup/row, metadata reservations
    /// before vector/map construction, one unit per copied scalar plus its
    /// variable payload bytes, and each repeated global parameter transcript.
    /// Native compilation reserves the lowered text's byte length. Resolver
    /// callbacks are checked before AND after; a refusal is latched, suppresses
    /// subsequent catalog calls, and wins over the resulting parser diagnostic.
    ///
    /// Shape/type/transcript admission still precedes all catalog callbacks.
    /// Definition admission precedes expanded native binding; its entire
    /// statement-instance bill precedes that allocation, followed by controls
    /// at record boundaries and final acceptance. No row refreshes an allowance.
    ///
    /// Parsing this object's original text and constructing caller arguments
    /// are outside this method. Nested paths are checked before each selector.
    /// A bounded field lookup, scalar construction,
    /// native parser operation or host callback is not internally preemptible.
    /// These logical ingress prices are not allocator-byte bounds or a claim
    /// that compilation has become a streaming operation.
    pub fn bind_with_limit_controlled<C>(
        &self,
        arguments: &GqlParameters,
        relation: RelationId,
        max_rows: usize,
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        mut control: impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    ) -> Result<BoundGraphWriteScriptBatch, GraphUnwindBindError<C>> {
        work(&mut control, 1)?;
        let Some(GqlParameterValue::List(source)) = arguments.get(&self.source_parameter) else {
            return Err(GraphUnwindWriteError::SourceParameter.into());
        };
        let rows = source.values();
        if rows.is_empty() {
            return Err(GraphUnwindWriteError::Empty.into());
        }
        let limit = max_rows.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        if rows.len() > limit {
            return Err(GraphUnwindWriteError::TooManyRows {
                limit,
                observed: rows.len(),
            }
            .into());
        }
        if arguments.len() != self.external_parameters.len() + 1 {
            return Err(GraphUnwindWriteError::ArgumentNames.into());
        }
        for name in self.external_parameters.iter() {
            work(&mut control, 1)?;
            if arguments.get(name).is_none() {
                return Err(GraphUnwindWriteError::ArgumentNames.into());
            }
        }

        // Expand parameter documents, not graph state. Preserve earlier alias
        // bindings by reference; every intermediate cardinality is admitted.
        let rows = expansion::expand(self, rows, limit, &mut control)?;
        work(&mut control, self.fields.len() as u64 + 1)?;
        let mut kinds = vec![CanonicalScalarKind::Null; self.fields.len()];
        for row_index in 0..rows.len() {
            work(&mut control, 1)?;
            let row = rows.at(row_index, 0);
            if !matches!(
                row,
                GraphValue::Map { .. } | GraphValue::Scalar(CanonicalScalar::Null)
            ) {
                return Err(GraphUnwindWriteError::Row {
                    row: row_index,
                    offset: self.source_offset,
                    kind: GraphUnwindRowError::ExpectedMap,
                }
                .into());
            }
            for (field, kind) in self.fields.iter().zip(kinds.iter_mut()) {
                let row = rows.at(row_index, field.source);
                if let Some(value) = scalar_field(row, field, row_index, &mut control)? {
                    let actual = CanonicalScalarKind::of(value);
                    if actual == CanonicalScalarKind::Null {
                        continue;
                    }
                    if *kind != CanonicalScalarKind::Null && *kind != actual {
                        return Err(GraphUnwindWriteError::Row {
                            row: row_index,
                            offset: field.offset,
                            kind: GraphUnwindRowError::IncompatibleFieldTypes,
                        }
                        .into());
                    }
                    *kind = actual;
                }
            }
        }

        let mut globals = GqlParameters::new();
        for name in self.external_parameters.iter() {
            work(&mut control, name.len() as u64 + 1)?;
            let value = arguments
                .get(name)
                .ok_or(GraphUnwindWriteError::ArgumentNames)?;
            globals
                .insert(name.clone(), value)
                .map_err(|source| GraphUnwindWriteError::Parameter { row: 0, source })?;
        }
        work(&mut control, (globals.len() + self.fields.len()) as u64 + 1)?;
        let mut declarations: Vec<_> = globals.parameter_types().collect();
        declarations.extend(
            self.fields
                .iter()
                .zip(&kinds)
                .map(|(field, kind)| (field.parameter.as_str(), GqlParameterType::Scalar(*kind))),
        );

        work(&mut control, rows.len() as u64)?;
        let mut sets = Vec::with_capacity(rows.len());
        let mut expanded_bytes = 0_u128;
        for row_index in 0..rows.len() {
            work(&mut control, globals.canonical_byte_len() as u64 + 1)?;
            let mut values = globals.clone();
            for field in self.fields.iter() {
                let row = rows.at(row_index, field.source);
                let value = scalar_field(row, field, row_index, &mut control)?;
                reserve_scalar(value, &mut control)?;
                values = values
                    .with_scalar(
                        field.parameter.clone(),
                        value.cloned().unwrap_or(CanonicalScalar::Null),
                    )
                    .map_err(|source| GraphUnwindWriteError::Parameter {
                        row: row_index,
                        source,
                    })?;
            }
            expanded_bytes += values.canonical_byte_len() as u128;
            if expanded_bytes > MAX_UNWIND_BOUND_PARAMETER_BYTES as u128 {
                return Err(GraphUnwindWriteError::ExpandedParametersTooLarge {
                    limit: MAX_UNWIND_BOUND_PARAMETER_BYTES,
                    observed: expanded_bytes,
                }
                .into());
            }
            sets.push(values);
        }

        work(&mut control, self.lowered.len() as u64)?;
        let mut interrupted = None;
        let prepared = PreparedGraphWriteScript::prepare_with_parameter_types(
            &self.lowered,
            relation,
            &declarations,
            |kind, name| {
                if interrupted.is_some() {
                    return None;
                }
                if let Err(error) = control(GraphUnwindBindEvent::Work(1)) {
                    interrupted = Some(error);
                    return None;
                }
                let symbol = resolve(kind, name);
                if let Err(error) = control(GraphUnwindBindEvent::Work(1)) {
                    interrupted = Some(error);
                    return None;
                }
                symbol
            },
        );
        if let Some(error) = interrupted {
            return Err(GraphUnwindBindError::Interrupted(error));
        }
        work(&mut control, 1)?;
        let prepared = prepared.map_err(GraphUnwindWriteError::Definition)?;
        control(GraphUnwindBindEvent::Definition(&prepared))
            .map_err(GraphUnwindBindError::Interrupted)?;

        let mut before_allocation = true;
        prepared
            .bind_parameter_sets_controlled(&sets, limit, |at| {
                let units = if at.is_none() && before_allocation {
                    before_allocation = false;
                    // The native binder checks its 65,536-statement hard bound
                    // before this event, so this multiplication cannot wrap.
                    sets.len() as u64 * prepared.statements().len() as u64
                } else {
                    1
                };
                control(GraphUnwindBindEvent::Work(units))
            })
            .map_err(|error| match error {
                GraphWriteScriptBatchBindError::Binding(error) => {
                    GraphUnwindWriteError::Binding(error).into()
                }
                GraphWriteScriptBatchBindError::Interrupted { source, .. } => {
                    GraphUnwindBindError::Interrupted(source)
                }
            })
    }
}
