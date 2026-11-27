//! Cross-block identity admission for a cold read-only open.
//!
//! One statement needs only its destination, lifetime and a shared family
//! coordinate. Changed content versions use the ordinary history validator.

use super::*;
use fgdb_types::ids::ObjectId;
use std::collections::btree_map::Entry;

// Nodes hold up to eleven keys and values and at least five after a split,
// apart from the root. Include internal pointers, alignment and spare slots.
// INITIAL_EDGE_HISTORY_BYTES covers root nodes and the first small allocations.
const SINGLETON_BYTES: usize = 192;

struct Singleton {
    family: usize,
    dst: VId,
    created_at: CommitSeq,
    retired_at: Option<CommitSeq>,
}

struct FamilyHead {
    family: (VId, RelationId),
    block: ObjectId,
}

pub(super) struct EdgeAdmission {
    // None marks an identity transferred to the canonical validator.
    first: BTreeMap<EId, Option<Singleton>>,
    changed: crate::root::EdgeHistoryValidator,
    families: BTreeMap<(VId, RelationId), usize>,
    heads: Vec<FamilyHead>,
    charge: MemoryCharge,
}

impl EdgeAdmission {
    pub(super) fn new(
        cx: &impl BufferedReadCx,
        pool: &MemoryPool,
        has_blocks: bool,
    ) -> Result<Self, BufferedReadError> {
        Ok(Self {
            first: BTreeMap::new(),
            changed: Default::default(),
            families: BTreeMap::new(),
            heads: Vec::new(),
            charge: pool.reserve(
                cx,
                if has_blocks {
                    INITIAL_EDGE_HISTORY_BYTES
                } else {
                    0
                },
            )?,
        })
    }

    fn advance_family(
        &mut self,
        cx: &impl BufferedReadCx,
        at: usize,
        family: (VId, RelationId),
        block: ObjectId,
        predecessor: Option<crate::DeltaBlockVersion>,
    ) -> Result<usize, BufferedReadError> {
        let slot = self.families.entry(family);
        let expected = match &slot {
            Entry::Occupied(slot) => Some(self.heads[*slot.get()].block),
            Entry::Vacant(_) => None,
        };
        let declared = predecessor.map(|link| link.0);
        if declared != expected {
            return Err(
                StoreError::MalformedRoot(crate::root::RootError::BlockChainMismatch {
                    at,
                    declared,
                    expected,
                })
                .into(),
            );
        }
        match slot {
            Entry::Occupied(slot) => {
                let index = *slot.get();
                self.heads[index].block = block;
                Ok(index)
            }
            Entry::Vacant(slot) => {
                self.charge.grow(cx, HISTORY_ENTRY_BYTES)?;
                let index = self.heads.len();
                self.heads.push(FamilyHead { family, block });
                slot.insert(index);
                Ok(index)
            }
        }
    }

    /// The block decoder has checked each row and its strict canonical order.
    /// The predecessor and cross-block history checks still run in root order.
    pub(super) fn observe_block(
        &mut self,
        cx: &impl BufferedReadCx,
        at: usize,
        id: ObjectId,
        entries: &[AdjacencyEntry],
        predecessor: Option<crate::DeltaBlockVersion>,
    ) -> Result<(), BufferedReadError> {
        let Some(head) = entries.first() else {
            return Ok(());
        };
        let family = self.advance_family(cx, at, (head.src, head.relation), id, predecessor)?;
        // Pay for the whole format-bounded input before inserting its first
        // key. Refund repeated keys after the walk, keeping new births to one
        // pool acquisition per block.
        self.charge.grow(cx, entries.len() * SINGLETON_BYTES)?;
        let before = self.first.len();
        for row in entries {
            match self.first.entry(row.eid) {
                Entry::Vacant(slot) => {
                    slot.insert(Some(Singleton {
                        family,
                        dst: row.dst,
                        created_at: row.created_at,
                        retired_at: row.retired_at,
                    }));
                }
                Entry::Occupied(mut slot) => {
                    if let Some(first) = slot.get_mut() {
                        let (src, relation) = self.heads[first.family].family;
                        let original = AdjacencyEntry {
                            eid: row.eid,
                            src,
                            relation,
                            dst: first.dst,
                            created_at: first.created_at,
                            retired_at: first.retired_at,
                        };
                        // A singleton may be repeated exactly or acquire its
                        // first retirement. Every other change goes through
                        // the full history law, including attempts to reopen
                        // a retired version or change a birth coordinate.
                        if original == *row
                            || (original.retired_at.is_none()
                                && (AdjacencyEntry {
                                    retired_at: row.retired_at,
                                    ..original
                                }) == *row)
                        {
                            first.retired_at = row.retired_at;
                            continue;
                        }
                        self.charge.grow(cx, HISTORY_ENTRY_BYTES)?;
                        self.changed
                            .observe_block(at, std::slice::from_ref(&original))
                            .map_err(StoreError::MalformedRoot)?;
                        slot.insert(None);
                    }
                    let previous = self.changed.statement_count();
                    self.charge.grow(cx, HISTORY_ENTRY_BYTES)?;
                    self.changed
                        .observe_block(at, std::slice::from_ref(row))
                        .map_err(StoreError::MalformedRoot)?;
                    if self.changed.statement_count() == previous {
                        drop(
                            self.charge
                                .split_off(HISTORY_ENTRY_BYTES)
                                .expect("reserved the incoming version"),
                        );
                    }
                }
            }
        }
        let unused = (entries.len() - (self.first.len() - before)) * SINGLETON_BYTES;
        if unused != 0 {
            drop(
                self.charge
                    .split_off(unused)
                    .expect("reserved the incoming keys"),
            );
        }
        Ok(())
    }
}
