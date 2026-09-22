//! The local-owner CLI's bounded inputs and lossless output contract.
//!
//! This is not a remote authenticated session or an authorization service. A
//! caller with the database's storage keys already has local-owner authority.
//! Reads use the native read binder; there is no keyword-based read-only test,
//! query interpolation, guessed catalog, retry-as-another-command or alternate
//! engine. Network clients must use Fabric's separate security boundary.

#![forbid(unsafe_code)]

pub mod output;
pub mod write;

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterValue, GqlParameters, GqlScalarParameter, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, ReverseSymbolCatalog,
};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const MAX_INPUT_BYTES: usize = 65_536;
pub const MAX_SYMBOLS: usize = 4_096;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

pub const HELP: &str = "fgdb — local-owner database CLI\n\
Usage: fgdb <init|query|write|import-csv|compact> --db DIR --keys-file FILE [OPTIONS]\n\
       fgdb --help | --version\n\
\n\
Common options:\n\
  --format human|ndjson       Output format (default: human)\n\
  --max-output-bytes N        Complete response cap (default: 8388608)\n\
Query/write/import options:\n\
  --query-file FILE|-        Required UTF-8 statement/script; - reads stdin\n\
  --symbols-file FILE        Explicit kind<TAB>name<TAB>id catalog\n\
  --params-file FILE         Typed kind<TAB>name<TAB>value arguments\n\
  --max-rows N               Result row cap (default: 10000)\n\
  --max-work N               Native work and scratch-unit cap (default: 1000000)\n\
Write/import options:\n\
  --relation ID              Required explicit vertex-effect relation coordinate\n\
  --max-changes N            Each of effects/new vertices/new edges (default: 10000)\n\
  --max-statements N         Whole-program cap (default: 64; import maximum: 65536)\n\
CSV import options:\n\
  --csv-file FILE|-          Required CSV with exact parameter-name header\n\
  --types-file FILE          Optional kind<TAB>name declarations; no value inference\n\
  --max-input-bytes N        CSV source cap (default: 16777216; maximum: 67108864)\n\
CSV types: int64, uint64, int (nullable), text, bool, null. Undeclared\n\
parameters retain native numeric inference. Unquoted \\N is null, not empty text.\n\
--params-file applies to query/write, not CSV. Only one input may use stdin.\n\
Each write/import is ONE atomic native program, using engine-owned identities.\n\
No per-row commits, implicit retries, or query-to-write fallback occur.\n\
\n\
The keys file is exactly 96 binary bytes: k_oid[32], namespace[32], dek[32].\n\
On Unix it must be a regular file with no group/other permission bits.\n\
query includes native EXPLAIN, but never performs writes or retries.\n\
commit_outcome_unknown requires recovery; committed_needs_recovery is committed.\n\
write_completed_output_failed means execution completed; do not blindly retry.\n\
This binary does not connect to a network server or implement remote RBAC.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Usage,
    Input,
    Keys,
    Database,
    Query,
    Write,
    CommitOutcomeUnknown,
    CommittedNeedsRecovery,
    WriteOutput,
    OutputLimit,
    Output,
    Context,
}
impl Error {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Usage => "usage",
            Self::Input => "invalid_input",
            Self::Keys => "keys_refused",
            Self::Database => "database_refused",
            Self::Query => "query_refused",
            Self::Write => "write_failed",
            Self::CommitOutcomeUnknown => "commit_outcome_unknown",
            Self::CommittedNeedsRecovery => "committed_needs_recovery",
            Self::WriteOutput => "write_completed_output_failed",
            Self::OutputLimit => "output_limit",
            Self::Output => "output_failed",
            Self::Context => "context_stopped",
        }
    }
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage | Self::Input | Self::Keys => 2,
            Self::Query | Self::Write | Self::OutputLimit => 3,
            Self::Database
            | Self::Output
            | Self::Context
            | Self::CommitOutcomeUnknown
            | Self::CommittedNeedsRecovery
            | Self::WriteOutput => 1,
        }
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.code())
    }
}
impl core::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Init,
    Query,
    Write,
    ImportCsv,
    Compact,
}
impl Command {
    pub const fn is_write(self) -> bool {
        matches!(self, Self::Write | Self::ImportCsv)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Human,
    Ndjson,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    Help,
    Version,
    Run(Options),
}
#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    pub db: PathBuf,
    pub keys_file: PathBuf,
    pub query_file: Option<PathBuf>,
    pub symbols_file: Option<PathBuf>,
    pub params_file: Option<PathBuf>,
    pub format: Format,
    pub max_rows: u64,
    pub max_work: u64,
    pub max_output_bytes: usize,
    pub write: Option<WriteOptions>,
}

