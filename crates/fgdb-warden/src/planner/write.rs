//! Target admission for ordinary vertex/edge creation, update and deletion.
//!
//! These are borrowed host-supplied images, NOT bearer-authenticated facts.
//! The trusted mutation planner must obtain before-images from its pinned
//! source/overlay, check every affected object (including detached edges), and
//! apply exactly the checked after-images in the same conflict-validated write.
//! A successful check is neither a transferable authority nor a commit receipt.
//! Touched fields come from original write intents, before no-op elimination;
//! even assigning the existing value of a forbidden property must be refused.
//! Sample fresh trusted time and checkpoint the permit again before publication.
//! This module changes no storage, performs no I/O, and claims no raw Database
//! enforcement, durable revocation, constraint checking, or timing isolation.
//!
//! Signed work measures the authorized logical image, not private preservation
//! work. Adding unchanged hidden labels/properties must not move the caller's
//! work-limit refusal threshold. Hidden fields still undergo canonical-shape
//! validation, exact preservation checks and live permit polling; they are NOT
//! omitted from either image. Physical scan/compare costs remain the trusted
//! host's resource obligation, not a promise made by this logical work budget.
//! This law applies to valid, scope-admitted images with unchanged hidden data;
//! it does not declassify malformed images, target existence, or write conflicts.

use super::{ExecutionPermit, WriteAccess};
use crate::{Error, LimitDimension};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};

/// Logical comparison charge, not an allocation or latency measurement.
const COMPARE_BYTES_PER_UNIT: usize = 64;

/// Complete original vertex fields. Labels and property keys must be strictly
/// increasing, exactly as in the admitted native row; never pass masked fields.
#[derive(Clone, Copy)]
pub struct VertexWriteImage<'a> {
    pub id: VId,
    pub labels: &'a [LabelId],
    pub properties: &'a [(PropertyKeyId, CanonicalScalar)],
}

/// Original attempted field writes, including no-ops and removal of absent
/// fields. Both slices must be strictly increasing. All changed fields must be
/// declared; creation and deletion change every present field. The host derives
/// this from its original intents, never only from the normalized net effect.
#[derive(Clone, Copy)]
pub struct VertexWriteFields<'a> {
    pub labels: &'a [LabelId],
    pub properties: &'a [PropertyKeyId],
}

/// An existing endpoint in the host's exact before/after graph state. Endpoint
/// existence and correspondence to this ID are the trusted source's obligation.
#[derive(Clone, Copy)]
pub struct WriteEndpoint<'a> {
    pub id: VId,
    pub labels: &'a [LabelId],
}

/// Complete original edge fields, including both endpoint authorization inputs.
/// In-place updates cannot change identity, relation, or endpoint identities.
#[derive(Clone, Copy)]
pub struct EdgeWriteImage<'a> {
    pub id: EId,
    pub relation: RelationId,
    pub source: WriteEndpoint<'a>,
    pub destination: WriteEndpoint<'a>,
    pub properties: &'a [(PropertyKeyId, CanonicalScalar)],
}

impl core::fmt::Debug for VertexWriteImage<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexWriteImage([REDACTED])")
    }
}
impl core::fmt::Debug for VertexWriteFields<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("VertexWriteFields([REDACTED])")
    }
}
impl core::fmt::Debug for WriteEndpoint<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("WriteEndpoint([REDACTED])")
    }
}
impl core::fmt::Debug for EdgeWriteImage<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeWriteImage([REDACTED])")
    }
}

