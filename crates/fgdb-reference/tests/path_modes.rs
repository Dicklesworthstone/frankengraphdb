//! Laws of the GQL path modes (ISO/IEC 39075; plan:657 names WALK/TRAIL/
//! ACYCLIC/SIMPLE).
//!
//! The four modes form a CONTAINMENT CHAIN:
//!
//! ```text
//!   Acyclic  ⊆  Simple  ⊆  Trail  ⊆  Walk
//! ```
//!
//! That chain is the strongest available law, because it constrains the modes
//! *against each other* rather than each in isolation. But nesting alone is not
//! enough: an implementation that COLLAPSED two adjacent modes into one would
//! satisfy it perfectly. So every adjacent pair also gets a DISTINGUISHING
//! graph — a path admitted by the looser mode and refused by the tighter one:
//!
//!   * Trail vs Simple — a figure-eight, where a vertex recurs mid-path but no
//!     edge does.
//!   * Simple vs Acyclic — a triangle closed back to its start, the one case
//!     Simple permits and Acyclic does not.
//!
//! Without those two graphs, three of the four modes could be aliases.

use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::{PathMode, ReferenceGraph};
use fgdb_types::{CanonicalScalar, EId, VId};

const REL: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

const ALL_MODES: [PathMode; 4] = [
    PathMode::Acyclic,
    PathMode::Simple,
    PathMode::Trail,
    PathMode::Walk,
];

fn vertex(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(vid as i64))],
        valid_time: None,
    }
}

fn edge_of(eid: u128, src: u128, dst: u128, relation: RelationId) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
    edge_of(eid, src, dst, REL)
}

fn build(vertices: &[u128], edges: &[(u128, u128, u128)]) -> ReferenceGraph {
    let mut g = ReferenceGraph::new();
    for v in vertices {
        g.apply_row(&vertex(*v)).expect("vertex applies");
    }
    for (eid, src, dst) in edges {
        g.apply_row(&edge(*eid, *src, *dst)).expect("edge applies");
    }
    g
}

fn edge_paths(
    g: &ReferenceGraph,
    from: u128,
    to: u128,
    mode: PathMode,
    hops: usize,
) -> Vec<Vec<u128>> {
    g.paths(VId(from), VId(to), REL, mode, hops)
        .into_iter()
        .map(|p| p.edge_ids().into_iter().map(|e| e.0).collect())
        .collect()
}

// ---------------------------------------------------------------------------
// The basics
// ---------------------------------------------------------------------------

/// A straight line has exactly one path, in every mode.
#[test]
fn a_simple_chain_has_one_path_in_every_mode() {
    let g = build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3)]);
    for mode in ALL_MODES {
        assert_eq!(
            edge_paths(&g, 1, 3, mode, 8),
            vec![vec![10, 11]],
            "{mode:?} on a chain"
        );
    }
}

/// `max_hops` bounds the search, and a path needing more hops is absent rather
/// than truncated — a truncated path is not a path.
#[test]
fn max_hops_bounds_the_search() {
    let g = build(&[1, 2, 3, 4], &[(10, 1, 2), (11, 2, 3), (12, 3, 4)]);
    assert_eq!(
        edge_paths(&g, 1, 4, PathMode::Walk, 2),
        Vec::<Vec<u128>>::new()
    );
    assert_eq!(
        edge_paths(&g, 1, 4, PathMode::Walk, 3),
        vec![vec![10, 11, 12]]
    );
}

/// Relations are not conflated: a path over REL cannot traverse an OTHER edge.
#[test]
fn a_path_stays_within_its_relation() {
    let mut g = build(&[1, 2, 3], &[(10, 1, 2)]);
    g.apply_row(&edge_of(11, 2, 3, OTHER)).expect("applies");
    assert_eq!(
        edge_paths(&g, 1, 3, PathMode::Walk, 8),
        Vec::<Vec<u128>>::new(),
        "the OTHER-relation hop must not be traversable under REL"
    );
}

