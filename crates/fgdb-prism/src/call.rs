//! Compile the registered CALL grammar before observing a graph. The signature
//! is an in-core host capability, not an external-memory exposure declaration.

use fgdb_crypto::{Digest, Hasher};
use std::collections::BTreeMap;

pub const FNX_SIGNATURE_REGISTRY_VERSION: u16 = 1;
pub const MAX_FNX_CALL_BYTES: usize = 16 * 1024;
pub const FNX_NUMERIC_PROFILE: &str = "fnx-f64-canonical-node-order-v1";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FnxArgument {
    Integer(i64),
    Float(f64),
    Boolean(bool),
}
pub type FnxParameters = BTreeMap<String, FnxArgument>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxParameterType {
    FiniteFloat,
    PositiveInteger,
    Boolean,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnxParameterSpec {
    pub name: &'static str,
    pub value_type: FnxParameterType,
    pub default: FnxArgument,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxOutput {
    Vertex,
    Score,
}
impl FnxOutput {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Score => "score",
        }
    }
}

/// Exposure is deliberately explicit: none of the current signatures has a
/// spill implementation or a checkpointable fnx iteration loop. A server must
/// not infer a larger-than-memory or OLTP-isolated operator from this catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxImplementationClass {
    InCoreDecodedCache,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnxSignature {
    pub name: &'static str,
    /// The one supplied snapshot projection, not a mutable named graph.
    pub graph_input_arity: u8,
    pub parameters: &'static [FnxParameterSpec],
    pub outputs: &'static [FnxOutput],
    pub implementation: FnxImplementationClass,
    pub numeric_profile: &'static str,
    pub rng_policy: &'static str,
    pub graph_laws: &'static str,
    pub complexity: &'static str,
}
const PAGERANK_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec { name: "alpha", value_type: FnxParameterType::FiniteFloat, default: FnxArgument::Float(0.85) },
    FnxParameterSpec { name: "max_iter", value_type: FnxParameterType::PositiveInteger, default: FnxArgument::Integer(100) },
    FnxParameterSpec { name: "tol", value_type: FnxParameterType::FiniteFloat, default: FnxArgument::Float(1.0e-6) },
    FnxParameterSpec { name: "weighted", value_type: FnxParameterType::Boolean, default: FnxArgument::Boolean(true) },
];
const SIGNATURES: &[FnxSignature] = &[FnxSignature {
    name: "fnx.pagerank",
    graph_input_arity: 1,
    parameters: PAGERANK_PARAMETERS,
    outputs: &[FnxOutput::Vertex, FnxOutput::Score],
    implementation: FnxImplementationClass::InCoreDecodedCache,
    numeric_profile: FNX_NUMERIC_PROFILE,
    rng_policy: "none; no algorithm RNG or user callback",
    graph_laws: "explicit direction/parallel/self-loop projection; nonnegative finite weighted rows; isolates retained",
    complexity: "O(k * (n + adjacency_entries)); O(n + adjacency_entries) resident working data",
}];