/// Explicit local-owner write admission. These are not remote capabilities.
#[derive(Debug, PartialEq, Eq)]
pub struct WriteOptions {
    pub relation: RelationId,
    pub csv_file: Option<PathBuf>,
    pub types_file: Option<PathBuf>,
    pub max_statements: usize,
    pub max_input_bytes: usize,
    pub max_changes: u64,
}

/// Parse bounded argv without displaying a path, statement or argument value.
/// Duplicate options are errors rather than last-wins security configuration.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Invocation, Error> {
    let mut words = Vec::new();
    let mut bytes = 0usize;
    for arg in args {
        bytes = bytes.checked_add(arg.len()).ok_or(Error::Usage)?;
        if bytes > MAX_INPUT_BYTES || words.len() == 64 {
            return Err(Error::Usage);
        }
        words.push(arg);
    }
    if words.as_slice() == ["--help"] {
        return Ok(Invocation::Help);
    }
    if words.as_slice() == ["--version"] {
        return Ok(Invocation::Version);
    }
    let command = match words.first().map(String::as_str) {
        Some("init") => Command::Init,
        Some("query") => Command::Query,
        Some("write") => Command::Write,
        Some("import-csv") => Command::ImportCsv,
        Some("compact") => Command::Compact,
        _ => return Err(Error::Usage),
    };
    let mut values = BTreeMap::new();
    for pair in words[1..].chunks(2) {
        if pair.len() != 2 || pair[1].is_empty() {
            return Err(Error::Usage);
        }
        let key = pair[0].as_str();
        if !matches!(
            key,
            "--db"
                | "--keys-file"
                | "--query-file"
                | "--symbols-file"
                | "--params-file"
                | "--format"
                | "--max-rows"
                | "--max-work"
                | "--max-output-bytes"
                | "--relation"
                | "--csv-file"
                | "--types-file"
                | "--max-statements"
                | "--max-input-bytes"
                | "--max-changes"
        ) || values.insert(key, pair[1].as_str()).is_some()
        {
            return Err(Error::Usage);
        }
    }
    let db = values.remove("--db").ok_or(Error::Usage)?.into();
    let keys_file = values.remove("--keys-file").ok_or(Error::Usage)?.into();
    let query_file = values.remove("--query-file").map(PathBuf::from);
    let symbols_file = values.remove("--symbols-file").map(PathBuf::from);
    let params_file = values.remove("--params-file").map(PathBuf::from);
    let format = match values.remove("--format").unwrap_or("human") {
        "human" => Format::Human,
        "ndjson" => Format::Ndjson,
        _ => return Err(Error::Usage),
    };
    let has_statement = command == Command::Query || command.is_write();
    if has_statement != query_file.is_some()
        || (!has_statement
            && (symbols_file.is_some()
                || params_file.is_some()
                || values.contains_key("--max-rows")
                || values.contains_key("--max-work")))
    {
        return Err(Error::Usage);
    }
    let write = if command.is_write() {
        let number = values.remove("--relation").ok_or(Error::Usage)?;
        if !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::Usage);
        }
        let relation = RelationId(number.parse().map_err(|_| Error::Usage)?);
        let csv_file = values.remove("--csv-file").map(PathBuf::from);
        let types_file = values.remove("--types-file").map(PathBuf::from);
        if (command == Command::ImportCsv) != csv_file.is_some()
            || (command == Command::ImportCsv && params_file.is_some())
            || (command == Command::Write
                && (types_file.is_some() || values.contains_key("--max-input-bytes")))
            || (query_file.as_deref() == Some(std::path::Path::new("-"))
                && csv_file.as_deref() == Some(std::path::Path::new("-")))
        {
            return Err(Error::Usage);
        }
        let ceiling = if command == Command::ImportCsv {
            fgdb_gql::PreparedGraphWriteScript::MAX_BATCH_STATEMENTS
        } else {
            fgdb_gql::MAX_GRAPH_MUTATION_STATEMENTS
        };
        let max_statements =
            bounded_number(values.remove("--max-statements"), 64, ceiling as u64)? as usize;
        let max_input_bytes = bounded_number(
            values.remove("--max-input-bytes"),
            16 * 1024 * 1024,
            fgdb_gql::csv_parameters::CsvParameterLimits::HARD.max_input_bytes as u64,
        )? as usize;
        let max_changes = bounded_number(values.remove("--max-changes"), 10_000, 1_000_000)?;
        Some(WriteOptions {
            relation,
            csv_file,
            types_file,
            max_statements,
            max_input_bytes,
            max_changes,
        })
    } else {
        if [
            "--relation",
            "--csv-file",
            "--types-file",
            "--max-statements",
            "--max-input-bytes",
            "--max-changes",
        ]
        .iter()
        .any(|key| values.contains_key(key))
        {
            return Err(Error::Usage);
        }
        None
    };
    let max_rows = bounded_number(values.remove("--max-rows"), 10_000, 1_000_000)?;
    let max_work = bounded_number(values.remove("--max-work"), 1_000_000, 100_000_000)?;
    let max_output_bytes = bounded_number(
        values.remove("--max-output-bytes"),
        8 * 1024 * 1024,
        MAX_OUTPUT_BYTES as u64,
    )? as usize;
    Ok(Invocation::Run(Options {
        command,
        db,
        keys_file,
        query_file,
        symbols_file,
        params_file,
        format,
        max_rows,
        max_work,
        max_output_bytes,
        write,
    }))
}

