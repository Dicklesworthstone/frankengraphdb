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
        ("MATCH (n) WHERE id(n)>=0 RETURN n", false),
        ("MATCH (n) WHERE n=n OR n<>n RETURN n", false),
        ("MATCH (n) WHERE n IS NOT NULL RETURN n", false),
        ("MATCH (n) WHERE 1+2=3 RETURN n", false),
        ("MATCH (n) WHERE false OR true RETURN n", false),
        ("MATCH (n) WHERE 1/0=0 RETURN n LIMIT 0", false),
        ("MATCH (n) RETURN n,n.p", true),
        ("MATCH (n) WHERE n.p=1 RETURN n", true),
        ("MATCH (n) WHERE n.p IS NULL RETURN n", true),
        ("MATCH (n) WHERE n.p IS NOT NULL RETURN n", true),
        ("MATCH (n) WHERE n.p=n.p RETURN n", true),
        ("MATCH (n) WHERE n.p>1 OR n.p<0 RETURN n", true),
        ("MATCH (n) WHERE true OR n.p=1 RETURN n", true),
        ("MATCH (n) WHERE false AND n.p=1 RETURN n", true),
        ("MATCH (n) WHERE coalesce(n.p,1)=1 RETURN n", true),
        (
            "MATCH (n) WHERE coalesce(CASE WHEN true THEN 1 ELSE n.p END,0)=1 RETURN n",
            true,
        ),
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
            "MATCH (n) WHERE id(n)>=0 RETURN n ORDER BY n LIMIT 0",
            false,
            false,
        ),
        (
            "MATCH (n) WHERE id(n)>=0 RETURN n ORDER BY n.p LIMIT 0",
            true,
            true,
        ),
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
        ("MATCH (n) WHERE id(n)>=0 RETURN COUNT(*) AS rows", false),
        (
            "MATCH (n) WHERE 1/0=0 RETURN COUNT(*) AS rows LIMIT 0",
            false,
        ),
        (
            "MATCH (n) WHERE true OR n.p=1 RETURN COUNT(*) AS rows LIMIT 0",
            true,
        ),
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

fn selected_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => {
            Some(GraphSymbol::Relation(fgdb_delta_types::RelationId(1)))
        }
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        // The low 32 bits deliberately match p. Demand is keyed by the complete
        // identity, never a truncated physical slot or dictionary position.
        (GraphSymbolKind::Property, "q") => {
            Some(GraphSymbol::Property(PropertyKeyId((1_u64 << 40) | 1)))
        }
        _ => None,
    }
}

