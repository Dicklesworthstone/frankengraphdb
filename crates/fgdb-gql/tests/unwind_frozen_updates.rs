//! Public UNWIND binding and actual GLA/mutation kernels, with hand-enumerated
//! graph expectations. The small host below implements the documented private
//! workspace seam; durable staging and authorization have separate fgdb laws.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::unwind_write::{
    GraphUnwindBindError, GraphUnwindBindEvent, GraphUnwindWriteError, GraphUnwindWriteText,
};
use fgdb_gql::{
    BoundGraphWriteScriptBatch, GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy, GraphIntegerErrorKind, GraphMutationAction,
    GraphMutationBatch, GraphMutationError, GraphMutationIntent, GraphMutationProgramDimension,
    GraphMutationProgramError, GraphMutationStats, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWriteProgramError, GraphWriteProgramPolicy, GraphWriteProgramStats,
    GraphWriteScriptExecutionError, GraphWriteStatement, GraphWriteStepError, GraphWriteStepStats,
    PreparedGraphMutation, PreparedGraphMutationProgram, PreparedGraphWriteProgram,
    ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const ID: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
const Q: PropertyKeyId = PropertyKeyId(3);
const REKEY: &str = "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=row.b";
type Fault = GraphWriteProgramError<(), (), usize>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(ID)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

struct Catalog;
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(ID, "id");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}

fn arguments(rows: &[(i64, i64)]) -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "rows",
            rows.iter()
                .map(|&(a, b)| {
                    GraphValue::map(vec![
                        ("a".into(), GraphValue::Scalar(CanonicalScalar::Int(a))),
                        ("b".into(), GraphValue::Scalar(CanonicalScalar::Int(b))),
                    ])
                    .unwrap()
                })
                .collect(),
        )
        .unwrap()
}

