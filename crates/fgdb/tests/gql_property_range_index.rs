//! Ordered property range admission: independent storage answers and bounded charges.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphText, PreparedTemporalGraphText,
};
use fgdb_types::{
    CanonicalDecimal, CanonicalF64, CanonicalScalar, CanonicalScalarKind, CommitSeq,
    DatabaseSecurityNamespaceId, PurposeContexts, VId,
};
use std::cmp::Ordering;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(5);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Label, "Tag") => Some(GraphSymbol::Label(L)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000)
}

fn identities(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    // Deliberately independent of VertexPredicate and its encoded index keys.
    fn accepts(self, actual: &CanonicalScalar, bound: &CanonicalScalar) -> bool {
        if matches!(actual, CanonicalScalar::Null)
            || matches!(bound, CanonicalScalar::Null)
            || std::mem::discriminant(actual) != std::mem::discriminant(bound)
        {
            return false;
        }
        let order = actual.cmp(bound);
        match self {
            Self::Lt => order == Ordering::Less,
            Self::Le => order != Ordering::Greater,
            Self::Gt => order == Ordering::Greater,
            Self::Ge => order != Ordering::Less,
        }
    }
}

fn scalar(kind: usize, value: i64) -> CanonicalScalar {
    match kind {
        0 => CanonicalScalar::Int(value),
        1 => CanonicalScalar::Decimal(CanonicalDecimal::from_integer(i128::from(value)).unwrap()),
        2 => CanonicalScalar::Float(CanonicalF64::new(value as f64)),
        // Ordered ASCII witnesses, including exact lower/upper endpoints.
        3 => CanonicalScalar::ucs_basic_text(match value {
            i64::MIN..=-1 => "alpha",
            0 => "beta",
            1 => "delta",
            2 => "omega",
            _ => "zulu",
        })
        .unwrap(),
        _ => unreachable!(),
    }
}

struct Case {
    name: String,
    bounds: Vec<(Op, CanonicalScalar)>,
    residual: bool,
    query: PreparedGraphPattern<GraphValueRow>,
}

impl Case {
    fn new(bounds: Vec<(Op, CanonicalScalar)>, residual: bool) -> Self {
        let mut args = GqlParameters::new();
        let mut declarations = Vec::new();
        let mut clauses = Vec::new();
        for (index, (op, value)) in bounds.iter().enumerate() {
            let name = if index == 0 { "a" } else { "b" };
            declarations.push((
                name,
                GqlParameterType::Scalar(CanonicalScalarKind::of(value)),
            ));
            args = args.with_scalar(name, value.clone()).unwrap();
            clauses.push(format!("n.p {} ${name}", op.text()));
        }
        if residual {
            clauses.push("n.q > 0".to_owned());
        }
        let label = if residual { ":Tag" } else { "" };
        let text = format!("MATCH (n{label}) WHERE {} RETURN n", clauses.join(" AND "));
        let query = PreparedGraphText::prepare_with_parameter_types(&text, &declarations, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
        Self {
            name: format!("{text}: {bounds:?}"),
            bounds,
            residual,
            query,
        }
    }
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for kind in 0..4 {
        for op in [Op::Lt, Op::Le, Op::Gt, Op::Ge] {
            cases.push(Case::new(vec![(op, scalar(kind, 0))], false));
        }
        // All four endpoint policies, each with persistent exact-bound rows.
        for lower in [Op::Gt, Op::Ge] {
            for upper in [Op::Lt, Op::Le] {
                cases.push(Case::new(
                    vec![(lower, scalar(kind, 0)), (upper, scalar(kind, 2))],
                    false,
                ));
            }
        }
        for (lower, upper) in [(Op::Ge, Op::Le), (Op::Gt, Op::Le), (Op::Ge, Op::Lt)] {
            cases.push(Case::new(
                vec![(lower, scalar(kind, 0)), (upper, scalar(kind, 0))],
                false,
            ));
        }
        cases.push(Case::new(
            vec![(Op::Ge, scalar(kind, 2)), (Op::Le, scalar(kind, 0))],
            false,
        ));
        cases.push(Case::new(
            vec![(Op::Ge, scalar(kind, 0)), (Op::Lt, scalar(kind, 2))],
            true,
        ));
    }
    // An apparently nonempty cross-kind key interval still has no SQL matches.
    cases.push(Case::new(
        vec![(Op::Ge, scalar(0, 0)), (Op::Le, scalar(1, 2))],
        false,
    ));
    cases.push(Case::new(vec![(Op::Ge, CanonicalScalar::Null)], false));
    cases
}

fn oracle(db: &Database<MemVfs>, at: CommitSeq, case: &Case) -> Vec<VId> {
    db.vertices_at(at)
        .unwrap()
        .into_iter()
        .filter(|row| {
            let actual = row.props.iter().find(|(key, _)| *key == P);
            actual.is_some_and(|(_, value)| {
                case.bounds
                    .iter()
                    .all(|(op, bound)| op.accepts(value, bound))
            }) && (!case.residual
                || (row.labels.contains(&L)
                    && row.props.iter().any(|(key, value)| {
                        *key == Q && matches!(value, CanonicalScalar::Int(n) if *n > 0)
                    })))
        })
        .map(|row| row.vid)
        .collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }

