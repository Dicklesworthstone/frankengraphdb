//! End-to-end process tests over the actual durable engine, not a fake backend.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::{EId, VId, context::PurposeContexts, ids::DatabaseSecurityNamespaceId};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture { home: PathBuf }
impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("fgdb-cli-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&home).unwrap();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(home.join("keys")).unwrap();
        file.write_all(&[0x5a; 32]).unwrap();
        file.write_all(&[0x77; 32]).unwrap();
        file.write_all(&[0x3c; 32]).unwrap();
        file.sync_all().unwrap();
        fs::write(home.join("symbols"), "relation\tKNOWS\t1\nlabel\tUnmentionedStoredLabel\t17\n").unwrap();
        Self { home }
    }
    fn path(&self, name: &str) -> PathBuf { self.home.join(name) }
    fn command(&self, operation: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command.arg(operation).arg("--db").arg(self.path("db"))
            .arg("--keys-file").arg(self.path("keys")).args(["--format", "ndjson"]);
        command
    }
    fn query(&self, statement: &str) -> Output {
        fs::write(self.path("query"), statement).unwrap();
        self.command("query").arg("--query-file").arg(self.path("query"))
            .arg("--symbols-file").arg(self.path("symbols")).output().unwrap()
    }
    fn seed(&self) {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let cx = PurposeContexts::narrow_runtime_root(&root).commit();
        runtime.block_on(async {
            let mut db = Database::open(&cx, &self.path("db"), keys()).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(1), vec![LabelId(17)], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(&cx, batch).await.unwrap();
        });
    }
}
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x5a; 32], DatabaseSecurityNamespaceId([0x77; 32]), [0x3c; 32]) }
fn success(output: &Output) {
    assert!(output.status.success(), "stderr={}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"type\":\"complete\""));
}

#[test]
fn create_query_compact_reopen_uses_the_real_durable_engine() {
    let fixture = Fixture::new();
    success(&fixture.command("init").output().unwrap());
    fixture.seed();
    let before = fixture.query("MATCH (a)-[:KNOWS]->(b) RETURN b");
    success(&before);
    assert!(String::from_utf8_lossy(&before.stdout).contains("{\"type\":\"vertex\",\"value\":\"2\"}"));
    success(&fixture.command("compact").output().unwrap());
    let after = fixture.query("MATCH (a)-[:KNOWS]->(b) RETURN b");
    success(&after);
    assert_eq!(before.stdout, after.stdout);
    success(&fixture.query("EXPLAIN MATCH (a)-[:KNOWS]->(b) RETURN b"));
}

#[test]
fn labels_reflection_uses_the_supplied_catalog_not_name_probing() {
    let fixture = Fixture::new();
    success(&fixture.command("init").output().unwrap());
    fixture.seed();
    let output = fixture.query("MATCH (a)-[:KNOWS]->(b) RETURN labels(a)");
    success(&output);
    let expected = fgdb_gql::algebra::GraphValue::List(vec![
        fgdb_gql::algebra::GraphValue::Scalar(
            fgdb_types::CanonicalScalar::ucs_basic_text("UnmentionedStoredLabel").unwrap())
    ].into_boxed_slice()).canonical_bytes().unwrap();
    let hex: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
    assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("\"hex\":\"{hex}\"")));
}

#[test]
fn read_command_never_runs_a_mutation_or_leaks_its_statement() {
    let fixture = Fixture::new();
    success(&fixture.command("init").output().unwrap());
    fixture.seed();
    let before = fixture.query("MATCH (a)-[:KNOWS]->(b) RETURN b");
    let rejected = fixture.query("INSERT (secret_marker_vertex)");
    assert_eq!(rejected.status.code(), Some(3));
    for bytes in [&rejected.stdout, &rejected.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("secret_marker_vertex"));
    }
    assert_eq!(before.stdout, fixture.query("MATCH (a)-[:KNOWS]->(b) RETURN b").stdout);
}

#[test]
fn input_and_output_refusals_are_not_partial_results_or_mutations() {
    let fixture = Fixture::new();
    let rejected = fixture.command("init").args(["--max-output-bytes", "1"]).output().unwrap();
    assert_eq!(rejected.status.code(), Some(3));
    assert!(!fixture.path("db").exists());
    success(&fixture.command("init").output().unwrap());
    fixture.seed();
    fs::write(fixture.path("query"), "MATCH (a)-[:KNOWS]->(b) RETURN b").unwrap();
    let output = fixture.command("query").arg("--query-file").arg(fixture.path("query"))
        .arg("--symbols-file").arg(fixture.path("symbols")).args(["--max-output-bytes", "1"]).output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "{\"version\":1,\"type\":\"error\",\"code\":\"output_limit\"}\n");
}

#[test]
fn invalid_or_overlong_key_files_are_refused_without_creating_database() {
    for length in [0, 95, 97] {
        let fixture = Fixture::new();
        fs::write(fixture.path("keys"), vec![1; length]).unwrap();
        let output = fixture.command("init").output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!fixture.path("db").exists());
    }
}

#[cfg(unix)]
#[test]
fn group_readable_key_material_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fs::set_permissions(fixture.path("keys"), fs::Permissions::from_mode(0o640)).unwrap();
    let output = fixture.command("init").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!fixture.path("db").exists());
}

#[test]
fn query_does_not_silently_create_a_missing_database() {
    let fixture = Fixture::new();
    let output = fixture.query("MATCH (a) RETURN a");
    assert_eq!(output.status.code(), Some(1));
    // The engine may create/open parent support files while attempting recovery;
    // it must not report a successful empty query for a missing database.
    assert!(!String::from_utf8_lossy(&output.stdout).contains("\"type\":\"complete\""));
}

#[test]
fn help_and_version_do_not_require_database_keys() {
    for option in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_fgdb")).arg(option).output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
    }
    assert!(Path::new(env!("CARGO_BIN_EXE_fgdb")).is_file());
}