fn bind(text: &str, rows: &[(i64, i64)]) -> BoundGraphWriteScriptBatch {
    GraphUnwindWriteText::parse(text)
        .unwrap()
        .bind_with_limit_resolver(&arguments(rows), R, 64, Catalog)
        .unwrap()
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(100_000, 100_000, 2_000_000, 1_000_000),
        100_000,
        0,
        0,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Vertex {
    labels: BTreeSet<LabelId>,
    properties: BTreeMap<PropertyKeyId, CanonicalPropertyValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Graph {
    vertices: BTreeMap<VId, Vertex>,
    edges: Vec<(VId, RelationId, VId)>,
}

impl Graph {
    fn new(vertices: &[(u128, i64, i64)]) -> Self {
        Self {
            vertices: vertices
                .iter()
                .map(|&(vertex, id, p)| {
                    (
                        VId(vertex),
                        Vertex {
                            labels: BTreeSet::from([L]),
                            properties: BTreeMap::from([
                                (ID, CanonicalScalar::Int(id).into()),
                                (P, CanonicalScalar::Int(p).into()),
                            ]),
                        },
                    )
                })
                .collect(),
            edges: Vec::new(),
        }
    }

    fn stage(&mut self, proposal: GraphMutationBatch, control: &Control) -> Result<(), usize> {
        for intent in proposal.into_intents() {
            control.checkpoint()?;
            match intent {
                GraphMutationIntent::Property { vertex, key, value } => {
                    let properties = &mut self.vertices.get_mut(&vertex).unwrap().properties;
                    match value {
                        Some(value) => {
                            properties.insert(key, value);
                        }
                        None => {
                            properties.remove(&key);
                        }
                    }
                }
                GraphMutationIntent::Label {
                    vertex,
                    label,
                    present,
                } => {
                    let labels = &mut self.vertices.get_mut(&vertex).unwrap().labels;
                    if present {
                        labels.insert(label);
                    } else {
                        labels.remove(&label);
                    }
                }
                _ => panic!("the fixture executes only vertex field updates"),
            }
        }
        Ok(())
    }

    fn select(
        &self,
        selection: &PreparedGraphPattern<GraphValueRow>,
        allowance: GqlQueryPolicy,
        control: &Control,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
        let test_vertex = |vertex, predicates: &[VertexPredicate]| {
            let vertex = &self.vertices[&vertex];
            Ok::<_, ()>(predicates.iter().all(|predicate| {
                predicate.matches_borrowed(
                    vertex.labels.iter().copied(),
                    vertex.properties.iter().map(|(key, value)| (*key, value)),
                )
            }))
        };
        let property = |vertex, key| Ok(self.vertices[&vertex].properties.get(&key));
        let records = (self.vertices.len() + self.edges.len()) as u64;
        if !selection.plan().projects_property_maps() {
            return selection.plan().execute_governed_with_properties(
                records,
                self.vertices.keys().copied(),
                self.edges.iter().copied(),
                test_vertex,
                property,
                allowance,
                || control.checkpoint(),
            );
        }
        // SET n={...} needs the complete original property map. Supply it via
        // the actual element-map accessor, including the reverse catalog used
        // by ClearProperties; no guessed projection or fake source row.
        let maps: BTreeMap<_, _> = self
            .vertices
            .iter()
            .map(|(&vertex, data)| {
                let entries = data
                    .properties
                    .iter()
                    .map(|(key, value)| {
                        let name = match *key {
                            ID => "id",
                            P => "p",
                            Q => "q",
                            _ => panic!("fixture property catalog"),
                        };
                        (
                            name.into(),
                            GraphValue::Scalar(value.as_scalar().unwrap().clone()),
                        )
                    })
                    .collect();
                (vertex, GraphValue::map(entries).unwrap())
            })
            .collect();
        selection.plan().execute_governed_with_element_maps(
            records,
            self.vertices.keys().copied(),
            self.edges
                .iter()
                .enumerate()
                .map(|(at, &(source, relation, target))| {
                    (EId(at as u128 + 1), source, relation, target)
                }),
            test_vertex,
            property,
            |_, _| Ok(None),
            |_| Ok(None),
            |_| Ok(None),
            |vertex| Ok(maps.get(&vertex)),
            |_| Ok(None),
            allowance,
            || control.checkpoint(),
        )
    }
}

#[derive(Default)]
struct Control {
    calls: Cell<usize>,
    stop: Option<usize>,
}
impl Control {
    fn checkpoint(&self) -> Result<(), usize> {
        let at = self.calls.get();
        self.calls.set(at + 1);
        if self.stop == Some(at) {
            Err(at)
        } else {
            Ok(())
        }
    }
}

fn execute(
    graph: &mut Graph,
    program: &PreparedGraphWriteProgram,
    policy: GraphWriteProgramPolicy,
    control: &Control,
) -> Result<GraphWriteProgramStats, Fault> {
    let mut workspace = graph.clone();
    let mut pending = Vec::new();
    let stats = program.execute_governed::<(), (), usize>(
        policy,
        |statement, input, remaining| {
            let GraphWriteStatement::Mutation(mutation) = input else {
                panic!("the fixture executes only mutation programs")
            };
            let proposal = mutation
                .execute_governed(
                    remaining.mutations,
                    |selection, allowance| workspace.select(selection, allowance, control),
                    || control.checkpoint(),
                )
                .map_err(GraphWriteStepError::Mutation)?;
            let stats = proposal.stats();
            if program.freezes_vertex_update_input() {
                pending.push(proposal);
                if statement + 1 == program.statements().len() {
                    for proposal in pending.drain(..) {
                        workspace.stage(proposal, control).map_err(|error| {
                            GraphWriteStepError::Mutation(GqlQueryError::Interrupted(error))
                        })?;
                    }
                }
            } else {
                workspace.stage(proposal, control).map_err(|error| {
                    GraphWriteStepError::Mutation(GqlQueryError::Interrupted(error))
                })?;
            }
            Ok(GraphWriteStepStats::Mutation(stats))
        },
        || control.checkpoint(),
    )?;
    *graph = workspace;
    Ok(stats)
}

#[test]
fn rekeys_swaps_and_repeated_targets_keep_the_entry_selection_and_input_order() {
    for (initial, rows, expected, visits) in [
        (vec![(10, 1, 10)], [(1, 2), (2, 3)], vec![(10, 2, 10)], 1),
        (
            vec![(10, 1, 10), (20, 2, 20)],
            [(1, 2), (2, 1)],
            vec![(10, 2, 10), (20, 1, 20)],
            2,
        ),
        (vec![(10, 1, 10)], [(1, 2), (1, 3)], vec![(10, 3, 10)], 2),
    ] {
        let bound = bind(REKEY, &rows);
        assert!(bound.program().freezes_vertex_update_input());
        let mut graph = Graph::new(&initial);
        let stats = execute(&mut graph, bound.program(), policy(), &Control::default()).unwrap();
        assert_eq!(graph, Graph::new(&expected));
        assert_eq!(stats.completed_statements, 2);
        assert_eq!(stats.target_vertex_visits, visits);
        assert_eq!(stats.mutation_effects, visits);
    }
}

#[test]
fn overlapping_labels_and_rhs_freeze_while_disjoint_arithmetic_stays_sequential() {
    let rows = [(1, 1), (1, 2)];
    let frozen = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) \
         SET n.id=n.id+row.b, n.p=n.p+1",
        &rows,
    );
    assert!(frozen.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10)]);
    execute(&mut graph, frozen.program(), policy(), &Control::default()).unwrap();
    assert_eq!(graph, Graph::new(&[(10, 3, 11)]));

    let disjoint = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.p=n.p+row.b",
        &rows,
    );
    assert!(!disjoint.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10)]);
    execute(
        &mut graph,
        disjoint.program(),
        policy(),
        &Control::default(),
    )
    .unwrap();
    assert_eq!(graph, Graph::new(&[(10, 1, 13)]));

    let labels = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) REMOVE n:Entity",
        &rows,
    );
    assert!(labels.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10)]);
    let stats = execute(&mut graph, labels.program(), policy(), &Control::default()).unwrap();
    let mut expected = Graph::new(&[(10, 1, 10)]);
    expected.vertices.get_mut(&VId(10)).unwrap().labels.clear();
    assert_eq!(graph, expected);
    assert_eq!(stats.target_vertex_visits, 2);
    assert_eq!(stats.mutation_effects, 2);

    let replacement = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n={id:row.b}",
        &[(1, 2), (1, 3)],
    );
    assert!(replacement.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10)]);
    graph
        .vertices
        .get_mut(&VId(10))
        .unwrap()
        .properties
        .insert(Q, CanonicalScalar::Int(20).into());
    execute(
        &mut graph,
        replacement.program(),
        policy(),
        &Control::default(),
    )
    .unwrap();
    let mut expected = Graph::new(&[(10, 3, 10)]);
    expected
        .vertices
        .get_mut(&VId(10))
        .unwrap()
        .properties
        .remove(&P);
    assert_eq!(graph, expected);
}

