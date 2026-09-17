mod quickstart {
    include!("../examples/gql_quickstart.rs");
}

use fgdb::QueryValue;
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

fn int(value: i64) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
}
fn text(value: &str) -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(
        CanonicalScalar::ucs_basic_text(value).unwrap(),
    ))
}
fn null() -> QueryValue {
    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
}

#[test]
fn every_quickstart_step_has_exact_expected_rows() {
    let steps = quickstart::run().expect("quickstart must execute without skipped errors");
    // Hand-computed from the eight people and five KNOWS/four WORKS_AT edges.
    // Writes have no result columns/rows; subsequent reads verify their effects.
    let expected = vec![
        ("insert social graph", vec![]),
        ("insert workplaces", vec![]),
        (
            "match ordered",
            vec![
                vec![text("Charles"), int(1791)],
                vec![text("Ada"), int(1815)],
                vec![text("Grace"), int(1906)],
                vec![text("Alan"), int(1912)],
            ],
        ),
        (
            "optional workplaces",
            vec![
                vec![text("Ada"), text("Engine")],
                vec![text("Alan"), text("Lab")],
                vec![text("Barbara"), null()],
                vec![text("Charles"), text("Engine")],
                vec![text("Donald"), null()],
                vec![text("Edsger"), null()],
                vec![text("Frances"), null()],
                vec![text("Grace"), text("Lab")],
            ],
        ),
        (
            "count by team",
            vec![
                vec![int(1), QueryValue::Count(2)],
                vec![int(2), QueryValue::Count(6)],
            ],
        ),
        (
            "with pipeline",
            vec![vec![QueryValue::Count(2), QueryValue::Integer(3606)]],
        ),
        (
            "union",
            vec![
                vec![text("Ada")],
                vec![text("Charles")],
                vec![text("Grace")],
            ],
        ),
        ("shortest walk", vec![vec![int(3)]]),
        ("set birth year", vec![]),
        ("updated value", vec![vec![int(1816)]]),
        ("historical value", vec![vec![int(1815)]]),
        ("delete relationship", vec![]),
        ("deleted relationship absent", vec![]),
        ("merge matched", vec![]),
        ("merge matched value", vec![vec![int(1817)]]),
        ("merge created", vec![]),
        ("merge created value", vec![vec![int(1918)]]),
        (
            "collect list",
            vec![vec![QueryValue::Value(GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("Ada").unwrap()),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("Charles").unwrap()),
                ]
                .into_boxed_slice(),
            ))]],
        ),
        (
            "unwind list",
            vec![vec![text("Ada")], vec![text("Grace")], vec![text("Ada")]],
        ),
        ("reopened value", vec![vec![text("Ada"), int(1817)]]),
    ];
    assert_eq!(steps.len(), expected.len(), "no script step may disappear");
    for (step, (name, rows)) in steps.iter().zip(expected) {
        assert_eq!(step.name, name, "script order");
        assert_eq!(step.rows, rows, "{name}: {}", step.statement);
    }
}
