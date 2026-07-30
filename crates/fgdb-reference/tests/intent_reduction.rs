//! Laws of the intent layer — Appendix B commands reducing to canonical effects.
//!
//! THE MISMATCH TRICHOTOMY IS THE REASON THIS FILE EXISTS. `CompareAndSet`
//! declares what a failed precondition means: `NoOp`, `StatementError`, or
//! `TxnAbort`. Those three are invisible at the effect level — a conflated
//! implementation emits the same effects for two of them and differs only in
//! what SURVIVES. So each policy gets a law asserting what the OTHER TWO would
//! get wrong:
//!
//!   * `NoOp` — no effect, statement continues, no failure reported.
//!   * `StatementError` — this statement's effects discarded, EARLIER
//!     statements' effects kept, failure reported with both values.
//!   * `TxnAbort` — NOTHING survives, including statements that already
//!     succeeded.
//!
//! Conflating StatementError with TxnAbort loses earlier work; conflating it
//! with NoOp loses the error. Both are silent at the effect level.

use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_reference::intents::{
    Intent, MismatchPolicy, Outcome, Statement, StatementFailure, evaluate,
};
use fgdb_types::{CanonicalScalar, EId, ObjectId, VId};

const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const NAME: PropertyKeyId = PropertyKeyId(100);
const RANK: PropertyKeyId = PropertyKeyId(101);

fn text(v: &str) -> CanonicalScalar {
    CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic(v).expect("bounded"))
}

fn vtx(vid: u128) -> ElementId {
    ElementId::Vertex(VId(vid))
}

/// Two vertices, ada named "ada", plus one edge.
fn basis() -> ReferenceGraph {
    let mut g = ReferenceGraph::new();
    for (vid, name) in [(1u128, "ada"), (2, "grace")] {
        g.apply_row(&DeltaRow::CreateVertex {
            vid: VId(vid),
            birth_ordinal: vid as u64,
            labels: vec![LABEL],
            props: vec![(NAME, text(name))],
            valid_time: None,
        })
        .expect("applies");
    }
    g.apply_row(&DeltaRow::CreateEdge {
        eid: EId(10),
        birth_ordinal: 1,
        src: VId(1),
        relation: REL,
        dst: VId(2),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    })
    .expect("applies");
    g
}

fn cas(expected: Option<&str>, value: &str, policy: MismatchPolicy) -> Intent {
    Intent::CompareAndSet {
        elem: vtx(1),
        name: NAME,
        expected: expected.map(text),
        value: text(value),
        mismatch: policy,
    }
}

fn set_rank(value: i64) -> Intent {
    Intent::SetProp {
        elem: vtx(1),
        name: RANK,
        value: CanonicalScalar::Int(value),
    }
}

// ---------------------------------------------------------------------------
// Reduction basics: an intent computes its own before-image
// ---------------------------------------------------------------------------

/// The difference between an intent and an effect: an effect DECLARES its before
/// image, and finalization is what computes it. A caller never supplies one.
#[test]
fn set_prop_derives_its_before_image_from_state() {
    let g = basis();
    let outcome = evaluate(
        &g,
        &[Statement::new(vec![Intent::SetProp {
            elem: vtx(1),
            name: NAME,
            value: text("ada-renamed"),
        }])],
    );

    assert_eq!(
        outcome.effects(),
        &[DeltaRow::Property {
            elem: vtx(1),
            property: NAME,
            before: Some(text("ada")),
            after: Some(text("ada-renamed")),
        }],
        "the before image is read from the graph, not accepted from the caller"
    );
}

/// Setting a property to what it already holds emits NO effect. A no-op write is
/// not a change, and emitting a row that changes nothing would put noise in the
/// delta stream that every downstream consumer must then process.
#[test]
fn a_no_op_write_emits_no_effect() {
    let outcome = evaluate(
        &basis(),
        &[Statement::new(vec![Intent::SetProp {
            elem: vtx(1),
            name: NAME,
            value: text("ada"),
        }])],
    );
    assert!(outcome.effects().is_empty());
    assert!(!outcome.is_aborted());
}

