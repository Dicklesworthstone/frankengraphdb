//! Frozen batch profiles require disjoint query keys, not storage encodings.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}

fn program(text: &str, values: &[CanonicalScalar]) -> PreparedGraphWriteProgram {
    // Each parameter keeps its declared kind. A script can contain two MERGE
    // statements of one shape with differently typed parameter names; the
    // batch binder considers their keys together for its physical profile.
    let mut parameters = GqlParameters::new();
    let script = values
        .iter()
        .enumerate()
        .map(|(row, value)| {
            let name = format!("id{row}");
            parameters = std::mem::take(&mut parameters)
                .with_scalar(&name, value.clone())
                .unwrap();
            text.replace("$id", &format!("${name}"))
        })
        .collect::<Vec<_>>()
        .join("; ");
    let declarations: Vec<_> = parameters.parameter_types().collect();
    PreparedGraphWriteScript::prepare_with_parameter_types(
        &script,
        RelationId(1),
        &declarations,
        symbols,
    )
    .unwrap()
    .bind_parameter_sets(std::slice::from_ref(&parameters))
    .unwrap()
    .program()
    .clone()
}

fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}

#[test]
fn query_equal_numeric_keys_never_receive_disjoint_merge_profiles() {
    for values in [
        [CanonicalScalar::Int(1), float(1.0)],
        [float(1.0), CanonicalScalar::Int(1)],
        [CanonicalScalar::Int(0), float(-0.0)],
        [CanonicalScalar::Int(i64::MIN), float(i64::MIN as f64)],
        [
            CanonicalScalar::Int(9_007_199_254_740_992),
            float(9_007_199_254_740_992.0),
        ],
        [float(f64::NAN), float(-f64::NAN)],
    ] {
        for text in [
            "MERGE (n:N {id:$id})",
            "MERGE (n:N {id:$id}) SET n.value=7",
            "MERGE (n:N {id:$id}) SET n.id=$id",
            "MERGE (n:N {id:$id}) SET n.value=COALESCE(n.value,0)+1",
        ] {
            assert!(
                !program(text, &values).freezes_vertex_upsert_input(),
                "{text}: {values:?}"
            );
        }
        for text in [
            "MATCH (a:N {id:$id}), (b:N {id:100}) MERGE (a)-[:R]->(b)",
            "MATCH (a:N {id:100}), (b:N {id:$id}) MERGE (a)-[:R]->(b)",
        ] {
            assert!(
                !program(text, &values).freezes_edge_merge_input(),
                "{text}: {values:?}"
            );
        }
    }
}

#[test]
fn exact_numeric_distinctions_remain_eligible_without_rounding_or_decimal_coercion() {
    for values in [
        [CanonicalScalar::Int(1), float(1.5)],
        [CanonicalScalar::Int(0), float(f64::NAN)],
        [CanonicalScalar::Int(i64::MAX), float(i64::MAX as f64)],
        [
            CanonicalScalar::Int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
        ],
        [
            CanonicalScalar::Int(1),
            CanonicalScalar::Decimal(
                CanonicalDecimal::parse_canonical("1.000000000000000000").unwrap(),
            ),
        ],
        [float(f64::NEG_INFINITY), float(f64::INFINITY)],
    ] {
        for text in ["MERGE (n:N {id:$id})", "MERGE (n:N {id:$id}) SET n.id=$id"] {
            assert!(
                program(text, &values).freezes_vertex_upsert_input(),
                "{text}: {values:?}"
            );
        }
        assert!(
            program(
                "MATCH (a:N {id:$id}), (b:N {id:100}) MERGE (a)-[:R]->(b)",
                &values,
            )
            .freezes_edge_merge_input(),
            "{values:?}"
        );
    }
}

#[test]
fn distinct_key_upserts_admit_target_local_arithmetic_in_each_clause() {
    let values = [CanonicalScalar::Int(1), CanonicalScalar::Int(2)];
    for text in [
        "MERGE (n:N {id:$id}) SET n.value=COALESCE(n.value,0)+1",
        "MERGE (n:N {id:$id}) ON CREATE SET n.value=n.id+10",
        "MERGE (n:N {id:$id}) ON MATCH SET n.value=n.value+1",
        "MERGE (n:N {id:$id}) ON CREATE SET n.value=10 \
         ON MATCH SET n.value=n.value+1 SET n.value=n.value*2",
    ] {
        let frozen = program(text, &values);
        assert!(frozen.freezes_vertex_upsert_input(), "{text}");
        let ordinary = PreparedGraphWriteProgram::prepare(frozen.statements().to_vec()).unwrap();
        assert!(!ordinary.freezes_vertex_upsert_input(), "{text}");
        assert_eq!(
            frozen.canonical_bytes(),
            ordinary.canonical_bytes(),
            "{text}"
        );
    }
}

#[test]
fn arithmetic_cannot_freeze_duplicate_keys_key_changes_or_required_label_actions() {
    let distinct = [CanonicalScalar::Int(1), CanonicalScalar::Int(2)];
    let duplicate = [CanonicalScalar::Int(1), CanonicalScalar::Int(1)];
    assert!(
        !program(
            "MERGE (n:N {id:$id}) SET n.value=COALESCE(n.value,0)+1",
            &duplicate,
        )
        .freezes_vertex_upsert_input()
    );
    for text in [
        "MERGE (n:N {id:$id}) SET n.id=n.id+1",
        "MERGE (n:N {id:$id}) ON CREATE SET n.id=n.id+1",
        "MERGE (n:N {id:$id}) ON MATCH SET n.id=n.id+1",
        "MERGE (n:N {id:$id}) SET n:N, n.value=COALESCE(n.value,0)+1",
    ] {
        assert!(
            !program(text, &distinct).freezes_vertex_upsert_input(),
            "{text}"
        );
    }
}
