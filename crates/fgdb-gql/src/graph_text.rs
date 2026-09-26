//! Bounded graph-pattern text preparation, not a query interpreter.
//!
//! Parse once, resolve graph names once, then bind typed numeric arguments into
//! the existing GraphPatternBuilder. Execution uses the ordinary governed GLA
//! entrypoints. This profile is separate from the legacy two-hop statement and
//! artifact contract; it never falls back to that parser after a refusal.

mod aggregate;
mod boolean;
mod literal;
mod ordering;
mod parameters;
mod scoped;
pub use crate::algebra::GraphPathFunction;
use crate::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow,
    IntegerComparison, MAX_PATTERN_EDGES, MAX_PATTERN_IDENTITIES, MAX_PATTERN_NAME_BYTES,
    MAX_PATTERN_PREDICATES, MAX_PATTERN_VERTICES, PatternBuildError, PreparedGraphPattern,
    VertexPredicate,
};
use crate::{GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters};
pub use aggregate::{GraphAggregateTextSlot, PreparedGraphAggregateText};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use scoped::{BoundScope, ScopeSyntax};
use std::collections::BTreeMap;

/// Definition admission, not execution work or a promise of cheap matching.
pub const MAX_GRAPH_TEXT_BYTES: usize = 65_536;
pub const MAX_GRAPH_TEXT_TOKENS: usize = 8_192;

/// Scalar function names the shared expression compiler accepts before `(`.
/// TOUPPER, TOLOWER and SIZE are the openCypher spellings of UPPER, LOWER and
/// CHAR_LENGTH (fgdb-xakp1): a scalar is never a list, so SIZE of one is its
/// character count. A leading SIZE(...) row value keeps the list-or-text
/// Size node instead. The Boolean lowering lookahead and the compiler read
/// this one table, so a name cannot be known to one and not the other.
const SCALAR_FUNCTIONS: [&str; 13] = [
    "ABS",
    "COALESCE",
    "NULLIF",
    "UPPER",
    "TOUPPER",
    "LOWER",
    "TOLOWER",
    "TRIM",
    "SUBSTRING",
    "CHAR_LENGTH",
    "SIZE",
    "TOSTRING",
    "TOINTEGER",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphSymbolKind {
    Relation,
    Label,
    Property,
}

/// A host catalog resolves a name in the requested domain. Returning a symbol
/// from another domain is an error, never an integer cast or a guessed name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphSymbol {
    Relation(RelationId),
    Label(LabelId),
    Property(PropertyKeyId),
}

impl GraphSymbol {
    #[must_use]
    pub const fn kind(self) -> GraphSymbolKind {
        match self {
            Self::Relation(_) => GraphSymbolKind::Relation,
            Self::Label(_) => GraphSymbolKind::Label,
            Self::Property(_) => GraphSymbolKind::Property,
        }
    }
}

impl core::fmt::Debug for GraphSymbol {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}([REDACTED])", self.kind())
    }
}

pub const COMMON_GRAPH_SYMBOLS: &[&str] = &[
    "Person", "Agent", "Company", "User", "Account", "Device", "Post", "Comment", "Tag", "Group",
    "Member", "Admin", "Item", "Product", "Order", "Customer", "Source", "Copy", "Node", "Edge",
    "Entity", "Link", "L", "M", "N", "A", "B", "C", "KNOWS", "WORKS_AT", "SHIPS", "BACKS", "TO",
    "R", "S", "T", "RELATION", "REL",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReverseSymbolCatalog {
    pub labels: BTreeMap<LabelId, String>,
    pub relations: BTreeMap<RelationId, String>,
}

impl ReverseSymbolCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_label(&mut self, id: LabelId, name: impl Into<String>) {
        self.labels.insert(id, name.into());
    }

    pub fn insert_relation(&mut self, id: RelationId, name: impl Into<String>) {
        self.relations.insert(id, name.into());
    }

    pub fn from_resolver<R: GraphSymbolResolver + ?Sized>(resolver: &mut R, text: &str) -> Self {
        if let Some(catalog) = resolver.reverse_catalog() {
            return catalog;
        }
        let mut catalog = Self::default();
        let mut probe_name = |name: &str| {
            if let Some(GraphSymbol::Label(id)) =
                resolver.resolve_symbol(GraphSymbolKind::Label, name)
            {
                catalog.insert_label(id, name);
            }
            if let Some(GraphSymbol::Relation(id)) =
                resolver.resolve_symbol(GraphSymbolKind::Relation, name)
            {
                catalog.insert_relation(id, name);
            }
        };
        for common in COMMON_GRAPH_SYMBOLS {
            probe_name(common);
        }
        for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if !word.is_empty()
                && (word.as_bytes()[0].is_ascii_alphabetic() || word.as_bytes()[0] == b'_')
            {
                probe_name(word);
            }
        }
        catalog
    }
}

pub trait GraphSymbolResolver {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol>;
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        None
    }
    fn reverse_label(&self, _id: LabelId) -> Option<String> {
        None
    }
    fn reverse_relation(&self, _id: RelationId) -> Option<String> {
        None
    }
}

impl<F> GraphSymbolResolver for F
where
    F: FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
{
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self(kind, name)
    }
}

/// Diagnostics contain byte positions and structural classes, not query text,
/// identifiers, catalog IDs, or supplied argument values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPatternTextError {
    pub offset: usize,
    pub kind: GraphPatternTextErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPatternTextErrorKind {
    DefinitionTooLarge,
    TooManyTokens,
    InvalidToken,
    NameTooLong,
    Expected(&'static str),
    IntegerOutOfRange,
    ScalarLiteral,
    BooleanExpression,
    BooleanNesting {
        limit: usize,
    },
    UnsupportedBooleanScope,
    UnknownVariable,
    UnknownSymbol(GraphSymbolKind),
    WrongSymbolKind {
        expected: GraphSymbolKind,
        found: GraphSymbolKind,
    },
    ParameterDeclaration,
    UnusedParameterDeclaration,
    ConflictingParameterTypes,
    MissingParameter,
    ParameterTypeMismatch {
        expected: GqlParameterType,
        found: GqlParameterType,
    },
    UnexpectedArguments,
    Build(PatternBuildError),
    OrderBuild(crate::algebra::GraphOrderError),
    AggregateBuild(crate::GraphAggregateBuildError),
}

impl core::fmt::Display for GraphPatternTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph-pattern text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphPatternTextError {}

fn error(offset: usize, kind: GraphPatternTextErrorKind) -> GraphPatternTextError {
    GraphPatternTextError { offset, kind }
}
fn built<T>(
    offset: usize,
    result: Result<T, PatternBuildError>,
) -> Result<T, GraphPatternTextError> {
    result.map_err(|kind| error(offset, GraphPatternTextErrorKind::Build(kind)))
}

#[derive(Clone, Copy)]
struct Name<'a> {
    text: &'a str,
    at: usize,
}
#[derive(Clone, Copy)]
enum TokenKind<'a> {
    Word(&'a str),
    Digits(&'a str),
    Parameter(&'a str),
    Quoted(&'a str),
    Punct(u8),
    End,
}
#[derive(Clone, Copy)]
struct Token<'a> {
    kind: TokenKind<'a>,
    at: usize,
}

#[derive(Clone)]
struct Lexer<'a> {
    text: &'a str,
    at: usize,
    tokens: usize,
}
impl<'a> Lexer<'a> {
    fn next(&mut self) -> Result<Token<'a>, GraphPatternTextError> {
        while let Some(ch) = self.text[self.at..].chars().next() {
            if !ch.is_whitespace() {
                break;
            }
            self.at += ch.len_utf8();
        }
        let at = self.at;
        let bytes = self.text.as_bytes();
        if at == bytes.len() {
            return Ok(Token {
                kind: TokenKind::End,
                at,
            });
        }
        if self.tokens == MAX_GRAPH_TEXT_TOKENS {
            return Err(error(at, GraphPatternTextErrorKind::TooManyTokens));
        }
        self.tokens += 1;
        let ch = bytes[at];
        if ch == b'\'' {
            return self.quoted();
        }
        let parameter = ch == b'$';
        if parameter {
            self.at += 1;
        }
        let start = self.at;
        if bytes
            .get(start)
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        {
            self.at += 1;
            while bytes
                .get(self.at)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                self.at += 1;
            }
            if self.at - start > MAX_PATTERN_NAME_BYTES {
                return Err(error(at, GraphPatternTextErrorKind::NameTooLong));
            }
            let name = &self.text[start..self.at];
            return Ok(Token {
                kind: if parameter {
                    TokenKind::Parameter(name)
                } else {
                    TokenKind::Word(name)
                },
                at,
            });
        }
        if parameter {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("parameter name immediately after $"),
            ));
        }
        if ch.is_ascii_digit() {
            self.at += 1;
            while bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            return Ok(Token {
                kind: TokenKind::Digits(&self.text[at..self.at]),
                at,
            });
        }
        if b"()[]{}:,.<>=!-*+/%|~".contains(&ch) {
            self.at += 1;
            return Ok(Token {
                kind: TokenKind::Punct(ch),
                at,
            });
        }
        Err(error(at, GraphPatternTextErrorKind::InvalidToken))
    }
}

#[derive(Clone)]
pub(crate) enum Number {
    Literal(GqlParameterValue),
    Parameter(usize),
}
impl Number {
    fn value(&self, arguments: &[GqlParameterValue]) -> GqlParameterValue {
        match self {
            Self::Literal(value) => value.clone(),
            Self::Parameter(at) => arguments[*at].clone(),
        }
    }
    fn signed(&self, arguments: &[GqlParameterValue]) -> i64 {
        match self.value(arguments) {
            GqlParameterValue::Int64(value) => value,
            _ => unreachable!("private numeric syntax and schema agree"),
        }
    }
    fn unsigned(&self, arguments: &[GqlParameterValue]) -> u64 {
        match self.value(arguments) {
            GqlParameterValue::UInt64(value) => value,
            _ => unreachable!("private numeric syntax and schema agree"),
        }
    }
}