/// `EnsureEdge` is idempotent: it emits an edge when none matches and nothing
/// when one does. Checked in both directions, since an implementation that
/// always emits and one that never emits each pass half of this.
#[test]
fn ensure_edge_is_idempotent() {
    let ensure = |eid: u128, dst: u128| Intent::EnsureEdge {
        eid: EId(eid),
        src: VId(1),
        etype: REL,
        dst: VId(dst),
        constraint_id: ObjectId([0x70; 32]),
        props: vec![],
    };

    // 1 -> 2 already exists.
    let existing = evaluate(&basis(), &[Statement::new(vec![ensure(99, 2)])]);
    assert!(
        existing.effects().is_empty(),
        "an edge that already exists must emit nothing"
    );

    // 1 -> 1 does not.
    let fresh = evaluate(&basis(), &[Statement::new(vec![ensure(99, 1)])]);
    assert_eq!(fresh.effects().len(), 1, "a missing edge must be created");

    // And evaluating the SAME ensure twice in one transaction emits once, which
    // requires the second evaluation to see the first's effect.
    let twice = evaluate(
        &basis(),
        &[Statement::new(vec![ensure(99, 1), ensure(98, 1)])],
    );
    assert_eq!(
        twice.effects().len(),
        1,
        "the second ensure sees the first's edge and emits nothing"
    );
}

// ---------------------------------------------------------------------------
// Read-your-own-writes
// ---------------------------------------------------------------------------

/// A later intent sees an earlier one's effects. Without this, the
/// `CompareAndSet` below would compare against "ada" — the value the transaction
/// had already replaced — and a transaction that cannot see its own writes is
/// not a transaction.
#[test]
fn a_later_intent_sees_an_earlier_ones_effects() {
    let outcome = evaluate(
        &basis(),
        &[Statement::new(vec![
            Intent::SetProp {
                elem: vtx(1),
                name: NAME,
                value: text("interim"),
            },
            // Expects the value the PREVIOUS intent wrote, not the basis value.
            cas(Some("interim"), "final", MismatchPolicy::StatementError),
        ])],
    );

    {
        let (effects, statement_failures) =
            outcome.committed_parts().expect("transaction committed");
        {
            assert!(
                statement_failures.is_empty(),
                "the CAS must have matched: {statement_failures:?}"
            );
            assert_eq!(effects.len(), 2);
            assert_eq!(
                effects[1],
                DeltaRow::Property {
                    elem: vtx(1),
                    property: NAME,
                    before: Some(text("interim")),
                    after: Some(text("final")),
                },
                "the second effect's before image is the first effect's after"
            );
        }
    }
}

/// And it holds ACROSS statements, not only within one.
#[test]
fn a_later_statement_sees_an_earlier_statements_effects() {
    let outcome = evaluate(
        &basis(),
        &[
            Statement::new(vec![Intent::SetProp {
                elem: vtx(1),
                name: NAME,
                value: text("interim"),
            }]),
            Statement::new(vec![cas(
                Some("interim"),
                "final",
                MismatchPolicy::StatementError,
            )]),
        ],
    );
    assert_eq!(outcome.effects().len(), 2);
    assert!(matches!(
        &outcome,
        Outcome::Committed { statement_failures, .. } if statement_failures.is_empty()
    ));
}

// ---------------------------------------------------------------------------
// THE TRICHOTOMY
// ---------------------------------------------------------------------------

/// NoOp: no effect, no failure, and the statement CONTINUES — the intent after
/// the failed guard still runs. An implementation that treated NoOp as a
/// statement error would drop that following intent.
#[test]
fn a_noop_mismatch_emits_nothing_and_the_statement_continues() {
    let outcome = evaluate(
        &basis(),
        &[Statement::new(vec![
            cas(Some("WRONG"), "never", MismatchPolicy::NoOp),
            set_rank(7),
        ])],
    );

    {
        let (effects, statement_failures) =
            outcome.committed_parts().expect("transaction committed");
        {
            assert!(
                statement_failures.is_empty(),
                "NoOp is not a failure: {statement_failures:?}"
            );
            assert_eq!(
                effects.len(),
                1,
                "the CAS emitted nothing and the FOLLOWING intent still ran"
            );
            assert!(matches!(
                effects[0],
                DeltaRow::Property { property, .. } if property == RANK
            ));
        }
    }
}

