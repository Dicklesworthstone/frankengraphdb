//! Sources may omit property payloads only when the complete edge evaluator
//! proves that topology and labels suffice, including private input columns.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::{AsyncEdgeScanPlan, EdgeScanPlan};
use fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan;
use fgdb_gql::{
    GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText,
};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}

fn prepared(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

#[test]
fn local_edge_property_demand_preserves_labels_identity_and_path_values() {
    for (text, requires_properties) in [
        ("MATCH (a)-[r:R]->(b) RETURN r,a,b", false),
        ("MATCH (a)<-[r:R]-(b) RETURN r,a,b", false),
        ("MATCH (a)-[r:R]-(b) RETURN r,a,b", false),
        ("MATCH (a)-[r]->(b) RETURN r,a,b", false),
        ("MATCH (a:L)-[r:R]->(b:L) RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(a) RETURN r,a", false),
        ("MATCH (a)-[r:R]->(b) WHERE a=b RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE a<>b RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE id(a)<id(b) RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE id(r)>=0 RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE r=r OR a=b RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE NOT (a=b) RETURN r,a,b", false),
        ("MATCH (a)-[r:R]->(b) WHERE 1+2=3 RETURN r,a,b", false),
        (
            "MATCH (a)-[r:R]->(b) WHERE 1/0=0 RETURN r,a,b LIMIT 0",
            false,
        ),
        (
            "MATCH p=(a)-[r:R]->(b) RETURN r,a,b,p,length(p) AS hops,nodes(p) AS ns,relationships(p) AS es",
            false,
        ),
        ("MATCH (a)-[r:R]->(b) RETURN r,a.p", true),
        ("MATCH (a)-[r:R]->(b) RETURN r,b.p", true),
        ("MATCH (a)-[r:R]->(b) RETURN r,r.p", true),
        ("MATCH (a:L)-[r:R]->(b) WHERE a.p=1 RETURN r,a,b", true),
        ("MATCH (a)-[r:R]->(b) WHERE a.p IS NULL RETURN r,a,b", true),
        (
            "MATCH (a)-[r:R]->(b) WHERE b.p IS NOT NULL RETURN r,a,b",
            true,
        ),
        ("MATCH (a)-[r:R]->(b) WHERE r.p=1 RETURN r,a,b", true),
        ("MATCH (a)-[r:R]->(b) WHERE a.p=b.p RETURN r,a,b", true),
        ("MATCH (a)-[r:R]->(b) WHERE true OR a.p=1 RETURN r,a,b", true),
        ("MATCH (a)-[r:R]->(b) WHERE true OR b.p=1 RETURN r,a,b", true),
        ("MATCH (a)-[r:R]->(b) WHERE true OR r.p=1 RETURN r,a,b", true),
        (
            "MATCH (a)-[r:R]->(b) WHERE false AND r.p=1 RETURN r,a,b",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE coalesce(r.p,1)=1 RETURN r,a,b",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE coalesce(CASE WHEN true THEN 1 ELSE r.p END,0)=1 RETURN r,a,b",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE a.p>1 OR b.p<0 RETURN r,a,b",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE 1/(a.p-1)>0 RETURN r,a,b LIMIT 0",
            true,
        ),
        ("MATCH (a)-[r:R]->(b) WHERE a<>a RETURN r,a.p", true),
    ] {
        let query = prepared(text);
        let definition = query.plan().canonical_bytes();
        let ordinary =
            EdgeScanPlan::compile(query.plan()).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        let asynchronous = AsyncEdgeScanPlan::compile(query.plan())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
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
        assert_eq!(query.plan().canonical_bytes(), definition);
    }
}

#[test]
fn property_demand_includes_hidden_sort_cells_even_on_empty_pages() {
    for (text, requires_properties, hidden) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN r,a ORDER BY r LIMIT 0",
            false,
            false,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN a ORDER BY r LIMIT 0",
            false,
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN r ORDER BY a.p LIMIT 0",
            true,
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE id(r)>=0 RETURN a ORDER BY r LIMIT 0",
            false,
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE id(r)>=0 RETURN r ORDER BY a.p LIMIT 0",
            true,
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN r.p AS value ORDER BY r LIMIT 0",
            true,
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN r,r.p AS value ORDER BY value LIMIT 0",
            true,
            false,
        ),
    ] {
        let query = prepared(text);
        let (input, tail) = AsyncEdgeScanPlan::compile_sort_input(query.plan())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(input.requires_properties(), requires_properties, "{text}");
        assert_eq!(
            tail.evaluation_width() > tail.visible_width(),
            hidden,
            "{text}"
        );
        assert_eq!(tail.count(), Some(0));
    }
}

#[test]
fn aggregate_property_demand_tracks_arguments_grouping_and_hidden_summaries() {
    for (text, requires_properties) in [
        ("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows", false),
        ("MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS rows", false),
        ("MATCH (a:L)-[r:R]->(b) RETURN COUNT(r) AS rows", false),
        (
            "MATCH (a)-[r:R]->(b) WHERE id(r)>=0 RETURN COUNT(*) AS rows",
            false,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE 1/0=0 RETURN COUNT(*) AS rows LIMIT 0",
            false,
        ),
        (
            "MATCH (a)-[r:R]->(b) WHERE true OR r.p=1 RETURN COUNT(*) AS rows LIMIT 0",
            true,
        ),
        ("MATCH (a)-[r:R]->(b) RETURN COUNT(a) AS rows", false),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS rows",
            false,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN a,COUNT(*) AS rows GROUP BY a",
            false,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows ORDER BY rows LIMIT 0",
            false,
        ),
        ("MATCH (a)-[r:R]->(b) RETURN COUNT(r.p) AS present", true),
        ("MATCH (a)-[r:R]->(b) RETURN SUM(r.p) AS total", true),
        ("MATCH (a)-[r:R]->(b) RETURN SUM(r.p+1) AS total", true),
        ("MATCH (a)-[r:R]->(b) RETURN SUM(a.p+b.p) AS total", true),
        (
            "MATCH (a)-[r:R]->(b) WHERE r.p IS NULL RETURN COUNT(*) AS rows",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows GROUP BY r.p",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows GROUP BY a ORDER BY SUM(r.p+1) DESC LIMIT 0",
            true,
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows HAVING SUM(r.p)>0",
            true,
        ),
    ] {
        let aggregate = PreparedGraphAggregateText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let AsyncSpillAggregatePlan::Edge(input) = AsyncSpillAggregatePlan::compile(&aggregate)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        else {
            panic!("expected a single-edge aggregate input: {text}");
        };
        assert_eq!(input.requires_properties(), requires_properties, "{text}");
    }
}

#[test]
fn joined_and_probed_edge_plans_keep_complete_property_demand() {
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,a,b,c",
        "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN r,a,b",
        "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R]->(c) WHERE c.p=1 } RETURN r,a,b LIMIT 0",
    ] {
        let query = prepared(text);
        let ordinary =
            EdgeScanPlan::compile(query.plan()).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert!(ordinary.requires_properties(), "{text}");
        assert!(AsyncEdgeScanPlan::compile(query.plan()).is_err(), "{text}");
        assert!(
            AsyncEdgeScanPlan::compile_sort_input(query.plan()).is_err(),
            "{text}"
        );
    }
}
