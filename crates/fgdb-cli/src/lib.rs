//! The local-owner CLI's bounded inputs and lossless output contract.
//!
//! This is not a remote authenticated session or an authorization service. A
//! caller with the database's storage keys already has local-owner authority.
//! Reads use the native read binder; there is no keyword-based read-only test,
//! query interpolation, guessed catalog, retry-as-another-command or alternate
//! engine. Network clients must use Fabric's separate security boundary.

#![forbid(unsafe_code)]

pub mod output;

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameterValue, GqlParameters, GqlScalarParameter, GraphSymbol, GraphSymbolKind};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const MAX_INPUT_BYTES: usize = 65_536;
pub const MAX_SYMBOLS: usize = 4_096;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

pub const HELP: &str = "fgdb — local-owner database CLI\n\
Usage: fgdb <init|query|compact> --db DIR --keys-file FILE [OPTIONS]\n\
       fgdb --help | --version\n\
\n\
Common options:\n\
  --format human|ndjson       Output format (default: human)\n\
  --max-output-bytes N        Complete response cap (default: 8388608)\n\
Query options:\n\
  --query-file FILE|-        Required UTF-8 statement; - reads stdin\n\
  --symbols-file FILE        Explicit kind<TAB>name<TAB>id catalog\n\
  --params-file FILE         Typed kind<TAB>name<TAB>value arguments\n\
  --max-rows N               Result row cap (default: 10000)\n\
  --max-work N               Native work and scratch-unit cap (default: 1000000)\n\
\n\
The keys file is exactly 96 binary bytes: k_oid[32], namespace[32], dek[32].\n\
On Unix it must be a regular file with no group/other permission bits.\n\
query includes native EXPLAIN, but never performs writes or retries.\n\
This binary does not connect to a network server or implement remote RBAC.\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Usage,
    Input,
    Keys,
    Database,
    Query,
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
            Self::OutputLimit => "output_limit",
            Self::Output => "output_failed",
            Self::Context => "context_stopped",
        }
    }
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage | Self::Input | Self::Keys => 2,
            Self::Query | Self::OutputLimit => 3,
            Self::Database | Self::Output | Self::Context => 1,
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
pub enum Command { Init, Query, Compact }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format { Human, Ndjson }
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation { Help, Version, Run(Options) }
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
}

/// Parse bounded argv without displaying a path, statement or argument value.
/// Duplicate options are errors rather than last-wins security configuration.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Invocation, Error> {
    let mut words = Vec::new();
    let mut bytes = 0usize;
    for arg in args {
        bytes = bytes.checked_add(arg.len()).ok_or(Error::Usage)?;
        if bytes > MAX_INPUT_BYTES || words.len() == 32 { return Err(Error::Usage); }
        words.push(arg);
    }
    if words.as_slice() == ["--help"] { return Ok(Invocation::Help); }
    if words.as_slice() == ["--version"] { return Ok(Invocation::Version); }
    let command = match words.first().map(String::as_str) {
        Some("init") => Command::Init,
        Some("query") => Command::Query,
        Some("compact") => Command::Compact,
        _ => return Err(Error::Usage),
    };
    let mut values = BTreeMap::new();
    for pair in words[1..].chunks(2) {
        if pair.len() != 2 || pair[1].is_empty() { return Err(Error::Usage); }
        let key = pair[0].as_str();
        if !matches!(key, "--db" | "--keys-file" | "--query-file" | "--symbols-file"
            | "--params-file" | "--format" | "--max-rows" | "--max-work" | "--max-output-bytes")
            || values.insert(key, pair[1].as_str()).is_some() {
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
    if (command == Command::Query) != query_file.is_some()
        || (command != Command::Query && (symbols_file.is_some() || params_file.is_some()
            || values.contains_key("--max-rows") || values.contains_key("--max-work"))) {
        return Err(Error::Usage);
    }
    let max_rows = bounded_number(values.remove("--max-rows"), 10_000, 1_000_000)?;
    let max_work = bounded_number(values.remove("--max-work"), 1_000_000, 100_000_000)?;
    let max_output_bytes = bounded_number(values.remove("--max-output-bytes"),
        8 * 1024 * 1024, MAX_OUTPUT_BYTES as u64)? as usize;
    Ok(Invocation::Run(Options { command, db, keys_file, query_file, symbols_file,
        params_file, format, max_rows, max_work, max_output_bytes }))
}

fn bounded_number(value: Option<&str>, default: u64, limit: u64) -> Result<u64, Error> {
    let Some(value) = value else { return Ok(default); };
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) { return Err(Error::Usage); }
    value.parse::<u64>().ok().filter(|n| *n > 0 && *n <= limit).ok_or(Error::Usage)
}

