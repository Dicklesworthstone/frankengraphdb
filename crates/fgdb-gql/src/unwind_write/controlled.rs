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
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        control: impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    ) -> Result<BoundGraphWriteScriptBatch, GraphUnwindBindError<C>> {
        self.bind_with_limit_controlled_resolver(arguments, relation, max_rows, resolve, control)
    }

    /// As [`Self::bind_with_limit_controlled`], with a host resolver whose
    /// pinned tzdb the batch's statements use (fgdb-txbg9).
    pub fn bind_with_limit_controlled_resolver<C>(
        &self,
        arguments: &GqlParameters,
        relation: RelationId,
        max_rows: usize,
        mut resolve: impl crate::GraphSymbolResolver,
        mut control: impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    ) -> Result<BoundGraphWriteScriptBatch, GraphUnwindBindError<C>> {
        work(&mut control, 1)?;
        let Some(source) = arguments.get(&self.source_parameter) else {
            return Err(GraphUnwindWriteError::SourceParameter.into());
        };
        // Keep the shared parameter handle alive while every selected row and
        // expanded frame borrows its payload. Unselected siblings are neither
        // cloned nor inserted into the native argument transcripts.
        let rows = if self.source_path.is_empty() {
            let GqlParameterValue::List(source) = &source else {
                return Err(GraphUnwindWriteError::SourceParameter.into());
            };
            source.values()
        } else {
            expansion::parameter_list(
                &source,
                &self.source_path,
                self.source_offset,
                0,
                &mut control,
            )?
        };
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
        if let Some(lowered) = self.with_map_fields(rows, &mut control)? {
            // The lowered text has no map operand left, so this re-entry
            // happens once, and the key fields bind as ordinary scalars.
            return lowered.bind_with_limit_controlled_resolver(
                arguments, relation, max_rows, resolve, control,
            );
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
        let parameter_sources =
            expansion::prepare_parameter_sources(self, arguments, &mut control)?;
        let rows =
            expansion::expand_with_parameters(self, rows, &parameter_sources, limit, &mut control)?;
        work(&mut control, self.fields.len() as u64 + 1)?;
        let mut kinds = vec![CanonicalScalarKind::Null; self.fields.len()];
        for row_index in 0..rows.len() {
            work(&mut control, 1)?;
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
            // Independent source lists are borrowed once by expansion. They
            // are not native mutation arguments and must not be cloned and
            // transcribed once per product row (including unused payloads).
            if self
                .sources
                .iter()
                .any(|source| source.parameter.as_deref() == Some(name.as_str()))
            {
                continue;
            }
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
        let prepared = PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
            &self.lowered,
            relation,
            &declarations,
            &mut MeteredResolver {
                resolve: &mut resolve,
                control: &mut control,
                interrupted: &mut interrupted,
            },
        );
        if let Some(error) = interrupted {
            return Err(GraphUnwindBindError::Interrupted(error));
        }
        work(&mut control, 1)?;
        let mut prepared = prepared.map_err(|mut error| {
            self.restore_script_offsets(&mut error);
            GraphUnwindWriteError::Definition(error)
        })?;
        control(GraphUnwindBindEvent::Definition(&prepared))
            .map_err(GraphUnwindBindError::Interrupted)?;

        let mut freeze_vertex_update_input = false;
        if rows.len() > 1 {
            // Inspect the resolved definition, not source spelling or current
            // graph contents. The definition callback still owns authorization
            // precedence, and no expanded executable statement exists yet.
            work(&mut control, self.lowered.len() as u64)?;
            for (statement, input) in prepared.statements().iter().enumerate() {
                use crate::GraphWriteTemplateStatement;
                let feeds_later_match = match input {
                    GraphWriteTemplateStatement::Mutation(mutation) => mutation.feeds_own_match(),
                    GraphWriteTemplateStatement::Delete(deletion) => {
                        deletion.selection.deletion_feeds_later_match()
                    }
                    GraphWriteTemplateStatement::Insert(_)
                    | GraphWriteTemplateStatement::VertexMerge(_)
                    | GraphWriteTemplateStatement::VertexUpsert(_)
                    | GraphWriteTemplateStatement::EdgeMerge(_)
                    | GraphWriteTemplateStatement::EdgeUpsert(_) => false,
                };
                if feeds_later_match {
                    if prepared.statements().len() == 1
                        && matches!(input, GraphWriteTemplateStatement::Mutation(mutation)
                            if mutation.is_vertex_update_only())
                    {
                        freeze_vertex_update_input = true;
                        continue;
                    }
                    return Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement }.into());
                }
            }
        }
        // The admitted definition callback saw the ordinary native template.
        // Only private batch provenance changes now: binding reads its typed
        // program, not script slices. The sole span starts in the unchanged
        // blanked prefix; its end must refer to the ORIGINAL user statement.
        // The resulting batch keeps these spans through execution errors.
        for span in &mut prepared.spans {
            debug_assert_eq!(self.original_offset(span.start), span.start);
            span.end = self.original_offset(span.end);
        }

        let mut before_allocation = true;
        let bound = prepared
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
                GraphWriteScriptBatchBindError::Binding(mut error) => {
                    if let GraphWriteScriptBatchError::Arguments { source, .. } = &mut error {
                        self.restore_script_offsets(source);
                    }
                    GraphUnwindWriteError::Binding(error).into()
                }
                GraphWriteScriptBatchBindError::Interrupted { source, .. } => {
                    GraphUnwindBindError::Interrupted(source)
                }
            })?;
        if freeze_vertex_update_input {
            // Recheck the resolved executable profile before issuing provenance;
            // a future binder extension must not widen this staging contract.
            bound
                .freeze_vertex_update_input()
                .ok_or_else(|| GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 }.into())
        } else {
            // The batch binder already staged distinct-key upserts and
            // constant updates once where that keeps the answer (fgdb-2l3pt).
            Ok(bound)
        }
    }
}