#[test]
fn map_expansion_statistics_are_catalog_bounded_in_both_program_meters() {
    fn check_bound(mutation: PreparedGraphMutation, maximum: u64) {
        let pure = PreparedGraphMutationProgram::prepare(vec![mutation.clone()]).unwrap();
        let mixed = PreparedGraphWriteProgram::prepare(vec![mutation.into()]).unwrap();
        for effects in [maximum, maximum + 1] {
            // These are deliberately synthetic host statistics: the guard must
            // distinguish the exact catalog bound from an impossible extra
            // field. Actual expanded proposals run in the kernel law above.
            let stats = GraphMutationStats {
                selection: GqlExecutionStats {
                    snapshot_records: 1,
                    result_rows: 1,
                },
                evaluator: GlaExecutionStats::default(),
                target_vertices: u64::from(effects != 0),
                target_edges: 0,
                effects,
            };
            let pure_result = pure.execute_governed(
                policy().mutations,
                |_, _| Ok::<_, GqlQueryError<GraphMutationError<()>, usize>>(stats),
                || Ok(()),
            );
            let mixed_result = mixed.execute_governed::<(), (), usize>(
                policy(),
                |_, _, _| Ok(GraphWriteStepStats::Mutation(stats)),
                || Ok(()),
            );
            if effects == maximum {
                assert_eq!(pure_result.unwrap().effects, maximum);
                assert_eq!(mixed_result.unwrap().mutation_effects, maximum);
            } else {
                assert!(matches!(
                    pure_result,
                    Err(GraphMutationProgramError::InvalidStatistics { statement: 0 })
                ));
                assert!(matches!(
                    mixed_result,
                    Err(GraphWriteProgramError::Program(
                        GraphMutationProgramError::InvalidStatistics { statement: 0 }
                    ))
                ));
            }
        }
    }

    let bound = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n={id:row.b}",
        &[(1, 2)],
    );
    let GraphWriteStatement::Mutation(mutation) = &bound.program().statements()[0] else {
        panic!("replacement must lower to the native mutation")
    };
    check_bound(mutation.clone(), 3);
    let (target, current) = mutation
        .actions()
        .iter()
        .find_map(|action| match action {
            GraphMutationAction::ClearProperties {
                target, current, ..
            } => Some((*target, *current)),
            _ => None,
        })
        .unwrap();
    for (keep, maximum) in [
        (vec![], 3),
        (vec![ID], 2),
        (vec![ID, P], 1),
        (vec![ID, P, Q], 0),
    ] {
        check_bound(
            PreparedGraphMutation::prepare(
                mutation
                    .selection()
                    .expect("a MATCH mutation reads a graph selection")
                    .clone(),
                R,
                vec![GraphMutationAction::ClearProperties {
                    target,
                    current,
                    keep,
                }],
            )
            .unwrap(),
            maximum,
        );
    }

    // A scalar SET never acquires the catalog-size allowance.
    let scalar = bind(REKEY, &[(1, 2)]);
    let GraphWriteStatement::Mutation(scalar) = &scalar.program().statements()[0] else {
        panic!("scalar SET must lower to the native mutation")
    };
    check_bound(scalar.clone(), 1);

    // The pure mutation program also consumes a real, multi-removal proposal.
    let mut graph = Graph::new(&[(10, 1, 10)]);
    graph
        .vertices
        .get_mut(&VId(10))
        .unwrap()
        .properties
        .insert(Q, CanonicalScalar::Int(20).into());
    let pure = PreparedGraphMutationProgram::prepare(vec![mutation.clone()]).unwrap();
    let control = Control::default();
    let mut proposed = Vec::new();
    let stats = pure
        .execute_governed(
            policy().mutations,
            |input, allowance| {
                let proposal = input.execute_governed(
                    allowance,
                    |selection, budget| graph.select(selection, budget, &control),
                    || control.checkpoint(),
                )?;
                let stats = proposal.stats();
                proposed.push(proposal);
                Ok(stats)
            },
            || control.checkpoint(),
        )
        .unwrap();
    assert_eq!(stats.effects, 3);
    for proposal in proposed {
        graph.stage(proposal, &control).unwrap();
    }
    assert_eq!(
        graph.vertices[&VId(10)].properties,
        BTreeMap::from([(ID, CanonicalScalar::Int(2).into())])
    );
}