/// StatementError: this statement's effects are discarded, EARLIER statements'
/// effects are kept, and the failure is reported with both values so a caller
/// can see why. Conflating this with TxnAbort would lose statement 0's effect;
/// conflating it with NoOp would lose the error.
#[test]
fn a_statement_error_discards_only_its_own_statement() {
    let outcome = evaluate(
        &basis(),
        &[
            Statement::new(vec![set_rank(1)]),
            Statement::new(vec![
                set_rank(2),
                cas(Some("WRONG"), "never", MismatchPolicy::StatementError),
            ]),
            Statement::new(vec![set_rank(3)]),
        ],
    );

    {
        let (effects, statement_failures) =
            outcome.committed_parts().expect("transaction committed");
        {
            assert_eq!(
                statement_failures.len(),
                1,
                "exactly one statement failed: {statement_failures:?}"
            );
            assert_eq!(statement_failures[0].0, 1, "and it was statement 1");
            assert!(matches!(
                statement_failures[0].1,
                StatementFailure::Mismatch { .. }
            ));

            // Statement 1's set_rank(2) must NOT survive; 0 and 2 must.
            let ranks: Vec<i64> = effects
                .iter()
                .filter_map(|row| match row {
                    DeltaRow::Property {
                        property,
                        after: Some(CanonicalScalar::Int(v)),
                        ..
                    } if *property == RANK => Some(*v),
                    _ => None,
                })
                .collect();
            assert_eq!(
                ranks,
                vec![1, 3],
                "statement 1's effect is discarded; 0 and 2 survive"
            );
        }
    }
}

/// The failure names BOTH values. A mismatch reporting only "it did not match"
/// is unactionable — the caller cannot tell a stale read from a wrong guard.
#[test]
fn a_mismatch_reports_expected_and_actual() {
    let outcome = evaluate(
        &basis(),
        &[Statement::new(vec![cas(
            Some("WRONG"),
            "never",
            MismatchPolicy::StatementError,
        )])],
    );
    let (_, statement_failures) = outcome.committed_parts().expect("committed");
    let (expected, actual) = statement_failures[0]
        .1
        .mismatch_values()
        .expect("the failure is a mismatch");
    assert_eq!(expected, Some(&text("WRONG")));
    assert_eq!(actual, Some(&text("ada")));
}

/// TxnAbort: NOTHING survives — including statements that had already succeeded.
/// This is the case an implementation gets wrong by returning "the effects so
/// far", which would commit a prefix of a transaction the caller declared must
/// be all-or-nothing.
#[test]
fn a_txn_abort_discards_even_already_successful_statements() {
    let outcome = evaluate(
        &basis(),
        &[
            Statement::new(vec![set_rank(1)]),
            Statement::new(vec![set_rank(2)]),
            Statement::new(vec![cas(Some("WRONG"), "never", MismatchPolicy::TxnAbort)]),
            Statement::new(vec![set_rank(4)]),
        ],
    );

    {
        let (statement, failure) = outcome.aborted_parts().expect("transaction aborted");
        assert_eq!(statement, 2, "the abort names the statement that caused it");
        assert!(matches!(failure, StatementFailure::Mismatch { .. }));
    }
    assert!(
        outcome.effects().is_empty(),
        "an aborted transaction produces NO effects, not the prefix before the abort"
    );
}

/// The three policies on the SAME failing comparison produce three DIFFERENT
/// outcomes. This is the law that fails if any two are conflated, stated
/// directly rather than left implicit across the tests above.
#[test]
fn the_three_policies_differ_on_one_identical_mismatch() {
    let with = |policy| {
        evaluate(
            &basis(),
            &[
                Statement::new(vec![set_rank(1)]),
                Statement::new(vec![cas(Some("WRONG"), "never", policy)]),
            ],
        )
    };

    let noop = with(MismatchPolicy::NoOp);
    let stmt = with(MismatchPolicy::StatementError);
    let abort = with(MismatchPolicy::TxnAbort);

    // Effect counts: NoOp and StatementError both keep statement 0's effect,
    // TxnAbort keeps nothing.
    assert_eq!(noop.effects().len(), 1);
    assert_eq!(stmt.effects().len(), 1);
    assert_eq!(abort.effects().len(), 0);

    // But NoOp and StatementError differ in whether a failure was REPORTED,
    // which is the only thing separating them.
    let (_, n) = noop.committed_parts().expect("NoOp commits");
    let (_, s) = stmt.committed_parts().expect("StatementError commits");
    assert!(n.is_empty(), "NoOp reports no failure");
    assert_eq!(s.len(), 1, "StatementError reports one");
    assert!(abort.is_aborted());

    // All three are pairwise distinct outcomes.
    assert_ne!(noop, stmt);
    assert_ne!(stmt, abort);
    assert_ne!(noop, abort);
}

