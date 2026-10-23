//! Simultaneous assignment reduction. Borrowed scalars point into the frozen
//! selection/literals; computed scalars move into intent ownership without copies.

mod stored;

use super::*;
use crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
use crate::algebra_exec::charge_payload;
use crate::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
    GraphIntegerEvaluationError,
};
use fgdb_delta_types::ElementId;
use std::collections::BTreeMap;

type ResultOf<T, E, C> = Result<T, GqlQueryError<GraphMutationError<E>, C>>;

fn property_intent(
    target: ElementId,
    key: PropertyKeyId,
    value: Option<CanonicalPropertyValue>,
) -> GraphMutationIntent {
    match target {
        ElementId::Vertex(vertex) => GraphMutationIntent::Property { vertex, key, value },
        ElementId::Edge(edge) => GraphMutationIntent::EdgeProperty { edge, key, value },
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Property(PropertyKeyId),
    Label(LabelId),
    Delete,
}
enum Value<'a> {
    Property(Option<&'a CanonicalScalar>),
    /// Canonical NULL is stored, never a property-removal intention.
    Computed(CanonicalScalar),
    /// A checked list assignment, frozen under the same cumulative meter.
    Stored(CanonicalPropertyValue),
    Label(bool),
    /// DETACH DELETE when true. Preparation keeps one mode per statement.
    Delete(bool),
}
struct Proposal<'a> {
    value: Value<'a>,
    row: usize,
    action: usize,
}

struct Meter<F> {
    policy: GraphMutationPolicy,
    evaluator: GlaExecutionStats,
    checkpoint: F,
}

/// Record one element-field proposal: an equal repeat is one effect, a
/// different value for the same field is a conflict, and every new field
/// counts against the statement's effect limit.
#[allow(clippy::too_many_arguments)]
fn propose<'a, E, C, F: FnMut() -> Result<(), C>>(
    proposals: &mut BTreeMap<(ElementId, Field), Proposal<'a>>,
    meter: &mut Meter<F>,
    target: ElementId,
    field: Field,
    value: Value<'a>,
    row: usize,
    action: usize,
) -> ResultOf<(), E, C> {
    if let Some(previous) = proposals.get(&(target, field)) {
        if !equal(&previous.value, &value, &mut |event| meter.event(event))? {
            return Err(GqlQueryError::Source(
                GraphMutationError::ConflictingAssignment {
                    first_row: previous.row,
                    first_action: previous.action,
                    row,
                    action,
                },
            ));
        }
        return Ok(());
    }
    let observed = proposals.len() as u128 + 1;
    if observed > u128::from(meter.policy.max_effects) {
        return Err(GqlQueryError::Source(GraphMutationError::EffectLimit {
            limit: meter.policy.max_effects,
            observed,
        }));
    }
    meter.event(GlaExecutionEvent::ScratchEntry)?;
    proposals.insert((target, field), Proposal { value, row, action });
    Ok(())
}

/// A composite read value over one input row: a SET right-hand side or a
/// computed DELETE target (TCK Delete5).
/// The host's element property reader for a mutation (fgdb-694ga): one
/// stored property of a vertex or edge, read at the statement's frozen
/// selection cut, before any of its own effects. `Ok(None)` is an absent
/// property.
pub(crate) type MutationElementReader<'r, E> =
    dyn FnMut(ElementId, PropertyKeyId) -> Result<Option<CanonicalPropertyValue>, E> + 'r;

fn evaluate_composite<E, C, F: FnMut() -> Result<(), C>>(
    value: &crate::GraphSetValue,
    row: &GraphValueRow,
    row_at: usize,
    action_at: usize,
    meter: &mut Meter<F>,
    elements: &mut Option<&mut MutationElementReader<'_, E>>,
) -> ResultOf<crate::algebra::GraphValue, E, C> {
    // Without a host reader, `x.key` of a computed element stays a typed
    // error, never a guessed NULL.
    let mut read = |element: ElementId, key: PropertyKeyId| {
        elements.as_mut().map(|read| {
            (*read)(element, key)
                .map_err(|error| GqlQueryError::Source(GraphMutationError::Source(error)))
        })
    };
    crate::set_ops::evaluate_value_reading(
        value,
        row,
        action_at,
        &mut |event| meter.event(event),
        &mut read,
    )
    .map_err(|failure| match failure {
        crate::set_ops::ProjectionFailure::Control(error) => error,
        crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
            GqlQueryError::Source(GraphMutationError::Arithmetic {
                row: row_at,
                action: action_at,
                error,
            })
        }
    })
}
impl<F> Meter<F> {
    fn event<E, C>(&mut self, event: GlaExecutionEvent) -> ResultOf<(), E, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let work = u128::from(self.evaluator.work_units) + 1;
        let scratch = u128::from(self.evaluator.scratch_entries)
            + u128::from(event == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                self.policy.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                self.policy.query.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        self.evaluator = GlaExecutionStats {
            work_units: work as u64,
            scratch_entries: scratch as u64,
        };
        Ok(())
    }
}