#[test]
fn frozen_provenance_survives_clones_consumption_and_original_utf8_locations() {
    let text = format!("/* café */ {REKEY}");
    let bound = bind(&text, &[(1, 2), (2, 3)]);
    let ordinary =
        PreparedGraphWriteProgram::prepare(bound.program().statements().to_vec()).unwrap();
    assert_eq!(ordinary.statements(), bound.program().statements());
    assert!(!ordinary.freezes_vertex_update_input());
    assert_ne!(
        ordinary.canonical_bytes(),
        bound.program().canonical_bytes()
    );
    assert!(
        bound
            .program()
            .canonical_bytes()
            .starts_with(b"fgdb:mixed-write-program:frozen-unwind-vertex-updates:v1\0")
    );
    assert!(
        ordinary
            .canonical_bytes()
            .starts_with(b"fgdb:mixed-write-program:v1\0")
    );
    for row in 0..2 {
        let location = bound.location(row).unwrap();
        assert_eq!((location.argument_set, location.statement), (row, 0));
        assert_eq!(location.span.end, text.len());
        assert!(text.is_char_boundary(location.span.start));
        assert_eq!(bound.clone().location(row), Some(location));
    }
    let extracted = bound.clone().into_program();
    assert!(extracted.freezes_vertex_update_input());
    assert_eq!(extracted, *bound.program());
    assert!(extracted.clone().freezes_vertex_update_input());
    assert!(
        !bind(&text, &[(1, 2)])
            .program()
            .freezes_vertex_update_input()
    );
}

