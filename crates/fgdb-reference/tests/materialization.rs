//! Laws of delta-stream materialization.
//!
//! This is the first place in the codebase where a commit produces a **graph**
//! rather than bytes, so the milestone test is the one that builds a graph out
//! of delta rows and reads vertices, edges and neighbours back out of it.
//!
//! The rest of the file attacks the property that makes the oracle worth
//! having: **before-images are checked, not trusted.** An oracle that repaired
//! what it was handed would make every stream look applicable, which is the
//! same as checking nothing. So each mutating family gets a test that feeds it
//! a row whose before image is a lie, and asserts the row is refused AND the
//! state did not move.

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, EscrowDomainId, LabelId, LogicalDeltaTemplate,
    OperationKey, PropertyKeyId, RelationId, SchemaEpoch, ValidTimePeriod,
};
use fgdb_reference::{ApplyError, ReferenceDatabase, ReferenceGraph};
use fgdb_types::{
    BranchId, CanonicalScalar, CommitSeq, EId, GraphId, LogicalCommandSeq, ObjectId, VId,
};

fn oid(seed: u8) -> ObjectId {
    ObjectId([seed; 32])
}

const REL_KNOWS: RelationId = RelationId(1);
const REL_WORKS_AT: RelationId = RelationId(2);
const LABEL_PERSON: LabelId = LabelId(10);
const PROP_NAME: PropertyKeyId = PropertyKeyId(100);
const PROP_VISITS: PropertyKeyId = PropertyKeyId(101);

fn create_vertex(vid: u128, ordinal: u64, name: &str) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: ordinal,
        labels: vec![LABEL_PERSON],
        props: vec![(
            PROP_NAME,
            CanonicalScalar::Text(
                fgdb_types::CanonicalText::new_ucs_basic(name).expect("bounded text"),
            ),
        )],
        valid_time: None,
    }
}

fn create_edge(eid: u128, ordinal: u64, src: u128, relation: RelationId, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: ordinal,
        src: VId(src),
        relation,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic(value).expect("bounded text"))
}

/// A three-vertex social graph, built the only way this database can build
/// anything: by applying delta rows.
fn social_graph() -> ReferenceGraph {
    let mut graph = ReferenceGraph::new();
    for row in [
        create_vertex(1, 1, "ada"),
        create_vertex(2, 2, "grace"),
        create_vertex(3, 3, "alan"),
        create_edge(10, 4, 1, REL_KNOWS, 2),
        create_edge(11, 5, 1, REL_KNOWS, 3),
        create_edge(12, 6, 2, REL_KNOWS, 3),
    ] {
        graph.apply_row(&row).expect("row applies");
    }
    graph
}

// ---------------------------------------------------------------------------
// THE MILESTONE
// ---------------------------------------------------------------------------

/// A commit stream becomes a graph you can traverse. Everything else in this
/// project is in service of this being true.
#[test]
fn delta_rows_materialize_into_a_traversable_graph() {
    let graph = social_graph();

    assert_eq!(graph.vertex_count(), 3);
    assert_eq!(graph.edge_count(), 3);

    let ada = graph.vertex(VId(1)).expect("ada exists");
    assert_eq!(ada.props.get(&PROP_NAME), Some(&text("ada")));
    assert!(ada.labels.contains(&LABEL_PERSON));

    // The smallest thing that is recognisably a graph query.
    assert_eq!(
        graph.neighbours(VId(1), REL_KNOWS),
        vec![VId(2), VId(3)],
        "ada knows grace and alan"
    );
    assert_eq!(graph.neighbours(VId(3), REL_KNOWS), Vec::<VId>::new());
    assert_eq!(graph.in_edges(VId(3)), vec![EId(11), EId(12)]);
    assert_eq!(graph.out_edges(VId(1)), vec![EId(10), EId(11)]);

    // A relation nobody used yields nothing rather than everything — the
    // failure mode of a filter that is accidentally a no-op.
    assert_eq!(graph.neighbours(VId(1), REL_WORKS_AT), Vec::<VId>::new());
}

/// Replay is a function of the rows alone. Two independent materializations of
/// the same stream must be equal, or "deterministic" means nothing here.
#[test]
fn replaying_the_same_stream_yields_identical_state() {
    assert_eq!(social_graph(), social_graph());
}

/// Version identity is a history identity, not a population counter or a digest
/// of only the currently visible payload.
#[test]
fn version_identity_is_replayable_history_sensitive_and_population_independent() {
    let mut sparse = ReferenceGraph::new();
    sparse
        .apply_row(&create_vertex(1, 1, "ada"))
        .expect("create applies");
    let initial = sparse.vertex(VId(1)).expect("vertex exists").version;
    let update = DeltaRow::Property {
        elem: ElementId::Vertex(VId(1)),
        property: PROP_NAME,
        before: Some(text("ada")),
        after: Some(text("ada-updated")),
    };
    sparse.apply_row(&update).expect("update applies");
    let successor = sparse.vertex(VId(1)).expect("vertex exists").version;

    let mut populated = ReferenceGraph::new();
    populated
        .apply_row(&create_vertex(1, 1, "ada"))
        .expect("same create applies");
    populated
        .apply_row(&create_vertex(2, 2, "unrelated"))
        .expect("unrelated create applies");
    populated.apply_row(&update).expect("same update applies");
    assert_eq!(
        populated.vertex(VId(1)).expect("vertex exists").version,
        successor,
        "unrelated population must not enter an element version identity"
    );

    sparse
        .apply_row(&DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PROP_NAME,
            before: Some(text("ada-updated")),
            after: Some(text("ada")),
        })
        .expect("return update applies");
    assert_eq!(
        sparse
            .vertex(VId(1))
            .expect("vertex exists")
            .props
            .get(&PROP_NAME),
        Some(&text("ada"))
    );
    assert_ne!(
        sparse.vertex(VId(1)).expect("vertex exists").version,
        initial,
        "returning to the same payload is still a later system-time version"
    );
}