// ---------------------------------------------------------------------------
// Determinism and canonical output
// ---------------------------------------------------------------------------

/// Finalization is a function of basis and statements.
#[test]
fn evaluation_is_deterministic() {
    let g = basis();
    let statements = [Statement::new(vec![
        set_rank(9),
        cas(Some("ada"), "ada2", MismatchPolicy::StatementError),
    ])];
    assert_eq!(evaluate(&g, &statements), evaluate(&g, &statements));
    assert_eq!(
        evaluate(&g, &statements).effects(),
        evaluate(&basis(), &statements).effects(),
        "and of the basis VALUE, not the particular instance"
    );
}

/// Caller-supplied label and property order does not survive finalization: the
/// intent layer is where it stops mattering, so the effects it emits are already
/// in the form the canonical delta encoding requires.
#[test]
fn finalization_canonicalizes_caller_order() {
    let g = ReferenceGraph::new();
    let forward = evaluate(
        &g,
        &[Statement::new(vec![Intent::CreateVertex {
            vid: VId(7),
            labels: vec![LabelId(1), LabelId(9)],
            props: vec![(PropertyKeyId(2), text("a")), (PropertyKeyId(4), text("b"))],
        }])],
    );
    let reversed = evaluate(
        &g,
        &[Statement::new(vec![Intent::CreateVertex {
            vid: VId(7),
            labels: vec![LabelId(9), LabelId(1)],
            props: vec![(PropertyKeyId(4), text("b")), (PropertyKeyId(2), text("a"))],
        }])],
    );
    assert_eq!(
        forward.effects(),
        reversed.effects(),
        "input order must not survive into the effects"
    );

    // And a duplicate label collapses rather than producing an invalid row.
    let duped = evaluate(
        &g,
        &[Statement::new(vec![Intent::CreateVertex {
            vid: VId(7),
            labels: vec![LabelId(1), LabelId(1), LabelId(9)],
            props: vec![(PropertyKeyId(2), text("a")), (PropertyKeyId(4), text("b"))],
        }])],
    );
    assert_eq!(duped.effects(), forward.effects());
}

/// An intent whose reduction the graph refuses is a statement failure, not a
/// panic and not a silently dropped effect.
#[test]
fn an_intent_the_graph_refuses_fails_its_statement() {
    let outcome = evaluate(
        &basis(),
        &[
            Statement::new(vec![set_rank(1)]),
            // vertex 1 already exists: CreateVertex must be refused (§6.2,
            // identities are never recycled).
            Statement::new(vec![Intent::CreateVertex {
                vid: VId(1),
                labels: vec![LABEL],
                props: vec![],
            }]),
        ],
    );
    {
        let (effects, statement_failures) =
            outcome.committed_parts().expect("transaction committed");
        {
            assert_eq!(statement_failures.len(), 1);
            assert_eq!(statement_failures[0].0, 1);
            assert!(matches!(
                statement_failures[0].1,
                StatementFailure::Rejected(_)
            ));
            assert_eq!(effects.len(), 1, "statement 0 survives");
        }
    }
}

// ---------------------------------------------------------------------------
// The delete family: the cascade is COMPUTED, never supplied
// ---------------------------------------------------------------------------

fn vertex(vid: u128, name: &str) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(NAME, text(name))],
        valid_time: None,
    }
}

fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

fn graph(rows: Vec<DeltaRow>) -> ReferenceGraph {
    let mut g = ReferenceGraph::new();
    for row in rows {
        g.apply_row(&row).expect("fixture row applies");
    }
    g
}

