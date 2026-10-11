//! Run the real binary with JSON map parameters; each command reopens storage.
use super::*;

const DOCUMENT_UPSERT: &str = "UNWIND $payload.groups[-1].members AS row \
    MERGE (n:Entity {id:row.id}) SET n.name=row.name";

fn write_document(fixture: &Fixture, json: &str, text: &str) -> Output {
    let parameter = format!("payload=json:{json}");
    fixture.run("write", &["--param", &parameter, text])
}

#[test]
fn root_document_upsert_matches_flat_input_and_does_not_interpolate_values() {
    let document = Fixture::new(true);
    let written = write_document(
        &document,
        r#"{"groups":[{"members":42},{"members":[{"id":1,"name":"Original"},{"id":2,"name":"Bob"},{"id":1,"name":"Ada'; MATCH (n) DETACH DELETE n; //"}]}],"unused":"do not bind"}"#,
        DOCUMENT_UPSERT,
    );
    success(&written);
    assert!(output(&written).contains("\"statements\":3"));
    let flat = Fixture::new(true);
    success(&flat.write(
        r#"[{"id":1,"name":"Original"},{"id":2,"name":"Bob"},{"id":1,"name":"Ada'; MATCH (n) DETACH DELETE n; //"}]"#,
        UPSERT,
    ));
    let graph = |fixture: &Fixture| -> Vec<String> {
        String::from_utf8_lossy(
            &fixture
                .query("MATCH (n:Entity) RETURN n.id AS id,n.name AS name ORDER BY id")
                .stdout,
        )
        .lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .map(str::to_owned)
        .collect()
    };
    let rows = graph(&document);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].contains("DETACH DELETE"));
    assert!(rows[1].contains("Bob"));
    assert_eq!(rows, graph(&flat));
}

#[test]
fn invalid_document_roots_and_late_rows_refuse_before_opening_storage() {
    let fixture = Fixture::new(false);
    // A top-level scalar is not a json: document at all: the CLI refuses that
    // spelling as usage (exit 2) before any query is prepared, as its
    // json_parameter contract documents (scalars have their own spellings).
    // Every document that does parse is a query refusal (exit 3).
    for (json, code) in [
        ("42", 2),
        ("{}", 3),
        (r#"{"groups":null}"#, 3),
        (r#"{"groups":[{"members":[]}]}"#, 3),
        (r#"{"groups":[{"members":42}]}"#, 3),
        (
            r#"{"groups":[{"members":[{"id":1,"name":"Good"},{"id":2,"name":true}]}]}"#,
            3,
        ),
    ] {
        let result = write_document(&fixture, json, DOCUMENT_UPSERT);
        assert_eq!(result.status.code(), Some(code), "{}", output(&result));
        assert!(!fixture.db.exists(), "refused input opened storage");
        assert!(!output(&result).contains("\"kind\":\"written\""));
    }
}

#[test]
fn document_root_arithmetic_failure_keeps_durable_rows_and_frontier_unchanged() {
    let fixture = Fixture::new(true);
    success(&write_document(
        &fixture,
        r#"{"groups":[{"members":[{"id":1,"name":"A"},{"id":2,"name":"B"}]}]}"#,
        DOCUMENT_UPSERT,
    ));
    let text = "UNWIND $payload.ops AS row MATCH (n:Entity {id:row.id}) \
        SET n.score=10/row.divisor";
    // This control establishes that the shape and arithmetic actually execute.
    success(&write_document(
        &fixture,
        r#"{"ops":[{"id":1,"divisor":2},{"id":2,"divisor":2}]}"#,
        text,
    ));
    let before = fixture.query("MATCH (n:Entity) RETURN n.id,n.score ORDER BY n.id");
    let refused = write_document(
        &fixture,
        r#"{"ops":[{"id":1,"divisor":5},{"id":2,"divisor":0}]}"#,
        text,
    );
    assert!(!refused.status.success(), "{}", output(&refused));
    assert!(!output(&refused).contains("\"kind\":\"written\""));
    let after = fixture.query("MATCH (n:Entity) RETURN n.id,n.score ORDER BY n.id");
    assert_eq!(
        before.stdout, after.stdout,
        "failed document batch changed durable state"
    );
}

#[test]
fn repeated_document_sources_form_a_product_in_clause_order() {
    let fixture = Fixture::new(true);
    let text = "UNWIND $payload.ids AS id UNWIND $payload.names AS name \
        MERGE (n:Entity {id:id}) SET n.name=name";
    let written = write_document(&fixture, r#"{"ids":[1,2],"names":["First","Last"]}"#, text);
    success(&written);
    assert!(output(&written).contains("\"statements\":4"));
    let result = fixture.query("MATCH (n:Entity) WHERE n.name='Last' RETURN n.id");
    assert_eq!(
        row_count(&result),
        2,
        "product rows must not be zipped or deduplicated"
    );
    assert_eq!(row_count(&fixture.query("MATCH (n:Entity) RETURN n.id")), 2);
}