#[test]
fn late_rhs_failure_retains_the_original_input_location_and_publishes_nothing() {
    let text = "/* café */ UNWIND $rows AS row MATCH (n:Entity {id:row.a}) SET n.id=10/row.b";
    let bound = bind(text, &[(1, 2), (1, 0)]);
    let initial = Graph::new(&[(10, 1, 10)]);
    let mut graph = initial.clone();
    let error = execute(&mut graph, bound.program(), policy(), &Control::default()).unwrap_err();
    assert_eq!(graph, initial);
    let error = bound.execution_error(error);
    assert!(matches!(
        error,
        GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            source: GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                statement: 1,
                source: GqlQueryError::Source(GraphMutationError::Arithmetic { error, .. }),
            }),
        } if location.argument_set == 1
            && location.statement == 0
            && location.span.end == text.len()
            && text.is_char_boundary(location.span.start)
            && error.kind == GraphIntegerErrorKind::DivisionByZero
    ));
}

#[test]
fn row_local_conflicts_and_null_optional_targets_keep_the_native_mutation_laws() {
    let duplicate = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a})-[:R]->(m) \
         SET n.id=m.id+row.b",
        &[(99, 0), (1, 0)],
    );
    assert!(duplicate.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10), (20, 2, 20), (30, 3, 30)]);
    graph.edges = vec![(VId(10), R, VId(20)), (VId(10), R, VId(30))];
    let initial = graph.clone();
    let error = execute(
        &mut graph,
        duplicate.program(),
        policy(),
        &Control::default(),
    );
    assert!(matches!(
        error,
        Err(GraphWriteProgramError::Program(
            GraphMutationProgramError::Statement {
                statement: 1,
                source: GqlQueryError::Source(GraphMutationError::ConflictingAssignment { .. }),
            }
        ))
    ));
    assert_eq!(graph, initial);

    // The OPTIONAL target is NULL, so its invalid RHS is never evaluated. The
    // matched target still freezes its two proposals and takes the final row.
    let optional = bind(
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}) \
         OPTIONAL MATCH (n)-[:R]->(m) SET n.id=row.b, m.p=1/0",
        &[(1, 2), (1, 3)],
    );
    assert!(optional.program().freezes_vertex_update_input());
    let mut graph = Graph::new(&[(10, 1, 10)]);
    let stats = execute(
        &mut graph,
        optional.program(),
        policy(),
        &Control::default(),
    )
    .unwrap();
    assert_eq!(graph, Graph::new(&[(10, 3, 10)]));
    assert_eq!(stats.target_vertex_visits, 2);
    assert_eq!(stats.mutation_effects, 2);
}

