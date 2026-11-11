//! Temporary opposite incidence access for an already admitted projection.
//!
//! The selected identities, masks, cut and reduction laws are borrowed from
//! the original graph. Reversal preserves each parallel group's EId order,
//! finite reduced weight and selected endpoints. It needs neither another
//! projection digest nor another copy of the selected vertex directory.
//!
//! Only a missing compressed incoming locator index and, when admission could
//! not retain incoming degrees, one exact degree vector are owned here. The
//! original graph remains the sole result/certificate identity.

use super::construction::{BuildControl, Schedule, immediately};
use super::*;

/// The two kernel faces share this private row protocol. A borrowed face has
/// no digest, snapshot facade, source getter or independently reusable graph.
pub(crate) trait SealedRowSource {
    fn row_view(&self) -> SealedRowView<'_>;
}

#[derive(Clone, Copy)]
pub(crate) struct SealedRowView<'a> {
    graph: &'a SealedGraphView,
    config: &'a SealedProjectionSpec,
    incoming: Option<&'a SealedIncomingIndex>,
    degrees: &'a [usize],
    incoming_degrees: Option<&'a [usize]>,
}

impl SealedRowSource for SealedGraphView {
    fn row_view(&self) -> SealedRowView<'_> {
        SealedRowView {
            graph: self,
            config: &self.0.config,
            incoming: self.0.incoming.as_ref(),
            degrees: &self.0.degrees,
            incoming_degrees: self.incoming_degrees(),
        }
    }
}

impl SealedRowSource for SealedRowView<'_> {
    fn row_view(&self) -> SealedRowView<'_> {
        *self
    }
}

impl<'a> SealedRowView<'a> {
    pub(crate) fn node_count(self) -> usize {
        self.graph.node_count()
    }

    pub(crate) fn degree(self, source: usize) -> Option<usize> {
        self.degrees.get(source).copied()
    }

    pub(crate) fn incoming_degrees(self) -> Option<&'a [usize]> {
        self.incoming_degrees
    }

    pub(crate) fn is_undirected(self) -> bool {
        self.config.projection.directedness == Directedness::Undirected
    }

    pub(crate) fn has_unit_weights(self) -> bool {
        self.graph.has_unit_weights()
    }

    pub(crate) fn has_nonnegative_weights(self) -> bool {
        self.graph.has_nonnegative_weights()
    }

    pub(crate) fn retained_row_incidence_bound(
        self,
        ordinal: usize,
    ) -> Result<usize, SealedProjectionError> {
        let vertex = self
            .graph
            .vertex_id(ordinal)
            .ok_or(SealedProjectionError::UnknownOrdinal(ordinal))?;
        let relation = self
            .config
            .selection
            .relations
            .single()
            .ok_or(SealedProjectionError::RelationRequired)?;
        if !self.graph.0.mask.relation_visible {
            return Ok(0);
        }
        let direction = self.config.projection.directedness;
        let outgoing = if direction == Directedness::Reversed {
            0
        } else {
            self.graph.0.partition.retained_row_len(vertex, relation)
        };
        let incoming = if direction == Directedness::Directed {
            0
        } else {
            self.incoming
                .ok_or(SealedProjectionError::UnsupportedDirectedness(direction))?
                .retained_row_len(vertex, relation)
        };
        add(outgoing, incoming)
    }

    pub(crate) fn neighbor_cursor_controlled(
        self,
        cx: &Control<'_, impl Guard>,
        source: usize,
        lower_bound: Option<VId>,
    ) -> Result<SealedNeighborCursor<'a>, SealedProjectionError> {
        // Both ordinary and opposite degrees come from the same completely
        // admitted selected rows. Empty rows still retain their live opening
        // and first-pull checks, without decoding excluded or retired history.
        if self.degree(source) == Some(0) {
            return SealedNeighborCursor::empty(
                cx,
                &self.graph.0.vertices,
                self.graph.0.vertex_prefix.first(),
                self.config,
                self.graph.0.mask,
                source,
            );
        }
        SealedNeighborCursor::open(
            cx,
            &self.graph.0.partition,
            self.incoming,
            &self.graph.0.vertices,
            self.graph.0.vertex_prefix.first(),
            self.config,
            self.graph.0.mask,
            source,
            lower_bound,
        )
    }
}

enum Degrees<'a> {
    Borrowed(&'a [usize]),
    Owned(Vec<usize>),
}

impl Degrees<'_> {
    fn as_slice(&self) -> &[usize] {
        match self {
            Self::Borrowed(values) => values,
            Self::Owned(values) => values,
        }
    }
}

/// Private kernel workspace. Borrowed graph storage is already charged to the
/// original projection; only newly owned backing and locator construction peak
/// are charged to the executing kernel. This handle cannot outlive that graph.
pub(crate) struct SealedOppositeFace<'a> {
    graph: &'a SealedGraphView,
    config: SealedProjectionSpec,
    incoming: Option<SealedIncomingIndex>,
    degrees: Degrees<'a>,
    workspace_bytes: usize,
}

impl SealedRowSource for SealedOppositeFace<'_> {
    fn row_view(&self) -> SealedRowView<'_> {
        SealedRowView {
            graph: self.graph,
            config: &self.config,
            incoming: self.incoming.as_ref(),
            degrees: self.degrees.as_slice(),
            incoming_degrees: Some(&self.graph.0.degrees),
        }
    }
}

impl SealedOppositeFace<'_> {
    pub(crate) fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub(crate) fn charged_workspace_bytes(&self) -> usize {
        self.workspace_bytes
    }

    pub(crate) fn retained_row_incidence_bound(
        &self,
        ordinal: usize,
    ) -> Result<usize, SealedProjectionError> {
        self.row_view().retained_row_incidence_bound(ordinal)
    }
}