struct Edge<'a> {
    variable: Option<Name<'a>>,
    source: Name<'a>,
    relation: Name<'a>,
    direction: GlaDirection,
    destination: Name<'a>,
    walk: Option<crate::GraphWalkBounds>,
    search: crate::algebra::GraphWalkSearch,
}
enum Filter<'a> {
    PathCapture(Name<'a>),
    PathLength {
        variable: Name<'a>,
        comparison: IntegerComparison,
        value: Number,
    },
    PathNull {
        variable: Name<'a>,
        function: GraphPathFunction,
        is_null: bool,
    },
    Boolean {
        program: Vec<boolean::SyntaxItem<'a>>,
        at: usize,
    },
    Properties {
        left: Name<'a>,
        left_key: Name<'a>,
        right: Name<'a>,
        right_key: Name<'a>,
        comparison: IntegerComparison,
    },
    VertexNull {
        variable: Name<'a>,
        is_null: bool,
    },
    Property {
        variable: Name<'a>,
        key: Name<'a>,
        comparison: IntegerComparison,
        value: Number,
    },
    Scalar {
        variable: Name<'a>,
        key: Name<'a>,
        predicate: crate::algebra::ScalarPredicate,
    },
    Null {
        variable: Name<'a>,
        key: Name<'a>,
        is_null: bool,
    },
    Identity {
        left: Name<'a>,
        right: Name<'a>,
        equal: bool,
    },
}
struct Column<'a> {
    variable: Name<'a>,
    property: Option<Name<'a>>,
    path: Option<GraphPathFunction>,
    alias: Name<'a>,
}
struct Syntax<'a> {
    variables: Vec<Name<'a>>,
    path: Option<Name<'a>>,
    root_variables: usize,
    labels: Vec<(Name<'a>, Name<'a>)>,
    edges: Vec<Edge<'a>>,
    filters: Vec<Filter<'a>>,
    scopes: Vec<ScopeSyntax<'a>>,
    columns: Vec<Column<'a>>,
    visible_columns: Option<usize>,
    ordering: Vec<GraphValueOrder>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
    offset: Number,
    count: Option<Number>,
    distinct: bool,
    return_at: usize,
}

struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token<'a>,
    syntax: Syntax<'a>,
    predicates: usize,
    identities: usize,
    edge_count: usize,
    parameter_types: BTreeMap<String, GqlParameterType>,
    read_row_bindings: Vec<Name<'a>>,
    read_correlations: Vec<(Name<'a>, Name<'a>, usize)>,
    boundary_reads: Option<BoundaryReads<'a>>,
}