/// A missing endpoint yields nothing rather than panicking or inventing a path.
#[test]
fn absent_endpoints_yield_nothing() {
    let g = build(&[1, 2], &[(10, 1, 2)]);
    assert!(g.paths(VId(1), VId(99), REL, PathMode::Walk, 4).is_empty());
    assert!(g.paths(VId(99), VId(2), REL, PathMode::Walk, 4).is_empty());
}

// ---------------------------------------------------------------------------
// THE CHAIN: Acyclic ⊆ Simple ⊆ Trail ⊆ Walk
// ---------------------------------------------------------------------------

/// The nesting must hold on every graph tested here, not just a chosen one.
/// Checked as set containment on the actual path sets, so a mode that admits
/// something a looser mode rejects fails immediately.
#[test]
fn the_modes_nest_on_every_shape() {
    let shapes: Vec<(&str, ReferenceGraph, u128, u128)> = vec![
        ("chain", build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3)]), 1, 3),
        (
            "diamond",
            build(
                &[1, 2, 3, 4],
                &[(10, 1, 2), (11, 1, 3), (12, 2, 4), (13, 3, 4)],
            ),
            1,
            4,
        ),
        (
            "triangle",
            build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3), (12, 3, 1)]),
            1,
            1,
        ),
        (
            "figure-eight",
            build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 1), (12, 1, 3)]),
            1,
            3,
        ),
        ("self-loop", build(&[1, 2], &[(10, 1, 1), (11, 1, 2)]), 1, 2),
    ];

    for (name, g, from, to) in shapes {
        let acyclic = edge_paths(&g, from, to, PathMode::Acyclic, 6);
        let simple = edge_paths(&g, from, to, PathMode::Simple, 6);
        let trail = edge_paths(&g, from, to, PathMode::Trail, 6);
        let walk = edge_paths(&g, from, to, PathMode::Walk, 6);

        for path in &acyclic {
            assert!(
                simple.contains(path),
                "{name}: Acyclic ⊄ Simple on {path:?}"
            );
        }
        for path in &simple {
            assert!(trail.contains(path), "{name}: Simple ⊄ Trail on {path:?}");
        }
        for path in &trail {
            assert!(walk.contains(path), "{name}: Trail ⊄ Walk on {path:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The distinguishing graphs — without these, modes could be aliases
// ---------------------------------------------------------------------------

/// SIMPLE vs ACYCLIC. A triangle closed back to its start: `1→2→3→1`. Every
/// vertex is distinct except that the first equals the last, which is precisely
/// what Simple permits and Acyclic forbids.
#[test]
fn simple_admits_a_closed_walk_that_acyclic_refuses() {
    let g = build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3), (12, 3, 1)]);

    assert_eq!(
        edge_paths(&g, 1, 1, PathMode::Simple, 6),
        vec![vec![10, 11, 12]],
        "Simple admits the closed walk"
    );
    assert_eq!(
        edge_paths(&g, 1, 1, PathMode::Acyclic, 6),
        Vec::<Vec<u128>>::new(),
        "Acyclic refuses it — the two modes are NOT the same"
    );
}

/// TRAIL vs SIMPLE. A figure-eight: `1→2`, `2→1` on a different edge, then
/// `1→3`. The path `1→2→1→3` recurs at vertex 1 MID-PATH while repeating no
/// edge, so Trail admits it and Simple does not.
#[test]
fn trail_admits_a_revisited_vertex_that_simple_refuses() {
    let g = build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 1), (12, 1, 3)]);

    let trail = edge_paths(&g, 1, 3, PathMode::Trail, 6);
    assert!(
        trail.contains(&vec![10, 11, 12]),
        "Trail admits revisiting vertex 1 via a different edge; got {trail:?}"
    );

    let simple = edge_paths(&g, 1, 3, PathMode::Simple, 6);
    assert_eq!(
        simple,
        vec![vec![12]],
        "Simple admits only the direct hop — a MID-PATH return to the start is a \
         repeated vertex, not a permitted closure"
    );
}