// ---------------------------------------------------------------------------
// Before-images are checked, not trusted
// ---------------------------------------------------------------------------

/// A property row whose before image is a lie must be refused, and the state
/// must not move. This is the law the whole oracle rests on.
#[test]
fn a_property_row_with_a_false_before_image_is_refused() {
    let mut graph = social_graph();
    let before = graph.clone();

    let lie = DeltaRow::Property {
        elem: ElementId::Vertex(VId(1)),
        property: PROP_NAME,
        before: Some(text("NOT-ada")),
        after: Some(text("ada-updated")),
    };
    let result = graph.apply_row(&lie);

    assert!(
        matches!(result, Err(ApplyError::PropertyBeforeMismatch { .. })),
        "got {result:?}"
    );
    assert_eq!(
        graph, before,
        "a refused row must leave the state exactly as it was"
    );

    // And the truthful version of the same row applies.
    let truth = DeltaRow::Property {
        elem: ElementId::Vertex(VId(1)),
        property: PROP_NAME,
        before: Some(text("ada")),
        after: Some(text("ada-updated")),
    };
    graph.apply_row(&truth).expect("truthful row applies");
    assert_eq!(
        graph.vertex(VId(1)).expect("ada").props.get(&PROP_NAME),
        Some(&text("ada-updated"))
    );
}

#[test]
fn a_label_row_with_a_false_before_image_is_refused() {
    let mut graph = social_graph();
    let before = graph.clone();
    let result = graph.apply_row(&DeltaRow::LabelMembership {
        vid: VId(1),
        label: LABEL_PERSON,
        before: false, // it is actually present
        after: false,
    });
    assert!(
        matches!(
            result,
            Err(ApplyError::LabelBeforeMismatch {
                declared: false,
                actual: true,
                ..
            })
        ),
        "got {result:?}"
    );
    assert_eq!(graph, before);
}

/// THE CASCADE LAW. A vertex deletion declares the incident edges it retires,
/// and that set must be EXACTLY right. Both directions are tested because they
/// fail differently: too few leaves a dangling edge, too many claims a
/// retirement that never happened.
#[test]
fn a_vertex_deletion_must_declare_its_exact_cascade_image() {
    let mut graph = social_graph();
    let version = graph.vertex(VId(1)).expect("ada exists").version;
    let before = graph.clone();

    // Ada is incident to edges 10 and 11.
    let too_few = DeltaRow::DeleteVertex {
        vid: VId(1),
        before_version: version,
        sorted_retired_incident_edges: vec![EId(10)],
    };
    assert!(
        matches!(
            graph.apply_row(&too_few),
            Err(ApplyError::CascadeImageMismatch { .. })
        ),
        "an under-declared cascade would leave a dangling edge"
    );
    assert_eq!(graph, before);

    let too_many = DeltaRow::DeleteVertex {
        vid: VId(1),
        before_version: version,
        sorted_retired_incident_edges: vec![EId(10), EId(11), EId(12)],
    };
    assert!(
        matches!(
            graph.apply_row(&too_many),
            Err(ApplyError::CascadeImageMismatch { .. })
        ),
        "an over-declared cascade claims a retirement that never happened"
    );
    assert_eq!(graph, before);

    // Exactly right applies, and takes the edges with it.
    let exact = DeltaRow::DeleteVertex {
        vid: VId(1),
        before_version: version,
        sorted_retired_incident_edges: vec![EId(10), EId(11)],
    };
    graph.apply_row(&exact).expect("exact cascade applies");
    assert!(graph.vertex(VId(1)).is_none());
    assert!(graph.edge(EId(10)).is_none());
    assert!(graph.edge(EId(11)).is_none());
    assert!(
        graph.edge(EId(12)).is_some(),
        "an unrelated edge must survive"
    );
    assert_eq!(graph.edge_count(), 1);
}

