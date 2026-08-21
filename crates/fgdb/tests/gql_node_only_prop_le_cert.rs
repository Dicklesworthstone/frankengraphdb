//! Node-only `<=`, `<`, `=`, `<>`, `>=`, and unfiltered plans at one
//! snapshot must have pairwise-distinct certificates. The assertions freeze
//! transcript discrimination without pinning digest bytes.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlPlanCertificate, RelationBind};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const NODE_LE: &str = "MATCH (a:Person) WHERE a.k <= 1 RETURN a";
const NODE_LT: &str = "MATCH (a:Person) WHERE a.k < 1 RETURN a";
const NODE_EQ: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const NODE_NE: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";
const NODE_GE: &str = "MATCH (a:Person) WHERE a.k >= 1 RETURN a";
const NODE_PLAIN: &str = "MATCH (a:Person) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn bind() -> RelationBind {
    RelationBind::new()
        .with_label("Person", PERSON)
        .with_property("k", K)
}

#[test]
fn node_only_property_comparators_certify_as_distinct_plans() {
    let ((), report) = run_async_under_lab(0x32_c1, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-node-only-le-cert-{}", std::process::id()));
        let db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let statements = [NODE_LE, NODE_LT, NODE_EQ, NODE_NE, NODE_GE, NODE_PLAIN];
        let certs: Vec<GqlPlanCertificate> = statements
            .iter()
            .map(|statement| {
                db.gql_plan_certificate(statement, &bind())
                    .unwrap_or_else(|error| panic!("{statement:?} certifies: {error:?}"))
            })
            .collect();

        for cert in &certs[1..] {
            assert_eq!(cert.snapshot_seq, certs[0].snapshot_seq);
        }
        assert_eq!(
            certs[0],
            db.gql_plan_certificate(NODE_LE, &bind())
                .expect("node-only <= re-certifies"),
            "re-minting the same plan and snapshot is byte-identical"
        );

        for (left_at, left) in certs.iter().enumerate() {
            for (right_at, right) in certs.iter().enumerate().skip(left_at + 1) {
                assert_ne!(
                    left.digest, right.digest,
                    "{:?} and {:?} must not share a digest",
                    statements[left_at], statements[right_at]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
