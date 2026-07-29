//! Laws of the temporal selector — "what did this graph look like at time T".
//!
//! Two decisions in the implementation are observable and a plausible
//! alternative is wrong for each, so both get laws:
//!
//!   * **`[start, end)` is half-open.** With a closed upper bound two adjacent
//!     periods would both be live at the shared instant, so a value replaced at
//!     an instant would have two simultaneous versions. Swept across the
//!     boundary rather than probed in the middle, because the middle is where
//!     every convention agrees.
//!   * **An edge needs both endpoints live**, not just its own period. The
//!     obvious implementation filters edges by their own period alone and
//!     produces a historical view containing an edge to a vertex that does not
//!     exist at that instant — the dangling edge the non-temporal view refuses
//!     at apply time. A time-travel query that can return a graph the database
//!     would never have accepted is not answering the question it claims to.

use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId, ValidTimePeriod};
use fgdb_reference::ReferenceGraph;
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};

const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

fn period(start: i64, end: Option<i64>) -> ValidTimePeriod {
    ValidTimePeriod {
        start_micros: start,
        end_micros: end,
    }
}

fn vertex(vid: u128, valid: Option<ValidTimePeriod>) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(vid as i64))],
        valid_time: valid,
    }
}

fn edge(eid: u128, src: u128, dst: u128, valid: Option<ValidTimePeriod>) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: valid,
    }
}

fn graph(rows: Vec<DeltaRow>) -> ReferenceGraph {
    let mut g = ReferenceGraph::new();
    for row in rows {
        g.apply_row(&row).expect("row applies");
    }
    g
}

// ---------------------------------------------------------------------------
// The half-open boundary
// ---------------------------------------------------------------------------

/// `[start, end)` swept across the boundary. The instants that matter are
/// `start - 1`, `start`, `end - 1` and `end`; probing only the middle would pass
/// under every convention including the wrong one.
#[test]
fn a_period_is_half_open_at_both_ends() {
    let g = graph(vec![vertex(1, Some(period(100, Some(200))))]);

    assert!(!g.vertex_live_at(VId(1), 99), "before start: not live");
    assert!(g.vertex_live_at(VId(1), 100), "at start: LIVE");
    assert!(g.vertex_live_at(VId(1), 199), "one before end: live");
    assert!(
        !g.vertex_live_at(VId(1), 200),
        "AT END: not live — the upper bound is exclusive"
    );
    assert!(!g.vertex_live_at(VId(1), 201), "after end: not live");
}

/// The reason half-open is the right choice: two adjacent periods must never
/// both be live. A replacement at an instant leaves exactly one version at
/// every instant, which is what makes "the state at T" a function rather than a
/// set.
#[test]
fn adjacent_periods_never_overlap() {
    let g = graph(vec![
        vertex(1, Some(period(0, Some(10)))),
        vertex(2, Some(period(10, Some(20)))),
    ]);

    for micros in [0, 5, 9] {
        assert_eq!(g.vertices_as_of(micros), vec![VId(1)], "at {micros}");
    }
    for micros in [10, 15, 19] {
        assert_eq!(
            g.vertices_as_of(micros),
            vec![VId(2)],
            "at {micros}: the handover instant belongs to the successor alone"
        );
    }
    assert_eq!(g.vertices_as_of(20), Vec::<VId>::new(), "both have ended");
}

/// An unbounded period is live forever after its start; an absent period is
/// live at every instant. Absence is not "valid for no time" — valid time is an
/// optional assertion about when a fact holds, and a fact carrying no such
/// assertion is not thereby time-limited to nothing.
#[test]
fn unbounded_and_absent_periods_read_correctly() {
    let g = graph(vec![vertex(1, None), vertex(2, Some(period(50, None)))]);

    assert_eq!(g.vertices_as_of(i64::MIN), vec![VId(1)]);
    assert_eq!(g.vertices_as_of(49), vec![VId(1)]);
    assert_eq!(g.vertices_as_of(50), vec![VId(1), VId(2)]);
    assert_eq!(g.vertices_as_of(i64::MAX), vec![VId(1), VId(2)]);
}

// ---------------------------------------------------------------------------
// THE TRAP: temporal referential integrity
// ---------------------------------------------------------------------------

/// An edge whose own period is live must STILL be invisible when an endpoint is
/// not. This is the law a naive implementation breaks, and breaking it produces
/// a historical graph the database would have refused to accept at apply time.
#[test]
fn an_edge_is_invisible_when_an_endpoint_is_not_live() {
    // Vertex 1 lives [0,100); vertex 2 lives forever; the edge lives forever.
    let g = graph(vec![
        vertex(1, Some(period(0, Some(100)))),
        vertex(2, None),
        edge(10, 1, 2, None),
    ]);

    // While both endpoints are live, the edge is visible.
    assert!(g.edge_live_at(EId(10), 50));
    assert_eq!(g.edges_as_of(50), vec![EId(10)]);
    assert_eq!(g.neighbours_as_of(VId(1), REL, 50), vec![VId(2)]);

    // After vertex 1 ends, the edge's OWN period is still live — and the edge
    // must be gone anyway.
    assert!(
        !g.edge_live_at(EId(10), 100),
        "an edge to a vertex that does not exist at T is not part of the graph at T"
    );
    assert_eq!(g.edges_as_of(100), Vec::<EId>::new());
    assert_eq!(g.vertices_as_of(100), vec![VId(2)]);

    // And the traversal agrees with the edge set — they cannot disagree about
    // what exists.
    assert_eq!(g.neighbours_as_of(VId(1), REL, 100), Vec::<VId>::new());
    assert_eq!(g.neighbours_as_of(VId(2), REL, 100), Vec::<VId>::new());
}

