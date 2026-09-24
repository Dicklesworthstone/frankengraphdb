//! A temporary, source-masked edge image for the existing physical cursor.
//! Owning fields is not topology authority: the source admits both endpoints,
//! relation and historical visibility before constructing either record form.

use super::{EdgeScanRow, GlaExecutionEvent};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};

pub enum EdgeScanRecord<'a> {
    Borrowed(EdgeScanRow<'a>),
    Owned {
        source: VId,
        target: VId,
        relation: RelationId,
        properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
}
impl EdgeScanRecord<'_> {
    pub fn as_row(&self) -> EdgeScanRow<'_> {
        match self {
            Self::Borrowed(row) => *row,
            Self::Owned {
                source,
                target,
                relation,
                properties,
            } => EdgeScanRow {
                source: *source,
                target: *target,
                relation: *relation,
                properties,
            },
        }
    }

    /// Copy only permitted fields, retaining canonical key order. Hidden
    /// payloads are never cloned or encoded. Admit each field and payload unit
    /// before copying; failure releases no partial record. These logical units
    /// are not allocator bytes, a spill policy, or physical noninterference.
    pub fn copy_masked<E>(
        row: EdgeScanRow<'_>,
        mut allows_property: impl FnMut(PropertyKeyId) -> bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut properties = Vec::new();
        for (key, value) in row.properties {
            control(GlaExecutionEvent::Work)?;
            if allows_property(*key) {
                control(GlaExecutionEvent::ScratchEntry)?;
                crate::algebra_exec::charge_payload(value, &mut |_| {
                    control(GlaExecutionEvent::Work)?;
                    control(GlaExecutionEvent::ScratchEntry)
                })?;
                properties.push((*key, value.clone()));
            }
        }
        Ok(Self::Owned {
            source: row.source,
            target: row.target,
            relation: row.relation,
            properties,
        })
    }
}
impl core::fmt::Debug for EdgeScanRecord<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeScanRecord([REDACTED])")
    }
}

#[cfg(test)]
mod tests;
