use fgdb::Database;
#[test]
fn   deliberate_red_proof( ) {
let     _db:Result<Database,fgdb::Error>=Database::open(":memory:");
assert!(_db.is_ok());
}