fn equal<E>(
    left: &Value<'_>,
    right: &Value<'_>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    control(GlaExecutionEvent::Work)?;
    match (left, right) {
        (Value::Property(left), Value::Property(right)) => {
            for scalar in [*left, *right].into_iter().flatten() {
                charge_payload(scalar, control)?;
            }
            Ok(left == right)
        }
        (Value::Computed(left), Value::Computed(right)) => {
            charge_payload(left, control)?;
            charge_payload(right, control)?;
            Ok(left == right)
        }
        (Value::Stored(left), Value::Stored(right)) => {
            stored::compare_payload(left, control)?;
            stored::compare_payload(right, control)?;
            Ok(left == right)
        }
        (Value::Computed(computed), Value::Property(scalar))
        | (Value::Property(scalar), Value::Computed(computed)) => {
            charge_payload(computed, control)?;
            if let Some(scalar) = scalar {
                charge_payload(scalar, control)?;
            }
            Ok(Some(computed) == *scalar)
        }
        (Value::Label(left), Value::Label(right)) => Ok(left == right),
        (Value::Delete(left), Value::Delete(right)) => Ok(left == right),
        // A List never equals a scalar, and a field holds one kind of value.
        _ => Ok(false),
    }
}

/// Same logical payload units used by value-row projection. Reserve before
/// the only scalar clone in proposal production, including a text sort key.
fn reserve_copy<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let sizes = match value {
        CanonicalScalar::Text(value) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
        _ => [0, 0],
    };
    control(GlaExecutionEvent::ScratchEntry)?;
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    Ok(())
}

/// The element a target cell names, or `None` for an absent OPTIONAL target.
fn element(value: &crate::algebra::GraphValue) -> Option<ElementId> {
    if let Some(vertex) = value.as_vertex() {
        Some(ElementId::Vertex(vertex))
    } else {
        value.as_edge().map(ElementId::Edge)
    }
}

/// The properties `SET target = {...}` unsets: every name in the target's
/// frozen property map that `keep` does not name, resolved through the
/// selection's reverse catalog (injective, checked at preparation).
fn cleared_fields<E, C, F: FnMut() -> Result<(), C>>(
    current: &crate::algebra::GraphValue,
    keep: &[fgdb_delta_types::PropertyKeyId],
    catalog: Option<&crate::ReverseSymbolCatalog>,
    row: usize,
    action: usize,
    meter: &mut Meter<F>,
) -> ResultOf<Vec<Field>, E, C> {
    let invalid = || {
        GqlQueryError::Source(GraphMutationError::InvalidPropertyMap {
            row,
            binding: action,
        })
    };
    let catalog = catalog.ok_or_else(invalid)?;
    let (names, _) = current.as_map().ok_or_else(invalid)?;
    let mut fields = Vec::new();
    for name in names {
        meter.event(GlaExecutionEvent::Work)?;
        let key = catalog
            .properties
            .iter()
            .find(|(_, known)| known.as_bytes() == name.as_bytes())
            .map(|(key, _)| *key)
            .ok_or_else(invalid)?;
        if !keep.contains(&key) {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            fields.push(Field::Property(key));
        }
    }
    Ok(fields)
}

