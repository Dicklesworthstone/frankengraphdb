// Run with: cargo run -p fgdb --example gql_quickstart
// The integration test includes this file, so it executes the identical script.
// All data enters through the public text compiler and engine-owned allocator.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
    PreparedGraphWriteScript,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const KNOWS: RelationId = RelationId(1);
const WORKS_AT: RelationId = RelationId(2);
type Error = Box<dyn std::error::Error>;

#[derive(Debug, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub statement: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<QueryValue>>,
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(KNOWS)),
        (GraphSymbolKind::Relation, "WORKS_AT") => Some(GraphSymbol::Relation(WORKS_AT)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Company") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "born") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Property, "team") => Some(GraphSymbol::Property(PropertyKeyId(3))),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn keys() -> DatabaseKeys {
    // Demonstration keys only. Applications must supply their own secret keys.
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn fresh_path() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fgdb-gql-quickstart-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

// Debug intentionally redacts engine values. Opt in to displaying only the
// public scalar/list domains projected by this demonstration, not engine state.
fn display_value(value: &fgdb_gql::algebra::GraphValue) -> Result<String, Error> {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalScalar;
    match value {
        GraphValue::Scalar(CanonicalScalar::Null) => Ok("NULL".to_owned()),
        GraphValue::Scalar(CanonicalScalar::Int(value)) => Ok(value.to_string()),
        GraphValue::Scalar(CanonicalScalar::Bool(value)) => Ok(value.to_string()),
        GraphValue::Scalar(CanonicalScalar::Text(value)) => Ok(format!("{:?}", value.as_str())),
        GraphValue::List(values) => {
            let cells = values
                .iter()
                .map(display_value)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("[{}]", cells.join(", ")))
        }
        _ => Err("quickstart projected an unexpected value domain".into()),
    }
}

fn display_cell(value: &QueryValue) -> Result<String, Error> {
    match value {
        QueryValue::Value(value) => display_value(value),
        QueryValue::Count(value) => Ok(value.to_string()),
        QueryValue::Integer(value) => Ok(value.to_string()),
        QueryValue::Average(_) => Err("quickstart projected an unexpected average".into()),
    }
}

fn read_step(
    db: &Database,
    contexts: &PurposeContexts,
    steps: &mut Vec<Step>,
    name: &'static str,
    statement: &str,
    parameters: &GqlParameters,
) -> Result<(), Error> {
    println!("\n{name}: {statement}");
    let result = db.query(&contexts.query(), statement, parameters, symbols, policy())?;
    let QueryResult::Rows { columns, rows } = result else {
        return Err("read returned a write receipt".into());
    };
    println!("  {columns:?}");
    for row in &rows {
        let cells = row
            .iter()
            .map(display_cell)
            .collect::<Result<Vec<_>, _>>()?;
        println!("  [{}]", cells.join(", "));
    }
    if rows.is_empty() {
        println!("  (no rows)");
    }
    steps.push(Step {
        name,
        statement: statement.to_owned(),
        columns,
        rows,
    });
    Ok(())
}

async fn write_step(
    db: &mut Database,
    contexts: &PurposeContexts,
    steps: &mut Vec<Step>,
    name: &'static str,
    statement: &str,
    relation: RelationId,
) -> Result<(), Error> {
    println!("\n{name}: {statement}");
    // Bind once, then execute the real native program. This public entrypoint
    // avoids a caller-supplied ID counter; every identity belongs to the engine.
    let program = PreparedGraphWriteScript::prepare(statement, relation, symbols)?
        .bind_parameters(&GqlParameters::new())?;
    let (receipt, completion) = db
        .execute_graph_write_program_returning_autocommit_engine_governed(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            &program,
            GraphWriteProgramPolicy::new(policy(), 100, 100, 100),
        )
        .await?;
    println!("  (no result rows); {completion:?}; {:?}", receipt.stats());
    steps.push(Step {
        name,
        statement: statement.to_owned(),
        columns: Vec::new(),
        rows: Vec::new(),
    });
    Ok(())
}