/// Delete effects are compare-and-set operations over complete element
/// versions. A stale identity must refuse before either the element or its
/// cascade moves, even when every other before-image is exact.
#[test]
fn stale_delete_versions_are_refused_without_moving_state() {
    let mut graph = social_graph();
    let settled = graph.clone();

    let mut stale_edge_version = graph.edge(EId(10)).expect("edge exists").version;
    stale_edge_version.0[0] ^= 0x01;
    let edge_result = graph.apply_row(&DeltaRow::DeleteEdge {
        eid: EId(10),
        before_version: stale_edge_version,
    });
    assert!(matches!(
        edge_result,
        Err(ApplyError::ElementVersionMismatch {
            elem: ElementId::Edge(EId(10)),
            ..
        })
    ));
    assert_eq!(graph, settled, "a refused edge delete is atomic");

    let mut stale_vertex_version = graph.vertex(VId(1)).expect("vertex exists").version;
    stale_vertex_version.0[0] ^= 0x01;
    let vertex_result = graph.apply_row(&DeltaRow::DeleteVertex {
        vid: VId(1),
        before_version: stale_vertex_version,
        sorted_retired_incident_edges: vec![EId(10), EId(11)],
    });
    assert!(matches!(
        vertex_result,
        Err(ApplyError::ElementVersionMismatch {
            elem: ElementId::Vertex(VId(1)),
            ..
        })
    ));
    assert_eq!(graph, settled, "a refused vertex delete is atomic");
}

/// A complete version identity advances with an element mutation. Reusing the
/// identity captured before that mutation must not retire the newer version.
#[test]
fn an_element_mutation_makes_an_earlier_delete_version_stale() {
    let mut graph = social_graph();
    let stale = graph.vertex(VId(1)).expect("ada exists").version;
    graph
        .apply_row(&DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PROP_NAME,
            before: Some(text("ada")),
            after: Some(text("ada-updated")),
        })
        .expect("property successor applies");
    assert_ne!(
        graph.vertex(VId(1)).expect("ada remains").version,
        stale,
        "a payload successor must have a distinct version identity"
    );

    let settled = graph.clone();
    assert!(matches!(
        graph.apply_row(&DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: stale,
            sorted_retired_incident_edges: vec![EId(10), EId(11)],
        }),
        Err(ApplyError::ElementVersionMismatch { .. })
    ));
    assert_eq!(graph, settled);
}

/// Every row family that changes a materialized vertex or edge record advances
/// the same version chain. Coordinate-level escrow, sketch, schema, and
/// constraint state are separate reference-model records.
#[test]
fn every_materialized_element_mutation_family_advances_its_version() {
    let mut graph = social_graph();
    let vertex = ElementId::Vertex(VId(1));
    let mut vertex_version = graph.element_version(vertex).expect("vertex version");
    let vertex_rows = [
        DeltaRow::LabelMembership {
            vid: VId(1),
            label: LabelId(11),
            before: false,
            after: true,
        },
        DeltaRow::Property {
            elem: vertex,
            property: PROP_NAME,
            before: Some(text("ada")),
            after: Some(text("ada-updated")),
        },
        DeltaRow::ValidTime {
            elem: vertex,
            contract_id: oid(0x61),
            before: None,
            after: Some(ValidTimePeriod {
                start_micros: 10,
                end_micros: Some(20),
            }),
        },
        DeltaRow::Counter {
            operation_key: OperationKey([0x62; 32]),
            elem: vertex,
            property: PROP_VISITS,
            algebra_profile: oid(0x63),
            delta: 1,
            before: 0,
            after: 1,
        },
    ];
    for row in vertex_rows {
        graph.apply_row(&row).expect("vertex successor applies");
        let successor = graph.element_version(vertex).expect("vertex version");
        assert_ne!(successor, vertex_version, "row did not advance: {row:?}");
        vertex_version = successor;
    }

    let edge = ElementId::Edge(EId(10));
    let mut edge_version = graph.element_version(edge).expect("edge version");
    let edge_rows = [
        DeltaRow::Property {
            elem: edge,
            property: PROP_NAME,
            before: None,
            after: Some(text("edge")),
        },
        DeltaRow::ValidTime {
            elem: edge,
            contract_id: oid(0x64),
            before: None,
            after: Some(ValidTimePeriod {
                start_micros: 30,
                end_micros: None,
            }),
        },
        DeltaRow::Counter {
            operation_key: OperationKey([0x65; 32]),
            elem: edge,
            property: PROP_VISITS,
            algebra_profile: oid(0x66),
            delta: 2,
            before: 0,
            after: 2,
        },
    ];
    for row in edge_rows {
        graph.apply_row(&row).expect("edge successor applies");
        let successor = graph.element_version(edge).expect("edge version");
        assert_ne!(successor, edge_version, "row did not advance: {row:?}");
        edge_version = successor;
    }
}

#[test]
fn an_edge_to_a_missing_endpoint_is_refused() {
    let mut graph = social_graph();
    let before = graph.clone();
    let result = graph.apply_row(&create_edge(99, 7, 1, REL_KNOWS, 4242));
    assert!(
        matches!(result, Err(ApplyError::DanglingEndpoint { .. })),
        "a graph holding an edge to a nonexistent vertex is not a graph; got {result:?}"
    );
    assert_eq!(graph, before);
}