impl GraphUnwindWriteText {
    /// `SET n += row` (or `row.key` naming a map) assigns the row's map. When
    /// every input row holds a map with row 0's key set there, the operand is
    /// that literal map, one scalar field per key: `SET n += {k1: $f1, ...}`
    /// (fgdb-0s2pq). One native template serves every row, and a missing key
    /// would assign NULL where `+=` leaves a field unchanged, so differing key
    /// sets refuse. Only root rows are inspected, before expansion; any other
    /// use of a map keeps the scalar-field refusal.
    fn with_map_fields<C>(
        &self,
        rows: &[GraphValue],
        control: &mut impl FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
    ) -> Result<Option<Self>, GraphUnwindBindError<C>> {
        let mut maps: Vec<(usize, Vec<Box<str>>)> = Vec::new();
        for (index, field) in self.fields.iter().enumerate() {
            work(control, 1)?;
            if field.source != 0 {
                continue;
            }
            // An empty map has nothing to assign; it keeps the scalar refusal.
            let Some((keys, _)) = field_value(&rows[0], &field.path, field.offset, 0, control)?
                .and_then(GraphValue::as_map)
                .filter(|(keys, _)| !keys.is_empty())
            else {
                continue;
            };
            let sites = self.parameter_sites(&field.parameter);
            if sites.is_empty() || !sites.iter().all(|&site| self.assigns_map_at(site)) {
                continue;
            }
            let refusal = |row, kind| GraphUnwindWriteError::Row {
                row,
                offset: field.offset,
                kind,
            };
            for (row_index, row) in rows.iter().enumerate().skip(1) {
                work(control, keys.len() as u64 + 1)?;
                let same = field_value(row, &field.path, field.offset, row_index, control)?
                    .and_then(GraphValue::as_map)
                    .is_some_and(|(actual, _)| actual == keys);
                if !same {
                    return Err(refusal(row_index, GraphUnwindRowError::MapKeysDiffer).into());
                }
            }
            let plain = |key: &str| {
                let mut bytes = key.bytes();
                bytes
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
                    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            };
            if !keys.iter().all(|key| plain(key)) {
                return Err(refusal(0, GraphUnwindRowError::UnsupportedMapKey).into());
            }
            maps.push((index, keys.to_vec()));
        }
        if maps.is_empty() {
            return Ok(None);
        }

        let mut reserved: std::collections::BTreeSet<String> = self
            .fields
            .iter()
            .map(|field| field.parameter.clone())
            .chain(self.external_parameters.iter().cloned())
            .chain([self.source_parameter.clone()])
            .collect();
        let mut cursor = 0;
        let mut fields = Vec::with_capacity(self.fields.len());
        let mut replacements = std::collections::BTreeMap::new();
        for (index, field) in self.fields.iter().enumerate() {
            let Some((_, keys)) = maps.iter().find(|(map, _)| *map == index) else {
                fields.push(field.clone());
                continue;
            };
            work(control, keys.len() as u64 + 1)?;
            let mut operand = String::from("{");
            for (at, key) in keys.iter().enumerate() {
                // 2,704 names exceed the 1,000-argument cap each row's
                // parameter set enforces below, so this cannot run out first.
                let parameter = fresh_parameter(&mut reserved, &mut cursor)
                    .ok_or(GraphUnwindWriteError::ArgumentNames)?;
                if at != 0 {
                    operand.push_str(", ");
                }
                operand.push_str(key);
                operand.push_str(": $");
                operand.push_str(&parameter);
                let mut path = field.path.to_vec();
                path.push(UnwindFieldAccess::Key(key.to_string()));
                fields.push(UnwindField {
                    source: field.source,
                    path: path.into_boxed_slice(),
                    parameter,
                    offset: field.offset,
                });
            }
            operand.push('}');
            replacements.insert(field.parameter.as_str(), operand);
        }

        // Splice each operand over its `$name` hole. An expanded short alias
        // already has a provenance edit over exactly that hole; it is
        // replaced. Later edits shift by the growth before them, which keeps
        // original_offset's cumulative arithmetic.
        let mut sites: Vec<(usize, &str)> = replacements
            .iter()
            .flat_map(|(parameter, operand)| {
                self.parameter_sites(parameter)
                    .into_iter()
                    .map(move |site| (site, operand.as_str()))
            })
            .collect();
        sites.sort_unstable();
        let mut lowered = String::with_capacity(self.lowered.len());
        let mut source_offsets = Vec::with_capacity(self.source_offsets.len() + sites.len());
        let mut copied = 0;
        let mut growth = 0;
        let mut edits = self.source_offsets.iter().peekable();
        for (site, operand) in sites {
            let hole = site..site + 3;
            while let Some(edit) = edits.next_if(|edit| edit.generated.end <= hole.start) {
                source_offsets.push(UnwindSourceOffset {
                    generated: edit.generated.start + growth..edit.generated.end + growth,
                    original: edit.original.clone(),
                });
            }
            let original = match edits.next_if(|edit| edit.generated == hole) {
                Some(edit) => edit.original.clone(),
                None => {
                    let start = self.original_offset(hole.start);
                    start..start + 3
                }
            };
            lowered.push_str(&self.lowered[copied..hole.start]);
            let generated = lowered.len();
            lowered.push_str(operand);
            source_offsets.push(UnwindSourceOffset {
                generated: generated..lowered.len(),
                original,
            });
            growth += operand.len() - 3;
            copied = hole.end;
        }
        for edit in edits {
            source_offsets.push(UnwindSourceOffset {
                generated: edit.generated.start + growth..edit.generated.end + growth,
                original: edit.original.clone(),
            });
        }
        lowered.push_str(&self.lowered[copied..]);
        Ok(Some(Self {
            lowered,
            source_offsets: source_offsets.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            ..self.clone()
        }))
    }