/// The cascade image of a `DeleteVertex` row, or `None` for any other row.
///
/// An accessor rather than a `panic!` arm on the non-matching case: tests assert
/// with `expect`, and a `panic!` in a test file moves the workspace UBS panic
/// class.
fn cascade_of(row: &DeltaRow) -> Option<&Vec<EId>> {
    match row {
        DeltaRow::DeleteVertex {
            sorted_retired_incident_edges,
            ..
        } => Some(sorted_retired_incident_edges),
        _ => None,
    }
}

/// The `(before, after)` images of a `Property` row.
fn property_images(row: &DeltaRow) -> Option<(&Option<CanonicalScalar>, &Option<CanonicalScalar>)> {
    match row {
        DeltaRow::Property { before, after, .. } => Some((before, after)),
        _ => None,
    }
}

/// THE LAW THE DELETE FAMILY EXISTS FOR. `DeleteVertex` takes no edge list, and
/// finalization computes the retired-edge image from the state it evaluates
/// against. The materializer then checks that image for EQUALITY, so a computed
/// cascade is the only kind that can be right — and the intent's signature makes
/// a wrong one unspellable rather than merely refused.
#[test]
fn a_delete_vertex_intent_computes_its_own_cascade() {
    let basis = graph(vec![
        vertex(1, "ada"),
        vertex(2, "grace"),
        vertex(3, "hopper"),
        edge(10, 1, 2),
        edge(11, 3, 1),
        edge(12, 2, 3),
    ]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![Intent::DeleteVertex { vid: VId(1) }])],
    );
    let (effects, failures) = outcome.committed_parts().expect("committed");
    assert!(failures.is_empty());
    assert_eq!(effects.len(), 1);
    assert_eq!(
        cascade_of(&effects[0]).expect("a DeleteVertex row"),
        &vec![EId(10), EId(11)],
        "both incident edges, sorted, and NOT the edge between 2 and 3"
    );

    // And the computed image is exactly what the materializer accepts.
    let mut applied = basis.clone();
    applied
        .apply_row(&effects[0])
        .expect("the cascade image is exact");
    assert!(applied.vertex(VId(1)).is_none());
    assert!(applied.edge(EId(10)).is_none() && applied.edge(EId(11)).is_none());
    assert!(applied.edge(EId(12)).is_some(), "unrelated edges survive");
}

/// A self-loop is incident twice and must appear ONCE in the cascade image.
///
/// The equality check makes this observable: a naive concatenation of out-edges
/// and in-edges lists the loop twice, and the materializer then refuses the row as
/// claiming a retirement that never happened. The failure is a refusal rather than
/// a corruption, which is why it is worth having the law rather than trusting the
/// helper.
#[test]
fn a_self_loop_appears_once_in_the_cascade() {
    let basis = graph(vec![vertex(1, "ada"), edge(20, 1, 1)]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![Intent::DeleteVertex { vid: VId(1) }])],
    );
    let (effects, _) = outcome.committed_parts().expect("committed");
    assert_eq!(
        cascade_of(&effects[0]).expect("a DeleteVertex row"),
        &vec![EId(20)]
    );
    let mut applied = basis.clone();
    applied.apply_row(&effects[0]).expect("applies");
    assert_eq!(applied.vertex_count(), 0);
    assert_eq!(applied.edge_count(), 0);
}

/// Deleting what is not there emits nothing rather than failing — the same
/// reading that makes a no-op `SetProp` emit nothing. A delete is a statement
/// about the END state, and the end state is already what was asked for.
#[test]
fn deleting_an_absent_element_emits_nothing() {
    let basis = graph(vec![vertex(1, "ada")]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![
            Intent::DeleteVertex { vid: VId(99) },
            Intent::DeleteEdge { eid: EId(99) },
        ])],
    );
    let (effects, failures) = outcome.committed_parts().expect("committed");
    assert!(effects.is_empty(), "no effects: {effects:?}");
    assert!(failures.is_empty(), "and not a failure either");
}

/// Deleting an edge retires only that edge, leaving both endpoints.
#[test]
fn a_delete_edge_intent_leaves_its_endpoints() {
    let basis = graph(vec![vertex(1, "ada"), vertex(2, "grace"), edge(10, 1, 2)]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![Intent::DeleteEdge { eid: EId(10) }])],
    );
    let (effects, _) = outcome.committed_parts().expect("committed");
    let mut applied = basis.clone();
    for row in effects {
        applied.apply_row(row).expect("applies");
    }
    assert_eq!(applied.vertex_count(), 2);
    assert_eq!(applied.edge_count(), 0);
}