/// Identities are never recycled (§6.2), so a create naming an existing
/// identity is always a defect and never an update.
#[test]
fn recreating_an_existing_identity_is_refused() {
    let mut graph = social_graph();
    assert!(matches!(
        graph.apply_row(&create_vertex(1, 9, "impostor")),
        Err(ApplyError::VertexAlreadyExists { .. })
    ));
    assert!(matches!(
        graph.apply_row(&create_edge(10, 9, 2, REL_KNOWS, 3)),
        Err(ApplyError::EdgeAlreadyExists { .. })
    ));
    assert_eq!(
        graph.vertex(VId(1)).expect("ada").props.get(&PROP_NAME),
        Some(&text("ada")),
        "the impostor must not have overwritten anything"
    );
}

/// Retirement removes the visible vertex, not its allocation history. A new
/// create under that VId would give one stable identity two unrelated births.
#[test]
fn a_retired_vertex_identity_remains_permanently_spent() {
    let mut graph = ReferenceGraph::new();
    graph
        .apply_row(&create_vertex(1, 1, "first"))
        .expect("first birth applies");
    let before_version = graph.vertex(VId(1)).expect("vertex exists").version;
    graph
        .apply_row(&DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version,
            sorted_retired_incident_edges: vec![],
        })
        .expect("retirement applies");

    assert_eq!(
        graph.apply_row(&create_vertex(1, 2, "impostor")),
        Err(ApplyError::VertexIdentitySpent { vid: VId(1) })
    );
    assert!(
        graph.vertex(VId(1)).is_none(),
        "the refused second birth stays invisible"
    );

    graph
        .apply_row(&create_vertex(2, 3, "fresh"))
        .expect("a genuinely fresh VId still applies after a retirement");
    assert!(graph.vertex(VId(2)).is_some());
}

/// Edge retirement obeys the same permanent-spend law independently of the
/// endpoints chosen by the attempted second birth.
#[test]
fn a_retired_edge_identity_remains_permanently_spent() {
    let mut graph = social_graph();
    let before_version = graph.edge(EId(10)).expect("edge exists").version;
    graph
        .apply_row(&DeltaRow::DeleteEdge {
            eid: EId(10),
            before_version,
        })
        .expect("retirement applies");

    assert_eq!(
        graph.apply_row(&create_edge(10, 7, 2, REL_KNOWS, 3)),
        Err(ApplyError::EdgeIdentitySpent { eid: EId(10) })
    );
    assert!(
        graph.edge(EId(10)).is_none(),
        "the refused second birth stays invisible"
    );

    graph
        .apply_row(&create_edge(13, 8, 2, REL_KNOWS, 3))
        .expect("a genuinely fresh EId still applies after a retirement");
    assert!(graph.edge(EId(13)).is_some());
}

#[test]
fn deleting_something_absent_is_refused() {
    let mut graph = social_graph();
    assert!(matches!(
        graph.apply_row(&DeltaRow::DeleteEdge {
            eid: EId(9999),
            before_version: oid(2)
        }),
        Err(ApplyError::NoSuchEdge { .. })
    ));
    assert!(matches!(
        graph.apply_row(&DeltaRow::DeleteVertex {
            vid: VId(9999),
            before_version: oid(2),
            sorted_retired_incident_edges: vec![],
        }),
        Err(ApplyError::NoSuchVertex { .. })
    ));
}

// ---------------------------------------------------------------------------
// Counters, escrow, sketches: checked arithmetic and idempotence
// ---------------------------------------------------------------------------

fn counter(key: u8, before: i128, delta: i128, after: i128) -> DeltaRow {
    DeltaRow::Counter {
        operation_key: OperationKey([key; 32]),
        elem: ElementId::Vertex(VId(1)),
        property: PROP_VISITS,
        algebra_profile: oid(0x50),
        delta,
        before,
        after,
    }
}

#[test]
fn a_counter_row_that_does_not_close_is_refused() {
    let mut graph = social_graph();
    let before = graph.clone();
    let result = graph.apply_row(&counter(1, 0, 5, 6));
    assert!(
        matches!(result, Err(ApplyError::ArithmeticDoesNotClose { .. })),
        "a row disagreeing with its own arithmetic would install a value no \
         addition produced; got {result:?}"
    );
    assert_eq!(graph, before);
}

#[test]
fn counter_rows_accumulate_and_are_idempotent_under_replay() {
    let mut graph = social_graph();
    graph.apply_row(&counter(1, 0, 5, 5)).expect("first");
    graph.apply_row(&counter(2, 5, 3, 8)).expect("second");
    assert_eq!(
        graph.counter(ElementId::Vertex(VId(1)), PROP_VISITS),
        Some(8)
    );

    // Replaying an already-applied operation key must be a no-op, not a second
    // addition — this is why the plan dedupes on operation keys before summing.
    let settled = graph.clone();
    graph.apply_row(&counter(1, 0, 5, 5)).expect("replay");
    graph.apply_row(&counter(2, 5, 3, 8)).expect("replay");
    assert_eq!(graph, settled, "replay must not double-count");
}

/// Idempotence must not become amnesia. A repeated row is a no-op, but a
/// DIFFERENT row bearing an already-used key is a defect — and skipping it
/// would be strictly worse than double-counting, because the effect would
/// vanish with nothing reporting it.
#[test]
fn an_operation_key_reused_by_a_different_row_is_refused() {
    let mut graph = social_graph();
    graph.apply_row(&counter(1, 0, 5, 5)).expect("first");
    let settled = graph.clone();

    let result = graph.apply_row(&counter(1, 5, 7, 12));
    assert!(
        matches!(result, Err(ApplyError::OperationKeyReused { .. })),
        "got {result:?}"
    );
    assert_eq!(graph, settled);

    // The identical row under the same key is still a clean no-op.
    graph.apply_row(&counter(1, 0, 5, 5)).expect("replay");
    assert_eq!(graph, settled);
}

