#![recursion_limit = "256"]

//! Generated native write semantics, independently applied to ReferenceGraph.
//! No engine deltas or evaluator outputs are used to compute expected state.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteScript,
};
use fgdb_reference::ReferenceGraph;
use fgdb_types::{
    CanonicalScalar, CanonicalText, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const KEY: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const L: LabelId = LabelId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(KEY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        _ => None,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
        10_000,
        100,
        100,
    )
}
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}
fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}
fn create_vertex(
    model: &mut ReferenceGraph,
    id: VId,
    labels: Vec<LabelId>,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
) {
    model
        .apply_row(&DeltaRow::CreateVertex {
            vid: id,
            birth_ordinal: id.0 as u64,
            labels,
            props,
            valid_time: None,
        })
        .unwrap();
}
fn create_edge(
    model: &mut ReferenceGraph,
    id: EId,
    src: VId,
    dst: VId,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
) {
    model
        .apply_row(&DeltaRow::CreateEdge {
            eid: id,
            birth_ordinal: id.0 as u64,
            src,
            relation: R,
            dst,
            canonical_key: None,
            props,
            valid_time: None,
        })
        .unwrap();
}
fn property(
    model: &mut ReferenceGraph,
    elem: ElementId,
    key: PropertyKeyId,
    after: Option<CanonicalScalar>,
) {
    let before = match elem {
        ElementId::Vertex(id) => model.vertex(id).unwrap().props.get(&key),
        ElementId::Edge(id) => model.edge(id).unwrap().props.get(&key),
    }
    .cloned();
    model
        .apply_row(&DeltaRow::Property {
            elem,
            property: key,
            before,
            after,
        })
        .unwrap();
}
fn selected(model: &ReferenceGraph, key: i64) -> Vec<VId> {
    model
        .iter_vertices()
        .filter(|(_, v)| v.props.get(&KEY) == Some(&int(key)))
        .map(|(id, _)| id)
        .collect()
}
#[derive(Clone)]
enum Op {
    Set(i64, PropertyKeyId, Option<CanonicalScalar>),
    Label(i64, bool),
    Delete(i64, bool),
    EdgeDelete(i64, i64),
    VertexMerge(i64, i64, i64, VId),
    EdgeMerge(i64, i64, i64, i64, EId),
}
#[derive(Default)]
struct Coverage {
    families: [usize; 11],
    vertex_branches: [usize; 2],
    edge_branches: [usize; 2],
    cascades: usize,
    refusals: usize,
}
impl Op {
    fn apply(&self, model: &mut ReferenceGraph, coverage: &mut Coverage) -> Result<(), ()> {
        match self {
            Self::Set(key, prop, value) => {
                for id in selected(model, *key) {
                    property(model, ElementId::Vertex(id), *prop, value.clone());
                }
            }
            Self::Label(key, after) => {
                for id in selected(model, *key) {
                    let before = model.vertex(id).unwrap().labels.contains(&L);
                    model
                        .apply_row(&DeltaRow::LabelMembership {
                            vid: id,
                            label: L,
                            before,
                            after: *after,
                        })
                        .unwrap();
                }
            }
            Self::Delete(key, detach) => {
                let ids = selected(model, *key);
                if !detach && ids.iter().any(|id| !model.incident_edges(*id).is_empty()) {
                    return Err(());
                }
                for id in ids {
                    let incident = model.incident_edges(id);
                    if !incident.is_empty() {
                        coverage.cascades += 1;
                    }
                    model
                        .apply_row(&DeltaRow::DeleteVertex {
                            vid: id,
                            before_version: model.vertex(id).unwrap().version,
                            sorted_retired_incident_edges: incident,
                        })
                        .unwrap();
                }
            }
            Self::EdgeDelete(left, right) => {
                let sources = selected(model, *left);
                let targets = selected(model, *right);
                let edges: Vec<_> = model
                    .iter_edges()
                    .filter(|(_, e)| {
                        e.relation == R && sources.contains(&e.src) && targets.contains(&e.dst)
                    })
                    .map(|(id, e)| (id, e.version))
                    .collect();
                for (eid, before_version) in edges {
                    model
                        .apply_row(&DeltaRow::DeleteEdge {
                            eid,
                            before_version,
                        })
                        .unwrap();
                }
            }
            Self::VertexMerge(key, fresh, matched, allocated) => {
                let ids = selected(model, *key);
                assert!(ids.len() <= 1, "generator guarantees unique keys");
                let (id, value) = if let Some(id) = ids.first() {
                    coverage.vertex_branches[1] += 1;
                    (*id, *matched)
                } else {
                    coverage.vertex_branches[0] += 1;
                    create_vertex(model, *allocated, vec![], vec![(KEY, int(*key))]);
                    (*allocated, *fresh)
                };
                property(model, ElementId::Vertex(id), Q, Some(int(value)));
            }
            Self::EdgeMerge(left, right, fresh, matched, allocated) => {
                let sources = selected(model, *left);
                let targets = selected(model, *right);
                assert_eq!((sources.len(), targets.len()), (1, 1));
                let existing: Vec<_> = model
                    .iter_edges()
                    .filter(|(_, e)| e.src == sources[0] && e.dst == targets[0] && e.relation == R)
                    .map(|(id, _)| id)
                    .collect();
                assert!(existing.len() <= 1, "generator avoids ambiguous MERGE");
                let (id, value) = if let Some(id) = existing.first() {
                    coverage.edge_branches[1] += 1;
                    (*id, *matched)
                } else {
                    coverage.edge_branches[0] += 1;
                    create_edge(model, *allocated, sources[0], targets[0], vec![]);
                    (*allocated, *fresh)
                };
                property(model, ElementId::Edge(id), Q, Some(int(value)));
            }
        }
        Ok(())
    }
}
struct Case {
    text: String,
    ops: Vec<Op>,
    family: usize,
    allocation: Option<ElementId>,
}
fn case(text: String, op: Op, family: usize) -> Case {
    Case {
        text,
        ops: vec![op],
        family,
        allocation: None,
    }
}
// Three disjoint components allow destructive operations in every round, not
// just one cascade followed by dozens of vacuous no-op statements.
fn generated(
    seed: u64,
) -> (
    Vec<(VId, Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>,
    Vec<(EId, VId, VId)>,
    Vec<Case>,
) {
    let mut rng = Rng(seed);
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    let mut cases = Vec::new();
    for round in 0..3_i64 {
        let base = 1 + round * 10;
        for offset in 0..6 {
            let key = base + offset;
            let labels = if rng.next() % 2 == 0 { vec![L] } else { vec![] };
            let value = match offset % 3 {
                0 => int((rng.next() % 100) as i64),
                1 => CanonicalScalar::Text(
                    CanonicalText::new_ucs_basic(&format!("seed{seed}-{}", rng.next())).unwrap(),
                ),
                _ => CanonicalScalar::Null,
            };
            vertices.push((VId(key as u128), labels, vec![(KEY, int(key)), (P, value)]));
        }
        // Parallel edges, a loop, and a generated branch. base+5 is isolated.
        for (index, (s, d)) in [
            (0, 1),
            (0, 1),
            (1, 1),
            (1, 2),
            (2, 3 + (rng.next() % 2) as i64),
        ]
        .into_iter()
        .enumerate()
        {
            edges.push((
                EId(100 + round as u128 * 10 + index as u128),
                VId((base + s) as u128),
                VId((base + d) as u128),
            ));
        }
        let v = (rng.next() % 1000) as i64 + 1;
        for (literal, value) in [
            (v.to_string(), int(v)),
            ("NULL".into(), CanonicalScalar::Null),
        ] {
            cases.push(case(
                format!("MATCH (n) WHERE n.key={base} SET n.p={literal}"),
                Op::Set(base, P, Some(value)),
                0,
            ));
        }
        cases.push(case(
            format!("MATCH (n) WHERE n.key={base} REMOVE n.p"),
            Op::Set(base, P, None),
            1,
        ));
        for present in [true, false] {
            let verb = if present { "SET" } else { "REMOVE" };
            cases.push(case(
                format!("MATCH (n) WHERE n.key={base} {verb} n:L"),
                Op::Label(base, present),
                if present { 2 } else { 3 },
            ));
        }
        cases.push(case(
            format!("MATCH (n) WHERE n.key={} DELETE n", base + 5),
            Op::Delete(base + 5, false),
            4,
        ));
        cases.push(case(
            format!("MATCH (n) WHERE n.key={base} DELETE n"),
            Op::Delete(base, false),
            5,
        ));
        let new_key = 1000 + round;
        for matched in [false, true] {
            let id = VId(2000 + round as u128);
            let mut c = case(
                format!(
                    "MERGE (n {{key:{new_key}}}) ON CREATE SET n.q={v} ON MATCH SET n.q={}",
                    v + 1
                ),
                Op::VertexMerge(new_key, v, v + 1, id),
                6,
            );
            if !matched {
                c.allocation = Some(ElementId::Vertex(id));
            }
            cases.push(c);
        }
        for matched in [false, true] {
            let id = EId(3000 + round as u128);
            let mut c = case(
                format!(
                    "MATCH (a),(b) WHERE a.key={new_key} AND b.key={} MERGE (a)-[e:R]->(b) ON CREATE SET e.q={v} ON MATCH SET e.q={}",
                    base + 4,
                    v + 1
                ),
                Op::EdgeMerge(new_key, base + 4, v, v + 1, id),
                7,
            );
            if !matched {
                c.allocation = Some(ElementId::Edge(id));
            }
            cases.push(c);
        }
        cases.push(Case { text: format!("MATCH (n) WHERE n.key={base} SET n.q={v}; MATCH (n) WHERE n.key={base} SET n:L; MATCH (n) WHERE n.key={base} REMOVE n.q"),
            ops: vec![Op::Set(base, Q, Some(int(v))), Op::Label(base, true), Op::Set(base, Q, None)], family: 8, allocation: None });
        cases.push(Case {
            text: format!(
                "MATCH (n) WHERE n.key={base} SET n.q={v}; MATCH (n) WHERE n.key={base} DELETE n"
            ),
            ops: vec![Op::Set(base, Q, Some(int(v))), Op::Delete(base, false)],
            family: 8,
            allocation: None,
        });
        cases.push(case(
            format!(
                "MATCH (a)-[e:R]->(b) WHERE a.key={base} AND b.key={} DELETE e",
                base + 1
            ),
            Op::EdgeDelete(base, base + 1),
            10,
        ));
        cases.push(case(
            format!("MATCH (n) WHERE n.key={} DETACH DELETE n", base + 1),
            Op::Delete(base + 1, true),
            9,
        ));
    }
    (vertices, edges, cases)
}
#[derive(Debug, PartialEq, Eq)]
struct State {
    vertices: Vec<(VId, Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>,
    edges: Vec<(
        EId,
        VId,
        RelationId,
        VId,
        Vec<(PropertyKeyId, CanonicalScalar)>,
    )>,
}
fn actual(db: &Database<MemVfs>) -> State {
    State {
        vertices: db
            .vertices()
            .unwrap()
            .into_iter()
            .map(|v| (v.vid, v.labels, v.props))
            .collect(),
        edges: db
            .edges()
            .unwrap()
            .into_iter()
            .map(|e| {
                (
                    e.entry.eid,
                    e.entry.src,
                    e.entry.relation,
                    e.entry.dst,
                    e.props,
                )
            })
            .collect(),
    }
}
fn expected(model: &ReferenceGraph) -> State {
    State {
        vertices: model
            .iter_vertices()
            .map(|(id, v)| {
                (
                    id,
                    v.labels.iter().copied().collect(),
                    v.props.iter().map(|(k, v)| (*k, v.clone())).collect(),
                )
            })
            .collect(),
        edges: model
            .iter_edges()
            .map(|(id, e)| {
                (
                    id,
                    e.src,
                    e.relation,
                    e.dst,
                    e.props.iter().map(|(k, v)| (*k, v.clone())).collect(),
                )
            })
            .collect(),
    }
}
#[test]
fn generated_native_mutations_match_reference_and_reopen() {
    for seed in [0x4f7a_0001_u64, 0x4f7a_1032, 0x4f7a_2953, 0x4f7a_4914] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let (vertices, edges, cases) = generated(seed);
            let mut model = ReferenceGraph::new();
            let mut batch = WriteBatch::new(R);
            for (id, labels, props) in vertices {
                batch.create_vertex(id, labels.clone(), props.clone());
                create_vertex(&mut model, id, labels, props);
            }
            for (id, src, dst) in edges {
                let props = vec![(Q, int(id.0 as i64))];
                batch.add_edge(id, src, dst, props.clone());
                create_edge(&mut model, id, src, dst, props);
            }
            db.write(&commit, batch).await.unwrap();
            assert_eq!(
                actual(&db),
                expected(&model),
                "seed {seed:#x}: initial state"
            );
            assert!(cases.len() >= 30);
            let mut coverage = Coverage::default();
            for (index, case) in cases.into_iter().enumerate() {
                let before = actual(&db);
                let frontier = db.frontier().unwrap();
                let mut candidate = model.clone();
                let accepted = case
                    .ops
                    .iter()
                    .try_for_each(|op| op.apply(&mut candidate, &mut coverage))
                    .is_ok();
                let script = PreparedGraphWriteScript::prepare(&case.text, R, symbols)
                    .unwrap_or_else(|error| {
                        panic!(
                            "seed {seed:#x}, statement {index}: {}: {error:?}",
                            case.text
                        )
                    });
                let mut allocations = 0;
                let result = db
                    .execute_graph_write_script_autocommit_governed(
                        &txcx,
                        &query,
                        &commit,
                        &script,
                        &GqlParameters::new(),
                        policy(),
                        |_| {
                            allocations += 1;
                            case.allocation.ok_or(())
                        },
                    )
                    .await;
                assert_eq!(
                    result.is_ok(),
                    accepted,
                    "seed {seed:#x}, statement {index}: {}: {result:?}",
                    case.text
                );
                coverage.families[case.family] += 1;
                if accepted {
                    model = candidate;
                    assert_eq!(
                        allocations,
                        usize::from(case.allocation.is_some()),
                        "{}",
                        case.text
                    );
                } else {
                    coverage.refusals += 1;
                    assert_eq!(actual(&db), before, "refusal changed state: {}", case.text);
                    assert_eq!(
                        db.frontier().unwrap(),
                        frontier,
                        "refusal published: {}",
                        case.text
                    );
                }
                assert_eq!(
                    actual(&db),
                    expected(&model),
                    "seed {seed:#x}, statement {index}: {}",
                    case.text
                );
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
            assert!(
                coverage.families.iter().all(|count| *count >= 3),
                "{:?}",
                coverage.families
            );
            assert_eq!(coverage.vertex_branches, [3, 3]);
            assert_eq!(coverage.edge_branches, [3, 3]);
            assert_eq!(coverage.cascades, 3);
            assert_eq!(coverage.refusals, 6);
            let frontier = db.frontier().unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(
                actual(&db),
                expected(&model),
                "seed {seed:#x}: reopened state"
            );
        });
        assert!(report.lab_test_passed(), "seed {seed:#x}: {report:?}");
    }
}