fn selected_plan(text: &str) -> GlaPlan<GraphValueRow> {
    PreparedGraphText::prepare(text, selected_symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .plan()
        .clone()
}

#[test]
fn selected_vertex_property_demand_unifies_complete_local_operands() {
    use VertexScanPropertyDemand::{Complete, Topology, VertexProperty};

    let p = VertexProperty(PropertyKeyId(1));
    let q = VertexProperty(PropertyKeyId((1_u64 << 40) | 1));
    for (text, expected) in [
        ("MATCH (n:L) WHERE id(n)>=0 RETURN n,n AS again", Topology),
        ("MATCH (n) RETURN n,n.p,n.p AS again", p),
        ("MATCH (n) RETURN n,n.q", q),
        ("MATCH (n) WHERE n.p=1 RETURN n,n.p", p),
        ("MATCH (n) WHERE n.p='one' RETURN n,n.p", p),
        ("MATCH (n) WHERE n.p IS NULL RETURN n", p),
        ("MATCH (n) WHERE n.p IS NOT NULL RETURN n,n.p", p),
        ("MATCH (n) WHERE n.p=n.p RETURN n,n.p", p),
        ("MATCH (n) WHERE n.p>1 OR n.p<0 RETURN n,n.p", p),
        ("MATCH (n) WHERE true OR n.p=1 RETURN n", p),
        ("MATCH (n) WHERE false AND n.p=1 RETURN n", p),
        ("MATCH (n) WHERE coalesce(n.p,n.p,1)=1 RETURN n,n.p", p),
        (
            "MATCH (n) WHERE coalesce(CASE WHEN true THEN 1 ELSE n.p END,0)=1 RETURN n",
            p,
        ),
        ("MATCH (n) WHERE 1/(n.p-1)>0 RETURN n LIMIT 0", p),
        ("MATCH (n) WHERE n<>n RETURN n,n.p", p),
        ("MATCH (n) RETURN n,n.p,n.q", Complete),
        ("MATCH (n) WHERE n.p=n.q RETURN n", Complete),
        ("MATCH (n) WHERE n.q IS NULL RETURN n,n.p", Complete),
        ("MATCH (n) WHERE true OR n.q=1 RETURN n,n.p", Complete),
        ("MATCH (n) WHERE false AND n.q=1 RETURN n,n.p", Complete),
        (
            "MATCH (n) WHERE coalesce(n.p,n.q)=1 RETURN n LIMIT 0",
            Complete,
        ),
        (
            "MATCH (n) WHERE coalesce(CASE WHEN true THEN n.p ELSE n.q END,0)=1 RETURN n",
            Complete,
        ),
    ] {
        let logical = selected_plan(text);
        let ordinary = VertexScanPlan::compile(&logical).unwrap();
        let asynchronous = AsyncVertexScanPlan::compile(&logical).unwrap();
        assert_eq!(ordinary.property_demand(), expected, "{text}");
        assert_eq!(asynchronous.property_demand(), expected, "{text}");
        assert_eq!(
            ordinary.requires_properties(),
            expected != Topology,
            "{text}"
        );
        assert_eq!(
            asynchronous.requires_properties(),
            expected != Topology,
            "{text}"
        );
    }
}

#[test]
fn selected_vertex_property_demand_keeps_hidden_sort_and_aggregate_inputs() {
    use VertexScanPropertyDemand::{Complete, Topology, VertexProperty};

    let p = VertexProperty(PropertyKeyId(1));
    let q = VertexProperty(PropertyKeyId((1_u64 << 40) | 1));
    for (text, expected) in [
        ("MATCH (n) RETURN n ORDER BY n LIMIT 0", Topology),
        ("MATCH (n) RETURN n ORDER BY n.p LIMIT 0", p),
        ("MATCH (n) RETURN n.p AS value ORDER BY n LIMIT 0", p),
        ("MATCH (n) RETURN n,n.p AS value ORDER BY value LIMIT 0", p),
        ("MATCH (n) WHERE n.p>0 RETURN n ORDER BY n.p LIMIT 0", p),
        ("MATCH (n) RETURN n,n.p ORDER BY n.q LIMIT 0", Complete),
        (
            "MATCH (n) WHERE n.q IS NULL RETURN n ORDER BY n.p LIMIT 0",
            Complete,
        ),
    ] {
        let logical = selected_plan(text);
        let (compiled, tail) = AsyncVertexScanPlan::compile_sort_input(&logical).unwrap();
        assert_eq!(compiled.property_demand(), expected, "{text}");
        assert_eq!(
            compiled.requires_properties(),
            expected != Topology,
            "{text}"
        );
        assert_eq!(tail.count(), Some(0));
    }
    for (text, expected) in [
        ("MATCH (n) RETURN COUNT(*) AS rows", Topology),
        ("MATCH (n:L) RETURN n,COUNT(*) AS rows GROUP BY n", Topology),
        ("MATCH (n) RETURN COUNT(n.p) AS present", p),
        ("MATCH (n) RETURN SUM(n.p) AS total", p),
        ("MATCH (n) RETURN SUM(n.q) AS total", q),
        ("MATCH (n) RETURN SUM(n.p+n.p) AS total", p),
        (
            "MATCH (n) WHERE n.p>0 RETURN COUNT(n.p) AS present,SUM(n.p) AS total",
            p,
        ),
        (
            "MATCH (n) WHERE true OR n.p=1 RETURN COUNT(*) AS rows LIMIT 0",
            p,
        ),
        (
            "MATCH (n) RETURN COUNT(*) AS rows GROUP BY n ORDER BY SUM(n.p) DESC LIMIT 0",
            p,
        ),
        ("MATCH (n) RETURN SUM(n.p+n.q) AS total", Complete),
        (
            "MATCH (n) WHERE true OR n.q=1 RETURN SUM(n.p) AS total LIMIT 0",
            Complete,
        ),
        (
            "MATCH (n) RETURN n.p AS bucket,COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.q) DESC LIMIT 0",
            Complete,
        ),
    ] {
        let aggregate = PreparedGraphAggregateText::prepare(text, selected_symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let AsyncSpillAggregatePlan::Vertex(compiled) =
            AsyncSpillAggregatePlan::compile(&aggregate).unwrap()
        else {
            panic!("expected the direct vertex aggregate input: {text}");
        };
        assert_eq!(compiled.property_demand(), expected, "{text}");
        assert_eq!(
            compiled.requires_properties(),
            expected != Topology,
            "{text}"
        );
    }
}

#[test]
fn selected_vertex_property_demand_retains_complete_probe_and_unknown_programs() {
    use VertexScanPropertyDemand::Complete;

    for text in [
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n,n.p",
        "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(m) } RETURN n,n.p",
    ] {
        let logical = selected_plan(text);
        let ordinary = VertexScanPlan::compile(&logical).unwrap();
        assert!(ordinary.has_graph_probes());
        assert_eq!(ordinary.property_demand(), Complete, "{text}");
        assert!(AsyncVertexScanPlan::compile(&logical).is_err());
    }

    // Unknown future bindings/projections cannot accidentally inherit the
    // selected-field proof. These cannot be produced by today's compiler.
    let logical = selected_plan("MATCH (n) RETURN n,n.p");
    let mut unknown_binding = VertexScanPlan::compile(&logical).unwrap();
    unknown_binding.tests = vec![Test::Binding(GlaOperator::Distinct)].into();
    assert_eq!(unknown_binding.property_demand(), Complete);
    let mut unknown_projection = VertexScanPlan::compile(&logical).unwrap();
    unknown_projection.projection = Arc::new(GlaOperator::Empty);
    assert_eq!(unknown_projection.property_demand(), Complete);
}