    fn value(&mut self) -> Option<CanonicalScalar> {
        let kind = (self.next() >> 32) % 6;
        let value = ((self.next() >> 32) % 7) as i64 - 3;
        match kind {
            0..=3 => Some(scalar(kind as usize, value)),
            4 => Some(CanonicalScalar::Null),
            _ => None,
        }
    }
}

#[test]
fn range_answers_equal_storage_scan_across_history_pinning_and_reopen() {
    for seed in [11_u64, 23, 0xdead_f00d] {
        let ((), report) = run_async_under_lab(0xc10_0000 + seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut rng = Rng(seed);
            let mut initial = WriteBatch::new(R);
            // Unchanged witnesses make every nonempty range and every endpoint
            // distinction observable at every cut, independent of the seed.
            for kind in 0..4 {
                for (offset, value) in [-2, 0, 1, 2].into_iter().enumerate() {
                    initial.create_vertex(
                        VId((1 + kind * 4 + offset) as u128),
                        vec![L],
                        vec![(P, scalar(kind, value)), (Q, CanonicalScalar::Int(1))],
                    );
                }
            }
            initial.create_vertex(VId(90), vec![L], vec![]);
            initial.create_vertex(VId(91), vec![L], vec![(P, CanonicalScalar::Null)]);
            for id in 100..140 {
                let mut props = vec![(Q, CanonicalScalar::Int(1))];
                if let Some(value) = rng.value() {
                    props.push((P, value));
                }
                initial.create_vertex(VId(id), vec![L], props);
            }
            // Guaranteed repeated membership, removal and deletion witnesses.
            for id in 150..154 {
                initial.create_vertex(
                    VId(id),
                    vec![L],
                    vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(1))],
                );
            }
            let old = db.write(&commit, initial).await.unwrap();
            let pinned = db.read_session().unwrap();
            let cases = cases();
            let original: Vec<_> = cases.iter().map(|case| oracle(&db, old, case)).collect();
            let mut cuts = vec![old];
            let temporal = PreparedTemporalGraphText::prepare(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at WHERE n.p >= 0 AND n.p < 2 RETURN n",
                symbols,
            )
            .unwrap();
            let temporal_case =
                Case::new(vec![(Op::Ge, scalar(0, 0)), (Op::Lt, scalar(0, 2))], false);
            let original_temporal = oracle(&db, old, &temporal_case);
            for step in 0..=6_u64 {
                if step != 0 {
                    let mut batch = WriteBatch::new(R);
                    for id in 100..140 {
                        batch.set_vertex_property(VId(id), P, rng.value());
                    }
                    batch.set_vertex_property(
                        VId(150),
                        P,
                        match step {
                            1 | 3 => Some(CanonicalScalar::Int(5)),
                            2 | 4 => Some(CanonicalScalar::Int(1)),
                            5 => Some(CanonicalScalar::Null),
                            _ => None,
                        },
                    );
                    batch.set_vertex_label(VId(151), L, step % 2 == 0);
                    batch.set_vertex_property(
                        VId(152),
                        Q,
                        Some(CanonicalScalar::Int((step % 2) as i64)),
                    );
                    if step == 2 {
                        batch.delete_vertex(VId(153));
                    }
                    for kind in 0..4 {
                        batch.create_vertex(
                            VId(u128::from(200 + step * 4 + kind)),
                            vec![L],
                            vec![(P, scalar(kind as usize, 1)), (Q, CanonicalScalar::Int(1))],
                        );
                    }
                    cuts.push(db.write(&commit, batch).await.unwrap());
                }
                if step == 3 {
                    db.compact(&commit).await.unwrap();
                }
                if step == 4 {
                    drop(db);
                    db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                }
                for (case, before) in cases.iter().zip(&original) {
                    for at in &cuts {
                        let run = db
                            .execute_graph_pattern_governed_at(&cx, &case.query, *at, policy())
                            .unwrap();
                        assert_eq!(
                            identities(&run.value),
                            oracle(&db, *at, case),
                            "seed={seed} step={step} at={at:?} {}",
                            case.name
                        );
                    }
                    let retained = pinned
                        .execute_graph_pattern_governed(&cx, &case.query, policy())
                        .unwrap();
                    assert_eq!(
                        identities(&retained.value),
                        *before,
                        "pinned seed={seed} step={step} {}",
                        case.name
                    );
                }
                for at in &cuts {
                    let args = GqlParameters::new().with_uint64("at", at.0).unwrap();
                    let bound = temporal.bind_parameters(&args).unwrap();
                    let run = db
                        .execute_temporal_graph_text_governed(&cx, &bound, policy())
                        .unwrap();
                    assert_eq!(
                        identities(&run.value),
                        oracle(&db, *at, &temporal_case),
                        "temporal seed={seed} step={step} at={at:?}"
                    );
                }
                let args = GqlParameters::new().with_uint64("at", old.0).unwrap();
                let bound = temporal.bind_parameters(&args).unwrap();
                let retained = pinned
                    .execute_temporal_graph_text_governed(&cx, &bound, policy())
                    .unwrap();
                assert_eq!(identities(&retained.value), original_temporal);
            }
        });
        assert!(report.lab_test_passed(), "seed={seed}: {report:?}");
    }
}