/// Explicit per-invocation schema bindings. Names never become invented IDs.
#[derive(Default)]
pub struct Symbols(BTreeMap<(GraphSymbolKind, String), GraphSymbol>);
impl Symbols {
    pub fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > MAX_INPUT_BYTES { return Err(Error::Input); }
        let mut symbols = Self::default();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.split('\t');
            let kind = fields.next().ok_or(Error::Input)?;
            let name = fields.next().ok_or(Error::Input)?;
            let number = fields.next().ok_or(Error::Input)?;
            if fields.next().is_some() || name.is_empty() || name.len() > 256
                || name.chars().any(char::is_control)
                || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Input);
            }
            let symbol = match kind {
                "relation" => GraphSymbol::Relation(RelationId(number.parse().map_err(|_| Error::Input)?)),
                "label" => GraphSymbol::Label(LabelId(number.parse().map_err(|_| Error::Input)?)),
                "property" => GraphSymbol::Property(PropertyKeyId(number.parse().map_err(|_| Error::Input)?)),
                _ => return Err(Error::Input),
            };
            if symbols.0.len() == MAX_SYMBOLS
                || symbols.0.insert((symbol.kind(), name.to_owned()), symbol).is_some() {
                return Err(Error::Input);
            }
        }
        Ok(symbols)
    }
    pub fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.0.get(&(kind, name.to_owned())).copied()
    }
}

/// Strict typed TSV. `text` consumes the rest of its line verbatim (including
/// tabs and quotes); it is admitted as a scalar, NEVER interpolated into GQL.
/// Duplicate names and scalar/transcript limits use the engine's own checks.
pub fn parse_parameters(text: &str) -> Result<GqlParameters, Error> {
    if text.len() > MAX_INPUT_BYTES { return Err(Error::Input); }
    let mut parameters = GqlParameters::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, '\t');
        let kind = fields.next().ok_or(Error::Input)?;
        let name = fields.next().ok_or(Error::Input)?;
        let value = fields.next().ok_or(Error::Input)?;
        let parameter = match kind {
            "int64" => GqlParameterValue::Int64(value.parse().map_err(|_| Error::Input)?),
            "uint64" => GqlParameterValue::UInt64(value.parse().map_err(|_| Error::Input)?),
            "text" => scalar_parameter(CanonicalScalar::ucs_basic_text(value).map_err(|_| Error::Input)?)?,
            "bool" => scalar_parameter(CanonicalScalar::Bool(match value {
                "true" => true, "false" => false, _ => return Err(Error::Input),
            }))?,
            "null" if value.is_empty() => scalar_parameter(CanonicalScalar::Null)?,
            _ => return Err(Error::Input),
        };
        parameters.insert(name, parameter).map_err(|_| Error::Input)?;
    }
    Ok(parameters)
}
fn scalar_parameter(value: CanonicalScalar) -> Result<GqlParameterValue, Error> {
    GqlScalarParameter::new(value).map(GqlParameterValue::Scalar).map_err(|_| Error::Input)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(s: &str) -> Result<Invocation, Error> { parse_args(s.split_whitespace().map(str::to_owned)) }
    #[test]
    fn required_duplicate_and_unknown_options_fail_closed() {
        for args in ["", "query", "query --db db --keys-file keys", "init --db db --keys-file keys --db other",
            "init --db db --keys-file keys --unsafe true", "init --db db --keys-file keys --query-file q",
            "query --db db --keys-file keys --query-file q --max-rows 0",
            "query --db db --keys-file keys --query-file q --max-work 18446744073709551616"] {
            assert_eq!(parse(args), Err(Error::Usage));
        }
        assert!(matches!(parse("query --db db --keys-file keys --query-file - --format ndjson"), Ok(Invocation::Run(_))));
    }
    #[test]
    fn symbols_are_explicit_domain_separated_and_duplicates_refuse() {
        let symbols = Symbols::parse("relation\tX\t1\nlabel\tX\t2\nproperty\tx\t3\n").unwrap();
        assert!(matches!(symbols.resolve(GraphSymbolKind::Relation, "X"), Some(GraphSymbol::Relation(RelationId(1)))));
        assert!(symbols.resolve(GraphSymbolKind::Relation, "x").is_none());
        assert!(Symbols::parse("label\tX\t1\nlabel\tX\t2").is_err());
        assert!(Symbols::parse("unknown\tX\t1").is_err());
        assert!(Symbols::parse("relation\tX\t1\textra").is_err());
    }
    #[test]
    fn typed_parameters_use_engine_duplicate_and_range_checks() {
        assert!(parse_parameters("int64\tlo\t-9223372036854775808\nuint64\thi\t18446744073709551615\ntext\ttext\t'$x'\tMATCH\nbool\tb\ttrue\nnull\tn\t").is_ok());
        for text in ["int64\tx\t9223372036854775808", "uint64\tx\t-1", "bool\tx\tTRUE",
            "int64\tx\t1\ntext\tx\tsecret", "text\tbad name\tx", "null\tx\tfalse"] {
            assert_eq!(parse_parameters(text).err().unwrap(), Error::Input);
        }
    }
}
