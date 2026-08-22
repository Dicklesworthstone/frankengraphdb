use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;

const GE: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const GT: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
const EQ: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const NE: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const BARE: &str = "MATCH (a)-[:R]->(b) RETURN b";

#[test]
fn source_greater_or_equal_certificate_is_operator_distinct_and_deterministic() {
    let ((), report) = run_async_under_lab(0x64_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-where-prop-ge-cert-{}", std::process::id()));
        let db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            ),
        )
        .await
        .expect("database creates");
        let bind = RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_property("k", PropertyKeyId(7));

        let certificates = [GE, GT, EQ, NE, BARE].map(|statement| {
            db.gql_plan_certificate(statement, &bind)
                .expect("plan certifies")
        });
        for certificate in &certificates[1..] {
            assert_eq!(
                certificate.snapshot_seq, certificates[0].snapshot_seq,
                "all operator variants certify at one frontier"
            );
        }
        for left in 0..certificates.len() {
            for right in left + 1..certificates.len() {
                assert_ne!(
                    certificates[left].digest, certificates[right].digest,
                    "operator-distinct plans must not share a certificate digest"
                );
            }
        }
        assert_eq!(
            db.gql_plan_certificate(GE, &bind)
                .expect("greater-or-equal plan re-certifies"),
            certificates[0],
            "same plan at the same frontier re-mints byte-identically"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