pub struct FnxSignatureRegistry;
impl FnxSignatureRegistry {
    pub const fn version() -> u16 { FNX_SIGNATURE_REGISTRY_VERSION }
    pub const fn signatures() -> &'static [FnxSignature] { SIGNATURES }
    pub fn lookup(name: &str) -> Option<&'static FnxSignature> {
        SIGNATURES.iter().find(|signature| signature.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FnxBindErrorKind {
    TextTooLong,
    Expected(&'static str),
    UnknownProcedure,
    MissingParameter,
    InvalidArgument(&'static str),
    TooManyArguments,
    UnknownYield,
    DuplicateYield,
    TrailingInput,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxBindError {
    pub at: usize,
    pub kind: FnxBindErrorKind,
}
impl core::fmt::Display for FnxBindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Prism CALL refused at byte {}: {:?}", self.at, self.kind)
    }
}
impl core::error::Error for FnxBindError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageRankOptions {
    alpha: f64,
    max_iter: usize,
    tol: f64,
    weighted: bool,
}
impl PageRankOptions {
    pub fn new(alpha: f64, max_iter: usize, tol: f64, weighted: bool) -> Result<Self, FnxBindError> {
        let error = |name| FnxBindError { at: 0, kind: FnxBindErrorKind::InvalidArgument(name) };
        if !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
            return Err(error("alpha must be finite and 0 <= alpha < 1"));
        }
        if max_iter == 0 {
            return Err(error("max_iter must be a positive integer"));
        }
        if !tol.is_finite() || tol <= 0.0 {
            return Err(error("tol must be finite and positive"));
        }
        Ok(Self { alpha: if alpha == 0.0 { 0.0 } else { alpha }, max_iter, tol, weighted })
    }
    pub fn alpha(self) -> f64 { self.alpha }
    pub fn max_iter(self) -> usize { self.max_iter }
    pub fn tolerance(self) -> f64 { self.tol }
    pub fn weighted(self) -> bool { self.weighted }
}
impl Default for PageRankOptions {
    fn default() -> Self {
        Self { alpha: 0.85, max_iter: 100, tol: 1.0e-6, weighted: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxOutputColumn {
    pub field: FnxOutput,
    pub name: String,
}

/// Executable only after the entire statement, parameter types and output
/// schema have been bound. Private fields prevent constructing invalid options
/// or unregistered output slots after preparation.
#[derive(Clone, Debug, PartialEq)]
pub struct FnxCallSpec {
    options: PageRankOptions,
    outputs: Vec<FnxOutputColumn>,
    digest: Digest,
}
impl FnxCallSpec {
    pub fn pagerank(options: PageRankOptions) -> Self {
        Self::compiled(options, default_outputs())
    }
    /// Grammar: CALL fnx.pagerank([alpha [, max_iter [, tol [, weighted]]]])
    /// [YIELD vertex [AS name], score [AS name]] [;]. YIELD * is also supported.
    /// Each argument may be a literal or $parameter; omitted suffix arguments
    /// use the versioned signature defaults. No text reaches graph execution.
    pub fn bind(text: &str, parameters: &FnxParameters) -> Result<Self, FnxBindError> {
        if text.len() > MAX_FNX_CALL_BYTES {
            return Err(FnxBindError { at: 0, kind: FnxBindErrorKind::TextTooLong });
        }
        let mut parser = Parser { text, at: 0 };
        parser.keyword("CALL")?;
        let namespace = parser.word()?;
        parser.expect(b'.')?;
        let procedure = parser.word()?;
        if namespace != "fnx" || procedure != "pagerank" {
            return Err(parser.error(FnxBindErrorKind::UnknownProcedure));
        }
        parser.expect(b'(')?;
        let mut arguments = [
            PAGERANK_PARAMETERS[0].default,
            PAGERANK_PARAMETERS[1].default,
            PAGERANK_PARAMETERS[2].default,
            PAGERANK_PARAMETERS[3].default,
        ];
        let mut argument_offsets = [parser.at; 4];
        let mut count = 0;
        if !parser.consume(b')') {
            loop {
                if count == arguments.len() {
                    return Err(parser.error(FnxBindErrorKind::TooManyArguments));
                }
                parser.space();
                argument_offsets[count] = parser.at;
                arguments[count] = parser.argument(parameters)?;
                count += 1;
                if parser.consume(b')') { break; }
                parser.expect(b',')?;
            }
        }
        let alpha = float_argument(arguments[0], argument_offsets[0], "alpha")?;
        let tol = float_argument(arguments[2], argument_offsets[2], "tol")?;
        let max_iter = match arguments[1] {
            FnxArgument::Integer(value) if value > 0 => usize::try_from(value).map_err(|_| FnxBindError {
                at: argument_offsets[1], kind: FnxBindErrorKind::InvalidArgument("max_iter"),
            })?,
            _ => return Err(FnxBindError { at: argument_offsets[1], kind: FnxBindErrorKind::InvalidArgument("max_iter") }),
        };
        let weighted = match arguments[3] {
            FnxArgument::Boolean(value) => value,
            _ => return Err(FnxBindError { at: argument_offsets[3], kind: FnxBindErrorKind::InvalidArgument("weighted") }),
        };
        let options = PageRankOptions::new(alpha, max_iter, tol, weighted)?;
        let outputs = if parser.peek_word("YIELD") {
            parser.keyword("YIELD")?;
            if parser.consume(b'*') {
                default_outputs()
            } else {
                let mut outputs: Vec<FnxOutputColumn> = Vec::new();
                loop {
                    let field = match parser.word()? {
                        "vertex" => FnxOutput::Vertex,
                        "score" => FnxOutput::Score,
                        _ => return Err(parser.error(FnxBindErrorKind::UnknownYield)),
                    };
                    let name = if parser.peek_word("AS") {
                        parser.keyword("AS")?;
                        parser.word()?.to_owned()
                    } else {
                        field.name().to_owned()
                    };
                    if outputs.iter().any(|column| column.field == field || column.name == name) {
                        return Err(parser.error(FnxBindErrorKind::DuplicateYield));
                    }
                    outputs.push(FnxOutputColumn { field, name });
                    if !parser.consume(b',') { break; }
                }
                outputs
            }
        } else {
            default_outputs()
        };
        parser.consume(b';');
        parser.space();
        if parser.at != text.len() {
            return Err(parser.error(FnxBindErrorKind::TrailingInput));
        }
        Ok(Self::compiled(options, outputs))
    }
    fn compiled(options: PageRankOptions, outputs: Vec<FnxOutputColumn>) -> Self {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:bound-call:v1");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        hash.update(b"fnx.pagerank");
        hash.update(&options.alpha.to_bits().to_le_bytes());
        hash.update(&(options.max_iter as u128).to_le_bytes());
        hash.update(&options.tol.to_bits().to_le_bytes());
        hash.update(&[u8::from(options.weighted), outputs.len() as u8]);
        for output in &outputs {
            hash.update(&[match output.field { FnxOutput::Vertex => 0, FnxOutput::Score => 1 }]);
            hash.update(&(output.name.len() as u128).to_le_bytes());
            hash.update(output.name.as_bytes());
        }
        Self { options, outputs, digest: hash.finalize() }
    }
    pub fn signature(&self) -> &'static FnxSignature { &SIGNATURES[0] }
    pub fn options(&self) -> PageRankOptions { self.options }
    pub fn outputs(&self) -> &[FnxOutputColumn] { &self.outputs }
    pub fn digest(&self) -> Digest { self.digest }
}
fn default_outputs() -> Vec<FnxOutputColumn> {
    vec![
        FnxOutputColumn { field: FnxOutput::Vertex, name: "vertex".to_owned() },
        FnxOutputColumn { field: FnxOutput::Score, name: "score".to_owned() },
    ]
}
fn float_argument(argument: FnxArgument, at: usize, name: &'static str) -> Result<f64, FnxBindError> {
    let value = match argument {
        FnxArgument::Float(value) => value,
        // No silent loss of precision in integer-to-float parameter coercion.
        FnxArgument::Integer(value) if (-9_007_199_254_740_992..=9_007_199_254_740_992).contains(&value) => value as f64,
        _ => return Err(FnxBindError { at, kind: FnxBindErrorKind::InvalidArgument(name) }),
    };
    if !value.is_finite() {
        return Err(FnxBindError { at, kind: FnxBindErrorKind::InvalidArgument(name) });
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}
impl<'a> Parser<'a> {
    fn error(&self, kind: FnxBindErrorKind) -> FnxBindError { FnxBindError { at: self.at, kind } }
    fn space(&mut self) {
        while self.text.as_bytes().get(self.at).is_some_and(u8::is_ascii_whitespace) { self.at += 1; }
    }
    fn consume(&mut self, byte: u8) -> bool {
        self.space();
        if self.text.as_bytes().get(self.at) == Some(&byte) { self.at += 1; true } else { false }
    }
    fn expect(&mut self, byte: u8) -> Result<(), FnxBindError> {
        if self.consume(byte) { Ok(()) } else { Err(self.error(FnxBindErrorKind::Expected("punctuation"))) }
    }
    fn word(&mut self) -> Result<&'a str, FnxBindError> {
        self.space();
        let start = self.at;
        if !self.text.as_bytes().get(start).is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_') {
            return Err(self.error(FnxBindErrorKind::Expected("identifier")));
        }
        self.at += 1;
        while self.text.as_bytes().get(self.at).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') { self.at += 1; }
        Ok(&self.text[start..self.at])
    }
    fn peek_word(&mut self, expected: &str) -> bool {
        let saved = self.at;
        let result = self.word().is_ok_and(|word| word.eq_ignore_ascii_case(expected));
        self.at = saved;
        result
    }
    fn keyword(&mut self, expected: &'static str) -> Result<(), FnxBindError> {
        if self.word()?.eq_ignore_ascii_case(expected) { Ok(()) } else { Err(self.error(FnxBindErrorKind::Expected(expected))) }
    }
    fn argument(&mut self, parameters: &FnxParameters) -> Result<FnxArgument, FnxBindError> {
        self.space();
        if self.consume(b'$') {
            let name = self.word()?;
            return parameters.get(name).copied().ok_or_else(|| self.error(FnxBindErrorKind::MissingParameter));
        }
        for (word, value) in [("true", true), ("false", false)] {
            if self.peek_word(word) {
                self.keyword(word)?;
                return Ok(FnxArgument::Boolean(value));
            }
        }
        self.space();
        let start = self.at;
        while self.text.as_bytes().get(self.at).is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E')) { self.at += 1; }
        if self.at == start { return Err(self.error(FnxBindErrorKind::Expected("number, boolean or parameter"))); }
        let number = &self.text[start..self.at];
        let invalid = || FnxBindError { at: start, kind: FnxBindErrorKind::InvalidArgument("numeric literal") };
        if number.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
            number.parse::<f64>().map(FnxArgument::Float).map_err(|_| invalid())
        } else {
            number.parse::<i64>().map(FnxArgument::Integer).map_err(|_| invalid())
        }
    }
}