fn bounded_number(value: Option<&str>, default: u64, limit: u64) -> Result<u64, Error> {
    let Some(value) = value else {
        return Ok(default);
    };
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Usage);
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0 && *n <= limit)
        .ok_or(Error::Usage)
}

/// Explicit per-invocation schema bindings. Names never become invented IDs.
#[derive(Default)]
pub struct Symbols {
    forward: BTreeMap<(GraphSymbolKind, String), GraphSymbol>,
    reverse: ReverseSymbolCatalog,
}
impl Symbols {
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > MAX_INPUT_BYTES {
            return Err(Error::Input);
        }
        let mut symbols = Self::default();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.split('\t');
            let kind = fields.next().ok_or(Error::Input)?;
            let name = fields.next().ok_or(Error::Input)?;
            let number = fields.next().ok_or(Error::Input)?;
            if fields.next().is_some()
                || name.is_empty()
                || name.len() > 256
                || name.chars().any(char::is_control)
                || number.is_empty()
                || !number.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(Error::Input);
            }
            let symbol = match kind {
                "relation" => {
                    GraphSymbol::Relation(RelationId(number.parse().map_err(|_| Error::Input)?))
                }
                "label" => GraphSymbol::Label(LabelId(number.parse().map_err(|_| Error::Input)?)),
                "property" => {
                    GraphSymbol::Property(PropertyKeyId(number.parse().map_err(|_| Error::Input)?))
                }
                _ => return Err(Error::Input),
            };
            if symbols.forward.len() == MAX_SYMBOLS
                || symbols
                    .forward
                    .insert((symbol.kind(), name.to_owned()), symbol)
                    .is_some()
            {
                return Err(Error::Input);
            }
            // This format declares canonical names, not aliases. A second
            // label/relation name for one ID has no unambiguous reflection.
            let previous = match symbol {
                GraphSymbol::Label(id) => symbols.reverse.labels.insert(id, name.to_owned()),
                GraphSymbol::Relation(id) => symbols.reverse.relations.insert(id, name.to_owned()),
                GraphSymbol::Property(_) => None,
            };
            if previous.is_some() {
                return Err(Error::Input);
            }
        }
        Ok(symbols)
    }
    pub fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.forward.get(&(kind, name.to_owned())).copied()
    }
}

