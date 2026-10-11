//! Complete, bounded property maps for admitted snapshot and overlay rows.
//! Visibility is applied before catalog access, payload inspection or charging.

use super::source::SourceEvent;
use crate::ReadError;
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::ReverseSymbolCatalog;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct PropertyMaps {
    pub(crate) vertices: BTreeMap<VId, GraphValue>,
    pub(crate) edges: BTreeMap<EId, GraphValue>,
}

/// Enumerate effective source properties, never the catalog's known-key set.
/// Each retained cell and variable payload is reserved before copying. The
/// map constructor owns canonical key ordering and refuses duplicate names.
pub(crate) fn collect<'a, E>(
    properties: impl IntoIterator<Item = (PropertyKeyId, &'a CanonicalScalar)>,
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
            | CanonicalScalar::Float(_) => [0, 0],
        };
        for bytes in sizes {
            payload(bytes, control)?;
        }
        entries.push((name.as_str().into(), GraphValue::Scalar(value.clone())));
    }
    GraphValue::map(entries).ok_or_else(|| failure(ReadError::InvalidPropertyMap))
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
    fn masked_unknown_properties_match_physical_removal_in_values_and_charges() {
        let visible = [
            (PropertyKeyId(1), CanonicalScalar::Int(7)),
            (PropertyKeyId(2), CanonicalScalar::Null),
        ];
        let mut with_hidden = visible.to_vec();
        with_hidden.push((
            PropertyKeyId(99),
            CanonicalScalar::ucs_basic_text(&"hidden".repeat(1024)).unwrap(),
        ));
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(PropertyKeyId(1), "zeta");
        catalog.insert_property(PropertyKeyId(2), "alpha");
        let run = |properties: &[(PropertyKeyId, CanonicalScalar)]| {
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
        let value = CanonicalScalar::Int(1);
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
        let value = CanonicalScalar::ucs_basic_text(&"payload".repeat(33)).unwrap();
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