/// Either endpoint suffices to hide the edge, tested on both sides — a check
/// that only looked at `src` would pass the previous test.
#[test]
fn either_endpoint_ending_hides_the_edge() {
    let dst_ends = graph(vec![
        vertex(1, None),
        vertex(2, Some(period(0, Some(100)))),
        edge(10, 1, 2, None),
    ]);
    assert!(dst_ends.edge_live_at(EId(10), 99));
    assert!(
        !dst_ends.edge_live_at(EId(10), 100),
        "the DESTINATION ending must hide the edge too"
    );

    let src_ends = graph(vec![
        vertex(1, Some(period(0, Some(100)))),
        vertex(2, None),
        edge(10, 1, 2, None),
    ]);
    assert!(src_ends.edge_live_at(EId(10), 99));
    assert!(!src_ends.edge_live_at(EId(10), 100));
}

/// The edge's own period still matters — endpoint liveness is an additional
/// requirement, not a replacement. Without this, an implementation that checked
/// only the endpoints would pass every test above.
#[test]
fn the_edges_own_period_is_still_required() {
    let g = graph(vec![
        vertex(1, None),
        vertex(2, None),
        edge(10, 1, 2, Some(period(10, Some(20)))),
    ]);

    assert!(!g.edge_live_at(EId(10), 9), "before the edge's own start");
    assert!(g.edge_live_at(EId(10), 10));
    assert!(g.edge_live_at(EId(10), 19));
    assert!(
        !g.edge_live_at(EId(10), 20),
        "the edge's own end is exclusive"
    );

    // The endpoints are live throughout, so only the edge's period explains it.
    for micros in [9, 20] {
        assert_eq!(g.vertices_as_of(micros), vec![VId(1), VId(2)]);
        assert_eq!(g.edges_as_of(micros), Vec::<EId>::new());
    }
}

/// A traversal from a vertex that is not live yields nothing, rather than
/// reporting neighbours of a vertex that does not exist.
#[test]
fn traversing_from_a_dead_vertex_yields_nothing() {
    let g = graph(vec![
        vertex(1, Some(period(0, Some(50)))),
        vertex(2, None),
        edge(10, 1, 2, None),
    ]);
    assert_eq!(g.neighbours_as_of(VId(1), REL, 10), vec![VId(2)]);
    assert_eq!(g.neighbours_as_of(VId(1), REL, 50), Vec::<VId>::new());
}

// ---------------------------------------------------------------------------
// Agreement with the non-temporal view
// ---------------------------------------------------------------------------

/// With no periods anywhere, every temporal selector must agree with its
/// non-temporal counterpart at every instant. Otherwise the temporal path is a
/// second semantics rather than a refinement of one.
#[test]
fn without_periods_the_temporal_view_equals_the_plain_view() {
    let g = graph(vec![
        vertex(1, None),
        vertex(2, None),
        vertex(3, None),
        edge(10, 1, 2, None),
        edge(11, 1, 3, None),
    ]);

    for micros in [i64::MIN, -1, 0, 1, 1_000, i64::MAX] {
        assert_eq!(
            g.vertices_as_of(micros).len(),
            g.vertex_count(),
            "vertices at {micros}"
        );
        assert_eq!(
            g.edges_as_of(micros).len(),
            g.edge_count(),
            "edges at {micros}"
        );
        assert_eq!(
            g.neighbours_as_of(VId(1), REL, micros),
            g.neighbours(VId(1), REL),
            "neighbours at {micros}"
        );
    }
}

/// A ValidTime row applied after creation moves the element's visibility, so the
/// selector reads applied state rather than only what a create declared.
#[test]
fn a_valid_time_row_moves_what_the_selector_sees() {
    let mut g = graph(vec![vertex(1, None)]);
    assert!(g.vertex_live_at(VId(1), 0));

    g.apply_row(&DeltaRow::ValidTime {
        elem: ElementId::Vertex(VId(1)),
        contract_id: ObjectId([0x80; 32]),
        before: None,
        after: Some(period(100, Some(200))),
    })
    .expect("applies");

    assert!(!g.vertex_live_at(VId(1), 99), "the new period took effect");
    assert!(g.vertex_live_at(VId(1), 100));
    assert!(!g.vertex_live_at(VId(1), 200));
}
