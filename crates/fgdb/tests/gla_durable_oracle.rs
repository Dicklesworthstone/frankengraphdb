//! An independent nested-loop oracle remains after the production cutover.
//! It uses neither GlaPlan lowering nor VertexPredicate evaluation.

use asupersync::lab::run_async_under_lab;
use fgdb::{BoundPlan, Database, DatabaseKeys, EdgeRecord, RelationBind, VertexRow, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{EdgeDirection, ReturnProjection};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(4);
const HIGH: VId = VId((1_u128 << 96) + 7);

fn props_match(row: Option<&VertexRow>, tests: [Option<(PropertyKeyId, i64)>; 6]) -> bool {
    tests.into_iter().enumerate().all(|(comparison, test)| {
        let Some((key, expected)) = test else { return true; };
        let Some(row) = row else { return false; };
        row.props.iter().any(|(found_key, value)| {
            let CanonicalScalar::Int(actual) = value else { return false; };
            *found_key == key && match comparison {
                0 => *actual == expected,
                1 => *actual != expected,
                2 => *actual > expected,
                3 => *actual < expected,
                4 => *actual >= expected,
                _ => *actual <= expected,
            }
        })
    })
}

fn oriented(edge: &EdgeRecord, plan: &BoundPlan) -> Vec<(VId, VId)> {
    let (a, b) = (edge.entry.src, edge.entry.dst);
    match plan.direction {
        EdgeDirection::Incoming if plan.hop2_relation.is_some() => vec![(b, a)],
        EdgeDirection::Undirected if a != b => vec![(a, b), (b, a)],
        _ => vec![(a, b)],
    }
}

fn oracle(plan: &BoundPlan, vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<VId> {
    let rows: BTreeMap<_, _> = vertices.iter().map(|v| (v.vid, v)).collect();
    let source = [plan.src_prop, plan.src_prop_ne, plan.src_prop_gt, plan.src_prop_lt, plan.src_prop_ge, plan.src_prop_le];
    let destination = [plan.dst_prop, plan.dst_prop_ne, plan.dst_prop_gt, plan.dst_prop_lt, plan.dst_prop_ge, plan.dst_prop_le];
    let far = [plan.hop2_dst_prop, plan.hop2_dst_prop_ne, plan.hop2_dst_prop_gt, plan.hop2_dst_prop_lt, plan.hop2_dst_prop_ge, plan.hop2_dst_prop_le];
    let labeled = |vid, label: Option<LabelId>| label.is_none_or(|l| {
        rows.get(&vid).is_some_and(|row| row.labels.contains(&l))
    });
    let mut bag = Vec::new();
    if let Some(relation) = plan.relation {
        for first in edges.iter().filter(|edge| edge.entry.relation == relation) {
            for (a, b) in oriented(first, plan) {
                let property_destination = if plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some() { a } else { b };
                if !labeled(a, plan.src_label) || !labeled(b, plan.dst_label)
                    || !props_match(rows.get(&a).copied(), source)
                    || !props_match(rows.get(&property_destination).copied(), destination)
                    || (plan.eq.is_some() && a != b) || (plan.neq.is_some() && a == b)
                { continue; }
                if let Some(hop2) = plan.hop2_relation {
                    for second in edges.iter().filter(|edge| edge.entry.relation == hop2) {
                        for (via, c) in oriented(second, plan) {
                            if via == b && props_match(rows.get(&c).copied(), far) {
                                bag.push(match plan.projection {
                                    ReturnProjection::Source => a,
                                    ReturnProjection::Destination => b,
                                    ReturnProjection::Hop2Destination => c,
                                });
                            }
                        }
                    }
                } else {
                    bag.push(if plan.projection == ReturnProjection::Source { a } else { b });
                }
            }
        }
    } else if plan.src_label.is_some() {
        bag.extend(vertices.iter().filter(|row| labeled(row.vid, plan.src_label)
            && props_match(Some(row), source)).map(|row| row.vid));
    }
    bag.sort_unstable();
    bag.dedup();
    bag.into_iter()
        .skip(usize::try_from(plan.skip.unwrap_or(0)).unwrap_or(usize::MAX))
        .take(plan.limit.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX))
        .collect()
}

fn plans() -> Vec<BoundPlan> {
    let bind = RelationBind::new().with_relation("R", R).with_relation("S", S).with_label("L", L);
    let mut plans = Vec::new();
    for statement in ["MATCH (a)-[:R]->(b) RETURN b", "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c", "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c"] {
        let base = bind.bind(statement).expect("supported pattern");
        for direction in [EdgeDirection::Outgoing, EdgeDirection::Incoming, EdgeDirection::Undirected] {
            for projection in [ReturnProjection::Source, ReturnProjection::Destination, ReturnProjection::Hop2Destination] {
                let mut plan = base.clone();
                plan.direction = direction;
                plan.projection = projection;
                for filter in 0..24 {
                    let mut p = plan.clone();
                    match filter {
                        0 => p.src_prop = Some((P, 2)),
                        1 => p.src_prop_ne = Some((P, 2)),
                        2 => p.src_prop_gt = Some((P, 2)),
                        3 => p.src_prop_lt = Some((P, 2)),
                        4 => p.src_prop_ge = Some((P, 2)),
                        5 => p.src_prop_le = Some((P, 2)),
                        6 => p.dst_prop = Some((P, 2)),
                        7 => p.dst_prop_ne = Some((P, 2)),
                        8 => p.dst_prop_gt = Some((P, 2)),
                        9 => p.dst_prop_lt = Some((P, 2)),
                        10 => p.dst_prop_ge = Some((P, 2)),
                        11 => p.dst_prop_le = Some((P, 2)),
                        12 => p.hop2_dst_prop = Some((P, 2)),
                        13 => p.hop2_dst_prop_ne = Some((P, 2)),
                        14 => p.hop2_dst_prop_gt = Some((P, 2)),
                        15 => p.hop2_dst_prop_lt = Some((P, 2)),
                        16 => p.hop2_dst_prop_ge = Some((P, 2)),
                        17 => p.hop2_dst_prop_le = Some((P, 2)),
                        18 => p.eq = Some(("a".into(), "b".into())),
                        19 => p.neq = Some(("a".into(), "b".into())),
                        20 => { p.src_label = Some(L); p.dst_label = Some(L); }
                        21 => { p.skip = Some(1); p.limit = Some(1); }
                        22 => { p.src_prop_ge = Some((P, i64::MIN)); p.hop2_dst_prop_le = Some((P, i64::MAX)); }
                        _ => {}
                    }
                    plans.push(p);
                }
            }
        }
    }
    plans.push(bind.bind("MATCH (a:L) RETURN a").expect("node scan"));
    plans
}

#[test]
fn production_gla_answers_match_independent_enumeration_before_and_after_compaction() {
    let ((), report) = run_async_under_lab(0x61a2_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let keys = DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32]);
        let mut db = Database::open_memory(&cx, keys).await.expect("database");
        let mut seed = WriteBatch::new(R);
        for (vid, label, value) in [(VId(1), true, Some(-1)), (VId(2), true, Some(2)), (VId(3), false, Some(7)), (VId(4), true, None), (HIGH, true, Some(i64::MAX))] {
            seed.create_vertex(vid, if label { vec![L] } else { vec![] },
                value.map(|n| vec![(P, CanonicalScalar::Int(n))]).unwrap_or_default());
        }
        for (eid, a, b) in [(10, VId(1), VId(2)), (11, VId(1), VId(2)), (12, VId(2), VId(3)), (13, VId(3), VId(3)), (14, VId(4), VId(1)), (15, VId(2), HIGH)] {
            seed.add_edge(EId(eid), a, b, vec![]);
        }
        db.write(&cx, seed).await.expect("seed R");
        let mut second = WriteBatch::new(S);
        for (eid, a, b) in [(20, VId(2), VId(4)), (21, VId(3), VId(1)), (22, HIGH, VId(1))] {
            second.add_edge(EId(eid), a, b, vec![]);
        }
        db.write(&cx, second).await.expect("seed S");
        let basis = db.frontier().expect("basis");
        let pinned = db.read_session().expect("pin");
        let before_vertices = db.vertices().expect("independent source vertices");
        let before_edges = db.edges().expect("independent source edges");
        let plans = plans();
        assert_eq!(plans.len(), 649);
        for plan in &plans {
            let expected = oracle(plan, &before_vertices, &before_edges);
            assert_eq!(db.execute_prepared_gql(plan).expect("production GLA"), expected, "{plan:?}");
            assert_eq!(pinned.execute_prepared_gql(plan).expect("pinned GLA"), expected);
        }
        let mut update = WriteBatch::new(R);
        update.delete_edge(EId(12));
        update.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(i64::MIN)));
        update.set_vertex_label(VId(3), L, true);
        db.write(&cx, update).await.expect("change the graph");
        db.compact(&cx).await.expect("consolidate physical history");
        let vertices = db.vertices().expect("current source vertices");
        let edges = db.edges().expect("current source edges");
        for plan in &plans {
            let expected = oracle(plan, &vertices, &edges);
            let historical = oracle(plan, &before_vertices, &before_edges);
            assert_eq!(db.execute_prepared_gql(plan).expect("compacted GLA"), expected, "{plan:?}");
            assert_eq!(db.execute_prepared_gql_at(plan, basis).expect("historical GLA"), historical);
            assert_eq!(pinned.execute_prepared_gql(plan).expect("old immutable view"), historical);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
