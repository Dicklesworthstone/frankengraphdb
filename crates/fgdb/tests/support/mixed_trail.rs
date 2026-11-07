//! Finite products for mixed-hop collection integration laws. This oracle
//! uses no production matcher, cursor, path compiler, or authorization mask.

use fgdb_gql::algebra::{GraphPath, GraphValue};
use fgdb_types::{CanonicalScalar, EId, VId};

pub(super) fn routes(
    edges: &[(EId, VId, VId)],
    anchor: VId,
    direction: usize,
    variable: usize,
    atoms: usize,
    bounds: (usize, usize),
) -> Vec<GraphPath> {
    let bounds: Vec<_> = (0..atoms)
        .map(|atom| if atom == variable { bounds } else { (1, 1) })
        .collect();
    routes_for_bounds(edges, anchor, direction, &bounds)
}

/// Every atom contributes its own ordered choice. Equal flattened paths from
/// different segment boundaries remain separate occurrences in the product.
pub(super) fn routes_for_bounds(
    edges: &[(EId, VId, VId)],
    anchor: VId,
    direction: usize,
    bounds: &[(usize, usize)],
) -> Vec<GraphPath> {
    let mut prefixes = vec![(Vec::<(EId, VId)>::new(), anchor)];
    for &(minimum, maximum) in bounds {
        let mut next_prefixes = Vec::new();
        for (prefix, start) in prefixes {
            let mut frontier = vec![(Vec::<(EId, VId)>::new(), start)];
            let mut accepted = if minimum == 0 {
                frontier.clone()
            } else {
                Vec::new()
            };
            // Enumerate this atom by length, then restore prefix-first identity
            // order. Atom boundaries matter: a zero-hop choice precedes every
            // nonempty trail even when its following fixed edge has a larger ID.
            for length in 1..=maximum {
                let mut next = Vec::new();
                for (steps, at) in frontier {
                    for &(edge, source, target) in edges {
                        if prefix.iter().chain(&steps).any(|&(used, _)| used == edge) {
                            continue;
                        }
                        let to = match direction {
                            0 if source == at => target,
                            1 if target == at => source,
                            2 if source == at => target,
                            2 if target == at => source,
                            _ => continue,
                        };
                        let mut extended = steps.clone();
                        extended.push((edge, to));
                        next.push((extended, to));
                    }
                }
                if length >= minimum {
                    accepted.extend(next.iter().cloned());
                }
                frontier = next;
            }
            accepted.sort_unstable_by(|left, right| {
                left.0
                    .iter()
                    .map(|step| step.0)
                    .cmp(right.0.iter().map(|step| step.0))
            });
            for (steps, endpoint) in accepted {
                let mut combined = prefix.clone();
                combined.extend(steps);
                next_prefixes.push((combined, endpoint));
            }
        }
        prefixes = next_prefixes;
    }
    prefixes
        .into_iter()
        .map(|(steps, _)| GraphPath::new(anchor, steps.into_boxed_slice()))
        .collect()
}

pub(super) fn endpoint(path: &GraphPath) -> VId {
    path.steps().last().map_or(path.start(), |step| step.1)
}

pub(super) fn values(
    paths: impl IntoIterator<Item = GraphPath>,
    property: impl Fn(VId) -> i64,
) -> GraphValue {
    GraphValue::List(
        paths
            .into_iter()
            .map(|path| {
                let end = endpoint(&path);
                let nodes = std::iter::once(path.start())
                    .chain(path.steps().iter().map(|step| step.1))
                    .collect::<Vec<_>>();
                let edges = path.steps().iter().map(|step| step.0).collect::<Vec<_>>();
                let length = path.steps().len() as i64;
                GraphValue::List(
                    vec![
                        GraphValue::Path(path),
                        GraphValue::Vertices(nodes.into_boxed_slice()),
                        GraphValue::Edges(edges.into_boxed_slice()),
                        GraphValue::Scalar(CanonicalScalar::Int(length)),
                        GraphValue::Vertex(end),
                        GraphValue::Scalar(CanonicalScalar::Int(property(end))),
                        GraphValue::Scalar(CanonicalScalar::Null),
                    ]
                    .into_boxed_slice(),
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    )
}