#[test]
fn two_sided_range_charges_ignore_unrelated_vertex_growth() {
    let mut charges = Vec::new();
    for (seed, total) in [(0xc10_0042_u64, 1_000_usize), (0xc10_0043, 50_000)] {
        let (charged, report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut at = CommitSeq(0);
            for start in (0..total).step_by(1_000) {
                let mut batch = WriteBatch::new(R);
                for id in start..(start + 1_000).min(total) {
                    // Identical ten range candidates in both databases. Extra
                    // vertices lie on BOTH sides: one-sided pruning is not enough.
                    let value = if id < 10 {
                        41 + (id % 2) as i64
                    } else if id % 2 == 0 {
                        40 - (id % 97) as i64
                    } else {
                        43 + (id % 97) as i64
                    };
                    batch.create_vertex(
                        VId(id as u128),
                        vec![],
                        vec![(P, CanonicalScalar::Int(value))],
                    );
                }
                at = db.write(&commit, batch).await.unwrap();
            }
            let query = PreparedGraphText::prepare(
                "MATCH (n) WHERE n.p >= $lower AND n.p < $upper RETURN n",
                symbols,
            )
            .unwrap()
            .bind_parameters(
                &GqlParameters::new()
                    .with_int64("lower", 41)
                    .unwrap()
                    .with_int64("upper", 43)
                    .unwrap(),
            )
            .unwrap();
            let run = db
                .execute_graph_pattern_governed_at(&contexts.query(), &query, at, policy())
                .unwrap();
            assert_eq!(identities(&run.value), (0..10).map(VId).collect::<Vec<_>>());
            (run.rows.snapshot_records, run.evaluator.work_units)
        });
        assert!(report.lab_test_passed(), "total={total}: {report:?}");
        charges.push(charged);
    }
    assert_eq!(
        charges[0].0, charges[1].0,
        "SnapshotRecord charges must count resolved range candidates, not the graph"
    );
    assert_eq!(
        charges[0].1, charges[1].1,
        "Work charges must not grow with unrelated vertices"
    );
    assert!(
        charges[0].0 > 0,
        "the nonempty matching range must be charged"
    );
}

#[test]
fn range_bound_on_one_domain_does_not_prune_optional_match_domain() {
    let ((), report) = run_async_under_lab(0xc10_0050, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, p, q) in [(1, 9, 1), (2, 9, 8), (3, 7, 9)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let query = PreparedGraphText::prepare(
            "MATCH (a) WHERE a.q >= 1 AND a.q < 2 OPTIONAL MATCH (b) WHERE b.p=a.p RETURN b",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let result = db
            .execute_graph_pattern_governed(&contexts.query(), &query, policy())
            .unwrap();
        // b=2 lies outside a's range but belongs in b's independent domain.
        assert_eq!(identities(&result.value), vec![VId(1), VId(2)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