/// Property reads a graph-to-row WITH's scope makes through a projected MATCH
/// binding (`WITH n WHERE n.p = 3 ORDER BY n.q RETURN n.r`). Each is a hidden
/// column appended to the boundary projection after its `visible` columns.
/// Within the scope, `alias.property` resolves to one; the hidden columns are
/// projected away before any later stage or part. `width` is the boundary
/// row's width, visible plus hidden; an expression over any other row (a
/// nested scope) sees none of this. Each read is the carried alias, the
/// property name (with its first offset) and the column.
struct BoundaryReads<'a> {
    visible: usize,
    width: usize,
    reads: Vec<(&'a str, Name<'a>, usize)>,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Result<Self, GraphPatternTextError> {
        if text.len() > MAX_GRAPH_TEXT_BYTES {
            return Err(error(
                MAX_GRAPH_TEXT_BYTES,
                GraphPatternTextErrorKind::DefinitionTooLarge,
            ));
        }
        let mut lexer = Lexer {
            text,
            at: 0,
            tokens: 0,
        };
        let current = lexer.next()?;
        Ok(Self {
            lexer,
            current,
            predicates: 0,
            identities: 0,
            edge_count: 0,
            parameter_types: BTreeMap::new(),
            read_row_bindings: Vec::new(),
            read_correlations: Vec::new(),
            boundary_reads: None,
            syntax: Syntax {
                variables: Vec::new(),
                path: None,
                root_variables: 0,
                labels: Vec::new(),
                edges: Vec::new(),
                filters: Vec::new(),
                scopes: Vec::new(),
                columns: Vec::new(),
                visible_columns: None,
                ordering: Vec::new(),
                parameters: Vec::new(),
                parameter_offsets: Vec::new(),
                offset: Number::Literal(GqlParameterValue::UInt64(0)),
                count: None,
                distinct: false,
                return_at: 0,
            },
        })
    }
    fn advance(&mut self) -> Result<(), GraphPatternTextError> {
        self.current = self.lexer.next()?;
        Ok(())
    }
    fn is_word(&self, word: &str) -> bool {
        matches!(self.current.kind, TokenKind::Word(actual) if actual.eq_ignore_ascii_case(word))
    }
    fn word(&mut self, word: &'static str) -> Result<(), GraphPatternTextError> {
        if !self.is_word(word) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected(word),
            ));
        }
        self.advance()
    }
    fn take_word(&mut self, word: &'static str) -> Result<bool, GraphPatternTextError> {
        if !self.is_word(word) {
            return Ok(false);
        }
        self.advance()?;
        Ok(true)
    }
    fn is_punct(&self, ch: u8) -> bool {
        matches!(self.current.kind, TokenKind::Punct(actual) if actual == ch)
    }
    fn take(&mut self, ch: u8) -> Result<bool, GraphPatternTextError> {
        if !self.is_punct(ch) {
            return Ok(false);
        }
        self.advance()?;
        Ok(true)
    }
    fn punct(&mut self, ch: u8, expected: &'static str) -> Result<(), GraphPatternTextError> {
        if self.take(ch)? {
            Ok(())
        } else {
            Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected(expected),
            ))
        }
    }
    fn name(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let TokenKind::Word(text) = self.current.kind else {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("identifier"),
            ));
        };
        if [
            "MATCH", "WHERE", "RETURN", "ALL", "DISTINCT", "AS", "AND", "OR", "SKIP", "LIMIT",
            "OPTIONAL", "ORDER", "BY",
        ]
        .iter()
        .any(|word| text.eq_ignore_ascii_case(word))
        {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("non-keyword identifier"),
            ));
        }
        let name = Name {
            text,
            at: self.current.at,
        };
        if text.starts_with(Self::ANONYMOUS_PREFIX) {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Expected(
                    "identifier outside the reserved __fgdb_anonymous_ namespace",
                ),
            ));
        }
        self.advance()?;
        Ok(name)
    }
    fn variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self
            .syntax
            .variables
            .iter()
            .any(|var| var.text == name.text)
        {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }
    fn require_property_variable(&self, name: Name<'a>) -> Result<(), GraphPatternTextError> {
        if self
            .syntax
            .variables
            .iter()
            .any(|variable| variable.text == name.text)
            || self.syntax.edges.iter().any(|edge| {
                edge.walk.is_none()
                    && edge
                        .variable
                        .is_some_and(|variable| variable.text == name.text)
            })
        {
            return Ok(());
        }
        if self.syntax.path.is_some_and(|path| path.text == name.text)
            || self.syntax.edges.iter().any(|edge| {
                edge.variable
                    .is_some_and(|variable| variable.text == name.text)
            })
        {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Expected("single fixed-length relationship or vertex"),
            ));
        }
        Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable))
    }

    fn property_variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        self.require_property_variable(name)?;
        Ok(name)
    }
    fn capacity(
        &self,
        count: usize,
        limit: usize,
        dimension: crate::algebra::PatternLimitDimension,
    ) -> Result<(), GraphPatternTextError> {
        if count < limit {
            return Ok(());
        }
        Err(error(
            self.current.at,
            GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                dimension,
                limit,
                observed: count + 1,
            }),
        ))
    }
    /// Reserved namespace prefix for anonymous pattern-node bindings. The
    /// lexer's identifier grammar admits it and `name()` refuses it for
    /// user-written identifiers, making the names unreferenceable public
    /// syntax while remaining ordinary validated builder variables.
    const ANONYMOUS_PREFIX: &str = "__fgdb_anonymous_";

    fn anonymous_name(ordinal: usize) -> &'static str {
        const ANONYMOUS_NAMES: [&str; MAX_PATTERN_VERTICES] = [
            "__fgdb_anonymous_0",
            "__fgdb_anonymous_1",
            "__fgdb_anonymous_2",
            "__fgdb_anonymous_3",
            "__fgdb_anonymous_4",
            "__fgdb_anonymous_5",
            "__fgdb_anonymous_6",
            "__fgdb_anonymous_7",
            "__fgdb_anonymous_8",
            "__fgdb_anonymous_9",
            "__fgdb_anonymous_10",
            "__fgdb_anonymous_11",
            "__fgdb_anonymous_12",
            "__fgdb_anonymous_13",
            "__fgdb_anonymous_14",
            "__fgdb_anonymous_15",
            "__fgdb_anonymous_16",
            "__fgdb_anonymous_17",
            "__fgdb_anonymous_18",
            "__fgdb_anonymous_19",
            "__fgdb_anonymous_20",
            "__fgdb_anonymous_21",
            "__fgdb_anonymous_22",
            "__fgdb_anonymous_23",
            "__fgdb_anonymous_24",
            "__fgdb_anonymous_25",
            "__fgdb_anonymous_26",
            "__fgdb_anonymous_27",
            "__fgdb_anonymous_28",
            "__fgdb_anonymous_29",
            "__fgdb_anonymous_30",
            "__fgdb_anonymous_31",
            "__fgdb_anonymous_32",
            "__fgdb_anonymous_33",
            "__fgdb_anonymous_34",
            "__fgdb_anonymous_35",
            "__fgdb_anonymous_36",
            "__fgdb_anonymous_37",
            "__fgdb_anonymous_38",
            "__fgdb_anonymous_39",
            "__fgdb_anonymous_40",
            "__fgdb_anonymous_41",
            "__fgdb_anonymous_42",
            "__fgdb_anonymous_43",
            "__fgdb_anonymous_44",
            "__fgdb_anonymous_45",
            "__fgdb_anonymous_46",
            "__fgdb_anonymous_47",
            "__fgdb_anonymous_48",
            "__fgdb_anonymous_49",
            "__fgdb_anonymous_50",
            "__fgdb_anonymous_51",
            "__fgdb_anonymous_52",
            "__fgdb_anonymous_53",
            "__fgdb_anonymous_54",
            "__fgdb_anonymous_55",
            "__fgdb_anonymous_56",
            "__fgdb_anonymous_57",
            "__fgdb_anonymous_58",
            "__fgdb_anonymous_59",
            "__fgdb_anonymous_60",
            "__fgdb_anonymous_61",
            "__fgdb_anonymous_62",
            "__fgdb_anonymous_63",
            "__fgdb_anonymous_64",
        ];
        ANONYMOUS_NAMES[ordinal]
    }
    fn node(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.punct(b'(', "(")?;
        let name = if matches!(self.current.kind, TokenKind::Punct(b')' | b':')) {
            self.capacity(
                self.syntax.variables.len(),
                MAX_PATTERN_VERTICES,
                PatternLimitDimension::Vertices,
            )?;
            // Anonymous node: synthesize a private binding under the reserved
            // `__fgdb_anonymous_` prefix, skipping every user-written name and
            // every prior synthesis. The names are still validated builder
            // variables, so scoped bodies, boolean templates and property
            // filters work unchanged; they are invisible because `name()`
            // refuses the prefix and the RETURN * expansion skips it.
            let mut ordinal = 0usize;
            while self
                .syntax
                .variables
                .iter()
                .any(|var| var.text == Self::anonymous_name(ordinal))
            {
                ordinal += 1;
            }
            self.syntax.variables.push(Name {
                text: Self::anonymous_name(ordinal),
                at: self.current.at,
            });
            *self.syntax.variables.last().expect("just pushed")
        } else {
            let name = self.name()?;
            if self.syntax.path.is_some_and(|path| path.text == name.text) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("vertex distinct from path binding"),
                ));
            }
            if !self
                .syntax
                .variables
                .iter()
                .any(|var| var.text == name.text)
            {
                self.capacity(
                    self.syntax.variables.len(),
                    MAX_PATTERN_VERTICES,
                    PatternLimitDimension::Vertices,
                )?;
                self.syntax.variables.push(name);
            }
            name
        };
        while self.take(b':')? {
            self.capacity(
                self.predicates,
                MAX_PATTERN_PREDICATES,
                PatternLimitDimension::Predicates,
            )?;
            let label = self.name()?;
            self.syntax.labels.push((name, label));
            self.predicates += 1;
        }
        self.node_property_map(name)?;
        self.punct(b')', ")")?;
        Ok(name)
    }

    /// Inline maps are conjunctions in this positive pattern, not assignments
    /// or a separate evaluator. In particular, OPTIONAL owns these predicates
    /// before null extension, and equality with NULL is not an IS NULL test.
    fn node_property_map(&mut self, variable: Name<'a>) -> Result<(), GraphPatternTextError> {
        if !self.take(b'{')? || self.take(b'}')? {
            return Ok(());
        }
        let mut keys = Vec::new();
        loop {
            self.capacity(
                self.predicates,
                MAX_PATTERN_PREDICATES,
                crate::algebra::PatternLimitDimension::Predicates,
            )?;
            let key = self.name()?;
            if keys.contains(&key.text) {
                return Err(error(
                    key.at,
                    GraphPatternTextErrorKind::Expected("distinct property name in node map"),
                ));
            }
            self.punct(b':', ":")?;
            let row = match self.current.kind {
                TokenKind::Word(binding) => self
                    .read_row_bindings
                    .iter()
                    .position(|candidate| candidate.text == binding),
                _ => None,
            };
            if let Some(row) = row {
                self.advance()?;
                self.read_correlations.push((variable, key, row));
            } else {
                let at = self.current.at;
                let predicate = self.property_operand(variable, key, IntegerComparison::Equal)?;
                if matches!(predicate, Filter::Properties { .. }) {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected(
                            "literal or typed parameter in node map",
                        ),
                    ));
                }
                self.syntax.filters.push(predicate);
            }
            keys.push(key.text);
            self.predicates += 1;
            if !self.take(b',')? {
                break;
            }
        }
        self.punct(b'}', "}")
    }

    fn number(&mut self, expected: GqlParameterType) -> Result<Number, GraphPatternTextError> {
        let at = self.current.at;
        if let TokenKind::Parameter(name) = self.current.kind {
            self.check_declared_parameter(name, expected, at)?;
            let index = if let Some(index) = self
                .syntax
                .parameters
                .iter()
                .position(|spec| spec.name == name)
            {
                let spec = &mut self.syntax.parameters[index];
                if spec.parameter_type != expected {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::ConflictingParameterTypes,
                    ));
                }
                spec.occurrences += 1;
                index
            } else {
                let index = self.syntax.parameters.len();
                self.syntax.parameters.push(GqlParameterSpec {
                    name: name.to_owned(),
                    parameter_type: expected,
                    requires_positive: false,
                    occurrences: 1,
                });
                self.syntax.parameter_offsets.push(at);
                index
            };
            self.advance()?;
            return Ok(Number::Parameter(index));
        }
        let negative = expected == GqlParameterType::Int64 && self.take(b'-')?;
        let TokenKind::Digits(digits) = self.current.kind else {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("typed integer or numeric parameter"),
            ));
        };
        let magnitude = digits
            .parse::<u64>()
            .map_err(|_| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?;
        let value = match expected {
            GqlParameterType::Scalar(_) => {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("declared scalar parameter"),
                ));
            }
            GqlParameterType::UInt64 => GqlParameterValue::UInt64(magnitude),
            GqlParameterType::Int64 => {
                let signed = if negative {
                    -i128::from(magnitude)
                } else {
                    i128::from(magnitude)
                };
                GqlParameterValue::Int64(
                    i64::try_from(signed)
                        .map_err(|_| error(at, GraphPatternTextErrorKind::IntegerOutOfRange))?,
                )
            }
            GqlParameterType::List => {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("list parameters are not scalar operands"),
                ));
            }
        };
        self.advance()?;
        Ok(Number::Literal(value))
    }
    fn comparison(&mut self) -> Result<IntegerComparison, GraphPatternTextError> {
        use IntegerComparison::*;
        if self.take(b'=')? {
            return Ok(Equal);
        }
        if self.take(b'!')? {
            self.punct(b'=', "!=")?;
            return Ok(NotEqual);
        }
        if self.take(b'<')? {
            return Ok(if self.take(b'=')? {
                LessOrEqual
            } else if self.take(b'>')? {
                NotEqual
            } else {
                Less
            });
        }
        if self.take(b'>')? {
            return Ok(if self.take(b'=')? {
                GreaterOrEqual
            } else {
                Greater
            });
        }
        Err(error(
            self.current.at,
            GraphPatternTextErrorKind::Expected("comparison operator"),
        ))
    }
    /// Common scoped MATCH/WHERE grammar for ordinary and aggregate RETURN.
    fn parse_head(&mut self) -> Result<(), GraphPatternTextError> {
        self.parse_scoped_head()
    }
    fn parse(mut self) -> Result<Syntax<'a>, GraphPatternTextError> {
        self.parse_head()?;
        self.parse_columns()?;
        self.parse_row_ordering()?;
        self.parse_pagination()?;
        self.end()?;
        Ok(self.syntax)
    }

    fn parse_columns(&mut self) -> Result<(), GraphPatternTextError> {
        use crate::algebra::PatternLimitDimension;
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct {
            self.take_word("ALL")?;
        }
        if self.take(b'*')? {
            self.syntax.columns.extend(
                self.syntax
                    .variables
                    .iter()
                    .copied()
                    .filter(|name| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
                    .map(|name| Column {
                        variable: name,
                        property: None,
                        path: None,
                        alias: name,
                    }),
            );
            if let Some(name) = self.syntax.path {
                self.capacity(
                    self.syntax.columns.len(),
                    MAX_PATTERN_VERTICES,
                    PatternLimitDimension::Columns,
                )?;
                self.syntax.columns.push(Column {
                    variable: name,
                    property: None,
                    path: Some(GraphPathFunction::Value),
                    alias: name,
                });
            }
            for edge in &self.syntax.edges {
                if let Some(name) = edge.variable {
                    self.capacity(
                        self.syntax.columns.len(),
                        MAX_PATTERN_VERTICES,
                        PatternLimitDimension::Columns,
                    )?;
                    self.syntax.columns.push(Column {
                        variable: name,
                        property: None,
                        path: Some(GraphPathFunction::Edge),
                        alias: name,
                    });
                }
            }
        } else {
            loop {
                self.capacity(
                    self.syntax.columns.len(),
                    MAX_PATTERN_VERTICES,
                    PatternLimitDimension::Columns,
                )?;
                let expression = self.name()?;
                let start = expression.text.eq_ignore_ascii_case("startNode");
                let (variable, property, path) = if self.is_punct(b'(')
                    && (start || expression.text.eq_ignore_ascii_case("endNode"))
                {
                    // openCypher startNode(r)/endNode(r): the bound endpoint.
                    self.advance()?;
                    let edge = self.edge_variable()?;
                    self.punct(b')', ")")?;
                    (self.edge_endpoint(edge, start)?, None, None)
                } else if self.is_punct(b'(') {
                    let function = Self::path_function(expression)?;
                    self.advance()?;
                    let variable = match function {
                        GraphPathFunction::Labels => self.vertex_variable()?,
                        GraphPathFunction::Type => self.edge_variable()?,
                        _ => self.path_variable()?,
                    };
                    self.punct(b')', ")")?;
                    (variable, None, Some(function))
                } else if self
                    .syntax
                    .path
                    .is_some_and(|path| path.text == expression.text)
                {
                    (expression, None, Some(GraphPathFunction::Value))
                } else if self.syntax.edges.iter().any(|edge| {
                    edge.variable
                        .is_some_and(|name| name.text == expression.text)
                }) {
                    let property = if self.take(b'.')? {
                        self.require_property_variable(expression)?;
                        Some(self.name()?)
                    } else {
                        None
                    };
                    (expression, property, Some(GraphPathFunction::Edge))
                } else {
                    if !self
                        .syntax
                        .variables
                        .iter()
                        .any(|name| name.text == expression.text)
                    {
                        return Err(error(
                            expression.at,
                            GraphPatternTextErrorKind::UnknownVariable,
                        ));
                    }
                    let property = if self.take(b'.')? {
                        Some(self.name()?)
                    } else {
                        None
                    };
                    (expression, property, None)
                };
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else {
                    property.unwrap_or(expression)
                };
                if self
                    .syntax
                    .columns
                    .iter()
                    .any(|column| column.alias.text == alias.text)
                {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    ));
                }
                self.syntax.columns.push(Column {
                    variable,
                    property,
                    path,
                    alias,
                });
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok(())
    }

    fn path_function(name: Name<'a>) -> Result<GraphPathFunction, GraphPatternTextError> {
        // `length(p)` is the openCypher spelling of PATH_LENGTH (fgdb-j687q).
        if name.text.eq_ignore_ascii_case("path_length") || name.text.eq_ignore_ascii_case("length")
        {
            Ok(GraphPathFunction::Length)
        } else if name.text.eq_ignore_ascii_case("nodes") {
            Ok(GraphPathFunction::Nodes)
        } else if name.text.eq_ignore_ascii_case("edges") {
            Ok(GraphPathFunction::Edges)
        } else if name.text.eq_ignore_ascii_case("labels") {
            Ok(GraphPathFunction::Labels)
        } else if name.text.eq_ignore_ascii_case("type") {
            Ok(GraphPathFunction::Type)
        } else {
            Err(error(
                name.at,
                GraphPatternTextErrorKind::Expected("path function"),
            ))
        }
    }

    fn vertex_variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self.syntax.variables.iter().any(|v| v.text == name.text) {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }

    fn edge_variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self
            .syntax
            .edges
            .iter()
            .any(|e| e.variable.is_some_and(|v| v.text == name.text))
        {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }

    /// The vertex a directed single-hop pattern edge starts (`start`) or ends
    /// at. `(a)-[r]->(b)` starts at `a`; `(a)<-[r]-(b)` starts at `b`. An
    /// undirected or quantified edge has no statically known endpoint.
    fn edge_endpoint(
        &self,
        edge: Name<'a>,
        start: bool,
    ) -> Result<Name<'a>, GraphPatternTextError> {
        let found = self
            .syntax
            .edges
            .iter()
            .find(|e| e.variable.is_some_and(|v| v.text == edge.text))
            .ok_or_else(|| error(edge.at, GraphPatternTextErrorKind::UnknownVariable))?;
        match (found.direction, found.walk) {
            (GlaDirection::Forward, None) => Ok(if start {
                found.source
            } else {
                found.destination
            }),
            (GlaDirection::Reverse, None) => Ok(if start {
                found.destination
            } else {
                found.source
            }),
            _ => Err(error(
                edge.at,
                GraphPatternTextErrorKind::Expected(
                    "a directed single-hop edge for startNode/endNode",
                ),
            )),
        }
    }

    fn path_variable(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self.syntax.path.is_some_and(|path| path.text == name.text) {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }

    pub(in crate::graph_text) fn any_variable(
        &mut self,
    ) -> Result<Name<'a>, GraphPatternTextError> {
        let name = self.name()?;
        if !self.syntax.variables.iter().any(|v| v.text == name.text)
            && !self.syntax.path.is_some_and(|path| path.text == name.text)
            && !self
                .syntax
                .edges
                .iter()
                .any(|e| e.variable.is_some_and(|v| v.text == name.text))
        {
            return Err(error(name.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        Ok(name)
    }
    fn parse_pagination(&mut self) -> Result<(), GraphPatternTextError> {
        if self.take_word("SKIP")? {
            self.syntax.offset = self.number(GqlParameterType::UInt64)?;
        }
        if self.take_word("LIMIT")? {
            self.syntax.count = Some(self.number(GqlParameterType::UInt64)?);
        }
        Ok(())
    }
    fn end(&self) -> Result<(), GraphPatternTextError> {
        if !matches!(self.current.kind, TokenKind::End) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("end of statement"),
            ));
        }
        self.check_parameter_declarations()
    }
}