pub fn run() -> Result<Vec<Step>, Error> {
    let path = fresh_path();
    println!(
        "Database: {} (real VFS, two-fsync commits; retained after exit)",
        path.display()
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let mut db = Database::create(&contexts.commit(), &path, keys()).await?;
        let mut steps = Vec::new();
        let parameters = GqlParameters::new();
        write_step(&mut db, &contexts, &mut steps, "insert social graph", 
            "INSERT (a:Person {name:'Ada',born:1815,team:1}), (c:Person {name:'Charles',born:1791,team:1}), (g:Person {name:'Grace',born:1906,team:2}), (t:Person {name:'Alan',born:1912,team:2}), (e:Person {name:'Edsger',born:1930,team:2}), (b:Person {name:'Barbara',born:1939,team:2}), (d:Person {name:'Donald',born:1938,team:2}), (f:Person {name:'Frances',born:1932,team:2}), (x:Company {name:'Engine'}), (y:Company {name:'Lab'}), (a)-[:KNOWS]->(c), (c)-[:KNOWS]->(g), (g)-[:KNOWS]->(t), (a)-[:KNOWS]->(b), (d)-[:KNOWS]->(f)", KNOWS).await?;
        write_step(&mut db, &contexts, &mut steps, "insert workplaces",
            "MATCH (p:Person),(c:Company) WHERE (p.team=1 AND c.name='Engine') OR (p.name='Grace' AND c.name='Lab') OR (p.name='Alan' AND c.name='Lab') INSERT (p)-[:WORKS_AT]->(c)", WORKS_AT).await?;
        read_step(&db, &contexts, &mut steps, "match ordered",
            "MATCH (p:Person) WHERE p.born < 1920 RETURN p.name AS name,p.born AS born ORDER BY born", &parameters)?;
        read_step(&db, &contexts, &mut steps, "optional workplaces",
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:WORKS_AT]->(c) RETURN p.name AS name,c.name AS company ORDER BY name", &parameters)?;
        read_step(&db, &contexts, &mut steps, "count by team",
            "MATCH (p:Person) RETURN p.team AS team,COUNT(*) AS people GROUP BY p.team ORDER BY team", &parameters)?;
        read_step(&db, &contexts, &mut steps, "with pipeline",
            "MATCH (p:Person) WITH p.born AS year ORDER BY year LIMIT 2 RETURN COUNT(*) AS people,SUM(year) AS total", &parameters)?;
        read_step(&db, &contexts, &mut steps, "union",
            "MATCH (p:Person) WHERE p.team=1 RETURN p.name AS name UNION MATCH (q:Person) WHERE q.name='Grace' RETURN q.name AS name ORDER BY name", &parameters)?;
        read_step(&db, &contexts, &mut steps, "shortest walk",
            "MATCH p = ANY SHORTEST WALK (a)-[:KNOWS*1..7]->(b) WHERE a.name='Ada' AND b.name='Alan' RETURN path_length(p)", &parameters)?;
        let old = db.frontier()?;
        write_step(&mut db, &contexts, &mut steps, "set birth year",
            "MATCH (p:Person) WHERE p.name='Ada' SET p.born=1816", KNOWS).await?;
        read_step(&db, &contexts, &mut steps, "updated value",
            "MATCH (p:Person) WHERE p.name='Ada' RETURN p.born AS born", &parameters)?;
        let historical = GqlParameters::new().with_uint64("old", old.0)?;
        read_step(&db, &contexts, &mut steps, "historical value",
            "MATCH (p:Person) FOR SYSTEM_TIME AS OF SEQ $old WHERE p.name='Ada' RETURN p.born AS born", &historical)?;
        write_step(&mut db, &contexts, &mut steps, "delete relationship",
            "MATCH (a)-[e:KNOWS]->(b) WHERE a.name='Ada' AND b.name='Charles' DELETE e", KNOWS).await?;
        read_step(&db, &contexts, &mut steps, "deleted relationship absent",
            "MATCH (a)-[:KNOWS]->(b) WHERE a.name='Ada' AND b.name='Charles' RETURN b.name AS name", &parameters)?;
        write_step(&mut db, &contexts, &mut steps, "merge matched",
            "MERGE (p:Person {name:'Ada'}) ON MATCH SET p.born=1817 ON CREATE SET p.born=0", KNOWS).await?;
        read_step(&db, &contexts, &mut steps, "merge matched value",
            "MATCH (p:Person) WHERE p.name='Ada' RETURN p.born AS born", &parameters)?;
        write_step(&mut db, &contexts, &mut steps, "merge created",
            "MERGE (p:Person {name:'Katherine'}) ON MATCH SET p.born=0 ON CREATE SET p.born=1918", KNOWS).await?;
        read_step(&db, &contexts, &mut steps, "merge created value",
            "MATCH (p:Person) WHERE p.name='Katherine' RETURN p.born AS born", &parameters)?;
        read_step(&db, &contexts, &mut steps, "collect list",
            "MATCH (p:Person) WHERE p.team=1 RETURN collect(p.name) AS names", &parameters)?;
        read_step(&db, &contexts, &mut steps, "unwind list",
            "MATCH (p:Person) WHERE p.name='Ada' WITH ['Ada','Grace','Ada'] AS names UNWIND names AS name RETURN name", &parameters)?;
        println!("\nEXPLAIN omitted: fgdb-gql-explain-certificate-pyn9 is not closed at authoring time.");
        drop(db);
        let db = Database::open(&contexts.commit(), &path, keys()).await?;
        read_step(&db, &contexts, &mut steps, "reopened value",
            "MATCH (p:Person) WHERE p.name='Ada' RETURN p.name AS name,p.born AS born", &parameters)?;
        Ok(steps)
    })
}

#[cfg(not(test))]
fn main() -> Result<(), Error> {
    run().map(|_| ())
}