#[test]
fn an_escrow_row_with_a_false_before_value_is_refused() {
    let mut graph = ReferenceGraph::new();
    // The subject must EXIST: fgdb-wodn made the materializer honour the
    // check the conflict keys always claimed (Counter-arm precedent).
    graph
        .apply_row(&DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        })
        .expect("subject vertex");
    let domain = EscrowDomainId(7);
    let row = |key: u8, before: i128, delta: i128, after: i128| DeltaRow::Escrow {
        domain_id: domain,
        epoch: 1,
        operation_key: OperationKey([key; 32]),
        subject: ElementId::Vertex(VId(1)),
        subject_property: None,
        delta,
        before_value: before,
        after_value: after,
    };
    graph.apply_row(&row(1, 0, 100, 100)).expect("first");
    assert_eq!(graph.escrow_balance(domain), 100);

    let settled = graph.clone();
    assert!(matches!(
        graph.apply_row(&row(2, 55, 10, 65)),
        Err(ApplyError::EscrowBeforeMismatch { .. })
    ));
    assert_eq!(graph, settled);
}

/// fgdb-wodn — the subject check the conflict keys always claimed. An Escrow
/// row naming a subject that does not exist is refused, and one that exists
/// joins the element's version chain (the Counter arm's precedent), making
/// "a concurrent write to either invalidates it" true in the materializer.
#[test]
fn an_escrow_row_requires_and_versions_its_subject() {
    let domain = EscrowDomainId(9);
    let row = |key: u8, before: i128, delta: i128, after: i128| DeltaRow::Escrow {
        domain_id: domain,
        epoch: 1,
        operation_key: OperationKey([key; 32]),
        subject: ElementId::Vertex(VId(1)),
        subject_property: None,
        delta,
        before_value: before,
        after_value: after,
    };

    // No vertex 1 anywhere: refused, and nothing is applied.
    let mut graph = ReferenceGraph::new();
    assert!(matches!(
        graph.apply_row(&row(1, 0, 100, 100)),
        Err(ApplyError::NoSuchVertex { .. })
    ));
    assert_eq!(graph, ReferenceGraph::new());

    // With the subject present the row applies AND moves the subject's
    // version chain: the claimed check is observable, not just emitted.
    graph
        .apply_row(&DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![],
            props: vec![],
            valid_time: None,
        })
        .expect("subject vertex");
    let version_before = graph.element_version(ElementId::Vertex(VId(1)));
    graph
        .apply_row(&row(2, 0, 100, 100))
        .expect("escrow applies");
    assert_eq!(graph.escrow_balance(domain), 100);
    assert_ne!(
        graph.element_version(ElementId::Vertex(VId(1))),
        version_before,
        "the subject participates in the version chain, as the conflict rule claims"
    );
}

#[test]
fn a_sketch_row_with_a_false_before_digest_is_refused() {
    let mut graph = ReferenceGraph::new();
    let profile = oid(0x60);
    graph
        .apply_row(&DeltaRow::Sketch {
            operation_key: OperationKey([1; 32]),
            sketch_profile_oid: profile,
            before_state_digest: [0u8; 32],
            after_state_oid: oid(0x61),
        })
        .expect("genesis sketch");

    let settled = graph.clone();
    assert!(matches!(
        graph.apply_row(&DeltaRow::Sketch {
            operation_key: OperationKey([2; 32]),
            sketch_profile_oid: profile,
            before_state_digest: [0xff; 32],
            after_state_oid: oid(0x62),
        }),
        Err(ApplyError::SketchBeforeMismatch { .. })
    ));
    assert_eq!(graph, settled);
}

#[test]
fn a_schema_row_must_name_the_current_epoch() {
    let mut graph = ReferenceGraph::new();
    assert_eq!(graph.schema_epoch(), SchemaEpoch(0));
    graph
        .apply_row(&DeltaRow::Schema {
            transition_oid: oid(0x70),
            before_epoch: SchemaEpoch(0),
            after_epoch: SchemaEpoch(1),
        })
        .expect("first transition");
    assert_eq!(graph.schema_epoch(), SchemaEpoch(1));

    assert!(
        matches!(
            graph.apply_row(&DeltaRow::Schema {
                transition_oid: oid(0x71),
                before_epoch: SchemaEpoch(0),
                after_epoch: SchemaEpoch(2),
            }),
            Err(ApplyError::SchemaEpochMismatch { .. })
        ),
        "a transition from an epoch we already left must not apply"
    );
    assert_eq!(graph.schema_epoch(), SchemaEpoch(1));
}

