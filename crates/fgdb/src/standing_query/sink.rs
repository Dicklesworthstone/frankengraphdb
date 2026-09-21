//! Typed downstream preparation shared by complete-group consumers.
//!
//! A sink's exclusive-borrow guard owns all fallible work. The source prepares
//! its aggregate and complete-group result, then this sink, before publishing
//! any participant. Dropping a guard abandons the whole downstream transition.

use super::{Meter, StandingQueryFailure, output, row};
use fgdb_delta_types::ZSet;
use fgdb_gql::GraphAggregateRow;

pub(super) trait PreparedSink {
    /// Return an owned final aggregate derivative when this sink has that row
    /// domain. Other sinks retain their native derivative themselves. Moving
    /// the prepared delta adds no clone or fallible callback; collection
    /// publication retains its ordinary allocator/panic boundary.
    fn commit(self) -> Option<ZSet<GraphAggregateRow>>;
}

pub(super) trait GroupSink {
    type Prepared<'a>: PreparedSink
    where
        Self: 'a;

    fn prepare<'a>(
        &'a mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Self::Prepared<'a>, StandingQueryFailure>;
}

impl GroupSink for output::State {
    type Prepared<'a> = output::Update<'a>;

    fn prepare<'a>(
        &'a mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Self::Prepared<'a>, StandingQueryFailure> {
        output::State::prepare(self, delta, meter)
    }
}

impl PreparedSink for output::Update<'_> {
    fn commit(self) -> Option<ZSet<GraphAggregateRow>> {
        Some(output::Update::commit(self))
    }
}

impl GroupSink for row::State {
    type Prepared<'a> = row::Update<'a>;

    fn prepare<'a>(
        &'a mut self,
        delta: &ZSet<GraphAggregateRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Self::Prepared<'a>, StandingQueryFailure> {
        row::State::prepare(self, delta, meter)
    }
}

impl PreparedSink for row::Update<'_> {
    fn commit(self) -> Option<ZSet<GraphAggregateRow>> {
        row::Update::commit(self);
        None
    }
}