#[test]
fn cumulative_limits_and_every_checkpoint_refusal_restore_the_entry_graph() {
    let bound = bind(REKEY, &[(1, 2), (1, 3)]);
    let initial = Graph::new(&[(10, 1, 10)]);
    let mut graph = initial.clone();
    let control = Control::default();
    let stats = execute(&mut graph, bound.program(), policy(), &control).unwrap();
    assert_eq!(graph, Graph::new(&[(10, 3, 10)]));
    assert!(control.calls.get() > 10);

    for dimension in [
        GraphMutationProgramDimension::Effects,
        GraphMutationProgramDimension::WorkUnits,
        GraphMutationProgramDimension::ScratchEntries,
    ] {
        let mut limited = policy();
        let limit = match dimension {
            GraphMutationProgramDimension::Effects => {
                limited.mutations.max_effects = stats.mutation_effects - 1;
                limited.mutations.max_effects
            }
            GraphMutationProgramDimension::WorkUnits => {
                limited.mutations.query.evaluator.max_work_units = stats.evaluator.work_units - 1;
                limited.mutations.query.evaluator.max_work_units
            }
            GraphMutationProgramDimension::ScratchEntries => {
                limited.mutations.query.evaluator.max_scratch_entries =
                    stats.evaluator.scratch_entries - 1;
                limited.mutations.query.evaluator.max_scratch_entries
            }
            _ => unreachable!(),
        };
        let mut graph = initial.clone();
        let error = execute(&mut graph, bound.program(), limited, &Control::default()).unwrap_err();
        let GraphWriteProgramError::Program(GraphMutationProgramError::Budget {
            statement,
            dimension: actual,
            limit: actual_limit,
            observed,
        }) = error
        else {
            panic!("cumulative budget refusal: {error:?}")
        };
        assert_eq!(actual, dimension);
        assert_eq!(actual_limit, limit);
        assert!(observed > u128::from(limit));
        if dimension == GraphMutationProgramDimension::WorkUnits {
            // Final program acceptance is after the frozen proposals stage.
            assert_eq!(statement, bound.program().statements().len());
            assert_eq!(observed, u128::from(stats.evaluator.work_units));
        }
        assert_eq!(graph, initial);
    }

    for stop in 0..control.calls.get() {
        let interrupted = Control {
            calls: Cell::new(0),
            stop: Some(stop),
        };
        let mut graph = initial.clone();
        let error = execute(&mut graph, bound.program(), policy(), &interrupted).unwrap_err();
        let source = match error {
            GraphWriteProgramError::Program(GraphMutationProgramError::Interrupted {
                source,
                ..
            })
            | GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                source: GqlQueryError::Interrupted(source),
                ..
            }) => source,
            other => panic!("checkpoint {stop}: {other:?}"),
        };
        assert_eq!(source, stop);
        assert_eq!(interrupted.calls.get(), stop + 1);
        assert_eq!(graph, initial, "checkpoint {stop}");
    }
}

#[test]
fn definition_admission_precedes_mode_selection_and_unsupported_profiles_still_refuse() {
    let arguments = arguments(&[(1, 2), (2, 3)]);
    for text in [
        REKEY,
        "UNWIND $rows AS row MATCH (n:Entity)-[e:R {id:row.a}]->(m) SET e.id=row.b",
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a})-[:R]->(m) DETACH DELETE n",
        "UNWIND $rows AS row MATCH (n:Entity {id:row.a}), (anchor:Entity {id:1}) DELETE n",
    ] {
        let definition = GraphUnwindWriteText::parse(text).unwrap();
        let mut definitions = 0;
        let denied = definition.bind_with_limit_controlled(&arguments, R, 64, symbols, |event| {
            if let GraphUnwindBindEvent::Definition(script) = event {
                definitions += 1;
                assert_eq!(script.statements().len(), 1);
                Err("definition denied")
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            denied,
            Err(GraphUnwindBindError::Interrupted("definition denied"))
        ));
        assert_eq!(definitions, 1);
        let admitted = definition.bind(&arguments, R, symbols);
        if text == REKEY {
            assert!(admitted.unwrap().program().freezes_vertex_update_input());
        } else {
            assert!(matches!(
                admitted,
                Err(GraphUnwindWriteError::RowFeedsLaterMatch { statement: 0 })
            ));
        }
    }
}