impl ExecutionPermit<'_, WriteAccess> {
    /// None -> Some creates, Some -> None deletes, Some -> Some updates.
    /// Both None is malformed. Every present image must satisfy the ORIGINAL
    /// vertex predicate. Every added/removed label and changed property needs
    /// explicit field authority and a touched-field declaration; unchanged
    /// hidden fields may be preserved only when they were not written at all.
    /// Deletion removes every field, so hidden fields cannot be erased through
    /// whole-object deletion. DETACH requires separate checks for every edge.
    ///
    /// Rejection terminally stops this permit and retains all spent work.
    /// No-op updates still require target admission and consume its budget.
    pub fn check_vertex_write_at(
        &mut self,
        now_ms: u64,
        before: Option<VertexWriteImage<'_>>,
        after: Option<VertexWriteImage<'_>>,
        touched: VertexWriteFields<'_>,
    ) -> Result<(), Error> {
        self.checkpoint_at(now_ms)?;
        let result = (|| {
            if !self.program.rights.can_write() {
                return Err(Error::PermissionDenied);
            }
            self.admit_touched_labels(now_ms, touched.labels)?;
            self.admit_touched_properties(now_ms, touched.properties)?;
            if before.is_none() && after.is_none() {
                return Err(Error::InvalidWriteImage);
            }
            if let (Some(old), Some(new)) = (before, after)
                && old.id != new.id
            {
                return Err(Error::InvalidWriteImage);
            }
            for image in [before, after].into_iter().flatten() {
                self.admit_write_vertex(now_ms, image.labels)?;
                self.admit_write_properties(now_ms, image.properties)?;
            }
            self.check_changed_labels(
                now_ms,
                before.map_or(&[], |image| image.labels),
                after.map_or(&[], |image| image.labels),
                touched.labels,
            )?;
            self.check_changed_properties(
                now_ms,
                before.map_or(&[], |image| image.properties),
                after.map_or(&[], |image| image.properties),
                touched.properties,
            )?;
            self.checkpoint_at(now_ms)
        })();
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    /// Check both endpoint predicates and the relation for EVERY present image.
    /// Rewiring is not an in-place update: separately authorize delete/create
    /// with the native identity rules. Self-loop endpoints must agree on labels.
    /// Property preservation/change and deletion use the vertex check's laws.
    /// The touched property list includes attempted no-op writes. Changes to
    /// endpoint labels require separate vertex write checks in the same batch.
    pub fn check_edge_write_at(
        &mut self,
        now_ms: u64,
        before: Option<EdgeWriteImage<'_>>,
        after: Option<EdgeWriteImage<'_>>,
        touched_properties: &[PropertyKeyId],
    ) -> Result<(), Error> {
        self.checkpoint_at(now_ms)?;
        let result = (|| {
            if !self.program.rights.can_write() {
                return Err(Error::PermissionDenied);
            }
            self.admit_touched_properties(now_ms, touched_properties)?;
            if before.is_none() && after.is_none() {
                return Err(Error::InvalidWriteImage);
            }
            if let (Some(old), Some(new)) = (before, after)
                && (old.id != new.id
                    || old.relation != new.relation
                    || old.source.id != new.source.id
                    || old.destination.id != new.destination.id)
            {
                return Err(Error::InvalidWriteImage);
            }
            for image in [before, after].into_iter().flatten() {
                self.charge_work_at(now_ms, 1)?;
                if !self.program.allows_relation(image.relation) {
                    return Err(Error::ScopeDenied);
                }
                self.admit_write_vertex(now_ms, image.source.labels)?;
                self.admit_write_vertex(now_ms, image.destination.labels)?;
                if image.source.id == image.destination.id {
                    let visible = image
                        .source
                        .labels
                        .iter()
                        .filter(|label| self.program.allows_label(**label))
                        .count();
                    self.charge_write_work(now_ms, visible as u128)?;
                    if image.source.labels != image.destination.labels {
                        return Err(Error::InvalidWriteImage);
                    }
                }
                self.admit_write_properties(now_ms, image.properties)?;
            }
            self.check_changed_properties(
                now_ms,
                before.map_or(&[], |image| image.properties),
                after.map_or(&[], |image| image.properties),
                touched_properties,
            )?;
            self.checkpoint_at(now_ms)
        })();
        if result.is_err() {
            self.stopped = true;
        }
        result
    }

    fn charge_write_work(&mut self, now_ms: u64, units: u128) -> Result<(), Error> {
        let Ok(units) = u64::try_from(units) else {
            self.stopped = true;
            return Err(Error::LimitExceeded(LimitDimension::Work));
        };
        self.charge_work_at(now_ms, units)
    }

    // The caller's capability may attenuate MaxWork. Charging hidden image
    // fields there would let a holder binary-search their count or byte size.
    // Polling remains mandatory even when no observable work is charged.
    fn charge_label_work(
        &mut self,
        now_ms: u64,
        label: LabelId,
        units: u128,
    ) -> Result<(), Error> {
        if self.program.allows_label(label) {
            self.charge_write_work(now_ms, units)
        } else {
            self.checkpoint_at(now_ms)
        }
    }

    fn charge_property_work(
        &mut self,
        now_ms: u64,
        key: PropertyKeyId,
        units: u128,
    ) -> Result<(), Error> {
        if self.program.allows_property(key) {
            self.charge_write_work(now_ms, units)
        } else {
            self.checkpoint_at(now_ms)
        }
    }

    fn admit_touched_labels(&mut self, now_ms: u64, labels: &[LabelId]) -> Result<(), Error> {
        let mut previous = None;
        for &label in labels {
            self.charge_write_work(now_ms, 1 + self.program.label_clauses.len() as u128)?;
            if previous.is_some_and(|old| old >= label) {
                return Err(Error::InvalidWriteImage);
            }
            if !self.program.allows_label(label) {
                return Err(Error::ScopeDenied);
            }
            previous = Some(label);
        }
        Ok(())
    }

    fn admit_touched_properties(
        &mut self,
        now_ms: u64,
        keys: &[PropertyKeyId],
    ) -> Result<(), Error> {
        let mut previous = None;
        for &key in keys {
            self.charge_work_at(now_ms, 1)?;
            if previous.is_some_and(|old| old >= key) {
                return Err(Error::InvalidWriteImage);
            }
            if !self.program.allows_property(key) {
                return Err(Error::ScopeDenied);
            }
            previous = Some(key);
        }
        Ok(())
    }

    fn admit_write_vertex(&mut self, now_ms: u64, labels: &[LabelId]) -> Result<(), Error> {
        // Count every before/after endpoint admission, including self loops.
        self.charge_nodes_at(now_ms, 1)?;
        let mut previous = None;
        let mut visible = 0_u128;
        for &label in labels {
            self.charge_label_work(now_ms, label, 1)?;
            if previous.is_some_and(|old| old >= label) {
                return Err(Error::InvalidWriteImage);
            }
            visible += u128::from(self.program.allows_label(label));
            previous = Some(label);
        }
        // The original labels still decide authorization. Only the visible
        // label domain participates in the holder-observable logical charge.
        let clauses = self.program.label_clauses.len() as u128;
        self.charge_write_work(now_ms, 1 + clauses * (1 + visible))?;
        if !self.program.allows_vertex(labels) {
            return Err(Error::ScopeDenied);
        }
        Ok(())
    }

    fn admit_write_properties(
        &mut self,
        now_ms: u64,
        properties: &[(PropertyKeyId, CanonicalScalar)],
    ) -> Result<(), Error> {
        let mut previous = None;
        for (key, scalar) in properties {
            self.charge_property_work(now_ms, *key, 1)?;
            if previous.is_some_and(|old| old >= *key) {
                return Err(Error::InvalidWriteImage);
            }
            scalar
                .canonical_encoded_len()
                .map_err(|_| Error::InvalidWriteImage)?;
            previous = Some(*key);
        }
        Ok(())
    }

    fn check_changed_labels(
        &mut self,
        now_ms: u64,
        mut before: &[LabelId],
        mut after: &[LabelId],
        touched: &[LabelId],
    ) -> Result<(), Error> {
        while !before.is_empty() || !after.is_empty() {
            let label = before
                .first()
                .into_iter()
                .chain(after.first())
                .copied()
                .min()
                .expect("nonempty merge");
            self.charge_label_work(now_ms, label, 1 + self.program.label_clauses.len() as u128)?;
            let changed = match (before.first(), after.first()) {
                (Some(old), Some(new)) if old == new => {
                    before = &before[1..];
                    after = &after[1..];
                    None
                }
                (Some(old), Some(new)) if old < new => {
                    let key = *old;
                    before = &before[1..];
                    Some(key)
                }
                (Some(old), None) => {
                    let key = *old;
                    before = &before[1..];
                    Some(key)
                }
                (_, Some(new)) => {
                    let key = *new;
                    after = &after[1..];
                    Some(key)
                }
                (None, None) => unreachable!("nonempty merge"),
            };
            if changed.is_some_and(|label| touched.binary_search(&label).is_err()) {
                return Err(Error::InvalidWriteImage);
            }
        }
        Ok(())
    }

    fn check_changed_properties(
        &mut self,
        now_ms: u64,
        mut before: &[(PropertyKeyId, CanonicalScalar)],
        mut after: &[(PropertyKeyId, CanonicalScalar)],
        touched: &[PropertyKeyId],
    ) -> Result<(), Error> {
        while !before.is_empty() || !after.is_empty() {
            let key = before
                .first()
                .map(|(key, _)| *key)
                .into_iter()
                .chain(after.first().map(|(key, _)| *key))
                .min()
                .expect("nonempty merge");
            self.charge_property_work(now_ms, key, 1)?;
            match (before.first(), after.first()) {
                (Some((old_key, old)), Some((new_key, new))) if old_key == new_key => {
                    if touched.binary_search(old_key).is_err() {
                        let old_len = old
                            .canonical_encoded_len()
                            .map_err(|_| Error::InvalidWriteImage)?;
                        let new_len = new
                            .canonical_encoded_len()
                            .map_err(|_| Error::InvalidWriteImage)?;
                        let units = (old_len as u128 + new_len as u128)
                            .div_ceil(COMPARE_BYTES_PER_UNIT as u128);
                        self.charge_property_work(now_ms, *old_key, units)?;
                        // Exact canonical equality, including HIDDEN values:
                        // only accounting is masked, never the preservation law.
                        if old != new {
                            return Err(Error::InvalidWriteImage);
                        }
                    }
                    before = &before[1..];
                    after = &after[1..];
                }
                (Some((old_key, _)), Some((new_key, _))) if old_key < new_key => {
                    if touched.binary_search(old_key).is_err() {
                        return Err(Error::InvalidWriteImage);
                    }
                    before = &before[1..];
                }
                (Some((old_key, _)), None) => {
                    if touched.binary_search(old_key).is_err() {
                        return Err(Error::InvalidWriteImage);
                    }
                    before = &before[1..];
                }
                (_, Some((new_key, _))) => {
                    if touched.binary_search(new_key).is_err() {
                        return Err(Error::InvalidWriteImage);
                    }
                    after = &after[1..];
                }
                (None, None) => unreachable!("nonempty merge"),
            }
        }
        Ok(())
    }
}