#[test]
fn valid_time_transitions_check_their_before_image() {
    let mut graph = social_graph();
    let period = |start: i64, end: Option<i64>| ValidTimePeriod {
        start_micros: start,
        end_micros: end,
    };
    graph
        .apply_row(&DeltaRow::ValidTime {
            elem: ElementId::Vertex(VId(1)),
            contract_id: oid(0x80),
            before: None,
            after: Some(period(100, None)),
        })
        .expect("open a period");
    assert_eq!(
        graph.vertex(VId(1)).expect("ada").valid_time,
        Some(period(100, None))
    );

    let settled = graph.clone();
    assert!(matches!(
        graph.apply_row(&DeltaRow::ValidTime {
            elem: ElementId::Vertex(VId(1)),
            contract_id: oid(0x80),
            before: None, // it is actually Some now
            after: Some(period(200, None)),
        }),
        Err(ApplyError::ValidTimeBeforeMismatch { .. })
    ));
    assert_eq!(graph, settled);
}

// ---------------------------------------------------------------------------
// Templates and coordinates
// ---------------------------------------------------------------------------

fn entry_with_schema(
    graph: u128,
    branch: u128,
    relation: RelationId,
    schema_epoch: SchemaEpoch,
    schema_transition: Option<ObjectId>,
    rows: Vec<DeltaRow>,
) -> CoordinateEntry {
    CoordinateEntry {
        graph: GraphId(graph),
        branch: BranchId(branch),
        relation,
        schema_epoch,
        schema_transition,
        rows,
    }
}

fn entry(graph: u128, branch: u128, rows: Vec<DeltaRow>) -> CoordinateEntry {
    entry_with_schema(graph, branch, REL_KNOWS, SchemaEpoch(0), None, rows)
}

/// A template touching two coordinates lands in two separate graph states.
/// Applying them to one shared map would silently merge two branch-local
/// versions of the same stable identity — an error a single-coordinate
/// materializer cannot even represent.
#[test]
fn a_multi_coordinate_template_lands_in_separate_graphs() {
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry(1, 1, vec![create_vertex(1, 1, "main-ada")]),
            entry(1, 2, vec![create_vertex(1, 1, "branch-ada")]),
        ],
    )
    .expect("template builds");

    let mut db = ReferenceDatabase::new();
    db.apply_template(&template, CommitSeq(1), LogicalCommandSeq(1))
        .expect("applies");

    assert_eq!(db.coordinate_count(), 2);
    assert_eq!(
        db.graph(GraphId(1), BranchId(1))
            .expect("main")
            .vertex(VId(1))
            .expect("ada")
            .props
            .get(&PROP_NAME),
        Some(&text("main-ada"))
    );
    assert_eq!(
        db.graph(GraphId(1), BranchId(2))
            .expect("branch")
            .vertex(VId(1))
            .expect("ada")
            .props
            .get(&PROP_NAME),
        Some(&text("branch-ada")),
        "the same stable VId may carry a distinct branch-local version"
    );
}

/// ALL OR NOTHING. A template applicable at its first coordinate and not its
/// second must leave the database exactly as it was — a partially applied
/// commit would describe a state no commit stream ever produced.
#[test]
fn a_template_that_fails_partway_applies_nothing() {
    let mut db = ReferenceDatabase::new();
    db.apply_template(
        &LogicalDeltaTemplate::build(
            oid(0x11),
            [0x22; 32],
            vec![entry(1, 1, vec![create_vertex(1, 1, "ada")])],
        )
        .expect("builds"),
        CommitSeq(1),
        LogicalCommandSeq(1),
    )
    .expect("first commit applies");
    let settled = db.clone();

    // Coordinate (1,1) is fine; coordinate (2,1) creates an edge to a vertex
    // that does not exist there.
    let bad = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry(1, 1, vec![create_vertex(2, 2, "grace")]),
            entry(2, 1, vec![create_edge(50, 3, 777, REL_KNOWS, 888)]),
        ],
    )
    .expect("builds");

    assert!(matches!(
        db.apply_template(&bad, CommitSeq(2), LogicalCommandSeq(2)),
        Err(ApplyError::DanglingEndpoint { .. })
    ));
    assert_eq!(
        db, settled,
        "the applicable first coordinate must not have landed either"
    );
}

#[test]
fn template_schema_binding_is_checked_before_apply() {
    let bad = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry_with_schema(
            1,
            1,
            REL_KNOWS,
            SchemaEpoch(9),
            None,
            vec![create_vertex(1, 1, "ada")],
        )],
    )
    .expect("template shape is canonical");
    let mut db = ReferenceDatabase::new();
    let settled = db.clone();

    assert_eq!(
        db.apply_template(&bad, CommitSeq(1), LogicalCommandSeq(1)),
        Err(ApplyError::SchemaBindingMismatch {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: REL_KNOWS,
            declared: SchemaEpoch(9),
            actual: SchemaEpoch(0),
        })
    );
    assert_eq!(db, settled, "a bad binding must leave no coordinate behind");
}