    /// Byte offsets of `$name` holes in the lowered text. Generated names are
    /// two letters; a longer user parameter that shares a prefix is not one.
    fn parameter_sites(&self, parameter: &str) -> Vec<usize> {
        let hole = format!("${parameter}");
        let bytes = self.lowered.as_bytes();
        self.lowered
            .match_indices(&hole)
            .map(|(site, _)| site)
            .filter(|&site| {
                bytes
                    .get(site + hole.len())
                    .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            })
            .collect()
    }

    /// Whether the hole at `site` is the right-hand side of `+=`.
    fn assigns_map_at(&self, site: usize) -> bool {
        self.lowered[..site]
            .trim_end_matches(|ch: char| ch.is_ascii_whitespace())
            .ends_with("+=")
    }
}

/// The batch's metered catalog. Each symbol lookup costs one unit of work
/// before and one after the host's own lookup, and the first refusal is kept
/// for the caller. The host's tzdb and reverse catalog stay visible to every
/// statement (fgdb-txbg9), which a closure would drop.
struct MeteredResolver<'r, R: ?Sized, F, C> {
    resolve: &'r mut R,
    control: &'r mut F,
    interrupted: &'r mut Option<C>,
}
impl<R, F, C> crate::GraphSymbolResolver for MeteredResolver<'_, R, F, C>
where
    R: crate::GraphSymbolResolver + ?Sized,
    F: FnMut(GraphUnwindBindEvent<'_>) -> Result<(), C>,
{
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if self.interrupted.is_some() {
            return None;
        }
        if let Err(error) = (self.control)(GraphUnwindBindEvent::Work(1)) {
            *self.interrupted = Some(error);
            return None;
        }
        let symbol = self.resolve.resolve_symbol(kind, name);
        if let Err(error) = (self.control)(GraphUnwindBindEvent::Work(1)) {
            *self.interrupted = Some(error);
            return None;
        }
        symbol
    }
    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.resolve.time_zones()
    }
    fn reverse_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
        self.resolve.reverse_catalog()
    }
    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.resolve.reverse_label(id)
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.resolve.reverse_relation(id)
    }
}
