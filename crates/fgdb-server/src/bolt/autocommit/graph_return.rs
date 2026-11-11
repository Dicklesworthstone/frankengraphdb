//! Bolt graph values are owned before publication, from the authorized final
//! write overlay. Hydration has no database handle, new permit or later read.

use super::{
    Entities, Refusal, Value, WireValue, admit_scalar_value, output_budget, output_bytes,
    output_slots, output_work, value,
};
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphElementMetadata, ReverseSymbolCatalog};
use fgdb_protocol::body::ErrorCode;
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

pub(super) struct ReturnEncoder<'a> {
    work: &'a mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
    metadata: &'a mut dyn fgdb::AuthorizedWriteOutput,
    catalog: &'a ReverseSymbolCatalog,
}

fn unavailable() -> Refusal {
    Refusal::new(
        ErrorCode::Execution,
        "graph value is unavailable in the write workspace",
    )
}

impl<'a> ReturnEncoder<'a> {
    pub(super) fn new(
        work: &'a mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
        metadata: &'a mut dyn fgdb::AuthorizedWriteOutput,
        catalog: &'a ReverseSymbolCatalog,
    ) -> Self {
        Self {
            work,
            metadata,
            catalog,
        }
    }

    pub(super) fn cell(
        &mut self,
        cell: WireValue,
        depth: usize,
        bytes: &mut usize,
    ) -> Result<Value, Refusal> {
        output_work(self.work, 1)?;
        if depth > fgdb_bolt::packstream::MAX_DEPTH {
            return Err(output_budget());
        }
        match cell {
            WireValue::Vertex(id) => self.node(id, depth, bytes),
            WireValue::Edge(id) => self
                .relationship(id, false, depth, bytes)
                .map(|(value, _, _)| value),
            WireValue::Vertices(ids) => {
                output_slots::<Value>(bytes, ids.len())?;
                let mut values = Vec::with_capacity(ids.len());
                for id in ids {
                    values.push(self.cell(WireValue::Vertex(id), depth + 1, bytes)?);
                }
                Ok(Value::List(values))
            }
            WireValue::Edges(ids) => {
                output_slots::<Value>(bytes, ids.len())?;
                let mut values = Vec::with_capacity(ids.len());
                for id in ids {
                    values.push(self.cell(WireValue::Edge(id), depth + 1, bytes)?);
                }
                Ok(Value::List(values))
            }
            WireValue::Path { start, steps } => self.path(start, &steps, depth, bytes),
            WireValue::List(items) => {
                output_slots::<Value>(bytes, items.len())?;
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.cell(item, depth + 1, bytes)?);
                }
                Ok(Value::List(values))
            }
            WireValue::Map(entries) => {
                output_slots::<(String, Value)>(bytes, entries.len())?;
                let mut values = Vec::with_capacity(entries.len());
                for (key, item) in entries {
                    output_work(self.work, key.len().div_ceil(64) as u64 + 1)?;
                    output_bytes(bytes, key.capacity())?;
                    values.push((key, self.cell(item, depth + 1, bytes)?));
                }
                Ok(Value::Map(values))
            }
            scalar => {
                admit_scalar_value(&scalar, depth, bytes, self.work)?;
                value(scalar, &Entities::default())
                    .map_err(|error| Refusal::new(ErrorCode::Statement, error.message))
            }
        }
    }

    fn read(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
    ) -> Result<GraphValue, Refusal> {
        self.metadata
            .metadata(element, kind, self.catalog)
            .map_err(|error| crate::execute::write_refusal(&error))?
            .ok_or_else(unavailable)
    }

    fn metadata_cell(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        depth: usize,
        bytes: &mut usize,
    ) -> Result<Value, Refusal> {
        let native = self.read(element, kind)?;
        let wire = crate::execute::graph_output_value(&native, self.work)?;
        drop(native);
        self.cell(wire, depth, bytes)
    }

    fn identity_text(&mut self, id: u128, bytes: &mut usize) -> Result<Value, Refusal> {
        // A full-width u128 has at most 39 decimal digits. Admit formatting
        // before allocating, preserving the same identity as read hydration.
        output_work(self.work, 1)?;
        output_bytes(bytes, 39)?;
        Ok(Value::String(id.to_string()))
    }

    fn node(&mut self, id: u128, depth: usize, bytes: &mut usize) -> Result<Value, Refusal> {
        output_slots::<Value>(bytes, 4)?;
        let element = ElementId::Vertex(VId(id));
        let labels = self.metadata_cell(element, GraphElementMetadata::Labels, depth + 1, bytes)?;
        let properties =
            self.metadata_cell(element, GraphElementMetadata::Properties, depth + 1, bytes)?;
        if !matches!(&labels, Value::List(values) if values.iter().all(|value| matches!(value, Value::String(_))))
            || !matches!(&properties, Value::Map(_))
        {
            return Err(unavailable());
        }
        Ok(Value::Struct {
            tag: 0x4e,
            fields: vec![
                Value::Int(i64::try_from(id).unwrap_or(-1)),
                labels,
                properties,
                self.identity_text(id, bytes)?,
            ],
        })
    }

    fn relationship(
        &mut self,
        id: u128,
        unbound: bool,
        depth: usize,
        bytes: &mut usize,
    ) -> Result<(Value, u128, u128), Refusal> {
        output_slots::<Value>(bytes, if unbound { 4 } else { 8 })?;
        let element = ElementId::Edge(EId(id));
        let kind = self.metadata_cell(element, GraphElementMetadata::Type, depth + 1, bytes)?;
        let properties =
            self.metadata_cell(element, GraphElementMetadata::Properties, depth + 1, bytes)?;
        let GraphValue::Vertex(source) = self.read(element, GraphElementMetadata::StartNode)? else {
            return Err(unavailable());
        };
        let GraphValue::Vertex(target) = self.read(element, GraphElementMetadata::EndNode)? else {
            return Err(unavailable());
        };
        if !matches!(&kind, Value::String(_)) || !matches!(&properties, Value::Map(_)) {
            return Err(unavailable());
        }
        let element_id = self.identity_text(id, bytes)?;
        let encoded = if unbound {
            Value::Struct {
                tag: 0x72,
                fields: vec![
                    Value::Int(i64::try_from(id).unwrap_or(-1)),
                    kind,
                    properties,
                    element_id,
                ],
            }
        } else {
            Value::Struct {
                tag: 0x52,
                fields: vec![
                    Value::Int(i64::try_from(id).unwrap_or(-1)),
                    Value::Int(i64::try_from(source.0).unwrap_or(-1)),
                    Value::Int(i64::try_from(target.0).unwrap_or(-1)),
                    kind,
                    properties,
                    element_id,
                    self.identity_text(source.0, bytes)?,
                    self.identity_text(target.0, bytes)?,
                ],
            }
        };
        Ok((encoded, source.0, target.0))
    }

    fn path(
        &mut self,
        start: u128,
        steps: &[(u128, u128)],
        depth: usize,
        bytes: &mut usize,
    ) -> Result<Value, Refusal> {
        // Path fields are lists; graph structures are two levels below the
        // path. The final retained-value walk independently checks that depth.
        let node_capacity = steps.len().checked_add(1).ok_or_else(output_budget)?;
        let index_capacity = steps.len().checked_mul(2).ok_or_else(output_budget)?;
        output_slots::<Value>(bytes, 3)?;
        output_slots::<Value>(bytes, node_capacity)?;
        output_slots::<Value>(bytes, steps.len())?;
        output_slots::<Value>(bytes, index_capacity)?;
        // Bound temporary tree nodes conservatively before allocating either
        // identity map. These bytes are released when the path is complete;
        // they never become a second result-retention allowance.
        let scratch = node_capacity.checked_mul(2048).ok_or_else(output_budget)?;
        output_bytes(bytes, scratch)?;
        let lookup_work = u64::from(node_capacity.ilog2()) + 1;
        output_work(self.work, lookup_work)?;
        let mut nodes = Vec::with_capacity(node_capacity);
        nodes.push(self.cell(WireValue::Vertex(start), depth + 2, bytes)?);
        let mut relationships = Vec::with_capacity(steps.len());
        let mut indices = Vec::with_capacity(index_capacity);
        let mut node_indices = BTreeMap::from([(start, 0_i64)]);
        let mut edge_indices = BTreeMap::<u128, (i64, u128, u128)>::new();
        let mut current = start;
        for &(edge, next) in steps {
            output_work(self.work, lookup_work.saturating_mul(4))?;
            let (edge_index, source, target) = if let Some(metadata) = edge_indices.get(&edge) {
                *metadata
            } else {
                let (encoded, source, target) = self.relationship(edge, true, depth + 2, bytes)?;
                let index = i64::try_from(relationships.len())
                    .ok()
                    .and_then(|value| value.checked_add(1))
                    .ok_or_else(output_budget)?;
                relationships.push(encoded);
                edge_indices.insert(edge, (index, source, target));
                (index, source, target)
            };
            let forward = if (source, target) == (current, next) {
                true
            } else if (source, target) == (next, current) {
                false
            } else {
                return Err(unavailable());
            };
            let node_index = if let Some(&index) = node_indices.get(&next) {
                index
            } else {
                let index = i64::try_from(nodes.len()).map_err(|_| output_budget())?;
                nodes.push(self.cell(WireValue::Vertex(next), depth + 2, bytes)?);
                node_indices.insert(next, index);
                index
            };
            indices.push(Value::Int(if forward { edge_index } else { -edge_index }));
            indices.push(Value::Int(node_index));
            current = next;
        }
        *bytes -= scratch;
        Ok(Value::Struct {
            tag: 0x50,
            fields: vec![
                Value::List(nodes),
                Value::List(relationships),
                Value::List(indices),
            ],
        })
    }
}