/// WALK vs TRAIL. With a cycle, Walk may traverse the same edge repeatedly and
/// therefore returns strictly more paths than Trail, which is bounded by the
/// edge count.
#[test]
fn walk_admits_repeated_edges_that_trail_refuses() {
    let g = build(&[1, 2], &[(10, 1, 2), (11, 2, 1)]);

    let walk = edge_paths(&g, 1, 2, PathMode::Walk, 5);
    let trail = edge_paths(&g, 1, 2, PathMode::Trail, 5);

    assert!(
        walk.contains(&vec![10, 11, 10]),
        "Walk may reuse edge 10; got {walk:?}"
    );
    assert!(
        !trail.contains(&vec![10, 11, 10]),
        "Trail must not reuse edge 10; got {trail:?}"
    );
    assert!(
        walk.len() > trail.len(),
        "with a cycle Walk is strictly larger: {} vs {}",
        walk.len(),
        trail.len()
    );
}

/// A self-loop is the smallest repeated-edge and repeated-vertex case at once,
/// and it separates all four modes cleanly.
#[test]
fn a_self_loop_separates_the_modes() {
    let g = build(&[1, 2], &[(10, 1, 1), (11, 1, 2)]);

    // Acyclic and Simple cannot take the loop at all: it returns to 1, which is
    // both the start and not the target.
    assert_eq!(edge_paths(&g, 1, 2, PathMode::Acyclic, 4), vec![vec![11]]);
    assert_eq!(edge_paths(&g, 1, 2, PathMode::Simple, 4), vec![vec![11]]);
    // Trail may take it once.
    let trail = edge_paths(&g, 1, 2, PathMode::Trail, 4);
    assert!(trail.contains(&vec![11]));
    assert!(trail.contains(&vec![10, 11]), "Trail takes the loop once");
    assert!(
        !trail.contains(&vec![10, 10, 11]),
        "but not twice — that repeats edge 10"
    );
    // Walk may take it repeatedly.
    let walk = edge_paths(&g, 1, 2, PathMode::Walk, 4);
    assert!(walk.contains(&vec![10, 10, 11]), "Walk repeats the loop");
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// Path order is a function of the graph and the query, not of search order.
/// Shorter paths first, then lexicographic by edge ids.
#[test]
fn path_order_is_canonical_and_stable() {
    let g = build(
        &[1, 2, 3, 4],
        &[(10, 1, 2), (11, 1, 3), (12, 2, 4), (13, 3, 4)],
    );
    let once = edge_paths(&g, 1, 4, PathMode::Acyclic, 6);
    let twice = edge_paths(&g, 1, 4, PathMode::Acyclic, 6);
    assert_eq!(once, twice, "two runs agree");
    assert_eq!(
        once,
        vec![vec![10, 12], vec![11, 13]],
        "canonical order: by length, then by edge ids"
    );

    // And lengths are non-decreasing across the whole result.
    let g2 = build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3), (12, 1, 3)]);
    let paths = g2.paths(VId(1), VId(3), REL, PathMode::Walk, 5);
    let lengths: Vec<usize> = paths.iter().map(|p| p.hop_count()).collect();
    assert!(
        lengths.windows(2).all(|w| w[0] <= w[1]),
        "lengths must be non-decreasing: {lengths:?}"
    );
}

/// The reported vertex sequence agrees with the edges traversed — a path that
/// described a different route than it walked would make every downstream
/// witness meaningless.
#[test]
fn a_paths_vertices_agree_with_its_edges() {
    let g = build(&[1, 2, 3], &[(10, 1, 2), (11, 2, 3)]);
    let paths = g.paths(VId(1), VId(3), REL, PathMode::Acyclic, 4);
    let path = paths.first().expect("one path");

    assert_eq!(path.vertices(), vec![VId(1), VId(2), VId(3)]);
    assert_eq!(path.edge_ids(), vec![EId(10), EId(11)]);
    assert_eq!(path.start, VId(1));
    assert_eq!(path.end(), VId(3));
    assert_eq!(path.hop_count(), 2);

    // Each step's edge really connects the previous vertex to the step's vertex.
    let mut previous = path.start;
    for step in &path.steps {
        let e = g.edge(step.edge).expect("edge exists");
        assert_eq!(e.src, previous, "step edge starts where the path was");
        assert_eq!(e.dst, step.to, "step edge ends where the path says");
        previous = step.to;
    }
}
