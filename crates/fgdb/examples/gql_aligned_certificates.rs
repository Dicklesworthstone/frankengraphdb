//! **One GQL execution, one snapshot, both existing certificate layers**.
//!
//! The aligned API prevents a caller from accidentally pairing statement/bind
//! evidence from one frontier with plan evidence from another. It still makes
//! no result-row attestation claim: rows are returned beside, not inside, the
//! two certificates.
//!
//! ```text
//! cargo run --example gql_aligned_certificates
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:KNOWS]->(b) RETURN b";

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!(
        "fgdb-aligned-certificates-example-{}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut db = Database::create(cx, &path, keys).await?;
        let bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let plan = db.prepare_gql_plan(QUERY, &bind)?;

        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        let first_seq = db.write(cx, first).await?;

        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(1), VId(3), vec![]);
        let second_seq = db.write(cx, second).await?;

        let (historical_rows, input_certificate, plan_certificate) =
            db.execute_gql_with_certificates_at(QUERY, &bind, first_seq)?;
        assert_eq!(historical_rows, vec![VId(2)]);
        assert!(input_certificate.verifies_at(QUERY, &bind, first_seq));
        assert!(plan_certificate.verifies_at(&plan, first_seq));
        assert_eq!(input_certificate.snapshot_seq, plan_certificate.snapshot_seq);

        let (live_rows, live_input, live_plan) =
            db.execute_gql_with_certificates(QUERY, &bind)?;
        assert_eq!(live_rows, vec![VId(2), VId(3)]);
        assert!(live_input.verifies_at(QUERY, &bind, second_seq));
        assert!(live_plan.verifies_at(&plan, second_seq));

        println!("historical {first_seq:?}: {historical_rows:?}");
        println!("live {second_seq:?}: {live_rows:?}");
        println!("OK: input and plan evidence are aligned per execution");
        Ok(())
    })
}