#[derive(Clone)]
enum BoundFilter {
    PathLength {
        variable: String,
        comparison: IntegerComparison,
        value: Number,
    },
    Property {
        variable: String,
        key: PropertyKeyId,
        comparison: IntegerComparison,
        value: Number,
    },
    Boolean(boolean::BoundBooleanTemplate),
}
#[derive(Clone)]
struct BoundColumn {
    alias: String,
    variable: String,
    key: Option<PropertyKeyId>,
    path: Option<GraphPathFunction>,
}
impl BoundColumn {
    fn declaration(&self) -> GraphColumn<'_> {
        if self.path == Some(GraphPathFunction::Edge)
            && let Some(key) = self.key
        {
            return GraphColumn::edge_property(&self.alias, &self.variable, key);
        }
        if let Some(function) = self.path {
            return GraphColumn::path(&self.alias, &self.variable, function);
        }
        match self.key {
            Some(key) => GraphColumn::property(&self.alias, &self.variable, key),
            None => GraphColumn::vertex(&self.alias, &self.variable),
        }
    }
}

/// Prepared syntax and schema, independent of parameter values and database
/// generations. Binding never lexes text, calls the catalog, or reads storage.
/// The host pins catalog/authorization validity; this is not a session lease.
#[derive(Clone)]
pub struct PreparedGraphText {
    statement: String,
    builder: GraphPatternBuilder,
    filters: Vec<BoundFilter>,
    scopes: Vec<BoundScope>,
    columns: Vec<BoundColumn>,
    visible_columns: Option<usize>,
    ordering: Vec<GraphValueOrder>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
    offset: Number,
    count: Option<Number>,
    distinct: bool,
    return_at: usize,
    pub reverse_catalog: Option<std::sync::Arc<ReverseSymbolCatalog>>,
}

