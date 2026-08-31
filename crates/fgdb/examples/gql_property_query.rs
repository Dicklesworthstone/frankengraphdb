//! **A real `main()` that queries with property predicates**.
//!
//! The bounded GQL executor supports integer property predicates on the
//! destination of a one-hop edge. This example seeds a small graph, runs
//! equality (`=`), less-than (`<`), and greater-than (`>`) predicates, and
//! prints the results — a standalone binary anyone can run.
//!
//! ```text
//! cargo run --example gql_property_query
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const KNOWS: RelationId = RelationId(1);
const AGE: PropertyKeyId = PropertyKeyId(7);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!("fgdb-gql-example-{}", std::process::id()));
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );

    println!("fgdb GQL property-predicate witness");
    println!("  database directory: {}", path.display());

    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut db = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![(AGE, CanonicalScalar::Int(30))]);
        batch.create_vertex(VId(3), vec![], vec![(AGE, CanonicalScalar::Int(25))]);
        batch.create_vertex(VId(4), vec![], vec![(AGE, CanonicalScalar::Int(40))]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        batch.add_edge(EId(12), VId(1), VId(4), vec![]);
        db.write(cx, batch).await?;

        let bind = RelationBind::new()
            .with_relation("KNOWS", KNOWS)
            .with_property("age", AGE);

        let eq = db.execute_gql("MATCH (a)-[:KNOWS]->(b) WHERE b.age = 30 RETURN a", &bind)?;
        let lt = db.execute_gql("MATCH (a)-[:KNOWS]->(b) WHERE b.age < 30 RETURN a", &bind)?;
        let gt = db.execute_gql("MATCH (a)-[:KNOWS]->(b) WHERE b.age > 30 RETURN a", &bind)?;

        println!("  MATCH ... WHERE b.age = 30 -> {eq:?}");
        println!("  MATCH ... WHERE b.age < 30 -> {lt:?}");
        println!("  MATCH ... WHERE b.age > 30 -> {gt:?}");

        assert_eq!(
            eq,
            vec![VId(1)],
            "equality predicate must return the source"
        );
        assert_eq!(
            lt,
            vec![VId(1)],
            "less-than predicate must return the source"
        );
        assert_eq!(
            gt,
            vec![VId(1)],
            "greater-than predicate must return the source"
        );

        println!("OK: property predicates work.");
        Ok(())
    })
}
