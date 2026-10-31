//! A source may omit property payloads only after the complete compiled input
//! proves that neither a predicate nor any projected cell can inspect them.

use super::*;
use crate::PreparedGraphAggregateText;
use crate::algebra::GraphPatternBuilder;
use crate::spill_aggregate::AsyncSpillAggregatePlan;

#[test]
fn property_access_proof_keeps_identity_labels_and_complete_predicate_programs() {
    for (text, requires_properties) in [
        ("MATCH (n) RETURN n", false),
        ("MATCH (n:L) RETURN n", false),
        ("MATCH (n:L) RETURN n,n AS again,n AS third", false),
        ("MATCH (n) WHERE n=n RETURN n", false),
        ("MATCH (n) WHERE n<>n RETURN n", false),
        ("MATCH (n) RETURN n,n.p", true),
        ("MATCH (n) WHERE n.p=1 RETURN n", true),
        ("MATCH (n) WHERE n.p IS NULL RETURN n", true),
        ("MATCH (n) WHERE n.p IS NOT NULL RETURN n", true),
        ("MATCH (n) WHERE n.p=n.p RETURN n", true),
        ("MATCH (n) WHERE n.p>1 OR n.p<0 RETURN n", true),
        ("MATCH (n) WHERE 1/(n.p-1)>0 RETURN n", true),
        ("MATCH (n) WHERE n<>n RETURN n,n.p", true),
    ] {
        let logical = plan(text);
        let ordinary = VertexScanPlan::compile(&logical).unwrap();
        let asynchronous = AsyncVertexScanPlan::compile(&logical).unwrap();
        assert_eq!(
            ordinary.requires_properties(),
            requires_properties,
            "{text}"
        );
        assert_eq!(
            asynchronous.requires_properties(),
            requires_properties,
            "{text}"
        );
    }
    // The typed identity projection is a different sealed output profile from
    // GraphValueRow. False identities must not inspect their omitted fields.
    for equal in [false, true] {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder
            .filter("n", VertexPredicate::HasLabel(LabelId(1)))
            .unwrap();
        builder.identity("n", "n", equal).unwrap();
        let prepared = builder.prepare("n", 0, None).unwrap();
        assert!(
            !VertexScanPlan::compile(prepared.plan())
                .unwrap()
                .requires_properties()
        );
        assert!(
            !AsyncVertexScanPlan::compile(prepared.plan())
                .unwrap()
                .requires_properties()
        );
    }
    let probe = plan("MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n");
    let ordinary = VertexScanPlan::compile(&probe).unwrap();
    assert!(ordinary.has_graph_probes());
    assert!(
        ordinary.requires_properties(),
        "nested access never inherits a partial row"
    );
    assert!(AsyncVertexScanPlan::compile(&probe).is_err());
}

#[test]
fn property_access_proof_includes_hidden_external_sort_cells_and_empty_pages() {
    for (text, requires_properties, hidden) in [
        (
            "MATCH (n:L) RETURN n,n AS again ORDER BY n LIMIT 0",
            false,
            false,
        ),
        ("MATCH (n) RETURN n ORDER BY n.p LIMIT 0", true, true),
        (
            "MATCH (n) RETURN n.p AS value ORDER BY n LIMIT 0",
            true,
            true,
        ),
        (
            "MATCH (n) RETURN n,n.p AS value ORDER BY value LIMIT 0",
            true,
            false,
        ),
    ] {
        let logical = plan(text);
        let (compiled, tail) = AsyncVertexScanPlan::compile_sort_input(&logical).unwrap();
        assert_eq!(
            compiled.requires_properties(),
            requires_properties,
            "{text}"
        );
        assert_eq!(
            tail.evaluation_width() > tail.visible_width(),
            hidden,
            "{text}"
        );
        assert_eq!(tail.count(), Some(0));
    }
}

#[test]
fn aggregate_property_access_tracks_its_actual_input_not_just_visible_results() {
    for (text, requires_properties) in [
        ("MATCH (n) RETURN COUNT(*) AS rows", false),
        ("MATCH (n:L) RETURN COUNT(n) AS rows", false),
        ("MATCH (n:L) RETURN n,COUNT(*) AS rows GROUP BY n", false),
        ("MATCH (n) RETURN COUNT(n.p) AS present", true),
        ("MATCH (n) RETURN SUM(n.p) AS total", true),
        ("MATCH (n) RETURN SUM(n.p+1) AS total", true),
        ("MATCH (n) WHERE n.p IS NULL RETURN COUNT(*) AS rows", true),
        (
            "MATCH (n) RETURN COUNT(*) AS rows GROUP BY n ORDER BY SUM(n.p) DESC LIMIT 0",
            true,
        ),
    ] {
        let aggregate =
            PreparedGraphAggregateText::prepare(text, |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            })
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let AsyncSpillAggregatePlan::Vertex(compiled) =
            AsyncSpillAggregatePlan::compile(&aggregate).unwrap()
        else {
            panic!("expected the direct vertex aggregate input: {text}");
        };
        assert_eq!(
            compiled.requires_properties(),
            requires_properties,
            "{text}"
        );
    }
}
