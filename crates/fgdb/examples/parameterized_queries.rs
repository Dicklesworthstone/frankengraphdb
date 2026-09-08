//! Reuse native typed parameters with the ordinary embedded query APIs.
//!
//! cargo run -p fgdb --example parameterized_queries

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaExecutionLimits, GqlEvidenceAuditError, GqlExecutionBudget, GqlParameterError,
    GqlParameters, PreparedGqlTemplate,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const KNOWS: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(2);
const AGE: PropertyKeyId = PropertyKeyId(3);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn arguments(min_age: i64) -> Result<GqlParameters, GqlParameterError> {
    GqlParameters::new()
        .with_int64("min_age", min_age)?
        .with_uint64("offset", 0)?
        .with_uint64("count", 10)
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async move {
        // Fixed fixture keys for a private, transient example database only.
        let keys = DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let mut db = Database::open_memory(&cx, keys).await?;
        let mut seed = WriteBatch::new(KNOWS);
        for (id, age) in [(1, 18), (2, 30), (3, 50)] {
            seed.create_vertex(
                VId(id),
                vec![PERSON],
                vec![(AGE, CanonicalScalar::Int(age))],
            );
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(1), VId(3), vec![]);
        db.write(&cx, seed).await?;

        let names = RelationBind::new()
            .with_relation("KNOWS", KNOWS)
            .with_label("Person", PERSON)
            .with_property("age", AGE);
        let template = PreparedGqlTemplate::prepare(
            "MATCH (a:Person)-[:KNOWS]->(b) WHERE b.age >= $min_age \
             RETURN b SKIP $offset LIMIT $count",
            &names,
        )?;
        println!("parameter schema: {:?}", template.parameter_schema());

        let everyone = template.bind_parameters(&arguments(20)?)?;
        let older = template.bind_parameters(&arguments(40)?)?;
        assert_eq!(db.execute_prepared_query(&everyone)?, vec![VId(2), VId(3)]);
        let limited = db.execute_prepared_query_limited(
            &older,
            GlaExecutionLimits::new(1_000, 1_000),
        )?;
        assert_eq!(limited.value, vec![VId(3)]);
        assert_eq!(
            db.execute_prepared_query_budgeted(&older, GqlExecutionBudget::new(2, 1))?
                .value,
            limited.value,
        );
        println!("age >= 40: {:?}; evaluator stats: {:?}", limited.value, limited.stats);

        let incorrect_type = GqlParameters::new()
            .with_uint64("min_age", 40)?
            .with_uint64("offset", 0)?
            .with_uint64("count", 10)?;
        assert!(matches!(
            template.bind_parameters(&incorrect_type),
            Err(GqlParameterError::TypeMismatch { .. })
        ));

        let artifact = db.execute_prepared_query_artifact(&older)?;
        let bytes = artifact.to_bytes();
        assert_eq!(db.audit_prepared_query_artifact(&older, &bytes)?.rows(), &[VId(3)]);
        let same_rows_different_binding = template.bind_parameters(&arguments(41)?)?;
        assert_eq!(db.execute_prepared_query(&same_rows_different_binding)?, vec![VId(3)]);
        assert!(matches!(
            db.audit_prepared_query_artifact(&same_rows_different_binding, &bytes),
            Err(GqlEvidenceAuditError::InputMismatch)
        ));

        let pinned = db.read_session()?;
        let mut change = WriteBatch::new(KNOWS);
        change.set_vertex_property(VId(2), AGE, Some(CanonicalScalar::Int(45)));
        db.write(&cx, change).await?;
        assert_eq!(db.execute_prepared_query(&older)?, vec![VId(2), VId(3)]);
        assert_eq!(pinned.execute_prepared_query(&older)?, vec![VId(3)]);
        assert_eq!(db.audit_prepared_query_artifact(&older, &bytes)?.rows(), &[VId(3)]);
        println!("OK: typed rebinding, limits, evidence replay, and pinned reads agree");
        Ok(())
    })
}
