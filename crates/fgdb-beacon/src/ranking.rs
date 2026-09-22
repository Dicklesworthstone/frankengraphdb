use std::cmp::Ordering;
use std::collections::BinaryHeap;

use fgdb_types::VId;

/// Lower cost wins, then lower VId. All producers reject non-finite inputs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ranked {
    pub cost: f64,
    pub id: VId,
    pub slot: usize,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cost
            .total_cmp(&other.cost)
            .then_with(|| self.id.cmp(&other.id))
            .then_with(|| self.slot.cmp(&other.slot))
    }
}

pub(crate) fn retain_best(heap: &mut BinaryHeap<Ranked>, value: Ranked, limit: usize) {
    if limit == 0 {
        return;
    }
    if heap.len() < limit {
        heap.push(value);
    } else if heap.peek().is_some_and(|worst| value < *worst) {
        heap.pop();
        heap.push(value);
    }
}