// Supply the complete catalog to reflective queries. A resolver closure only
// supports forward lookup and makes the engine fall back to probing names
// mentioned in the query/common-name list, which loses unrelated stored labels.
impl GraphSymbolResolver for &Symbols {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        Some(self.reverse.clone())
    }
    fn reverse_label(&self, id: LabelId) -> Option<String> {
        self.reverse.labels.get(&id).cloned()
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.reverse.relations.get(&id).cloned()
    }
}

/// Strict typed TSV. `text` consumes the rest of its line verbatim (including
/// tabs and quotes); it is admitted as a scalar, NEVER interpolated into GQL.
/// Duplicate names and scalar/transcript limits use the engine's own checks.
pub fn parse_parameters(text: &str) -> Result<GqlParameters, Error> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(Error::Input);
    }
    let mut parameters = GqlParameters::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, '\t');
        let kind = fields.next().ok_or(Error::Input)?;
        let name = fields.next().ok_or(Error::Input)?;
        let value = fields.next().ok_or(Error::Input)?;
        let parameter = match kind {
            "int64" => GqlParameterValue::Int64(value.parse().map_err(|_| Error::Input)?),
            "uint64" => GqlParameterValue::UInt64(value.parse().map_err(|_| Error::Input)?),
            "text" => {
                scalar_parameter(CanonicalScalar::ucs_basic_text(value).map_err(|_| Error::Input)?)?
            }
            "bool" => scalar_parameter(CanonicalScalar::Bool(match value {
                "true" => true,
                "false" => false,
                _ => return Err(Error::Input),
            }))?,
            "null" if value.is_empty() => scalar_parameter(CanonicalScalar::Null)?,
            _ => return Err(Error::Input),
        };
        parameters
            .insert(name, parameter)
            .map_err(|_| Error::Input)?;
    }
    Ok(parameters)
}
fn scalar_parameter(value: CanonicalScalar) -> Result<GqlParameterValue, Error> {
    GqlScalarParameter::new(value)
        .map(GqlParameterValue::Scalar)
        .map_err(|_| Error::Input)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> Result<Invocation, Error> {
        parse_args(s.split_whitespace().map(str::to_owned))
    }
    #[test]
    fn required_duplicate_and_unknown_options_fail_closed() {
        for args in [
            "",
            "query",
            "query --db db --keys-file keys",
            "init --db db --keys-file keys --db other",
            "init --db db --keys-file keys --unsafe true",
            "init --db db --keys-file keys --query-file q",
            "query --db db --keys-file keys --query-file q --max-rows 0",
            "query --db db --keys-file keys --query-file q --max-work 18446744073709551616",
        ] {
            assert_eq!(parse(args), Err(Error::Usage));
        }
        assert!(matches!(
            parse("query --db db --keys-file keys --query-file - --format ndjson"),
            Ok(Invocation::Run(_))
        ));
    }
    #[test]
    fn symbols_are_explicit_domain_separated_and_duplicates_refuse() {
        let symbols = Symbols::parse("relation\tX\t1\nlabel\tX\t2\nproperty\tx\t3\n").unwrap();
        assert!(matches!(
            symbols.resolve(GraphSymbolKind::Relation, "X"),
            Some(GraphSymbol::Relation(RelationId(1)))
        ));
        assert!(symbols.resolve(GraphSymbolKind::Relation, "x").is_none());
        assert!(Symbols::parse("label\tX\t1\nlabel\tX\t2").is_err());
        assert!(Symbols::parse("unknown\tX\t1").is_err());
        assert!(Symbols::parse("relation\tX\t1\textra").is_err());
    }
    #[test]
    fn full_reverse_catalog_preserves_names_absent_from_the_statement() {
        let symbols =
            Symbols::parse("label\tUnmentionedLabel\t17\nrelation\tUnmentionedRelation\t29\n")
                .unwrap();
        let resolver = &symbols;
        let catalog = resolver.reverse_catalog().unwrap();
        assert_eq!(
            catalog.labels.get(&LabelId(17)).map(String::as_str),
            Some("UnmentionedLabel")
        );
        assert_eq!(
            resolver.reverse_relation(RelationId(29)).as_deref(),
            Some("UnmentionedRelation")
        );
        for ambiguous in ["label\tA\t1\nlabel\tB\t1", "relation\tR\t1\nrelation\tS\t1"] {
            assert!(Symbols::parse(ambiguous).is_err());
        }
        // IDs remain independent across the two domains.
        assert!(Symbols::parse("label\tA\t1\nrelation\tR\t1").is_ok());
    }
    #[test]
    fn typed_parameters_use_engine_duplicate_and_range_checks() {
        assert!(parse_parameters("int64\tlo\t-9223372036854775808\nuint64\thi\t18446744073709551615\ntext\ttext\t'$x'\tMATCH\nbool\tb\ttrue\nnull\tn\t").is_ok());
        for text in [
            "int64\tx\t9223372036854775808",
            "uint64\tx\t-1",
            "bool\tx\tTRUE",
            "int64\tx\t1\ntext\tx\tsecret",
            "text\tbad name\tx",
            "null\tx\tfalse",
        ] {
            assert_eq!(parse_parameters(text).err().unwrap(), Error::Input);
        }
    }

    #[test]
    fn write_and_import_options_require_explicit_unambiguous_inputs() {
        for args in [
            "write --db db --keys-file keys --query-file q",
            "import-csv --db db --keys-file keys --query-file q --relation 1",
            "import-csv --db db --keys-file keys --query-file - --csv-file - --relation 1",
            "import-csv --db db --keys-file keys --query-file q --csv-file c --relation 1 --params-file p",
            "write --db db --keys-file keys --query-file q --relation 1 --csv-file c",
            "write --db db --keys-file keys --query-file q --relation 1 --types-file t",
            "write --db db --keys-file keys --query-file q --relation 1 --max-input-bytes 100",
            "query --db db --keys-file keys --query-file q --relation 1",
            "init --db db --keys-file keys --max-changes 3",
            "write --db db --keys-file keys --query-file q --relation -1",
            "write --db db --keys-file keys --query-file q --relation 1 --relation 2",
            "write --db db --keys-file keys --query-file q --relation 1 --max-statements 65",
            "import-csv --db db --keys-file keys --query-file q --csv-file c --relation 1 --max-statements 65537",
            "import-csv --db db --keys-file keys --query-file q --csv-file c --relation 1 --max-input-bytes 67108865",
            "write --db db --keys-file keys --query-file q --relation 1 --max-changes 0",
        ] {
            assert_eq!(parse(args), Err(Error::Usage), "{args}");
        }
        let Invocation::Run(options) = parse("import-csv --db db --keys-file keys --query-file q --csv-file - --relation 7 --max-statements 65536").unwrap()
            else { panic!("run") };
        assert_eq!(options.command, Command::ImportCsv);
        let write = options.write.unwrap();
        assert_eq!(write.relation, RelationId(7));
        assert_eq!(write.max_statements, 65_536);
        assert_eq!(write.max_changes, 10_000);
        assert_eq!(write.max_input_bytes, 16 * 1024 * 1024);
        assert!(matches!(
            parse("write --db db --keys-file keys --query-file - --relation 1 --params-file p"),
            Ok(Invocation::Run(_))
        ));
    }
}