#[test]
fn template_schema_transition_must_exactly_name_its_schema_row() {
    let transition = oid(0x70);
    let schema_row = |transition_oid| DeltaRow::Schema {
        transition_oid,
        before_epoch: SchemaEpoch(0),
        after_epoch: SchemaEpoch(1),
    };
    let cases = [
        (
            "wrong transition identity",
            Some(transition),
            vec![schema_row(oid(0x71))],
            vec![oid(0x71)],
        ),
        (
            "schema row without entry metadata",
            None,
            vec![schema_row(oid(0x71))],
            vec![oid(0x71)],
        ),
        (
            "entry metadata without a schema row",
            Some(transition),
            vec![create_vertex(1, 1, "ada")],
            vec![],
        ),
        (
            "multiple schema rows",
            Some(transition),
            vec![schema_row(transition), schema_row(oid(0x71))],
            vec![transition, oid(0x71)],
        ),
    ];

    for (name, declared, rows, schema_rows) in cases {
        let bad = LogicalDeltaTemplate::build(
            oid(0x11),
            [0x22; 32],
            vec![entry_with_schema(
                1,
                1,
                REL_KNOWS,
                SchemaEpoch(0),
                declared,
                rows,
            )],
        )
        .expect("template shape is canonical");
        let mut db = ReferenceDatabase::new();
        let settled = db.clone();

        assert_eq!(
            db.apply_template(&bad, CommitSeq(1), LogicalCommandSeq(1)),
            Err(ApplyError::SchemaTransitionMismatch {
                graph: GraphId(1),
                branch: BranchId(1),
                relation: REL_KNOWS,
                declared,
                schema_rows,
            }),
            "{name}"
        );
        assert_eq!(db, settled, "{name} must apply no row");
    }
}

#[test]
fn matching_template_schema_transition_applies() {
    let transition = oid(0x70);
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry_with_schema(
            1,
            1,
            REL_KNOWS,
            SchemaEpoch(0),
            Some(transition),
            vec![DeltaRow::Schema {
                transition_oid: transition,
                before_epoch: SchemaEpoch(0),
                after_epoch: SchemaEpoch(1),
            }],
        )],
    )
    .expect("template builds");
    let mut db = ReferenceDatabase::new();

    db.apply_template(&template, CommitSeq(1), LogicalCommandSeq(1))
        .expect("matching transition applies");
    assert_eq!(
        db.graph(GraphId(1), BranchId(1))
            .expect("coordinate exists")
            .schema_epoch(),
        SchemaEpoch(1)
    );
}

#[test]
fn same_coordinate_relation_entries_share_one_commit_sequence() {
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry_with_schema(
                1,
                1,
                REL_WORKS_AT,
                SchemaEpoch(0),
                None,
                vec![create_vertex(2, 2, "grace")],
            ),
            entry(1, 1, vec![create_vertex(1, 1, "ada")]),
        ],
    )
    .expect("distinct relations are distinct coordinate entries");
    let mut db = ReferenceDatabase::new();

    db.apply_template(&template, CommitSeq(1), LogicalCommandSeq(1))
        .expect("one atomic commit may carry both relation entries");
    let graph = db
        .graph(GraphId(1), BranchId(1))
        .expect("coordinate exists");
    assert_eq!(graph.vertex_count(), 2);
    assert_eq!(
        db.applied_through(GraphId(1), BranchId(1)),
        Some(CommitSeq(1))
    );
    assert_eq!(
        db.recorded_commits(GraphId(1), BranchId(1)),
        1,
        "two relation entries still belong to one atomic commit"
    );
}

#[test]
fn earlier_relation_transition_cannot_rewrite_a_later_entrys_validation_basis() {
    let transition = oid(0x70);
    let schema_entry = entry_with_schema(
        1,
        1,
        REL_KNOWS,
        SchemaEpoch(0),
        Some(transition),
        vec![DeltaRow::Schema {
            transition_oid: transition,
            before_epoch: SchemaEpoch(0),
            after_epoch: SchemaEpoch(1),
        }],
    );
    let prepared_after_transition = entry_with_schema(
        1,
        1,
        REL_WORKS_AT,
        SchemaEpoch(1),
        None,
        vec![create_vertex(1, 1, "ada")],
    );
    let bad = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        // Reverse source order proves the builder's canonical relation order
        // cannot change which pre-template state validates either entry.
        vec![prepared_after_transition, schema_entry.clone()],
    )
    .expect("template builds");
    let mut db = ReferenceDatabase::new();
    let settled = db.clone();

    assert_eq!(
        db.apply_template(&bad, CommitSeq(1), LogicalCommandSeq(1)),
        Err(ApplyError::SchemaBindingMismatch {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: REL_WORKS_AT,
            declared: SchemaEpoch(1),
            actual: SchemaEpoch(0),
        })
    );
    assert_eq!(db, settled, "preflight refusal must be all-or-nothing");

    let valid = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry_with_schema(
                1,
                1,
                REL_WORKS_AT,
                SchemaEpoch(0),
                None,
                vec![create_vertex(1, 1, "ada")],
            ),
            schema_entry,
        ],
    )
    .expect("template builds");
    db.apply_template(&valid, CommitSeq(1), LogicalCommandSeq(1))
        .expect("both entries share the pre-template binding");
    let graph = db
        .graph(GraphId(1), BranchId(1))
        .expect("coordinate exists");
    assert_eq!(graph.schema_epoch(), SchemaEpoch(1));
    assert!(graph.vertex(VId(1)).is_some());
}

