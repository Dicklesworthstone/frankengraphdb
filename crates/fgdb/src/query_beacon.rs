//! Native snapshot -> bounded scalar projection -> Beacon search. Historical
//! winners are selected by the SAME visitor as GLA, before any label/property
//! projection. This is per-execution resident construction, not a maintained
//! index, a new authority, a durable generation, or an external-memory claim.

use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::read::{ReadError as Error, ReadOptions, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, IndexConfig, WorkBudget, WorkControl};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CommitSeq, QueryCx};
use std::cell::RefCell;

pub(crate) type Options = ReadOptions<PropertyKeyId, LabelId>;
type Cancel = Box<asupersync::error::Error>;

/// A typed interruption is retained outside Beacon's narrow WorkControl sum.
/// No later wrapper check may replace an expiry/cancellation with a generic
/// index refusal. One meter is shared sequentially by visitor and builder.
pub(crate) struct Meter<E, F> {
    budget: WorkBudget,
    gate: F,
    failure: Option<E>,
}
impl<E, F: FnMut(usize) -> Result<(), E>> Meter<E, F> {
    pub(crate) fn new(units: usize, gate: F) -> Self {
        Self {
            budget: WorkBudget::new(units),
            gate,
            failure: None,
        }
    }
    pub(crate) fn refuse(&mut self, error: E) -> BeaconError {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        BeaconError::Cancelled
    }
    pub(crate) fn finish<R, T>(self, result: Result<T, BeaconError>) -> Result<T, Error<R, E>> {
        match self.failure {
            Some(error) => Err(Error::Interrupted(error)),
            None => result.map_err(Error::Index),
        }
    }
}
impl<E, F: FnMut(usize) -> Result<(), E>> WorkControl for Meter<E, F> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        if self.failure.is_some() {
            return Err(BeaconError::Cancelled);
        }
        if let Err(error) = (self.gate)(units) {
            return Err(self.refuse(error));
        }
        self.budget.charge(units)
    }
}

pub(crate) struct SharedWork<'a, W>(pub(crate) &'a RefCell<W>);
impl<W: WorkControl> WorkControl for SharedWork<'_, W> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.0.borrow_mut().charge(units)
    }
}

/// Private common preparation for privileged and Warden readers. Admitting a
/// row and a property are separate operations; neither callback sees copied
/// values. The iterator projects one borrowed winner per builder poll.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    config: IndexConfig,
    work: &RefCell<impl WorkControl>,
    mut admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    mut label_allowed: impl FnMut(LabelId) -> bool,
    mut property_allowed: impl FnMut(PropertyKeyId) -> bool,
) -> Result<BeaconIndex, BeaconError> {
    let mut scratch = 0usize;
    let mut source_work = |event| {
        work.borrow_mut().charge(1)?;
        if matches!(event, SourceEvent::ScratchEntry) {
            if scratch == options.policy.max_source_scratch {
                return Err(BeaconError::ResourceLimit {
                    resource: "source scratch entries",
                    limit: options.policy.max_source_scratch,
                });
            }
            scratch += 1;
        }
        Ok(())
    };
    let mut rows = Vec::new();
    source::visit_vertices(&snapshot.patches, at, &mut source_work, |row, control| {
        control(SourceEvent::Work)?;
        // Charge original-label examination, including authorization clauses.
        work.borrow_mut().charge(row.labels.len())?;
        if !admit(row)? {
            return Ok(());
        }
        if options
            .vertex_label
            .is_some_and(|label| !label_allowed(label) || row.labels.binary_search(&label).is_err())
        {
            return Ok(());
        }
        let limit = options.policy.max_staging_rows.min(config.max_documents);
        if rows.len() == limit {
            return Err(BeaconError::ResourceLimit {
                resource: "staged vertices",
                limit,
            });
        }
        rows.try_reserve(1)
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "vertex staging allocation",
                limit,
            })?;
        rows.push(row);
        Ok(())
    })?;
    let projected = rows.into_iter().map(|row| {
        options.projection.project(
            row.vid,
            &config,
            |key| {
                // Do not inspect an unauthorized property's value or its type.
                if !property_allowed(key) {
                    return None;
                }
                row.props
                    .binary_search_by_key(&key, |(key, _)| *key)
                    .ok()
                    .map(|slot| &row.props[slot].1)
            },
            &mut SharedWork(work),
        )
    });
    BeaconIndex::try_build(config.clone(), projected, &mut SharedWork(work))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: Search<'_>,
    work: &RefCell<impl WorkControl>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
) -> Result<Rows, BeaconError> {
    work.borrow_mut().charge(1)?;
    let config = options.config_for(query)?;
    query.validate(&config, &mut SharedWork(work))?;
    let index = build(
        snapshot,
        at,
        options,
        config,
        work,
        admit,
        label_allowed,
        property_allowed,
    )?;
    let rows = query.execute(&index.snapshot(), &mut SharedWork(work))?;
    // Even empty and native zero-k paths cannot skip final live admission.
    work.borrow_mut().charge(1)?;
    Ok(rows)
}

impl<V: Vfs + Clone> Database<V> {
    /// Search one admitted database generation. None selects its frontier;
    /// as_of selects that generation's historical winners, including updates,
    /// label changes and tombstones. No asynchronous stale index is consulted.
    /// This is a privileged API, like read_session; use beacon_search_authorized
    /// when serving capability holders. No graph/value/clock is fabricated.
    pub fn beacon_search(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            self.read_session()
                .map_err(Error::Read)?
                .beacon_search(cx, options, query)
        })
    }
}

impl EmbeddedReadView {
    /// Reuse this pinned generation, not the writer's current head. A later
    /// writer publication/drop cannot change these results. Construction is
    /// currently per-call and in-core; exact vector mode is exact only for the
    /// explicitly projected f32 coordinates, and ANN remains approximate.
    pub fn beacon_search(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let at = options.as_of.unwrap_or(self.frontier());
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate(
                &self.snapshot,
                at,
                options,
                query,
                &work,
                |_| Ok(true),
                |_| true,
                |_| true,
            );
            work.into_inner().finish(result)
        })
    }
}
