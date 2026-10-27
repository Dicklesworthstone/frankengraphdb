//! Complete, bounded property maps for admitted snapshot and overlay rows.
//! Visibility is applied before catalog access, payload inspection or charging.

use super::source::SourceEvent;
use crate::{ReadError, VertexRow};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_gql::{GraphElementMetadata, ReverseSymbolCatalog};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct PropertyMaps {
    pub(crate) vertices: BTreeMap<VId, GraphValue>,
    pub(crate) edges: BTreeMap<EId, GraphValue>,
}

/// A borrowed, visible record at the executing statement's pinned cut. The
/// host admits the element and both edge endpoints before constructing this
/// view; field masking still precedes every catalog lookup and allocation.
pub(crate) enum MetadataRecord<'a> {
    Vertex(&'a VertexRow),
    Edge(RelationId, &'a [(PropertyKeyId, CanonicalPropertyValue)]),
}

/// Materialize metadata through the caller's cumulative control. Reusing the
/// ordinary property-map collector keeps nested values, missing catalog names,
/// cancellation and pre-copy reservations identical to bound element reads.
pub(crate) fn metadata<E>(
    record: MetadataRecord<'_>,
    kind: GraphElementMetadata,
    catalog: &ReverseSymbolCatalog,
    mut visible_label: impl FnMut(LabelId) -> bool,
    visible_property: impl FnMut(PropertyKeyId) -> bool,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    failure: &impl Fn(ReadError) -> E,
) -> Result<GraphValue, E> {
    match (kind, record) {
        (GraphElementMetadata::Properties, record) => {
            let properties = match record {
                MetadataRecord::Vertex(row) => row.props.as_slice(),
                MetadataRecord::Edge(_, properties) => properties,
            };
            collect(
                properties.iter().map(|(key, value)| (*key, value)),
                Some(catalog),
                visible_property,
                control,
                failure,
            )
        }
        (GraphElementMetadata::Keys, record) => {
            let properties = match record {
                MetadataRecord::Vertex(row) => row.props.as_slice(),
                MetadataRecord::Edge(_, properties) => properties,
            };
            control(SourceEvent::Work)?;
            control(SourceEvent::ScratchEntry)?;
            let mut visible_property = visible_property;
            let mut names = BTreeMap::new();
            for (key, _) in properties {
                if !visible_property(*key) {
                    continue;
                }
                control(SourceEvent::Work)?;
                if names.len() >= GraphValue::MAX_LIST_NODES - 1 {
                    return Err(failure(ReadError::InvalidPropertyMap));
                }
                let name = catalog
                    .properties
                    .get(key)
                    .ok_or_else(|| failure(ReadError::UnmappedProperty(*key)))?;
                // Retain a borrowed catalog name, not a stored property value.
                // The ordered map both detects aliases and preserves keys()'s
                // canonical name order without copying any nested payload.
                control(SourceEvent::ScratchEntry)?;
                if names.insert(name.as_str(), *key).is_some() {
                    return Err(failure(ReadError::InvalidPropertyMap));
                }
            }
            let mut values = Vec::new();
            for (name, key) in names {
                control(SourceEvent::Work)?;
                control(SourceEvent::ScratchEntry)?;
                payload(name.len(), control)?;
                values.push(GraphValue::Scalar(
                    CanonicalScalar::ucs_basic_text(name)
                        .map_err(|_| failure(ReadError::UnmappedProperty(key)))?,
                ));
            }
            Some(GraphValue::List(values.into_boxed_slice()))
                .filter(GraphValue::validate_bounds)
                .ok_or_else(|| failure(ReadError::InvalidPropertyMap))
        }
        (GraphElementMetadata::Labels, MetadataRecord::Vertex(row)) => {
            control(SourceEvent::Work)?;
            control(SourceEvent::ScratchEntry)?;
            let mut names = Vec::new();
            // Stored vertex rows have canonical sorted, unique label IDs, the
            // same order used by labels() over a bound pattern variable.
            for &label in &row.labels {
                if !visible_label(label) {
                    continue;
                }
                control(SourceEvent::Work)?;
                if names.len() >= GraphValue::MAX_LIST_NODES - 1 {
                    return Err(failure(ReadError::InvalidPropertyMap));
                }
                let name = catalog
                    .labels
                    .get(&label)
                    .ok_or_else(|| failure(ReadError::UnmappedLabel(label)))?;
                control(SourceEvent::ScratchEntry)?;
                payload(name.len(), control)?;
                let name = CanonicalScalar::ucs_basic_text(name)
                    .map_err(|_| failure(ReadError::UnmappedLabel(label)))?;
                names.push(GraphValue::Scalar(name));
            }
            Some(GraphValue::List(names.into_boxed_slice()))
                .filter(GraphValue::validate_bounds)
                .ok_or_else(|| failure(ReadError::InvalidPropertyMap))
        }
        (GraphElementMetadata::Type, MetadataRecord::Edge(relation, _)) => {
            control(SourceEvent::Work)?;
            let name = catalog
                .relations
                .get(&relation)
                .ok_or_else(|| failure(ReadError::UnmappedRelation(relation)))?;
            control(SourceEvent::ScratchEntry)?;
            payload(name.len(), control)?;
            CanonicalScalar::ucs_basic_text(name)
                .map(GraphValue::Scalar)
                .map_err(|_| failure(ReadError::UnmappedRelation(relation)))
        }
        // The value evaluator rejects these domains before calling its host.
        // Retain a typed failure if a host invokes this helper independently.
        _ => Err(failure(ReadError::InvalidPropertyMap)),
    }
}

/// Enumerate effective source properties, never the catalog's known-key set.
/// Each retained cell and variable payload is reserved before copying. The
/// map constructor owns canonical key ordering and refuses duplicate names.
pub(crate) fn collect<'a, E>(
    properties: impl IntoIterator<Item = (PropertyKeyId, &'a CanonicalPropertyValue)>,
    catalog: Option<&ReverseSymbolCatalog>,
    mut visible: impl FnMut(PropertyKeyId) -> bool,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    failure: &impl Fn(ReadError) -> E,
) -> Result<GraphValue, E> {
    control(SourceEvent::Work)?;
    control(SourceEvent::ScratchEntry)?;
    let mut entries = Vec::new();
    for (key, value) in properties {
        // Hidden keys may be unmapped and have arbitrarily large payloads.
        // Neither fact may affect this execution's errors or resource charges.
        if !visible(key) {
            continue;
        }
        control(SourceEvent::Work)?;
        // The map itself is one value node; every property contributes one
        // scalar child. Check before retaining or copying the excess field.
        if entries.len() >= GraphValue::MAX_LIST_NODES - 1 {
            return Err(failure(ReadError::InvalidPropertyMap));
        }
        let name = catalog
            .and_then(|catalog| catalog.properties.get(&key))
            .ok_or_else(|| failure(ReadError::UnmappedProperty(key)))?;
        // Temporary entry, final key slot and final scalar slot. These are
        // logical scratch units, not an allocator-byte memory guarantee.
        for _ in 0..3 {
            control(SourceEvent::ScratchEntry)?;
        }
        payload(name.len(), control)?;
        stored_payload(value, control)?;
        // A stored List or Map is the query value it holds (fgdb-k3nub).
        let value = GraphValue::from_property_value(value)
            .ok_or_else(|| failure(ReadError::InvalidPropertyMap))?;
        entries.push((name.as_str().into(), value));
    }
    // Lists and Maps add nested nodes: the whole map must fit the bounds.
    GraphValue::map(entries)
        .filter(GraphValue::validate_bounds)
        .ok_or_else(|| failure(ReadError::InvalidPropertyMap))
}

/// Reserve a stored value's payload before copying it: a scalar's variable
/// bytes, and for a List or Map one scratch entry per element, each key's
/// bytes, and every nested scalar (fgdb-k3nub).
fn stored_payload<E>(
    value: &CanonicalPropertyValue,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    match value {
        CanonicalPropertyValue::Scalar(scalar) => scalar_payload(scalar, control),
        CanonicalPropertyValue::List(list) => list.values().iter().try_for_each(|value| {
            control(SourceEvent::ScratchEntry)?;
            stored_payload(value, &mut *control)
        }),
        CanonicalPropertyValue::Map(map) => map.entries().iter().try_for_each(|entry| {
            control(SourceEvent::ScratchEntry)?;
            payload(entry.key().as_str().len(), control)?;
            stored_payload(entry.value(), &mut *control)
        }),
    }
}

fn scalar_payload<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
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
        CanonicalScalar::Null
        | CanonicalScalar::Bool(_)
        | CanonicalScalar::Int(_)
        | CanonicalScalar::Decimal(_)
        | CanonicalScalar::Float(_)
        | CanonicalScalar::Date(_)
        | CanonicalScalar::LocalTime(_)
        | CanonicalScalar::Time(_)
        | CanonicalScalar::LocalDateTime(_)
        | CanonicalScalar::Duration(_) => [0, 0],
    };
    for bytes in sizes {
        payload(bytes, control)?;
    }
    Ok(())
}