#[test]
fn schema_rows_cannot_chain_across_relation_entries() {
    let first_transition = oid(0x70);
    let second_transition = oid(0x71);
    let chained = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry_with_schema(
                1,
                1,
                REL_KNOWS,
                SchemaEpoch(0),
                Some(first_transition),
                vec![DeltaRow::Schema {
                    transition_oid: first_transition,
                    before_epoch: SchemaEpoch(0),
                    after_epoch: SchemaEpoch(1),
                }],
            ),
            entry_with_schema(
                1,
                1,
                REL_WORKS_AT,
                SchemaEpoch(0),
                Some(second_transition),
                vec![DeltaRow::Schema {
                    transition_oid: second_transition,
                    before_epoch: SchemaEpoch(1),
                    after_epoch: SchemaEpoch(2),
                }],
            ),
        ],
    )
    .expect("template builds");
    let mut db = ReferenceDatabase::new();
    let settled = db.clone();

    assert_eq!(
        db.apply_template(&chained, CommitSeq(1), LogicalCommandSeq(1)),
        Err(ApplyError::SchemaEpochMismatch {
            declared: SchemaEpoch(1),
            actual: SchemaEpoch(0),
        })
    );
    assert_eq!(
        db, settled,
        "a later relation may not validate against an earlier entry's transition"
    );
}

/// fgdb-nrub (F4) — counters are element-keyed state (the conflict layer
/// emits ConflictKey::Element for them), so they die WITH the element:
/// identities never recycle (§6.2), and residue would be permanently dead
/// but present — observable to readers and to nothing else.
#[test]
fn a_deleted_elements_counters_are_reaped_with_it() {
    let mut graph = social_graph();
    let counter_on =
        |key: u8, elem: ElementId, before: i128, delta: i128, after: i128| DeltaRow::Counter {
            operation_key: OperationKey([key; 32]),
            elem,
            property: PROP_VISITS,
            algebra_profile: oid(0x50),
            delta,
            before,
            after,
        };
    graph
        .apply_row(&counter_on(1, ElementId::Vertex(VId(1)), 0, 5, 5))
        .expect("counter on vertex 1");
    graph
        .apply_row(&counter_on(2, ElementId::Vertex(VId(2)), 0, 7, 7))
        .expect("counter on vertex 2");
    graph
        .apply_row(&counter_on(3, ElementId::Edge(EId(12)), 0, 3, 3))
        .expect("counter on edge 12");

    // Deleting edge 12 reaps ITS counter and no other.
    let e12_version = graph.edge(EId(12)).expect("e12 exists").version;
    graph
        .apply_row(&DeltaRow::DeleteEdge {
            eid: EId(12),
            before_version: e12_version,
        })
        .expect("delete edge 12");
    assert_eq!(
        graph.counter(ElementId::Edge(EId(12)), PROP_VISITS),
        None,
        "the edge's counter dies with the edge"
    );
    assert_eq!(
        graph.counter(ElementId::Vertex(VId(1)), PROP_VISITS),
        Some(5),
        "an unrelated vertex's counter survives"
    );

    // Deleting vertex 1 reaps its counter AND the counters of its retired
    // incident edges (10, 11) — the cascade reaps the whole incident set's
    // element-keyed state.
    let v1_version = graph.vertex(VId(1)).expect("v1 exists").version;
    graph
        .apply_row(&DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: v1_version,
            sorted_retired_incident_edges: vec![EId(10), EId(11)],
        })
        .expect("exact cascade applies");
    assert_eq!(
        graph.counter(ElementId::Vertex(VId(1)), PROP_VISITS),
        None,
        "the vertex's counter dies with the vertex"
    );
    assert_eq!(
        graph.counter(ElementId::Vertex(VId(2)), PROP_VISITS),
        Some(7),
        "the surviving vertex's counter is untouched"
    );
}

/// fgdb-nrub (F5) — a valid-time row whose after-period ends before it
/// starts is refused: the before-image law checks agreement with the past,
/// and this one checks the period itself.
#[test]
fn an_inverted_valid_time_period_is_refused() {
    let mut graph = social_graph();
    let period = |start: i64, end: Option<i64>| ValidTimePeriod {
        start_micros: start,
        end_micros: end,
    };
    let row = |before, after| DeltaRow::ValidTime {
        elem: ElementId::Vertex(VId(1)),
        contract_id: oid(0x80),
        before,
        after,
    };

    assert!(
        matches!(
            graph.apply_row(&row(None, Some(period(200, Some(100))))),
            Err(ApplyError::InvertedValidTimePeriod {
                elem: ElementId::Vertex(VId(1)),
                ..
            })
        ),
        "start past end is not a weaker claim, it is not a period"
    );

    // Boundary honesty — the refusal owns exactly one law: an OPEN period
    // and a ZERO-LENGTH period are not inverted, and both still bind the
    // before-image chain.
    graph
        .apply_row(&row(None, Some(period(200, None))))
        .expect("an open period is not inverted");
    graph
        .apply_row(&row(Some(period(200, None)), Some(period(200, Some(200)))))
        .expect("a zero-length period is not inverted");
}