enum MapField<'a> {
    Frozen(&'a crate::algebra::GraphValue),
    Assigned(&'a CanonicalScalar),
    Stored(&'a CanonicalPropertyValue),
}

/// Merge the final proposal range for this actual element into its frozen
/// admitted map. No expression runs here and no graph source is consulted.
/// Catalog names were checked for injectivity when the query was prepared,
/// including IDs absent from the original map but introduced by this SET.
#[allow(clippy::too_many_arguments)]
fn returned_properties<E, C, F: FnMut() -> Result<(), C>>(
    target: ElementId,
    current: &crate::algebra::GraphValue,
    row_at: usize,
    binding_at: usize,
    proposals: &BTreeMap<(ElementId, Field), Proposal<'_>>,
    catalog: Option<&crate::ReverseSymbolCatalog>,
    meter: &mut Meter<F>,
) -> ResultOf<crate::algebra::GraphValue, E, C> {
    use crate::algebra::GraphValue;
    let invalid = || {
        GqlQueryError::Source(GraphMutationError::InvalidPropertyMap {
            row: row_at,
            binding: binding_at,
        })
    };
    meter.event(GlaExecutionEvent::Work)?;
    if proposals.contains_key(&(target, Field::Delete)) {
        return Err(GqlQueryError::Source(
            GraphMutationError::DeletedElementRead {
                row: row_at,
                binding: binding_at,
            },
        ));
    }
    let (old_keys, old_values) = current.as_map().ok_or_else(invalid)?;
    if old_keys.len() != old_values.len() || old_keys.len() >= GraphValue::MAX_LIST_NODES {
        return Err(invalid());
    }
    for (at, (key, value)) in old_keys.iter().zip(old_values).enumerate() {
        meter.event(GlaExecutionEvent::Work)?;
        // A frozen field is any stored value, a List or Map included
        // (fgdb-k3nub); a graph element never is.
        if !value.is_stored_value() || at != 0 && old_keys[at - 1].as_bytes() >= key.as_bytes() {
            return Err(invalid());
        }
    }
    let mut updates = BTreeMap::new();
    meter.event(GlaExecutionEvent::Work)?;
    for ((_, field), proposal) in proposals
        .range((target, Field::Property(PropertyKeyId(0)))..(target, Field::Label(LabelId(0))))
    {
        meter.event(GlaExecutionEvent::Work)?;
        let Field::Property(key) = field else {
            unreachable!("the property range ends before labels and deletion")
        };
        let name = catalog
            .and_then(|catalog| catalog.properties.get(key))
            .ok_or(GqlQueryError::Source(
                GraphMutationError::UnmappedProperty {
                    row: row_at,
                    binding: binding_at,
                },
            ))?;
        let value = match &proposal.value {
            Value::Property(value) => value.map(MapField::Assigned),
            Value::Computed(value) => Some(MapField::Assigned(value)),
            Value::Stored(value) => Some(MapField::Stored(value)),
            _ => unreachable!("property fields hold property proposals"),
        };
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        updates.insert(name.as_str(), value);
    }
    meter.event(GlaExecutionEvent::ScratchEntry)?;
    let mut keys = Vec::<Box<str>>::new();
    let mut values = Vec::new();
    let mut append = |key: &str, value: MapField<'_>| -> ResultOf<(), E, C> {
        meter.event(GlaExecutionEvent::Work)?;
        if keys.len() >= GraphValue::MAX_LIST_NODES - 1 {
            return Err(invalid());
        }
        // Reserve the key cell and its payload before either output grows.
        for _ in 0..=key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
        }
        let value = match value {
            MapField::Frozen(value) => value.copy_with_control(&mut |event| meter.event(event))?,
            MapField::Assigned(value) => {
                reserve_copy(value, &mut |event| meter.event(event))?;
                GraphValue::Scalar(value.clone())
            }
            MapField::Stored(value) => {
                stored::returned(value, row_at, binding_at, &mut |event| meter.event(event))?
            }
        };
        keys.push(key.into());
        values.push(value);
        Ok(())
    };
    let mut old = 0;
    for (key, value) in updates {
        while old < old_keys.len() && old_keys[old].as_bytes() < key.as_bytes() {
            append(&old_keys[old], MapField::Frozen(&old_values[old]))?;
            old += 1;
        }
        if old < old_keys.len() && old_keys[old].as_ref() == key {
            old += 1;
        }
        if let Some(value) = value {
            append(key, value)?;
        }
    }
    for at in old..old_keys.len() {
        append(&old_keys[at], MapField::Frozen(&old_values[at]))?;
    }
    Ok(GraphValue::Map {
        keys: keys.into_boxed_slice(),
        values: values.into_boxed_slice(),
    })
}