impl core::fmt::Debug for PreparedGraphText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphText")
            .field("columns", &self.columns.len())
            .field("parameters", &self.parameters.len())
            .field("scopes", &self.scopes.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphText {
    /// Prepare the bounded graph-pattern text profile. Keywords are ASCII
    /// case-insensitive; names are case-sensitive. RETURN defaults to ALL.
    /// The mandatory root may have AND-conjoined EXISTS/NOT EXISTS { MATCH }
    /// predicates, followed by correlated OPTIONAL MATCH clauses. A WHERE
    /// after OPTIONAL belongs to that child, before null extension. Scoped
    /// children require a visible correlation; other components may be
    /// independent. Nested scopes and wholly uncorrelated children refuse.
    /// Comma-separated components may join through property or identity
    /// comparisons without an edge between them. Independent scans use one
    /// complete admitted vertex domain, preserve isolates and bag occurrences,
    /// and conservatively retain whole-vertex transaction scan observations.
    /// This is governed nested-loop enumeration, not a hash/FreeJoin optimizer.
    /// Property WHERE operands also accept single-quoted UCS_BASIC strings,
    /// TRUE/FALSE/NULL, and IS [NOT] NULL. Only doubled quotes escape a quote;
    /// quoted keywords and parameter-looking text remain literal payloads.
    /// Nodes also accept `{key: literal, other: $parameter}` equality maps.
    /// Empty maps are no-ops; duplicate keys and expression operands refuse.
    /// Each entry shares the predicate budget and stays in its MATCH scope.
    /// WHERE also supports parentheses and NOT > AND > OR precedence, with
    /// three-valued comparisons over properties or vertex identities. The new
    /// Boolean program evaluates leaves eagerly; only final TRUE survives.
    /// Existing flat conjunctions retain their original lowering. Boolean
    /// subquery operands/mixed root EXISTS expressions remain unsupported;
    /// positive WHERE expressions inside EXISTS and OPTIONAL are supported.
    /// ORDER BY selects returned expressions or aliases, with ASC/DESC and
    /// independent NULLS FIRST/LAST (default LAST). Whole rows break ties.
    /// Ordering precedes SKIP/LIMIT; hidden sort expressions are refused.
    ///
    /// Plain MATCH and explicit MATCH WALK accept bounded `[:R*min..max]`,
    /// `[:R*k]` and `[:R*..max]` atoms. The omitted minimum is one; zero is
    /// explicit. Bounds are integer literals with 0 <= min <= max <= 1024.
    /// Both use WALK semantics: repeated edges and vertices contribute distinct
    /// occurrences, including duplicate endpoint rows. MATCH TRAIL forbids edge
    /// reuse instead; explicit path selectors retain their own semantics.
    /// A root `MATCH p = ...` captures its ordered path, including real edge IDs.
    /// MATCH ALL SHORTEST WALK prefix selects all tied minimum-hop occurrences
    /// within the interval, separately for each endpoint pair. This native
    /// profile requires exactly one quantified atom in its positive pattern;
    /// compound shortest-path patterns refuse rather than selecting each atom
    /// independently and pretending to minimize total path length.
    /// Endpoint predicates do not filter transit vertices. OPTIONAL and
    /// existential MATCH use the same finite bounds. The same head grammar feeds
    /// aggregate queries and query-selected writes. Bare/open-ended quantifiers,
    /// hop parameters and weighted search remain unsupported. Captured paths
    /// support RETURN p, path_length(p), nodes(p), edges(p), and conjunctive
    /// length comparisons or IS [NOT] NULL predicates. Scoped captures refuse.
    ///
    /// Syntax is completely validated before calling `resolve`. Each unique
    /// (kind,name) is resolved once across ALL scopes. Unknown/wrong-kind names
    /// refuse. Numeric arguments use one schema and retain original offsets.
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        Self::from_syntax(statement, Parser::new(statement)?.parse()?, resolve)
    }

    pub fn prepare_with_resolver(
        statement: &str,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphPatternTextError> {
        Self::from_syntax(statement, Parser::new(statement)?.parse()?, resolve)
    }

    fn from_syntax<'a>(
        statement: &str,
        syntax: Syntax<'a>,
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphPatternTextError> {
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'a>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) {
                return Ok(*value);
            }
            let value = resolve
                .resolve_symbol(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::WrongSymbolKind {
                        expected: kind,
                        found: value.kind(),
                    },
                ));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let (builder, filters) = scoped::resolve_pattern(
            &syntax.variables[..syntax.root_variables],
            &syntax.labels,
            &syntax.edges,
            syntax.filters,
            &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes {
            scopes.push(scope.resolve(&mut symbol)?);
        }
        let mut columns = Vec::new();
        let mut hidden_alias = 0usize;
        for (index, column) in syntax.columns.iter().enumerate() {
            let key = if let Some(name) = column.property {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                    unreachable!("symbol domain checked above")
                };
                Some(key)
            } else {
                None
            };
            columns.push(BoundColumn {
                alias: if syntax.visible_columns.is_some_and(|width| index >= width) {
                    loop {
                        let alias = format!("__fgdb_sort_{hidden_alias}");
                        hidden_alias += 1;
                        if !syntax
                            .columns
                            .iter()
                            .any(|column| column.alias.text == alias)
                        {
                            break alias;
                        }
                    }
                } else {
                    column.alias.text.to_owned()
                },
                variable: column.variable.text.to_owned(),
                key,
                path: if key.is_some()
                    && syntax.edges.iter().any(|edge| {
                        edge.variable
                            .is_some_and(|name| name.text == column.variable.text)
                    }) {
                    Some(GraphPathFunction::Edge)
                } else {
                    column.path
                },
            });
        }
        // Compile the actual scope topology for structural validation. Do not
        // flatten optional edges into mandatory MATCH or expose EXISTS locals.
        // Numeric operands are not inspected by this preparation-only check.
        let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
        let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
        built(
            syntax.return_at,
            builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
        )?;
        let needs_reverse = columns.iter().any(|c| {
            matches!(
                c.path,
                Some(GraphPathFunction::Labels | GraphPathFunction::Type)
            )
        });
        let reverse_catalog = if needs_reverse {
            Some(std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(
                &mut resolve,
                statement,
            )))
        } else {
            None
        };
        Ok(Self {
            statement: statement.to_owned(),
            builder,
            filters,
            scopes,
            columns,
            visible_columns: syntax.visible_columns,
            ordering: syntax.ordering,
            parameters: syntax.parameters,
            parameter_offsets: syntax.parameter_offsets,
            offset: syntax.offset,
            count: syntax.count,
            distinct: syntax.distinct,
            return_at: syntax.return_at,
            reverse_catalog,
        })
    }

    /// Explicit plaintext definition export; Debug never emits it.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    /// Versioned, value-independent template transcript: resolved builder
    /// template, filters, scopes, projections, ordering, paging shape and
    /// parameter identity. Statement text and parameter values never enter.
    #[must_use]
    pub fn template_bytes(&self) -> Vec<u8> {
        use crate::graph_text::boolean::append_number;
        fn append_name(bytes: &mut Vec<u8>, value: &str) {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.as_bytes());
        }
        fn comparison_tag(comparison: IntegerComparison) -> u8 {
            match comparison {
                IntegerComparison::Equal => 0,
                IntegerComparison::NotEqual => 1,
                IntegerComparison::Less => 2,
                IntegerComparison::LessOrEqual => 3,
                IntegerComparison::Greater => 4,
                IntegerComparison::GreaterOrEqual => 5,
            }
        }
        fn append_filter(bytes: &mut Vec<u8>, filter: &BoundFilter) {
            match filter {
                BoundFilter::PathLength {
                    variable,
                    comparison,
                    value,
                } => {
                    bytes.push(0);
                    append_name(bytes, variable);
                    bytes.push(comparison_tag(*comparison));
                    append_number(bytes, value);
                }
                BoundFilter::Property {
                    variable,
                    key,
                    comparison,
                    value,
                } => {
                    bytes.push(1);
                    append_name(bytes, variable);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    bytes.push(comparison_tag(*comparison));
                    append_number(bytes, value);
                }
                BoundFilter::Boolean(template) => {
                    bytes.push(2);
                    let encoded = template.template_bytes();
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&encoded);
                }
            }
        }
        fn append_scope(bytes: &mut Vec<u8>, scope: &BoundScope) {
            bytes.push(scope.kind_tag());
            bytes.extend_from_slice(&scope.builder().canonical_template_bytes());
            bytes.extend_from_slice(&(scope.filters().len() as u64).to_be_bytes());
            for filter in scope.filters() {
                append_filter(bytes, filter);
            }
        }
        let mut bytes = b"fgdb:gql:pattern-text-template:v1\0".to_vec();
        bytes.extend_from_slice(&self.builder.canonical_template_bytes());
        bytes.extend_from_slice(&(self.scopes.len() as u64).to_be_bytes());
        for scope in &self.scopes {
            append_scope(&mut bytes, scope);
        }
        bytes.extend_from_slice(&(self.filters.len() as u64).to_be_bytes());
        for filter in &self.filters {
            append_filter(&mut bytes, filter);
        }
        bytes.extend_from_slice(&(self.columns.len() as u64).to_be_bytes());
        for column in &self.columns {
            append_name(&mut bytes, &column.alias);
            append_name(&mut bytes, &column.variable);
            match column.key {
                None => bytes.push(0),
                Some(key) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
            }
            match column.path {
                None => bytes.push(0),
                Some(function) => {
                    bytes.push(1);
                    bytes.push(function as u8);
                }
            }
        }
        bytes.extend_from_slice(&(self.ordering.len() as u64).to_be_bytes());
        for order in &self.ordering {
            bytes.extend_from_slice(&(order.column as u64).to_be_bytes());
            bytes.push(u8::from(order.descending));
            bytes.push(u8::from(order.nulls_first));
        }
        append_number(&mut bytes, &self.offset);
        match &self.count {
            None => bytes.push(0),
            Some(count) => {
                bytes.push(1);
                append_number(&mut bytes, count);
            }
        }
        bytes.push(u8::from(self.distinct));
        if let Some(width) = self.visible_columns {
            bytes.extend_from_slice(b"visible-prefix\0");
            bytes.extend_from_slice(&(width as u64).to_be_bytes());
        }
        bytes
    }

    /// Logical template operators in declaration order, before lowering.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        let mut operators = self.builder.template_operators();
        operators.extend(std::iter::repeat_n("MatchScope", self.scopes.len()));
        for filter in &self.filters {
            operators.push(match filter {
                BoundFilter::PathLength { .. } => "FilterPathLength",
                BoundFilter::Property { .. } => "FilterProperty",
                BoundFilter::Boolean(_) => "FilterBoolean",
            });
        }
        operators.push("Project");
        if self.distinct {
            operators.push("Distinct");
        }
        if !self.ordering.is_empty() {
            operators.push("OrderBy");
        }
        operators.push("Limit");
        operators
    }

    /// Validate the exact argument set before building any concrete predicate.
    /// Returned plans are owned and immutable and use all existing governed
    /// snapshot/transaction entrypoints, including their original refusals.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphPattern<GraphValueRow>, GraphPatternTextError> {
        let values = self.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        let mut values = Vec::new();
        for (index, spec) in self.parameters.iter().enumerate() {
            let at = self.parameter_offsets[index];
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                ));
            }
            values.push(value);
        }
        if arguments.len() != values.len() {
            return Err(error(
                self.statement.len(),
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(values)
    }

    fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphPattern<GraphValueRow>, GraphPatternTextError> {
        let builder = scoped::bind_builder(&self.builder, &self.filters, values, self.return_at)?;
        let scopes = self
            .scopes
            .iter()
            .map(|scope| scope.bind_values(values, self.return_at))
            .collect::<Result<Vec<_>, _>>()?;
        let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
        let columns: Vec<_> = self.columns.iter().map(BoundColumn::declaration).collect();
        let pattern = built(
            self.return_at,
            builder.prepare_values_with_clauses(
                &clauses,
                &columns,
                self.offset.unsigned(values),
                self.count.as_ref().map(|count| count.unsigned(values)),
            ),
        )?;
        let pattern = if self.distinct {
            pattern
        } else {
            pattern.with_duplicates()
        };
        let mut pattern = if self.ordering.is_empty() {
            pattern
        } else {
            pattern.with_order_by(&self.ordering).map_err(|kind| {
                error(self.return_at, GraphPatternTextErrorKind::OrderBuild(kind))
            })?
        };
        if let Some(catalog) = &self.reverse_catalog {
            pattern.logical.reverse_catalog = Some(std::sync::Arc::clone(catalog));
        }
        Ok(match self.visible_columns {
            Some(width) => pattern.with_visible_columns(width),
            None => pattern,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use fgdb_types::{CanonicalScalar, VId};
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(3))),
            (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(4))),
            _ => None,
        }
    }
    fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
    }
    fn vertex_rows(rows: &[GraphValueRow]) -> Vec<Vec<VId>> {
        rows.iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|value| value.as_vertex().unwrap())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn element_name_functions_prepare_and_lower_positively() {
        for (function, expected_col) in [("labels(p)", "labels"), ("type(r)", "type")] {
            let text = format!("MATCH (p:L)-[r:R]->(q) RETURN {function}");
            let template = PreparedGraphText::prepare(&text, symbols).expect("positive prepare");
            let pattern = template
                .bind_parameters(&GqlParameters::new())
                .expect("bind");
            assert_eq!(pattern.columns(), &[expected_col]);
            assert!(pattern.plan().reverse_catalog.is_some());
        }
    }

    #[test]
    fn text_lowering_matches_manual_compiler_and_resolves_each_name_once() {
        let text = "MATCH (a:L)-[:R]->(b), (c)<-[:S]-(d), (b)-[:R]->(c), (d)-[:S]->(a) \
            WHERE c.n >= $min AND a <> d RETURN DISTINCT a AS owner,c.n AS score,d AS carrier SKIP $off LIMIT $count";
        let mut calls = BTreeMap::new();
        let template = PreparedGraphText::prepare(text, |kind, name| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls.values().all(|count| *count == 1));
        let arguments = GqlParameters::new()
            .with_int64("min", 7)
            .unwrap()
            .with_uint64("off", 1)
            .unwrap()
            .with_uint64("count", 3)
            .unwrap();
        let actual = template.bind_parameters(&arguments).unwrap();
        let mut expected = GraphPatternBuilder::new();
        for name in ["a", "b", "c", "d"] {
            expected.vertex(name).unwrap();
        }
        expected
            .filter("a", VertexPredicate::HasLabel(LabelId(3)))
            .unwrap();
        for (left, relation, direction, right) in [
            ("a", 1, GlaDirection::Forward, "b"),
            ("c", 2, GlaDirection::Reverse, "d"),
            ("b", 1, GlaDirection::Forward, "c"),
            ("d", 2, GlaDirection::Forward, "a"),
        ] {
            expected
                .edge(left, RelationId(relation), direction, right)
                .unwrap();
        }
        expected.identity("a", "d", false).unwrap();
        expected
            .filter(
                "c",
                VertexPredicate::IntegerProperty {
                    key: PropertyKeyId(4),
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 7,
                },
            )
            .unwrap();
        let expected = expected
            .prepare_values(
                &[
                    GraphColumn::vertex("owner", "a"),
                    GraphColumn::property("score", "c", PropertyKeyId(4)),
                    GraphColumn::vertex("carrier", "d"),
                ],
                1,
                Some(3),
            )
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(template.statement(), text);
        assert_eq!(actual.columns(), &["owner", "score", "carrier"]);
    }

    #[test]
    fn text_bags_and_cycles_match_independent_complete_assignment_enumeration() {
        type Atom = (usize, u64, u8, usize);
        type Case<'a> = (&'a str, &'a [Atom], [usize; 2], bool, bool);
        let cases: [Case<'_>; 3] = [
            (
                "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a,c",
                &[(0, 1, 0, 1), (1, 2, 0, 2)],
                [0, 2],
                false,
                false,
            ),
            (
                "MATCH (a)<-[:R]-(b)-[:S]-(c) WHERE a <> c RETURN ALL c,a",
                &[(0, 1, 1, 1), (1, 2, 2, 2)],
                [2, 0],
                false,
                true,
            ),
            (
                "MATCH (a)-[:R]-(b),(c)-[:S]->(b),(c)-[:R]->(a) RETURN DISTINCT c,a",
                &[(0, 1, 2, 1), (2, 2, 0, 1), (2, 1, 0, 0)],
                [2, 0],
                true,
                false,
            ),
        ];
        let universe: Vec<_> = (1..=2)
            .flat_map(|r| {
                (1..=2).flat_map(move |s| (1..=2).map(move |d| (VId(s), RelationId(r), VId(d))))
            })
            .collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge)
                .collect();
            if let Some(first) = edges.first().copied() {
                edges.push(first);
            }
            for (text, atoms, selected, distinct, unequal) in cases {
                let mut expected = Vec::new();
                for bits in 0..8 {
                    let assignment = [
                        VId(1 + (bits & 1)),
                        VId(1 + ((bits >> 1) & 1)),
                        VId(1 + ((bits >> 2) & 1)),
                    ];
                    if unequal && assignment[0] == assignment[2] {
                        continue;
                    }
                    let multiplicity = atoms
                        .iter()
                        .map(|&(left, relation, direction, right)| {
                            edges
                                .iter()
                                .filter(|&&(s, r, d)| {
                                    if r != RelationId(relation) {
                                        return false;
                                    }
                                    let (a, b) = (assignment[left], assignment[right]);
                                    match direction {
                                        0 => s == a && d == b,
                                        1 => d == a && s == b,
                                        _ => (s == a && d == b) || (s == b && d == a),
                                    }
                                })
                                .count()
                        })
                        .product::<usize>();
                    for _ in 0..multiplicity {
                        expected.push(selected.map(|at| assignment[at]).to_vec());
                    }
                }
                expected.sort();
                if distinct {
                    expected.dedup();
                }
                let pattern = query(text);
                let actual = pattern
                    .plan()
                    .execute_governed_with_properties(
                        edges.len() as u64,
                        [],
                        edges.iter().copied(),
                        |_, _| Ok::<_, ()>(true),
                        |_, _| Ok(None),
                        policy(),
                        || Ok::<_, ()>(()),
                    )
                    .unwrap();
                assert_eq!(vertex_rows(&actual.value), expected, "mask={mask}, {text}");
            }
        }
    }

    #[test]
    fn anonymous_nodes_match_named_middle_reference() {
        let edges: Vec<(VId, RelationId, VId)> = vec![
            (VId(1), RelationId(1), VId(2)),
            // A duplicate occurrence: anonymous matching must preserve
            // multiplicities exactly like the named spelling.
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(3), RelationId(1), VId(1)),
            (VId(4), RelationId(1), VId(2)),
        ];
        let anonymous = query("MATCH (a)-[:R]->()-[:R]->(c) RETURN a,c");
        let named = query("MATCH (a)-[:R]->(m)-[:R]->(c) RETURN a,c");
        let anonymous_start = query("MATCH ()-[:R]->(x) RETURN x");
        let named_start = query("MATCH (m)-[:R]->(x) RETURN x");
        let run = |pattern: &PreparedGraphPattern<GraphValueRow>| -> Vec<Vec<VId>> {
            pattern
                .plan()
                .execute_governed_with_properties(
                    edges.len() as u64,
                    [],
                    edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap()
                .value
                .iter()
                .map(|row| {
                    vertex_rows(std::slice::from_ref(row))
                        .pop()
                        .expect("one row")
                })
                .collect()
        };
        // Independent complete-assignment enumeration: one row per (a, m, c)
        // assignment weighted by the edge-occurrence product.
        let mut expected = Vec::new();
        for a in 1..=4 {
            for middle in 1..=4 {
                for c in 1..=4 {
                    let multiplicity = edges
                        .iter()
                        .filter(|&&(s, _, d)| s == VId(a) && d == VId(middle))
                        .count()
                        * edges
                            .iter()
                            .filter(|&&(s, _, d)| s == VId(middle) && d == VId(c))
                            .count();
                    for _ in 0..multiplicity {
                        expected.push(vec![VId(a), VId(c)]);
                    }
                }
            }
        }
        expected.sort();
        let anonymous_rows = vertex_rows(
            &anonymous
                .plan()
                .execute_governed_with_properties(
                    edges.len() as u64,
                    [],
                    edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap()
                .value,
        );
        assert!(
            !anonymous_rows.is_empty(),
            "anti-vacuity: two-hop rows exist"
        );
        assert_eq!(anonymous_rows, expected);
        assert_eq!(anonymous_rows, run(&named));
        let mut start_expected: Vec<Vec<VId>> = edges.iter().map(|&(_, _, d)| vec![d]).collect();
        start_expected.sort();
        assert_eq!(run(&anonymous_start), start_expected);
        assert_eq!(run(&anonymous_start), run(&named_start));
    }

    #[test]
    fn anonymous_bindings_stay_private_and_reserved() {
        // RETURN * must not leak synthesized bindings.
        let pattern = query("MATCH (a)-[:R]->() RETURN *");
        assert_eq!(pattern.columns(), &["a"]);
        // User-written identifiers may never use the reserved namespace.
        for text in [
            "MATCH (n), (__fgdb_anonymous_0) RETURN n",
            "MATCH (n:__fgdb_anonymous_0) RETURN n",
            "MATCH (n), (__fgdb_anonymous_63)-[:R]->(n) RETURN n",
        ] {
            let failure = PreparedGraphText::prepare(text, symbols).unwrap_err();
            assert!(
                matches!(
                    failure.kind,
                    GraphPatternTextErrorKind::Expected(
                        "identifier outside the reserved __fgdb_anonymous_ namespace"
                    )
                ),
                "{text}: {failure:?}"
            );
        }
    }

    #[test]
    fn binding_reuses_resolved_structure_and_enforces_exact_typed_arguments() {
        let calls = Cell::new(0);
        let source = "MATCH (n:L) WHERE n.n >= $x AND n.n <= $x RETURN n SKIP $page LIMIT $page";
        let template = PreparedGraphText::prepare(source, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        let before = calls.get();
        assert_eq!(template.parameter_schema().len(), 2);
        assert!(
            template
                .parameter_schema()
                .iter()
                .all(|spec| spec.occurrences == 2)
        );
        let args = GqlParameters::new()
            .with_int64("x", i64::MIN)
            .unwrap()
            .with_uint64("page", 0)
            .unwrap();
        let first = template.bind_parameters(&args).unwrap();
        let frozen = first.canonical_bytes();
        let changed = GqlParameters::new()
            .with_int64("x", i64::MAX)
            .unwrap()
            .with_uint64("page", 1)
            .unwrap();
        assert_ne!(
            frozen,
            template
                .bind_parameters(&changed)
                .unwrap()
                .canonical_bytes()
        );
        assert_eq!(first.canonical_bytes(), frozen);
        assert_eq!(calls.get(), before);
        assert!(matches!(
            template
                .bind_parameters(&GqlParameters::new())
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::MissingParameter
        ));
        let wrong = GqlParameters::new()
            .with_uint64("x", 1)
            .unwrap()
            .with_uint64("page", 1)
            .unwrap();
        assert!(matches!(
            template.bind_parameters(&wrong).unwrap_err().kind,
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        ));
        let extra = changed.with_int64("unused", 5).unwrap();
        assert_eq!(
            template.bind_parameters(&extra).unwrap_err().kind,
            GraphPatternTextErrorKind::UnexpectedArguments
        );
        let mut resolves = 0;
        let conflict = PreparedGraphText::prepare(
            "MATCH (n:L) WHERE n.n = $x RETURN n LIMIT $x",
            |kind, name| {
                resolves += 1;
                symbols(kind, name)
            },
        )
        .unwrap_err();
        assert_eq!(
            conflict.kind,
            GraphPatternTextErrorKind::ConflictingParameterTypes
        );
        assert_eq!(resolves, 0);
    }

    #[test]
    fn unsupported_or_malformed_input_never_reaches_name_resolution() {
        for text in [
            "",
            "MATCH",
            "MATCH () RETURN *",
            "MATCH (a)<-[:R]->(b) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN DISTINCT ALL a",
            "MATCH (a)-[:R]->(b) RETURN missing",
            "MATCH (a) RETURN a;",
            "MATCH (a) RETURN a DROP GRAPH x",
            "MATCH (a) RETURN a ORDER BY missing",
            "MATCH (a) WHERE a.n = 1 OR OR a.n = 2 RETURN a",
            "MATCH (a) WHERE a > a RETURN a",
            "MATCH (a) WHERE a.n = 1.5 RETURN a",
            "MATCH (a) WHERE a.n = $ x RETURN a",
            "MATCH (a) RETURN a SKIP -1",
            "MATCH (a) RETURN a LIMIT 18446744073709551616",
            "MATCH (a) WHERE a.n = 9223372036854775808 RETURN a",
            "MATCH (a) WHERE a.n = -9223372036854775809 RETURN a",
            "MATCH (a {n:}) RETURN a",
            "MATCH (a) RETURN *,a",
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b) } RETURN a",
        ] {
            let mut calls = 0;
            assert!(
                PreparedGraphText::prepare(text, |kind, name| {
                    calls += 1;
                    symbols(kind, name)
                })
                .is_err(),
                "{text}"
            );
            assert_eq!(calls, 0, "failed syntax called the catalog: {text}");
        }
    }
    #[test]
    fn named_relationship_captures_prepare_and_reach_name_resolution() {
        let mut calls = 0;
        let prepared = PreparedGraphText::prepare("MATCH (a)-[e:R]->(b) RETURN a", |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .expect("named relationship captures are legal since fgdb-kp80 (482e77f0)");
        let pattern = prepared.bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(pattern.columns(), &["a"]);
        let actual = pattern
            .plan()
            .execute_governed_with_identified_properties(
                3,
                [],
                [
                    (fgdb_types::EId(1), VId(1), RelationId(1), VId(2)),
                    (fgdb_types::EId(2), VId(1), RelationId(1), VId(3)),
                    (fgdb_types::EId(3), VId(2), RelationId(2), VId(3)),
                ],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(vertex_rows(&actual.value), vec![vec![VId(1)], vec![VId(1)]]);
        assert!(calls > 0, "legal input must reach name resolution");
    }

    #[test]
    fn inline_property_map_selects_only_equal_values() {
        let pattern = query("MATCH (a {n:1}) RETURN a");
        let values = [CanonicalScalar::Int(1), CanonicalScalar::Int(2)];
        let actual = pattern
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(1), VId(2), VId(3)],
                [],
                |vid, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| match predicate {
                        VertexPredicate::IntegerProperty {
                            key: _,
                            comparison,
                            value,
                        } => {
                            values.get(vid.0 as usize - 1) == Some(&CanonicalScalar::Int(*value))
                                && *comparison == IntegerComparison::Equal
                        }
                        _ => true,
                    }))
                },
                |vid, _| Ok(values.get(vid.0 as usize - 1)),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(vertex_rows(&actual.value), vec![vec![VId(1)]]);
    }

    #[test]
    fn lexical_and_structural_limits_refuse_before_unbounded_preparation() {
        let oversized = " ".repeat(MAX_GRAPH_TEXT_BYTES + 1);
        assert_eq!(
            PreparedGraphText::prepare(&oversized, symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::DefinitionTooLarge
        );
        let long_name = format!(
            "MATCH ({}) RETURN *",
            "x".repeat(MAX_PATTERN_NAME_BYTES + 1)
        );
        assert_eq!(
            PreparedGraphText::prepare(&long_name, symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::NameTooLong
        );
        let tokens = format!(
            "MATCH {}(n) RETURN n",
            "(n),".repeat(MAX_GRAPH_TEXT_TOKENS / 3)
        );
        assert_eq!(
            PreparedGraphText::prepare(&tokens, symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::TooManyTokens
        );
        let mut longest = "MATCH (n0)".to_owned();
        for at in 1..=MAX_PATTERN_EDGES {
            longest.push_str(&format!("-[:R]->(n{at})"));
        }
        let valid = query(&format!("{longest} RETURN * LIMIT 0"));
        assert_eq!(valid.columns().len(), MAX_PATTERN_VERTICES);
        longest.push_str("-[:R]->(overflow) RETURN *");
        assert!(matches!(
            PreparedGraphText::prepare(&longest, symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded { .. })
        ));
        let disconnected = query("MATCH (a)-[:R]->(b),(c)-[:S]->(d) RETURN *");
        assert_eq!(disconnected.columns(), &["a", "b", "c", "d"]);
        assert!(!disconnected.plan().scans_edges());
        assert!(disconnected.plan().reads_edges());
        assert_eq!(disconnected.required_vertex_label(), None);
    }

    #[test]
    fn property_nulls_and_duplicate_pagination_use_existing_value_semantics() {
        let null = CanonicalScalar::Null;
        let value = CanonicalScalar::Int(7);
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(3)),
            (VId(1), RelationId(1), VId(4)),
        ];
        let run = |tail: &str| {
            query(&format!("MATCH (a)-[:R]->(b) RETURN {tail}"))
                .plan()
                .execute_governed_with_properties(
                    3,
                    [],
                    edges,
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| {
                        Ok(match vid {
                            VId(2) => None,
                            VId(3) => Some(&null),
                            _ => Some(&value),
                        })
                    },
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap()
                .value
        };
        assert_eq!(run("b.n"), run("ALL b.n"));
        assert_eq!(run("b.n").len(), 3);
        assert_eq!(run("DISTINCT b.n").len(), 2);
        assert!(run("b.n SKIP 1 LIMIT 1")[0].get(0).unwrap().is_null());
        assert_eq!(
            run("b.n SKIP 2 LIMIT 1")[0].get(0).unwrap().as_scalar(),
            Some(&value)
        );
        assert!(run("b.n LIMIT 0").is_empty());
        assert_eq!(
            query("match (a)-[:R]->(b) return b.n as value").columns(),
            &["value"]
        );
        assert_eq!(query("MATCH (a)-[:R]->(b) RETURN b.n").columns(), &["n"]);
        assert_eq!(
            PreparedGraphText::prepare("MATCH (a)-[:R]->(b) RETURN a.n,b.n", symbols)
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)
        );
    }

    #[test]
    fn error_messages_are_redacted_and_symbol_domains_cannot_be_confused() {
        let query_text = "MATCH (private_name:SecretLabel) RETURN private_name";
        let failed = PreparedGraphText::prepare(query_text, |_, _| None).unwrap_err();
        assert_eq!(
            failed.kind,
            GraphPatternTextErrorKind::UnknownSymbol(GraphSymbolKind::Label)
        );
        assert_eq!(failed.offset, query_text.find("SecretLabel").unwrap());
        assert!(!format!("{failed:?} {failed}").contains("SecretLabel"));
        let wrong = PreparedGraphText::prepare(query_text, |_, _| {
            Some(GraphSymbol::Relation(RelationId(999)))
        })
        .unwrap_err();
        assert!(matches!(
            wrong.kind,
            GraphPatternTextErrorKind::WrongSymbolKind { .. }
        ));
        let prepared =
            PreparedGraphText::prepare("MATCH (secret_name) RETURN secret_name", symbols).unwrap();
        assert!(!format!("{prepared:?}").contains("secret_name"));
        assert!(!format!("{:?}", GraphSymbol::Relation(RelationId(999))).contains("999"));
    }

    #[test]
    fn text_queries_share_all_policy_dimensions_and_every_interruption_checkpoint() {
        let pattern = query("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN ALL a,c.n AS score");
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(2), VId(3)),
            (VId(2), RelationId(2), VId(4)),
        ];
        let scalar = CanonicalScalar::Int(7);
        let run = |cap| {
            pattern.plan().execute_governed_with_properties(
                4,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&scalar)),
                cap,
                || Ok::<_, ()>(()),
            )
        };
        let measured = run(policy()).unwrap();
        let exact = GqlQueryPolicy::new(
            4,
            4,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(run(exact).unwrap(), measured);
        assert!(matches!(
            run(GqlQueryPolicy::new(3, 4, 100_000, 100_000)),
            Err(GqlQueryError::Rows(_))
        ));
        assert!(matches!(
            run(GqlQueryPolicy::new(4, 3, 100_000, 100_000)),
            Err(GqlQueryError::Rows(_))
        ));
        assert!(matches!(
            run(GqlQueryPolicy::new(
                4,
                4,
                exact.evaluator.max_work_units - 1,
                100_000
            )),
            Err(GqlQueryError::Evaluator(_))
        ));
        assert!(matches!(
            run(GqlQueryPolicy::new(
                4,
                4,
                100_000,
                exact.evaluator.max_scratch_entries - 1
            )),
            Err(GqlQueryError::Evaluator(_))
        ));
        let mut calls = 0;
        pattern
            .plan()
            .execute_governed_with_properties(
                4,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&scalar)),
                policy(),
                || {
                    calls += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        for stop in 1..=calls {
            let mut at = 0;
            let result = pattern.plan().execute_governed_with_properties(
                4,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(Some(&scalar)),
                policy(),
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn lexical_boundaries_unicode_space_and_repeated_nodes_preserve_identity() {
        let source = "\u{2003}match (n:L:L)-[:R]->(m:L)-[:S]->(n) return distinct *";
        assert_eq!(query(source).columns(), &["n", "m"]);
        // Every UTF-8 prefix must be parsed or refused without invalid slicing.
        for at in (0..=source.len()).filter(|at| source.is_char_boundary(*at)) {
            let _ = PreparedGraphText::prepare(&source[..at], symbols);
        }
        for text in [
            "MATCHER (a) RETURN a",
            "MATCH (a) RETURN a LIMITER 1",
            "MATCH (a) WHERE a.n=1foo RETURN a",
        ] {
            assert!(PreparedGraphText::prepare(text, symbols).is_err());
        }
        assert!(
            PreparedGraphText::prepare(
                "MATCH (n:L) WHERE n.n=-9223372036854775808 RETURN n LIMIT 18446744073709551615",
                symbols
            )
            .is_ok()
        );
        assert_eq!(
            query("MATCH (a) RETURN a AS x,a AS y").columns(),
            &["x", "y"]
        );
    }

    #[test]
    fn inline_node_maps_lower_to_existing_predicates_in_every_match_scope() {
        for (inline, expanded) in [
            ("MATCH (a {}) RETURN a", "MATCH (a) RETURN a"),
            (
                "MATCH (a:L {n:7})-[:R]->(b {n:8}) RETURN a,b",
                "MATCH (a:L)-[:R]->(b) WHERE a.n=7 AND b.n=8 RETURN a,b",
            ),
            (
                "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b {n:7}) RETURN a,b",
                "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b.n=7 RETURN a,b",
            ),
            (
                "MATCH (a) MATCH (a)-[:R]->(b {n:7}) RETURN a,b",
                "MATCH (a) MATCH (a)-[:R]->(b) WHERE b.n=7 RETURN a,b",
            ),
            (
                "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b {n:7}) } RETURN a",
                "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(b) WHERE b.n=7 } RETURN a",
            ),
            (
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b {n:7}) } RETURN a",
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.n=7 } RETURN a",
            ),
            (
                "MATCH (a {n:'O''Brien 🦀, $x: RETURN'}) RETURN a",
                "MATCH (a) WHERE a.n='O''Brien 🦀, $x: RETURN' RETURN a",
            ),
            (
                "MATCH (a {n:TRUE}) RETURN a",
                "MATCH (a) WHERE a.n=TRUE RETURN a",
            ),
            (
                "MATCH (a {n:NULL}) RETURN a",
                "MATCH (a) WHERE a.n=NULL RETURN a",
            ),
        ] {
            assert_eq!(query(inline), query(expanded), "{inline}");
        }
        let text = "MATCH (a {n:$value})-[:R]->(b {n:$value}) RETURN a,b";
        let mut resolutions = 0;
        let template = PreparedGraphText::prepare(text, |kind, name| {
            resolutions += 1;
            symbols(kind, name)
        })
        .unwrap();
        assert_eq!(resolutions, 2);
        assert_eq!(template.parameter_schema().len(), 1);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let expanded = PreparedGraphText::prepare(
            "MATCH (a)-[:R]->(b) WHERE a.n=$value AND b.n=$value RETURN a,b",
            symbols,
        )
        .unwrap();
        for value in [i64::MIN, 0, i64::MAX] {
            let arguments = GqlParameters::new().with_int64("value", value).unwrap();
            assert_eq!(
                template.bind_parameters(&arguments).unwrap(),
                expanded.bind_parameters(&arguments).unwrap()
            );
        }
        let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
        assert_eq!(missing.kind, GraphPatternTextErrorKind::MissingParameter);
        assert_eq!(missing.offset, text.find("$value").unwrap());
    }

    #[test]
    fn inline_node_maps_keep_optional_rows_parallel_edges_and_null_semantics() {
        let values = BTreeMap::from([
            (VId(1), CanonicalScalar::Int(1)),
            (VId(2), CanonicalScalar::Int(7)),
            (VId(3), CanonicalScalar::Null),
        ]);
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(3)),
        ];
        let run = |text: &str| {
            query(text)
                .plan()
                .execute_governed_with_properties(
                    4,
                    [VId(1), VId(2), VId(3), VId(4)],
                    edges,
                    |vid, predicate| {
                        let properties: Vec<_> = values
                            .get(&vid)
                            .map(|value| (PropertyKeyId(4), value.clone()))
                            .into_iter()
                            .collect();
                        Ok::<_, ()>(predicate.iter().all(|p| p.matches(&[], &properties)))
                    },
                    |vid, _| Ok(values.get(&vid)),
                    policy(),
                    || Ok::<_, ()>(()),
                )
                .unwrap()
                .value
        };
        let rows = run("MATCH (a {n:1}) OPTIONAL MATCH (a)-[:R]->(b {n:7}) RETURN a,b");
        assert_eq!(vertex_rows(&rows), vec![vec![VId(1), VId(2)]; 2]);
        let absent = run("MATCH (a {n:1}) OPTIONAL MATCH (a)-[:R]->(b {n:9}) RETURN a,b");
        assert_eq!(absent.len(), 1);
        assert_eq!(absent[0].get(0).unwrap().as_vertex(), Some(VId(1)));
        assert!(absent[0].get(1).unwrap().is_null());
        assert!(run("MATCH (a {n:NULL}) RETURN a").is_empty());
        assert_eq!(
            vertex_rows(&run("MATCH (a) WHERE a.n IS NULL RETURN a")),
            vec![vec![VId(3)], vec![VId(4)]]
        );
        assert!(run("MATCH (a {n:1})-[:R]->(a {n:7}) RETURN a").is_empty());
    }

    #[test]
    fn inline_node_map_errors_precede_catalog_and_share_the_global_predicate_limit() {
        for text in [
            "MATCH (a {n:1,n:2}) RETURN a",
            "MATCH (a {n:1,}) RETURN a",
            "MATCH (a {n 1}) RETURN a",
            "MATCH (a {n:[1]}) RETURN a",
            "MATCH (a {n:{n:1}}) RETURN a",
            "MATCH (a {n:a.n}) RETURN a",
            "MATCH (a {n:1+2}) RETURN a",
            "MATCH (a {n:$x}) RETURN a LIMIT $x",
            "MATCH (a {n:'unterminated}) RETURN a",
        ] {
            let mut calls = 0;
            assert!(
                PreparedGraphText::prepare(text, |kind, name| {
                    calls += 1;
                    symbols(kind, name)
                })
                .is_err(),
                "{text}"
            );
            assert_eq!(calls, 0, "{text}");
        }
        let entries = (0..MAX_PATTERN_PREDICATES)
            .map(|index| format!("p{index}:1"))
            .collect::<Vec<_>>()
            .join(",");
        let resolve = |kind, name: &str| match kind {
            GraphSymbolKind::Property => name
                .strip_prefix('p')
                .and_then(|id| id.parse().ok())
                .map(|id| GraphSymbol::Property(PropertyKeyId(id))),
            _ => symbols(kind, name),
        };
        let exact = format!("MATCH (a {{{entries}}}) RETURN a");
        let template = PreparedGraphText::prepare(&exact, resolve).unwrap();
        assert!(template.bind_parameters(&GqlParameters::new()).is_ok());
        for text in [
            format!("MATCH (a {{{entries},extra:1}}) RETURN a"),
            format!("MATCH (a:L {{{entries}}}) RETURN a"),
            format!("MATCH (a {{{entries}}}) WHERE a.p0=1 RETURN a"),
            format!("MATCH (a {{{entries}}}) OPTIONAL MATCH (a {{n:1}}) RETURN a"),
        ] {
            let mut calls = 0;
            let failure = PreparedGraphText::prepare(&text, |kind, name| {
                calls += 1;
                resolve(kind, name)
            })
            .unwrap_err();
            assert!(matches!(
                failure.kind,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    limit: MAX_PATTERN_PREDICATES,
                    ..
                })
            ));
            assert_eq!(calls, 0);
        }
    }
}