/// A cascade computed against a state that a LATER intent in the same statement
/// changed must reflect that change — read-your-own-writes reaching into the
/// cascade.
///
/// Deleting the edge first and then the vertex must produce an EMPTY cascade
/// image, because by the time the delete is finalized the edge is already gone.
/// A cascade computed against the statement's opening state would list the edge,
/// and the materializer would refuse the row for claiming a retirement that had
/// already happened.
#[test]
fn a_cascade_sees_earlier_intents_in_the_same_statement() {
    let basis = graph(vec![vertex(1, "ada"), vertex(2, "grace"), edge(10, 1, 2)]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![
            Intent::DeleteEdge { eid: EId(10) },
            Intent::DeleteVertex { vid: VId(1) },
        ])],
    );
    let (effects, _) = outcome.committed_parts().expect("committed");
    assert_eq!(effects.len(), 2);
    let cascade = cascade_of(&effects[1]).expect("a DeleteVertex row");
    assert!(
        cascade.is_empty(),
        "the edge was already retired by the previous intent: {cascade:?}"
    );
    let mut applied = basis.clone();
    for row in effects {
        applied
            .apply_row(row)
            .expect("the whole statement applies in order");
    }
    assert_eq!(applied.vertex_count(), 1);
}

/// Removing a property is distinct from setting it to null: `after: None` versus
/// `after: Some(Null)`. An absent property and one holding null are different
/// states, and collapsing them would make removal unexpressible.
#[test]
fn removing_a_property_is_not_setting_it_to_null() {
    let basis = graph(vec![vertex(1, "ada")]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![Intent::RemoveProp {
            elem: ElementId::Vertex(VId(1)),
            name: NAME,
        }])],
    );
    let (effects, _) = outcome.committed_parts().expect("committed");
    let (before, after) = property_images(&effects[0]).expect("a Property row");
    assert!(before.is_some(), "the before image is computed");
    assert!(after.is_none(), "removal is an absent after image");
    let mut applied = basis.clone();
    applied.apply_row(&effects[0]).expect("applies");
    assert!(
        !applied
            .vertex(VId(1))
            .expect("the vertex survives")
            .props
            .contains_key(&NAME)
    );
}

/// Removing an absent property emits nothing.
#[test]
fn removing_an_absent_property_emits_nothing() {
    let basis = graph(vec![vertex(1, "ada")]);
    let outcome = evaluate(
        &basis,
        &[Statement::new(vec![Intent::RemoveProp {
            elem: ElementId::Vertex(VId(1)),
            name: PropertyKeyId(4242),
        }])],
    );
    let (effects, failures) = outcome.committed_parts().expect("committed");
    assert!(effects.is_empty() && failures.is_empty());
}

/// `EnsureVertex` is idempotent: the second evaluation emits nothing, and it does
/// NOT fail the way a second `CreateVertex` would.
#[test]
fn ensure_vertex_is_idempotent() {
    let basis = ReferenceGraph::new();
    let ensure = || {
        Statement::new(vec![Intent::EnsureVertex {
            vid: VId(1),
            labels: vec![LABEL],
            props: vec![(NAME, text("ada"))],
        }])
    };
    let outcome = evaluate(&basis, &[ensure()]);
    let (effects, _) = outcome.committed_parts().expect("committed");
    assert_eq!(effects.len(), 1);

    let mut applied = basis.clone();
    applied.apply_row(&effects[0]).expect("applies");

    let again = evaluate(&applied, &[ensure()]);
    let (effects, failures) = again.committed_parts().expect("committed");
    assert!(
        effects.is_empty() && failures.is_empty(),
        "the second ensure is a no-op, not a failure: {effects:?} {failures:?}"
    );

    // Whereas a bare create against the same state is a refusal.
    let created = evaluate(
        &applied,
        &[Statement::new(vec![Intent::CreateVertex {
            vid: VId(1),
            labels: vec![LABEL],
            props: vec![],
        }])],
    );
    let (effects, failures) = created.committed_parts().expect("committed");
    assert!(
        effects.is_empty() && failures.len() == 1,
        "a duplicate create is a statement error"
    );
}