impl SealedGraphView {
    /// Borrow the admitted selection and transpose its exact degree proof.
    /// No independently reusable projection or new digest is constructed.
    pub(crate) fn reverse_projection_controlled(
        &self,
        cx: &Control<'_, impl Guard>,
        max_workspace_bytes: usize,
    ) -> Result<SealedOppositeFace<'_>, SealedProjectionError> {
        let config = self.opposite_config()?;
        let mut control = BuildControl::new(cx.query, |_| cx.guard());
        immediately(self.construct_opposite(
            &mut control,
            config,
            max_workspace_bytes,
            &mut Schedule::immediate(),
        ))
    }

    pub(crate) async fn reverse_projection_cooperative_controlled<Y, F>(
        &self,
        cx: &Control<'_, impl Guard>,
        max_workspace_bytes: usize,
        quantum: NonZeroUsize,
        yield_now: Y,
    ) -> Result<SealedOppositeFace<'_>, SealedProjectionError>
    where
        Y: FnMut() -> F,
        F: Future<Output = ()>,
    {
        let config = self.opposite_config()?;
        let mut control = BuildControl::new(cx.query, |_| cx.guard());
        self.construct_opposite(
            &mut control,
            config,
            max_workspace_bytes,
            &mut Schedule::cooperative(quantum, yield_now),
        )
        .await
    }

    fn opposite_config(&self) -> Result<SealedProjectionSpec, SealedProjectionError> {
        let mut config = self.0.config;
        config.projection.directedness = match config.projection.directedness {
            Directedness::Directed => Directedness::Reversed,
            Directedness::Reversed => Directedness::Directed,
            Directedness::Undirected => {
                return Err(SealedProjectionError::UnsupportedDirectedness(
                    Directedness::Undirected,
                ));
            }
        };
        Ok(config)
    }

    async fn construct_opposite<Y, F>(
        &self,
        cx: &mut BuildControl<
            '_,
            impl FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>,
        >,
        config: SealedProjectionSpec,
        max_workspace_bytes: usize,
        schedule: &mut Schedule<Y>,
    ) -> Result<SealedOppositeFace<'_>, SealedProjectionError>
    where
        Y: FnMut() -> F,
        F: Future<Output = ()>,
    {
        schedule.check(cx).await?;
        let n = self.node_count();
        let mut workspace_bytes = 0;
        let mut degrees = if let Some(counts) = self.incoming_degrees() {
            Degrees::Borrowed(counts)
        } else if self.adjacency_entry_count() == 0 {
            // Every degree is zero on either face, including a hidden relation.
            Degrees::Borrowed(&self.0.degrees)
        } else {
            let requested = n
                .checked_mul(size_of::<usize>())
                .ok_or(ProjectionError::SizeOverflow)?;
            admit("workspace bytes", requested, max_workspace_bytes)?;
            let counts = reserve::<usize>(n)?;
            workspace_bytes = counts
                .capacity()
                .checked_mul(size_of::<usize>())
                .ok_or(ProjectionError::SizeOverflow)?;
            admit("workspace bytes", workspace_bytes, max_workspace_bytes)?;
            Degrees::Owned(counts)
        };

        // A reversed graph's opposite uses the original outgoing image; its
        // already-owned incoming index is neither rebuilt nor charged again.
        // A directed graph builds only missing authenticated locators, bounded
        // by the SAME whole retained source count admitted by its projection.
        let incoming = if config.projection.directedness == Directedness::Reversed
            && self.0.mask.relation_visible
        {
            let index = schedule
                .incoming(
                    cx,
                    &self.0.partition,
                    IncomingIndexLimits {
                        max_rows: self.scan_incidence_bound(),
                        max_incidences: self.scan_incidence_bound(),
                        max_workspace_bytes: max_workspace_bytes - workspace_bytes,
                    },
                )
                .await?;
            workspace_bytes = add(workspace_bytes, index.stats().charged_workspace_bytes)?;
            admit("workspace bytes", workspace_bytes, max_workspace_bytes)?;
            Some(index)
        } else {
            None
        };

        if let Degrees::Owned(counts) = &mut degrees {
            for _ in 0..n {
                schedule.check(cx).await?;
                counts.push(0);
            }
            // Tight projection budgets can omit the optional incoming degree
            // directory. Count each final reduced neighbor exactly once using
            // the original face, preserving masks, historical cut, loops and
            // parallel policies. Do not reconstruct vertices or hash a graph.
            for source in 0..n {
                schedule.check(cx).await?;
                let mut row = self.neighbor_cursor_controlled(&cx.source(), source, None)?;
                loop {
                    // End the borrowed RefCell-backed source control before
                    // either match arm can suspend the owning build future.
                    let step = {
                        let source_control = cx.source();
                        row.next_budgeted_controlled(&source_control, schedule.fuel())?
                    };
                    match step {
                        SealedScanStep::Item((target, _)) => {
                            schedule.check(cx).await?;
                            let count = counts
                                .get_mut(target)
                                .ok_or(SealedProjectionError::UnknownOrdinal(target))?;
                            *count = add(*count, 1)?;
                        }
                        SealedScanStep::End => break,
                        SealedScanStep::Yield => schedule.refill(cx).await?,
                    }
                }
            }
        }
        schedule.check(cx).await?;
        Ok(SealedOppositeFace {
            graph: self,
            config,
            incoming,
            degrees,
            workspace_bytes,
        })
    }
}