/// `labels(target)` after the statement: resolve each frozen name to its
/// label identity, apply this element's label proposals, and name the result
/// in identity order, exactly as a `labels()` read orders it. The query
/// admitted a catalog with one name per identity.
#[allow(clippy::too_many_arguments)]
fn returned_labels<E, C, F: FnMut() -> Result<(), C>>(
    target: ElementId,
    current: &crate::algebra::GraphValue,
    row_at: usize,
    binding_at: usize,
    proposals: &BTreeMap<(ElementId, Field), Proposal<'_>>,
    catalog: Option<&crate::ReverseSymbolCatalog>,
    meter: &mut Meter<F>,
) -> ResultOf<crate::algebra::GraphValue, E, C> {
    use crate::algebra::GraphValue;
    let unmapped = || {
        GqlQueryError::Source(GraphMutationError::UnmappedLabel {
            row: row_at,
            binding: binding_at,
        })
    };
    meter.event(GlaExecutionEvent::Work)?;
    if proposals.contains_key(&(target, Field::Delete)) {
        return Err(GqlQueryError::Source(
            GraphMutationError::DeletedElementRead {
                row: row_at,
                binding: binding_at,
            },
        ));
    }
    let catalog = catalog.ok_or_else(unmapped)?;
    let frozen = current.as_list().ok_or_else(unmapped)?;
    let mut labels = std::collections::BTreeSet::new();
    for name in frozen {
        let id = catalog
            .labels
            .iter()
            .find(|(_, candidate)| {
                CanonicalScalar::ucs_basic_text(candidate.as_str())
                    .is_ok_and(|text| name.as_scalar() == Some(&text))
            })
            .map(|(id, _)| *id)
            .ok_or_else(unmapped)?;
        for _ in 0..=catalog.labels.len() {
            meter.event(GlaExecutionEvent::Work)?;
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        labels.insert(id);
    }
    meter.event(GlaExecutionEvent::Work)?;
    for ((_, field), proposal) in
        proposals.range((target, Field::Label(LabelId(0)))..(target, Field::Delete))
    {
        meter.event(GlaExecutionEvent::Work)?;
        let (Field::Label(label), Value::Label(present)) = (field, &proposal.value) else {
            unreachable!("the label range holds label proposals")
        };
        if *present {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            labels.insert(*label);
        } else {
            labels.remove(label);
        }
    }
    let mut names = Vec::with_capacity(labels.len());
    for label in labels {
        let name = catalog.labels.get(&label).ok_or_else(unmapped)?;
        let text = CanonicalScalar::ucs_basic_text(name.as_str()).map_err(|_| unmapped())?;
        reserve_copy(&text, &mut |event| meter.event(event))?;
        names.push(GraphValue::Scalar(text));
    }
    Ok(GraphValue::List(names.into_boxed_slice()))
}

/// One RETURN input row per selection row, read after every proposal is
/// known: a property the statement assigns reads its proposal, any other
/// reads the frozen selection value. Values are copied, never moved, since
/// the intents still own the proposals.
fn returned_row<E, C, F: FnMut() -> Result<(), C>>(
    bindings: &[GraphMutationBinding],
    row: &GraphValueRow,
    row_at: usize,
    proposals: &BTreeMap<(ElementId, Field), Proposal<'_>>,
    catalog: Option<&crate::ReverseSymbolCatalog>,
    meter: &mut Meter<F>,
) -> ResultOf<GraphValueRow, E, C> {
    use crate::algebra::GraphValue;
    meter.event(GlaExecutionEvent::ScratchEntry)?;
    let mut values = Vec::with_capacity(bindings.len());
    for (binding_at, binding) in bindings.iter().enumerate() {
        meter.event(GlaExecutionEvent::Work)?;
        let value = match *binding {
            GraphMutationBinding::Input(column) => {
                row.values()[column].copy_with_control(&mut |event| meter.event(event))?
            }
            GraphMutationBinding::Property {
                target,
                key,
                current,
            } => match element(&row.values()[target]) {
                None => {
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                    GraphValue::Scalar(CanonicalScalar::Null)
                }
                Some(target) => {
                    // Two ordered-map searches, charged as work like any
                    // other proposal lookup.
                    meter.event(GlaExecutionEvent::Work)?;
                    if proposals.contains_key(&(target, Field::Delete)) {
                        return Err(GqlQueryError::Source(
                            GraphMutationError::DeletedElementRead {
                                row: row_at,
                                binding: binding_at,
                            },
                        ));
                    }
                    meter.event(GlaExecutionEvent::Work)?;
                    match proposals.get(&(target, Field::Property(key))) {
                        Some(Proposal {
                            value: Value::Property(None),
                            ..
                        }) => {
                            meter.event(GlaExecutionEvent::ScratchEntry)?;
                            GraphValue::Scalar(CanonicalScalar::Null)
                        }
                        Some(Proposal {
                            value: Value::Property(Some(scalar)),
                            ..
                        }) => {
                            reserve_copy(scalar, &mut |event| meter.event(event))?;
                            GraphValue::Scalar((*scalar).clone())
                        }
                        Some(Proposal {
                            value: Value::Computed(scalar),
                            ..
                        }) => {
                            reserve_copy(scalar, &mut |event| meter.event(event))?;
                            GraphValue::Scalar(scalar.clone())
                        }
                        Some(Proposal {
                            value: Value::Stored(value),
                            ..
                        }) => stored::returned(value, row_at, binding_at, &mut |event| {
                            meter.event(event)
                        })?,
                        Some(_) => unreachable!("property fields hold property proposals"),
                        None => row.values()[current]
                            .copy_with_control(&mut |event| meter.event(event))?,
                    }
                }
            },
            GraphMutationBinding::Properties { target, current } => {
                match element(&row.values()[target]) {
                    None => {
                        meter.event(GlaExecutionEvent::ScratchEntry)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                    Some(target) => returned_properties(
                        target,
                        &row.values()[current],
                        row_at,
                        binding_at,
                        proposals,
                        catalog,
                        meter,
                    )?,
                }
            }
            GraphMutationBinding::Labels { target, current } => {
                match element(&row.values()[target]) {
                    None => {
                        meter.event(GlaExecutionEvent::ScratchEntry)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                    Some(target) => returned_labels(
                        target,
                        &row.values()[current],
                        row_at,
                        binding_at,
                        proposals,
                        catalog,
                        meter,
                    )?,
                }
            }
        };
        values.push(value);
    }
    Ok(GraphValueRow::from_owned_values(values))
}

/// A selection source behind a trait object: called at most once.
type ErasedSource<'a, E, C> = Box<
    dyn FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>
        + 'a,
>;

/// `owned` are a chain step's input rows (fgdb-cpny9); `None` for every
/// other statement. `elements` is the host's element property reader
/// (fgdb-694ga); `None` keeps `x.key` of a computed element a typed error.
pub(super) fn execute<'a, E, C>(
    mutation: &PreparedGraphMutation,
    policy: GraphMutationPolicy,
    bindings: Option<&[GraphMutationBinding]>,
    owned: Option<Vec<GraphValueRow>>,
    source: impl FnOnce(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>
    + 'a,
    elements: Option<&mut MutationElementReader<'_, E>>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> ResultOf<(GraphMutationBatch, Vec<GraphValueRow>), E, C> {
    // The mutation engine is compiled once per error-type pair, not once per
    // caller closure type (fgdb-2goz): its source is boxed (one allocation
    // per statement) and its checkpoint is a trait object.
    execute_erased(
        mutation,
        policy,
        bindings,
        owned,
        Box::new(source),
        elements,
        &mut checkpoint,
    )
}

fn execute_erased<E, C>(
    mutation: &PreparedGraphMutation,
    policy: GraphMutationPolicy,
    bindings: Option<&[GraphMutationBinding]>,
    owned: Option<Vec<GraphValueRow>>,
    source: ErasedSource<'_, E, C>,
    mut elements: Option<&mut MutationElementReader<'_, E>>,
    mut checkpoint: &mut dyn FnMut() -> Result<(), C>,
) -> ResultOf<(GraphMutationBatch, Vec<GraphValueRow>), E, C> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let selected = mutation.select_governed(
        policy.query,
        owned,
        source,
        elements.as_deref_mut(),
        &mut checkpoint,
    )?;
    if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
        return Err(GqlQueryError::Source(
            GraphMutationError::InvalidSourceStatistics,
        ));
    }
    for (dimension, observed) in [
        (
            GqlBudgetDimension::SnapshotRecords,
            selected.rows.snapshot_records,
        ),
        (GqlBudgetDimension::ResultRows, selected.rows.result_rows),
    ] {
        policy
            .query
            .rows
            .check(dimension, observed)
            .map_err(GqlQueryError::Rows)?;
    }
    for (observed, limit, dimension) in [
        (
            selected.evaluator.work_units,
            policy.query.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        ),
        (
            selected.evaluator.scratch_entries,
            policy.query.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        ),
    ] {
        if observed > limit {
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed: u128::from(observed),
            }));
        }
    }
    let mut meter = Meter {
        policy,
        evaluator: selected.evaluator,
        checkpoint,
    };
    meter.event(GlaExecutionEvent::Work)?;
    let columns = &mutation.columns;
    let mut proposals = BTreeMap::<(ElementId, Field), Proposal<'_>>::new();
    for (row_at, row) in selected.value.iter().enumerate() {
        meter.event(GlaExecutionEvent::Work)?;
        if row.len() != columns.len() {
            return Err(GqlQueryError::Source(GraphMutationError::InputSchema {
                row: row_at,
                column: row.len().min(columns.len()),
            }));
        }
        for (column, (value, expression)) in row.values().iter().zip(columns).enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            let valid = expression.accepts(value);
            if !valid {
                return Err(GqlQueryError::Source(GraphMutationError::InputSchema {
                    row: row_at,
                    column,
                }));
            }
        }
        for (action_at, action) in mutation.actions.iter().enumerate() {
            meter.event(GlaExecutionEvent::Work)?;
            // A computed delete (TCK Delete5) deletes the element, or each
            // element of the path, its value is; NULL deletes nothing.
            let computed;
            let target_value = match (action, action.target()) {
                (GraphMutationAction::DeleteValue { value, .. }, _) => {
                    computed = evaluate_composite(
                        value,
                        row,
                        row_at,
                        action_at,
                        &mut meter,
                        &mut elements,
                    )?;
                    if !(computed.is_null()
                        || element(&computed).is_some()
                        || matches!(computed, crate::algebra::GraphValue::Path(_)))
                    {
                        return Err(GqlQueryError::Source(
                            GraphMutationError::NonElementDelete {
                                row: row_at,
                                action: action_at,
                            },
                        ));
                    }
                    &computed
                }
                (_, Some(column)) => &row.values()[column],
                (_, None) => continue,
            };
            // DELETE p / DETACH DELETE p (TCK Delete3): every node of the
            // path, and for a plain DELETE every relationship too, each its
            // own delete proposal. DETACH removes the rest of their edges.
            if let crate::algebra::GraphValue::Path(path) = target_value {
                let detach = action.detaches();
                let edges = path
                    .steps()
                    .iter()
                    .filter(|_| !detach)
                    .map(|(edge, _)| ElementId::Edge(*edge));
                for target in edges.chain(path.nodes().map(ElementId::Vertex)) {
                    meter.event(GlaExecutionEvent::Work)?;
                    propose(
                        &mut proposals,
                        &mut meter,
                        target,
                        Field::Delete,
                        Value::Delete(detach),
                        row_at,
                        action_at,
                    )?;
                }
                continue;
            }
            // A typed target column holds its element or canonical null (the
            // schema check above); a dynamically typed one, any value. NULL
            // changes nothing: an absent OPTIONAL target does not execute an
            // assignment RHS. Anything else that is not an element, or a
            // relationship as a label target, is a typed error.
            let target = match element(target_value) {
                Some(ElementId::Edge(_))
                    if matches!(action, GraphMutationAction::SetLabel { .. }) =>
                {
                    return Err(GqlQueryError::Source(
                        GraphMutationError::NonElementTarget {
                            row: row_at,
                            action: action_at,
                        },
                    ));
                }
                Some(target) => target,
                None if target_value.is_null() => continue,
                None => {
                    return Err(GqlQueryError::Source(
                        GraphMutationError::NonElementTarget {
                            row: row_at,
                            action: action_at,
                        },
                    ));
                }
            };
            let proposed = match action {
                GraphMutationAction::ClearProperties { current, keep, .. } => cleared_fields(
                    &row.values()[*current],
                    keep,
                    mutation.reverse_catalog(),
                    row_at,
                    action_at,
                    &mut meter,
                )?
                .into_iter()
                .map(|field| (field, Value::Property(None)))
                .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            let single = match action {
                GraphMutationAction::ClearProperties { .. } => None,
                GraphMutationAction::SetProperty { key, value, .. } => {
                    let value = match value {
                        GraphMutationValue::Column(column) => {
                            match row.values()[*column].as_scalar() {
                                Some(value) => Value::Property(Some(value)),
                                None => Value::Stored(stored::written(
                                    &row.values()[*column],
                                    row_at,
                                    action_at,
                                    &mut |event| meter.event(event),
                                )?),
                            }
                        }
                        GraphMutationValue::Literal(value) => Value::Property(Some(value.value())),
                        GraphMutationValue::Expression(expression) => {
                            let value = expression
                                .evaluate_scalar_with_control(row.values(), &mut |event| {
                                    meter.event(event)
                                })
                                .map_err(|failure| match failure {
                                    GraphIntegerEvaluationError::Control(error) => error,
                                    GraphIntegerEvaluationError::Value(error) => {
                                        GqlQueryError::Source(GraphMutationError::Arithmetic {
                                            row: row_at,
                                            action: action_at,
                                            error,
                                        })
                                    }
                                })?;
                            Value::Computed(value)
                        }
                        GraphMutationValue::Composite(value) => {
                            let value = evaluate_composite(
                                value,
                                row,
                                row_at,
                                action_at,
                                &mut meter,
                                &mut elements,
                            )?;
                            // Scalars move. Lists reserve the distinct storage
                            // representation before the canonical conversion.
                            match value {
                                crate::algebra::GraphValue::Scalar(value) => Value::Computed(value),
                                value => Value::Stored(stored::written(
                                    &value,
                                    row_at,
                                    action_at,
                                    &mut |event| meter.event(event),
                                )?),
                            }
                        }
                    };
                    Some((Field::Property(*key), value))
                }
                GraphMutationAction::RemoveProperty { key, .. } => {
                    Some((Field::Property(*key), Value::Property(None)))
                }
                GraphMutationAction::SetLabel { label, present, .. } => {
                    Some((Field::Label(*label), Value::Label(*present)))
                }
                GraphMutationAction::DetachDelete { .. } => {
                    Some((Field::Delete, Value::Delete(true)))
                }
                GraphMutationAction::Delete { .. } => Some((Field::Delete, Value::Delete(false))),
                // DETACH deletes a relationship as a plain DELETE does.
                GraphMutationAction::DeleteValue { detach, .. } => Some((
                    Field::Delete,
                    Value::Delete(*detach && matches!(target, ElementId::Vertex(_))),
                )),
            };
            for (field, value) in proposed.into_iter().chain(single) {
                propose(
                    &mut proposals,
                    &mut meter,
                    target,
                    field,
                    value,
                    row_at,
                    action_at,
                )?;
            }
        }
    }
    let mut returned = Vec::new();
    if let Some(bindings) = bindings {
        for (row_at, row) in selected.value.iter().enumerate() {
            returned.push(returned_row(
                bindings,
                row,
                row_at,
                &proposals,
                mutation.reverse_catalog(),
                &mut meter,
            )?);
        }
    }
    let mut intents = Vec::new();
    let mut previous_target = None;
    let mut target_vertices = 0_u64;
    let mut target_edges = 0_u64;
    for ((target, field), proposal) in proposals {
        meter.event(GlaExecutionEvent::Work)?;
        if previous_target != Some(target) {
            match target {
                ElementId::Vertex(_) => target_vertices += 1,
                ElementId::Edge(_) => target_edges += 1,
            }
            previous_target = Some(target);
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        let intent = match (target, field, proposal.value) {
            (target, Field::Property(key), Value::Property(value)) => {
                if let Some(scalar) = value {
                    reserve_copy(scalar, &mut |event| meter.event(event))?;
                }
                property_intent(
                    target,
                    key,
                    value.cloned().map(CanonicalPropertyValue::Scalar),
                )
            }
            (target, Field::Property(key), Value::Computed(value)) => {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                property_intent(target, key, Some(CanonicalPropertyValue::Scalar(value)))
            }
            (target, Field::Property(key), Value::Stored(value)) => {
                GraphMutationIntent::StoredProperty { target, key, value }
            }
            (ElementId::Vertex(vertex), Field::Label(label), Value::Label(present)) => {
                GraphMutationIntent::Label {
                    vertex,
                    label,
                    present,
                }
            }
            (ElementId::Vertex(vertex), Field::Delete, Value::Delete(true)) => {
                GraphMutationIntent::DetachDelete { vertex }
            }
            (ElementId::Vertex(vertex), Field::Delete, Value::Delete(false)) => {
                GraphMutationIntent::Delete { vertex }
            }
            (ElementId::Edge(edge), Field::Delete, Value::Delete(false)) => {
                GraphMutationIntent::DeleteEdge { edge }
            }
            _ => unreachable!("field and proposal are constructed together"),
        };
        intents.push(intent);
    }
    (meter.checkpoint)().map_err(GqlQueryError::Interrupted)?;
    let stats = GraphMutationStats {
        selection: selected.rows,
        evaluator: meter.evaluator,
        target_vertices,
        target_edges,
        effects: intents.len() as u64,
    };
    Ok((GraphMutationBatch { intents, stats }, returned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
    use fgdb_types::CanonicalPropertyValue;
    fn query() -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare("MATCH (a)-[:R]->(b) RETURN a,b.p AS p", |kind, _| {
            Some(match kind {
                GraphSymbolKind::Relation => GraphSymbol::Relation(RelationId(1)),
                GraphSymbolKind::Property => GraphSymbol::Property(PropertyKeyId(1)),
                GraphSymbolKind::Label => GraphSymbol::Label(LabelId(1)),
            })
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }
    fn mutation() -> PreparedGraphMutation {
        PreparedGraphMutation::prepare(
            query(),
            RelationId(1),
            vec![GraphMutationAction::SetProperty {
                target: 0,
                key: PropertyKeyId(2),
                value: GraphMutationValue::Column(1),
            }],
        )
        .unwrap()
    }
    fn run(
        values: &[CanonicalScalar],
        policy: GraphMutationPolicy,
    ) -> ResultOf<GraphMutationBatch, (), ()> {
        run_with(&mutation(), values, policy)
    }
    fn run_with(
        mutation: &PreparedGraphMutation,
        values: &[CanonicalScalar],
        policy: GraphMutationPolicy,
    ) -> ResultOf<GraphMutationBatch, (), ()> {
        let values: Vec<CanonicalPropertyValue> = values
            .iter()
            .cloned()
            .map(CanonicalPropertyValue::from)
            .collect();
        mutation.execute_governed(
            policy,
            |plan, policy| {
                plan.plan().execute_governed_with_properties(
                    values.len() as u64,
                    [],
                    (0..values.len()).map(|i| (VId(1), RelationId(1), VId(i as u128 + 10))),
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| Ok(Some(&values[vid.0 as usize - 10])),
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
    }
    fn policy() -> GraphMutationPolicy {
        GraphMutationPolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 100_000), 100)
    }
    /// Composite results use the shared evaluator: scalars and bounded lists
    /// are written, while bad references and graph-valued arguments refuse.
    #[test]
    fn composite_values_assign_scalars_and_lists_and_check_references() {
        use crate::GraphSetValue;
        use crate::algebra::GraphValue;
        let composite = |value| {
            PreparedGraphMutation::prepare(
                query(),
                RelationId(1),
                vec![GraphMutationAction::SetProperty {
                    target: 0,
                    key: PropertyKeyId(2),
                    value: GraphMutationValue::Composite(value),
                }],
            )
        };
        let int = |value| GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        // SET a.q = b.p IN [7]: one row per b, and every b of a agrees.
        let member = composite(GraphSetValue::In {
            value: Box::new(GraphSetValue::Column(1)),
            list: Box::new(GraphSetValue::List(vec![int(7)])),
        })
        .unwrap();
        for (p, expected) in [(7, true), (8, false)] {
            let values = [CanonicalScalar::Int(p)];
            assert_eq!(
                run_with(&member, &values, policy()).unwrap().intents(),
                &[GraphMutationIntent::Property {
                    vertex: VId(1),
                    key: PropertyKeyId(2),
                    value: Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Bool(
                        expected
                    ))),
                }]
            );
        }
        let expected =
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(1))].into_boxed_slice())
                .to_written_property()
                .unwrap();
        for expression in [
            GraphSetValue::Index {
                list: Box::new(GraphSetValue::List(vec![GraphSetValue::List(vec![int(1)])])),
                index: Box::new(int(0)),
            },
            GraphSetValue::List(vec![int(1)]),
        ] {
            let query = composite(expression).unwrap();
            assert_eq!(
                run_with(&query, &[CanonicalScalar::Int(7)], policy())
                    .unwrap()
                    .intents(),
                &[GraphMutationIntent::StoredProperty {
                    target: ElementId::Vertex(VId(1)),
                    key: PropertyKeyId(2),
                    value: expected.clone(),
                }]
            );
        }
        assert!(matches!(
            composite(GraphSetValue::Column(0)),
            Err(GraphMutationBuildError::ValueExpression {
                action: 0,
                kind: None
            })
        ));
        assert!(matches!(
            composite(GraphSetValue::Size(Box::new(GraphSetValue::Column(9)))),
            Err(GraphMutationBuildError::ValueExpression {
                action: 0,
                kind: Some(_)
            })
        ));
    }
    #[test]
    fn repeated_assignments_collapse_but_disagreement_never_selects_a_last_writer() {
        let values = [CanonicalScalar::Int(7), CanonicalScalar::Int(7)];
        let result = run(&values, policy()).unwrap();
        assert_eq!(
            result.intents(),
            &[GraphMutationIntent::Property {
                vertex: VId(1),
                key: PropertyKeyId(2),
                value: Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(7))),
            }]
        );
        assert_eq!(result.stats().selection.result_rows, 2);
        assert_eq!(
            (result.stats().target_vertices, result.stats().effects),
            (1, 1)
        );
        for values in [
            [CanonicalScalar::Int(7), CanonicalScalar::Int(9)],
            [CanonicalScalar::Int(9), CanonicalScalar::Int(7)],
            [CanonicalScalar::Int(7), CanonicalScalar::Null],
        ] {
            assert!(matches!(
                run(&values, policy()),
                Err(GqlQueryError::Source(
                    GraphMutationError::ConflictingAssignment { .. }
                ))
            ));
        }
    }
    #[test]
    fn selection_and_proposals_share_work_scratch_and_exact_effect_limits() {
        let values = [CanonicalScalar::Int(7), CanonicalScalar::Int(7)];
        let measured = run(&values, policy()).unwrap();
        let stats = measured.stats();
        let exact = GraphMutationPolicy::new(
            GqlQueryPolicy::new(
                2,
                2,
                stats.evaluator.work_units,
                stats.evaluator.scratch_entries,
            ),
            1,
        );
        assert_eq!(run(&values, exact).unwrap().stats(), stats);
        for cap in [
            GraphMutationPolicy::new(GqlQueryPolicy::new(1, 2, u64::MAX, u64::MAX), 1),
            GraphMutationPolicy::new(GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX), 1),
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(2, 2, stats.evaluator.work_units - 1, u64::MAX),
                1,
            ),
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(2, 2, u64::MAX, stats.evaluator.scratch_entries - 1),
                1,
            ),
            GraphMutationPolicy::new(GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX), 0),
        ] {
            assert!(run(&values, cap).is_err());
        }
    }
    #[test]
    fn schema_is_checked_before_a_source_can_run() {
        for action in [
            GraphMutationAction::DetachDelete { target: 1 },
            GraphMutationAction::RemoveProperty {
                target: usize::MAX,
                key: PropertyKeyId(1),
            },
            GraphMutationAction::SetProperty {
                target: 0,
                key: PropertyKeyId(2),
                value: GraphMutationValue::Column(0),
            },
        ] {
            assert!(PreparedGraphMutation::prepare(query(), RelationId(1), vec![action]).is_err());
        }
        assert!(matches!(
            PreparedGraphMutation::prepare(
                query(),
                RelationId(1),
                vec![
                    GraphMutationAction::DetachDelete { target: 0 },
                    GraphMutationAction::RemoveProperty {
                        target: 0,
                        key: PropertyKeyId(1)
                    },
                ]
            ),
            Err(GraphMutationBuildError::MixedDeletionAndUpdates)
        ));
        for column in [0, 2, usize::MAX] {
            let expression =
                GraphIntegerExpression::prepare(&[crate::GraphIntegerOp::Column(column)]).unwrap();
            assert!(
                matches!(PreparedGraphMutation::prepare(query(), RelationId(1), vec![
                GraphMutationAction::SetProperty { target: 0, key: PropertyKeyId(2), value: GraphMutationValue::Expression(expression) },
            ]), Err(GraphMutationBuildError::ValueColumn { column: found, .. }) if found == column)
            );
        }
    }

    #[test]
    fn late_rhs_source_failure_never_returns_an_earlier_partial_assignment() {
        let value = CanonicalPropertyValue::from(CanonicalScalar::Int(7));
        let reads = std::cell::Cell::new(0);
        let result: ResultOf<GraphMutationBatch, &str, ()> = mutation().execute_governed(
            policy(),
            |plan, policy| {
                plan.plan().execute_governed_with_properties(
                    2,
                    [],
                    [
                        (VId(1), RelationId(1), VId(10)),
                        (VId(2), RelationId(1), VId(11)),
                    ],
                    |_, _| Ok::<_, &str>(true),
                    |vid, _| {
                        reads.set(reads.get() + 1);
                        if vid == VId(11) {
                            Err("RHS property source failed")
                        } else {
                            Ok(Some(&value))
                        }
                    },
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphMutationError::Source(
                "RHS property source failed"
            )))
        ));
        assert_eq!(
            reads.get(),
            2,
            "a valid earlier match preceded the failing RHS read"
        );
    }

    #[test]
    fn overflow_refusal_leaves_both_mutation_counters_unchanged() {
        let policy = GraphMutationPolicy::new(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            u64::MAX,
        );
        let mut meter = Meter {
            policy,
            checkpoint: || Ok::<_, ()>(()),
            evaluator: GlaExecutionStats {
                work_units: u64::MAX,
                scratch_entries: 0,
            },
        };
        let before = meter.evaluator;
        assert!(matches!(meter.event::<(), ()>(GlaExecutionEvent::Work),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension: GlaLimitDimension::WorkUnits, observed, .. }))
                if observed == u128::from(u64::MAX) + 1));
        assert_eq!(meter.evaluator, before);
        meter.evaluator = GlaExecutionStats {
            work_units: 0,
            scratch_entries: u64::MAX,
        };
        let before = meter.evaluator;
        assert!(
            matches!(meter.event::<(), ()>(GlaExecutionEvent::ScratchEntry),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension: GlaLimitDimension::ScratchEntries, observed, .. }))
                if observed == u128::from(u64::MAX) + 1)
        );
        assert_eq!(meter.evaluator, before);
    }

    #[test]
    fn computed_scalars_share_assignment_equality_but_never_equal_removal() {
        for value in [None, Some(i64::MIN), Some(0), Some(i64::MAX)] {
            let scalar = value.map_or(CanonicalScalar::Null, CanonicalScalar::Int);
            for (left, right) in [
                (
                    Value::Computed(scalar.clone()),
                    Value::Property(Some(&scalar)),
                ),
                (
                    Value::Property(Some(&scalar)),
                    Value::Computed(scalar.clone()),
                ),
            ] {
                assert!(equal(&left, &right, &mut |_| Ok::<_, ()>(())).unwrap());
            }
            assert!(
                !equal(
                    &Value::Computed(scalar),
                    &Value::Property(None),
                    &mut |_| Ok::<_, ()>(())
                )
                .unwrap()
            );
        }
        assert!(
            !equal(
                &Value::Computed(CanonicalScalar::Int(1)),
                &Value::Property(Some(&CanonicalScalar::Bool(true))),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap()
        );
    }
}
