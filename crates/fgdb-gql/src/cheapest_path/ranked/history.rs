//! Prefix-local mode state. Relaxed suffixes are bounds, never dominance
//! witnesses for histories with different used edges or visited vertices.

use super::*;

pub(super) struct History {
    mode: GraphCheapestPathMode,
    source: VId,
    edges: BTreeSet<EId>,
    vertices: BTreeSet<VId>,
    closed: bool,
}
impl History {
    pub(super) fn new<E>(
        mode: GraphCheapestPathMode,
        source: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut history = Self {
            mode, source, edges: BTreeSet::new(), vertices: BTreeSet::new(), closed: false,
        };
        if matches!(mode, GraphCheapestPathMode::Acyclic | GraphCheapestPathMode::Simple) {
            control(GlaExecutionEvent::ScratchEntry)?;
            history.vertices.insert(source);
        }
        Ok(history)
    }

    // Callers charge logical work before every membership decision.
    pub(super) fn allows(&self, step: Step) -> bool {
        if self.closed { return false; }
        match self.mode {
            GraphCheapestPathMode::Walk => true,
            GraphCheapestPathMode::Trail => !self.edges.contains(&step.0),
            GraphCheapestPathMode::Acyclic => !self.vertices.contains(&step.1),
            GraphCheapestPathMode::Simple => step.1 == self.source || !self.vertices.contains(&step.1),
        }
    }

    pub(super) fn closes(&self, step: Step) -> bool {
        self.mode == GraphCheapestPathMode::Simple && step.1 == self.source
    }
    pub(super) fn closed(&self) -> bool { self.closed }

    // Only an allowed prefix step may enter this state. WALK does not acquire
    // history entries or change its original event stream.
    pub(super) fn advance<E>(
        &mut self, step: Step,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        match self.mode {
            GraphCheapestPathMode::Walk => {}
            GraphCheapestPathMode::Trail => {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.edges.insert(step.0);
            }
            GraphCheapestPathMode::Acyclic | GraphCheapestPathMode::Simple => {
                if self.closes(step) {
                    self.closed = true;
                } else {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    self.vertices.insert(step.1);
                }
            }
        }
        Ok(())
    }
}

// A partition's relaxed minimum is <= every permitted completion under the
// full (cost, path-lex) ordering. Refinement replaces an invalid minimum with
// disjoint first-different-action partitions along its permitted prefix only.
// No permitted completion can contain its first forbidden step, so discarding
// that entire subtree loses no answer. Therefore a permitted heap minimum is
// the next exact answer, even with negative weights; suffix bounds never merge
// histories. STOP must participate in the partition for variable-length ties.
impl Search {
    pub(super) fn admissible<E>(
        &self, entry: &Partition,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        if self.mode == GraphCheapestPathMode::Walk { return Ok(true); }
        let mut history = History::new(self.mode, self.source, control)?;
        for &step in &entry.steps {
            control(GlaExecutionEvent::Work)?;
            if !history.allows(step) { return Ok(false); }
            history.advance(step, control)?;
        }
        Ok(true)
    }
}