fn payload<E>(
    bytes: usize,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<(), E> {
    for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
        // Source scratch also consumes work through the shared AdmissionUsage.
        control(SourceEvent::ScratchEntry)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_masks_unknown_fields_before_errors_and_every_allocation() {
        let visible = VertexRow {
            vid: VId(u128::MAX),
            birth_ordinal: 0,
            created_at: fgdb_types::CommitSeq(1),
            retired_at: None,
            labels: vec![LabelId(1), LabelId(2)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(7).into())],
        };
        let mut hidden = visible.clone();
        hidden.labels.push(LabelId(99));
        hidden.props.push((
            PropertyKeyId(99),
            CanonicalScalar::ucs_basic_text(&"secret".repeat(2048))
                .unwrap()
                .into(),
        ));
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(LabelId(1), "zeta");
        catalog.insert_label(LabelId(2), "alpha");
        catalog.insert_property(PropertyKeyId(1), "value");
        catalog.insert_relation(RelationId(1), "LINK");
        let run = |row: &VertexRow, kind| {
            let mut events = Vec::new();
            let value = metadata(
                MetadataRecord::Vertex(row),
                kind,
                &catalog,
                |id| id != LabelId(99),
                |id| id != PropertyKeyId(99),
                &mut |event| {
                    events.push(event);
                    Ok::<_, ReadError>(())
                },
                &core::convert::identity,
            )
            .unwrap();
            (value, events)
        };
        for kind in [
            GraphElementMetadata::Labels,
            GraphElementMetadata::Properties,
            GraphElementMetadata::Keys,
        ] {
            assert_eq!(run(&hidden, kind), run(&visible, kind));
        }
        let mut large = visible.clone();
        large.props[0].1 = GraphValue::map(vec![(
            "nested".into(),
            GraphValue::List(
                vec![GraphValue::Scalar(
                    CanonicalScalar::ucs_basic_text(&"payload".repeat(8192)).unwrap(),
                )]
                .into_boxed_slice(),
            ),
        )])
        .unwrap()
        .to_property_value()
        .unwrap();
        assert_eq!(
            run(&large, GraphElementMetadata::Keys),
            run(&visible, GraphElementMetadata::Keys),
            "keys must not inspect or copy a visible property's nested payload"
        );
        let expected_labels = GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("zeta").unwrap()),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("alpha").unwrap()),
            ]
            .into_boxed_slice(),
        );
        assert_eq!(
            run(&visible, GraphElementMetadata::Labels).0,
            expected_labels
        );
        assert!(matches!(
            metadata(
                MetadataRecord::Vertex(&hidden),
                GraphElementMetadata::Labels,
                &catalog,
                |_| true,
                |_| true,
                &mut |_| Ok::<_, ReadError>(()),
                &core::convert::identity,
            ),
            Err(ReadError::UnmappedLabel(LabelId(99)))
        ));

        for kind in [
            GraphElementMetadata::Labels,
            GraphElementMetadata::Type,
            GraphElementMetadata::Properties,
            GraphElementMetadata::Keys,
        ] {
            let execute = |stop| {
                let mut calls = 0;
                let record = if kind == GraphElementMetadata::Type {
                    MetadataRecord::Edge(RelationId(1), &visible.props)
                } else {
                    MetadataRecord::Vertex(&visible)
                };
                let result = metadata(
                    record,
                    kind,
                    &catalog,
                    |_| true,
                    |_| true,
                    &mut |_| {
                        calls += 1;
                        if calls == stop { Err(calls) } else { Ok(()) }
                    },
                    &|_| usize::MAX,
                );
                (result, calls)
            };
            let (complete, calls) = execute(usize::MAX);
            assert!(complete.is_ok());
            assert!(calls > 0);
            for stop in 1..=calls {
                let (result, observed) = execute(stop);
                assert_eq!(result, Err(stop), "{kind:?} stop={stop}");
                assert_eq!(observed, stop);
            }
        }
    }

    #[test]
    fn masked_unknown_properties_match_physical_removal_in_values_and_charges() {
        let visible: [(PropertyKeyId, CanonicalPropertyValue); 2] = [
            (PropertyKeyId(1), CanonicalScalar::Int(7).into()),
            (PropertyKeyId(2), CanonicalScalar::Null.into()),
        ];
        let mut with_hidden = visible.to_vec();
        with_hidden.push((
            PropertyKeyId(99),
            CanonicalScalar::ucs_basic_text(&"hidden".repeat(1024))
                .unwrap()
                .into(),
        ));
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(PropertyKeyId(1), "zeta");
        catalog.insert_property(PropertyKeyId(2), "alpha");
        let run = |properties: &[(PropertyKeyId, CanonicalPropertyValue)]| {
            let mut events = Vec::new();
            let value = collect(
                properties.iter().map(|(key, value)| (*key, value)),
                Some(&catalog),
                |key| key != PropertyKeyId(99),
                &mut |event| {
                    events.push(event);
                    Ok::<_, ReadError>(())
                },
                &core::convert::identity,
            )
            .unwrap();
            (value, events)
        };
        let (value, events) = run(&visible);
        assert_eq!(run(&with_hidden), (value.clone(), events));
        assert_eq!(
            value,
            GraphValue::map(vec![
                ("alpha".into(), GraphValue::Scalar(CanonicalScalar::Null)),
                ("zeta".into(), GraphValue::Scalar(CanonicalScalar::Int(7))),
            ])
            .unwrap()
        );
        assert!(matches!(
            collect(
                with_hidden.iter().map(|(key, value)| (*key, value)),
                Some(&catalog),
                |_| true,
                &mut |_| Ok::<_, ReadError>(()),
                &core::convert::identity,
            ),
            Err(ReadError::UnmappedProperty(PropertyKeyId(99)))
        ));
    }

    #[test]
    fn empty_maps_need_no_catalog_and_ambiguous_names_refuse() {
        let empty = collect(
            [],
            None,
            |_| true,
            &mut |_| Ok::<_, ReadError>(()),
            &core::convert::identity,
        )
        .unwrap();
        assert_eq!(empty, GraphValue::map(Vec::new()).unwrap());
        let value = CanonicalPropertyValue::from(CanonicalScalar::Int(1));
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(PropertyKeyId(1), "same");
        catalog.insert_property(PropertyKeyId(2), "same");
        assert!(matches!(
            collect(
                [(PropertyKeyId(1), &value), (PropertyKeyId(2), &value)],
                Some(&catalog),
                |_| true,
                &mut |_| Ok::<_, ReadError>(()),
                &core::convert::identity,
            ),
            Err(ReadError::InvalidPropertyMap)
        ));
    }

    #[test]
    fn every_payload_reservation_refusal_returns_no_map_and_stops_callbacks() {
        let value = CanonicalPropertyValue::from(
            CanonicalScalar::ucs_basic_text(&"payload".repeat(33)).unwrap(),
        );
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(PropertyKeyId(1), "name".repeat(33));
        let run = |stop| {
            let mut calls = 0;
            let result = collect(
                [(PropertyKeyId(1), &value)],
                Some(&catalog),
                |_| true,
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(calls) } else { Ok(()) }
                },
                &|_| usize::MAX,
            );
            (result, calls)
        };
        let (expected, total) = run(usize::MAX);
        assert!(expected.is_ok());
        for stop in 1..=total {
            assert_eq!(run(stop), (Err(stop), stop));
        }
        assert_eq!(run(usize::MAX).0, expected);
    }
}
