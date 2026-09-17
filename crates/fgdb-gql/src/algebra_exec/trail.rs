//! Identity-sensitive expansion choices feeding the same binding continuation.

use super::{EId, GlaExecutionEvent, GraphPath, GraphWalkSearch, VId};
use crate::{GraphTrailCursor, GraphWalkBounds};
use std::collections::BTreeMap;

pub(super) enum IdentifiedExpansion<'a> {
    Path(crate::walk::GraphPathCursor<'a>),
    Trail {
        cursor: GraphTrailCursor<'a>,
        capture: bool,
    },
}

impl<'a> IdentifiedExpansion<'a> {
    pub(super) fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        search: GraphWalkSearch,
        adjacency: Option<&'a BTreeMap<VId, Vec<(EId, VId)>>>,
        capture: bool,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        if search == GraphWalkSearch::Trail {
            Ok(Self::Trail {
                cursor: GraphTrailCursor::new(source, bounds, adjacency, control)?,
                capture,
            })
        } else {
            crate::walk::GraphPathCursor::new(source, bounds, search, adjacency, control)
                .map(Self::Path)
        }
    }

    pub(super) fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(VId, Option<GraphPath>)>, E> {
        let path = match self {
            Self::Path(cursor) => cursor.next_with_control(control)?,
            Self::Trail {
                cursor,
                capture: true,
            } => cursor.next_with_control(control)?,
            Self::Trail {
                cursor,
                capture: false,
            } => {
                // Edge membership remains in the live frontier; endpoint-only
                // output need not clone any of that path into a GraphPath.
                return cursor
                    .next_endpoint_with_control(control)
                    .map(|value| value.map(|endpoint| (endpoint, None)));
            }
        };
        Ok(path.map(|path| {
            let endpoint = path.steps().last().map_or(path.start(), |step| step.1);
            (endpoint, Some(path))
        }))
    }
}
