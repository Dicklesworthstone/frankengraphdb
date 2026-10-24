//! Typed relational projection over complete GLA/set rows. Expressions see the
//! original row, never another output alias. No graph is traversed a second time.

#[cfg(test)]
mod map_overlay_tests;
pub(crate) mod membership;
mod subscript;
mod temporal;

pub(crate) use membership::{
    equal as evaluate_equality, evaluate as evaluate_membership, order as evaluate_order,
};
pub use temporal::GqlTimeZones;
pub(crate) use temporal::{
    arithmetic as temporal_arithmetic, construct as construct_temporal, field as temporal_field,
    field_scratch_entries as temporal_field_scratch_entries, is_field_name as temporal_field_name,
    timestamp_text,
};

use super::spill::AsyncSpillElementSource;
use super::{
    GraphSetBuildError, GraphSetColumnType, GraphSetQuantifier, PreparedGraphSet, SetNode,
    check_depth,
};
use crate::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow, MAX_PATTERN_VERTICES,
};
use crate::{
    GlaExecutionEvent, GqlScalarParameter, GraphIntegerError, GraphIntegerEvaluationError,
    GraphIntegerExpression,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_types::CanonicalScalar;
use std::borrow::Borrow;
use std::collections::BTreeSet;

/// Checked output expression. Column positions refer to the input relation,
/// including when that relation is itself a projection or compound set.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphSetValue {
    Column(usize),
    Literal(GqlScalarParameter),
    /// Canonical NULL with explicit static domain evidence. Nullable collection
    /// parameters retain their declared List/Any domain through binding; an
    /// ordinary literal NULL remains Scalar. No nonnull value can enter here.
    TypedNull(GraphSetColumnType),
    Integer(GraphIntegerExpression),
    List(Vec<GraphSetValue>),
    /// A signed integer selects a list member; a text key selects a map entry.
    /// NULL operands, missing keys and out-of-range indexes yield NULL. This
    /// is value access only: graph identities never become property maps.
    Index {
        list: Box<GraphSetValue>,
        index: Box<GraphSetValue>,
    },
    Size(Box<GraphSetValue>),
    /// Three-valued membership in an evaluated native list. Both operands
    /// address the original row and execute once. Empty lists yield FALSE,
    /// NULL lists yield UNKNOWN, and non-list values are expression errors.
    In {
        value: Box<GraphSetValue>,
        list: Box<GraphSetValue>,
    },
    /// Three-valued `left = right` under the membership law (fgdb-9cawv):
    /// lists and maps compare element by element, and a NULL operand is
    /// UNKNOWN. Both operands address the original row and execute once.
    Equal {
        left: Box<GraphSetValue>,
        right: Box<GraphSetValue>,
    },
    /// Three-valued `<`, `<=`, `>` or `>=` over lists (openCypher
    /// comparability): elements compare in order, the first unequal pair
    /// decides, and an equal prefix makes the shorter list the lesser.
    /// Both operands address the original row and execute once.
    Ordered {
        comparison: crate::algebra::IntegerComparison,
        left: Box<GraphSetValue>,
        right: Box<GraphSetValue>,
    },
    Value(GraphValue),
    /// A list-comprehension element (fgdb-20foe): the value `k` positions
    /// before the END of an element-scope row, which is the enclosing row
    /// followed by one element per enclosing binding, innermost last.
    Local(usize),
    /// `[x IN list WHERE filter | map]`. `filter` and `map` evaluate over the
    /// row extended by the element; only a TRUE filter keeps an element.
    /// Without `map` the element itself is kept. A NULL list is NULL.
    Comprehension {
        list: Box<GraphSetValue>,
        filter: Option<Box<GraphSetValue>>,
        map: Option<Box<GraphSetValue>>,
    },
    /// `any/all/none/single(x IN list WHERE predicate)`, three-valued over
    /// the predicate's per-element results. A NULL list is NULL.
    Quantifier {
        kind: GraphListQuantifier,
        list: Box<GraphSetValue>,
        predicate: Box<GraphSetValue>,
    },
    /// `list[from..to]`, with openCypher bounds (see slice_bounds); a NULL
    /// list or bound is NULL. `tail(list)` is `list[1..]`.
    Slice {
        list: Box<GraphSetValue>,
        from: Option<Box<GraphSetValue>>,
        to: Option<Box<GraphSetValue>>,
    },
    /// `range(start, end, step)`, end inclusive, step 1 when absent.
    Range {
        start: Box<GraphSetValue>,
        end: Box<GraphSetValue>,
        step: Option<Box<GraphSetValue>>,
    },
    /// `reduce(acc = init, x IN list | expr)`: `expr` evaluates over the
    /// row extended by the accumulator then the element (Local(1), Local(0)).
    /// A NULL list is NULL; an empty list is `init`.
    Reduce {
        init: Box<GraphSetValue>,
        list: Box<GraphSetValue>,
        expr: Box<GraphSetValue>,
    },
    /// `{k: e, ...}` (fgdb-2jw3z): keys unique and ascending by UTF-8 bytes,
    /// `values[i]` for `keys[i]`, so evaluation builds a canonical map. A map
    /// projection `n{.a, ...}` carries its source as `guard`: a NULL source
    /// makes the whole map NULL, as openCypher requires.
    MapLiteral {
        keys: Box<[Box<str>]>,
        values: Vec<GraphSetValue>,
        guard: Option<Box<GraphSetValue>>,
    },
    /// A map source followed by explicit overrides. Keys are unique and
    /// ascending; explicit entries replace matching base keys. A NULL base
    /// makes the whole result NULL without evaluating the override values.
    MapOverlay {
        base: Box<GraphSetValue>,
        keys: Box<[Box<str>]>,
        values: Vec<GraphSetValue>,
    },
    /// `m.key`: NULL for a NULL map or an absent key. A vertex or edge reads
    /// its stored property through `property` (fgdb-xnd7x). Any other value
    /// is a typed error.
    MapGet {
        map: Box<GraphSetValue>,
        key: Box<str>,
        property: GraphElementKey,
    },
    /// `keys(m)`: the map's keys as text, ascending; NULL for a NULL map.
    Keys(Box<GraphSetValue>),
    /// A scalar expression over composite operands (fgdb-o5o9t), such as
    /// `none(x IN l WHERE p) = (NOT any(x IN l WHERE p))`. Each of `values`
    /// evaluates once over the enclosing row; `body` then evaluates over that
    /// row extended by one slot per value, value `i` at
    /// `Local(values.len() - 1 - i)`.
    Let {
        values: Vec<GraphSetValue>,
        body: Box<GraphSetValue>,
    },
    /// A temporal constructor (fgdb-txbg9), such as `date({year: 1984})` or
    /// `localtime('21:40')`. NULL is NULL.
    Temporal {
        function: GraphTemporalFunction,
        argument: Box<GraphSetValue>,
        /// The statement's pinned tzdb, for named zones; without it a
        /// named zone refuses.
        zones: Option<temporal::GqlTimeZones>,
    },
    /// An openCypher value function over lists and text (fgdb-13l85). The
    /// argument of a one-argument function is that value; of a longer one,
    /// the list of its arguments, as for a dotted temporal call.
    Function {
        function: GraphValueFunction,
        argument: Box<GraphSetValue>,
    },
    /// A searched CASE whose results may be composite (fgdb-13l85), such as
    /// `CASE WHEN c THEN reverse(list) ELSE list END`. Arms run in order: the
    /// first condition that is TRUE selects its value, a FALSE or NULL one
    /// moves on, and a non-Boolean one is a typed error. Nothing after the
    /// selected arm executes. Without a selected arm the value is
    /// `otherwise`, or NULL when there is none.
    Case {
        arms: Vec<(GraphSetValue, GraphSetValue)>,
        otherwise: Option<Box<GraphSetValue>>,
    },
    /// openCypher rand() (fgdb-13l85): a float in [0, 1) that is a pure
    /// function of the call `site`, the row's ordinal in its projection
    /// stage and the `scope` innermost element values in force, so every
    /// execution of a statement over the same rows draws the same values.
    /// Equal elements of one row draw equally. Without a row ordinal it is
    /// a typed MissingColumn error, never a guessed key.
    Random {
        site: u32,
        scope: u32,
    },
}

/// How `x.key` reads a vertex or edge value (fgdb-xnd7x). The key binds at
/// preparation through the statement's symbol resolver, like a pattern
/// variable's property, so execution never needs a catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphElementKey {
    /// No resolver bound this read: an element operand is a typed error.
    Unbound,
    /// The catalog lacks the key: every element reads NULL.
    Missing,
    /// The element's stored property under this key, read at the source's cut.
    Property(PropertyKeyId),
}

/// The openCypher value functions over lists and text (fgdb-13l85).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphValueFunction {
    /// `reverse(x)`: a list's elements, or a text's Unicode scalars, in
    /// reverse order. NULL is NULL.
    Reverse,
    /// `split(text, delimiter)`: the text's pieces between delimiters, as a
    /// list of text; an empty delimiter gives each Unicode scalar. NULL in
    /// either argument is NULL.
    Split,
    /// openCypher `left + right` where an operand may be a list
    /// (fgdb-c4i0d): two lists concatenate, and a list and another value
    /// append or prepend it. Two scalars add as the scalar `+` does, texts
    /// concatenating. NULL in either operand is NULL. No call spells it.
    Add,
    /// `left - right` in the same chain: two scalars subtract as the scalar
    /// `-` does; a list operand is a typed error. NULL is NULL.
    Subtract,
    /// openCypher `coalesce(a, b, ...)` over a composite argument (a list,
    /// map or element): the first non-NULL argument, read in order, and no
    /// later argument runs; all NULL is NULL. The scalar COALESCE keeps every
    /// all-scalar call.
    Coalesce,
    /// openCypher `percentileDisc(x, p)` (fgdb-qbaex), applied after
    /// aggregation to `[collect(x), p]`: the sorted numbers' element at index
    /// ceil(p * n) - 1 (at least 0), Int or Float as collected. An empty
    /// collection is NULL; p outside [0, 1] or a non-number is a typed error.
    PercentileDisc,
    /// openCypher `percentileCont(x, p)`: the linear interpolation at
    /// p * (n - 1) between the neighbouring sorted numbers, always a Float.
    PercentileCont,
    /// `nodes(x)` of a path value, not a graph binding (an unwound path, a
    /// grouping key, a list local): its vertices in order. NULL is NULL; any
    /// other value is a typed error.
    PathNodes,
    /// `relationships(x)` of a path value: its edges in order. NULL is NULL;
    /// any other value is a typed error.
    PathRelationships,
}

impl GraphValueFunction {
    /// Its byte in the value transcripts.
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Reverse => 1,
            Self::Split => 2,
            Self::Add => 3,
            Self::Subtract => 4,
            Self::Coalesce => 5,
            Self::PercentileDisc => 6,
            Self::PercentileCont => 7,
            Self::PathNodes => 8,
            Self::PathRelationships => 9,
        }
    }

    /// How many arguments a call takes; `None` for one or more.
    pub(crate) const fn arity(self) -> Option<usize> {
        match self {
            Self::Reverse | Self::PathNodes | Self::PathRelationships => Some(1),
            Self::Split
            | Self::Add
            | Self::Subtract
            | Self::PercentileDisc
            | Self::PercentileCont => Some(2),
            Self::Coalesce => None,
        }
    }

    /// The function a call named `name` (any case) denotes.
    pub(crate) fn named(name: &str) -> Option<Self> {
        [("REVERSE", Self::Reverse), ("SPLIT", Self::Split)]
            .into_iter()
            .find(|(spelling, _)| name.eq_ignore_ascii_case(spelling))
            .map(|(_, function)| function)
    }
}

/// The openCypher temporal constructors in the value language. Each takes a
/// map of fields (over optional bases), ISO 8601 text, a temporal value to
/// convert, or NULL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphTemporalFunction {
    /// `date(x)`: calendar fields of one calendar, over a `date` base.
    Date,
    /// `localtime(x)`: clock fields, over a `time` base.
    LocalTime,
    /// `time(x)`: clock fields and a `timezone` offset, over a `time` base.
    Time,
    /// `localdatetime(x)`: calendar and clock fields, over `date`, `time`
    /// and `datetime` bases.
    LocalDateTime,
    /// `datetime(x)`: a timestamp at a UTC offset, from calendar and clock
    /// fields and a `timezone` offset over `date`, `time` and `datetime`
    /// bases, or from `epochSeconds` or `epochMillis`.
    DateTime,
    /// `date.truncate(unit, x[, map])`, and its siblings for the other
    /// kinds. The argument is the list of the call's arguments.
    TruncateDate,
    TruncateLocalTime,
    TruncateTime,
    TruncateLocalDateTime,
    TruncateDateTime,
    /// `datetime.fromepoch(seconds, nanoseconds)`, at UTC.
    FromEpoch,
    /// `datetime.fromepochmillis(milliseconds)`, at UTC.
    FromEpochMillis,
    /// `duration(x)`: from a map of counts, ISO 8601 text, a duration, or
    /// NULL.
    Duration,
    /// `duration.between(a, b)`: whole months, then days, then the time
    /// from `a` to `b`.
    Between,
    /// `duration.inMonths(a, b)`: the whole months of `between`.
    InMonths,
    /// `duration.inDays(a, b)`: the whole days from `a` to `b`.
    InDays,
    /// `duration.inSeconds(a, b)`: the whole time from `a` to `b`.
    InSeconds,
    /// `date.transaction(x)`, `.statement(x)` and `.realtime(x)` for every
    /// temporal kind: a clock read. NULL is NULL. A query context grants no
    /// clock capability, so any other argument is a typed refusal, never a
    /// host clock that would break replay.
    Clock,
}

impl GraphTemporalFunction {
    /// Its byte in the value transcripts.
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Date => 1,
            Self::LocalTime => 2,
            Self::Time => 3,
            Self::LocalDateTime => 4,
            Self::DateTime => 5,
            Self::TruncateDate => 6,
            Self::TruncateLocalTime => 7,
            Self::TruncateTime => 8,
            Self::TruncateLocalDateTime => 9,
            Self::TruncateDateTime => 10,
            Self::FromEpoch => 11,
            Self::FromEpochMillis => 12,
            Self::Duration => 13,
            Self::Between => 14,
            Self::InMonths => 15,
            Self::InDays => 16,
            Self::InSeconds => 17,
            Self::Clock => 18,
        }
    }

    /// The function a call named `name` (any case) denotes.
    pub(crate) fn named(name: &str) -> Option<Self> {
        [
            ("DATE", Self::Date),
            ("LOCALTIME", Self::LocalTime),
            ("TIME", Self::Time),
            ("LOCALDATETIME", Self::LocalDateTime),
            ("DATETIME", Self::DateTime),
            ("DURATION", Self::Duration),
        ]
        .into_iter()
        .find_map(|(spelling, function)| name.eq_ignore_ascii_case(spelling).then_some(function))
    }

    /// The function a call `name.operation(...)` (any case) denotes.
    pub(crate) fn dotted(name: &str, operation: &str) -> Option<Self> {
        let clock = ["TRANSACTION", "STATEMENT", "REALTIME"]
            .iter()
            .any(|spelling| operation.eq_ignore_ascii_case(spelling));
        let function = if clock {
            match Self::named(name)? {
                Self::Duration => return None,
                _ => Self::Clock,
            }
        } else if operation.eq_ignore_ascii_case("TRUNCATE") {
            match Self::named(name)? {
                Self::Date => Self::TruncateDate,
                Self::LocalTime => Self::TruncateLocalTime,
                Self::Time => Self::TruncateTime,
                Self::LocalDateTime => Self::TruncateLocalDateTime,
                Self::DateTime => Self::TruncateDateTime,
                _ => return None,
            }
        } else if name.eq_ignore_ascii_case("DATETIME")
            && operation.eq_ignore_ascii_case("FROMEPOCH")
        {
            Self::FromEpoch
        } else if name.eq_ignore_ascii_case("DATETIME")
            && operation.eq_ignore_ascii_case("FROMEPOCHMILLIS")
        {
            Self::FromEpochMillis
        } else if name.eq_ignore_ascii_case("DURATION") {
            [
                ("BETWEEN", Self::Between),
                ("INMONTHS", Self::InMonths),
                ("INDAYS", Self::InDays),
                ("INSECONDS", Self::InSeconds),
            ]
            .into_iter()
            .find_map(|(spelling, function)| {
                operation.eq_ignore_ascii_case(spelling).then_some(function)
            })?
        } else {
            return None;
        };
        Some(function)
    }

    /// How many arguments a dotted call takes; `None` for a constructor,
    /// whose one argument is not a list.
    pub(crate) const fn arity(self) -> Option<(usize, usize)> {
        match self {
            Self::TruncateDate
            | Self::TruncateLocalTime
            | Self::TruncateTime
            | Self::TruncateLocalDateTime
            | Self::TruncateDateTime => Some((2, 3)),
            Self::FromEpoch | Self::Between | Self::InMonths | Self::InDays | Self::InSeconds => {
                Some((2, 2))
            }
            Self::FromEpochMillis => Some((1, 1)),
            Self::Date
            | Self::LocalTime
            | Self::Time
            | Self::LocalDateTime
            | Self::DateTime
            | Self::Duration
            | Self::Clock => None,
        }
    }
}

/// The openCypher list predicate functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphListQuantifier {
    Any,
    All,
    None,
    Single,
}
impl GraphListQuantifier {
    /// The one transcript tag shared by bound values and unbound templates.
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::All => 1,
            Self::None => 2,
            Self::Single => 3,
        }
    }
    /// Combine per-element predicate results (Some(true), Some(false) or
    /// None for UNKNOWN) as openCypher does: a decisive element wins, then
    /// any UNKNOWN makes the answer UNKNOWN.
    #[must_use]
    pub fn combine(self, results: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
        let (mut trues, mut falses, mut unknowns) = (0_usize, 0_usize, 0_usize);
        for result in results {
            match result {
                Some(true) => trues += 1,
                Some(false) => falses += 1,
                None => unknowns += 1,
            }
        }
        match self {
            Self::Any if trues > 0 => Some(true),
            Self::None if trues > 0 => Some(false),
            Self::All if falses > 0 => Some(false),
            Self::Single if trues > 1 => Some(false),
            _ if unknowns > 0 => None,
            Self::Any => Some(false),
            Self::None | Self::All => Some(true),
            Self::Single => Some(trues == 1),
        }
    }
}
impl GraphSetValue {
    // A conservative preparation-time effect check. All children, including
    // lazy branches, participate; no value is evaluated and no source is read.
    // Typed admission already bounds this tree. The exhaustive match keeps new
    // expression variants from silently selecting a reader-free executor.
    pub(super) fn may_read_element_properties(&self) -> bool {
        match self {
            Self::Column(_)
            | Self::Literal(_)
            | Self::TypedNull(_)
            | Self::Integer(_)
            | Self::Value(_)
            | Self::Local(_)
            | Self::Random { .. } => false,
            Self::MapGet { map, property, .. } => {
                matches!(property, GraphElementKey::Property(_))
                    || map.may_read_element_properties()
            }
            Self::List(values) => values.iter().any(Self::may_read_element_properties),
            Self::Size(value)
            | Self::Keys(value)
            | Self::Temporal {
                argument: value, ..
            }
            | Self::Function {
                argument: value, ..
            } => value.may_read_element_properties(),
            Self::Index {
                list: left,
                index: right,
            }
            | Self::In {
                value: left,
                list: right,
            }
            | Self::Equal { left, right }
            | Self::Ordered { left, right, .. }
            | Self::Quantifier {
                list: left,
                predicate: right,
                ..
            } => left.may_read_element_properties() || right.may_read_element_properties(),
            Self::Comprehension { list, filter, map } => {
                list.may_read_element_properties()
                    || filter
                        .as_deref()
                        .is_some_and(Self::may_read_element_properties)
                    || map
                        .as_deref()
                        .is_some_and(Self::may_read_element_properties)
            }
            Self::Slice { list, from, to } => {
                list.may_read_element_properties()
                    || from
                        .as_deref()
                        .is_some_and(Self::may_read_element_properties)
                    || to.as_deref().is_some_and(Self::may_read_element_properties)
            }
            Self::Range { start, end, step } => {
                start.may_read_element_properties()
                    || end.may_read_element_properties()
                    || step
                        .as_deref()
                        .is_some_and(Self::may_read_element_properties)
            }
            Self::Reduce { init, list, expr } => {
                init.may_read_element_properties()
                    || list.may_read_element_properties()
                    || expr.may_read_element_properties()
            }
            Self::MapLiteral { values, guard, .. } => {
                guard
                    .as_deref()
                    .is_some_and(Self::may_read_element_properties)
                    || values.iter().any(Self::may_read_element_properties)
            }
            Self::MapOverlay { base, values, .. } | Self::Let { body: base, values } => {
                base.may_read_element_properties()
                    || values.iter().any(Self::may_read_element_properties)
            }
            Self::Case { arms, otherwise } => {
                arms.iter().any(|(condition, value)| {
                    condition.may_read_element_properties() || value.may_read_element_properties()
                }) || otherwise
                    .as_deref()
                    .is_some_and(Self::may_read_element_properties)
            }
        }
    }

    pub(crate) fn append_canonical_bytes(&self, bytes: &mut Vec<u8>) {
        append_value_transcript(self, bytes);
    }
    #[allow(dead_code)]
    pub(super) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        self.append_canonical_bytes(bytes);
    }
}
impl core::fmt::Debug for GraphSetValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetValue([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GraphSetProjection {
    name: String,
    value: GraphSetValue,
}
impl GraphSetProjection {
    pub(crate) fn admit_output(
        value: &GraphSetValue,
        types: &[GraphSetColumnType],
        column: usize,
    ) -> Result<GraphSetColumnType, GraphSetProjectionError> {
        value_type(value, types, column)
    }

    pub(crate) fn validate_output_name(
        name: &str,
        column: usize,
    ) -> Result<(), GraphSetProjectionError> {
        validate_name(name, column)
    }

    pub(crate) fn validate_binding_name(
        name: &str,
        column: usize,
    ) -> Result<(), GraphSetProjectionError> {
        validate_binding_name(name, column)
    }

    #[must_use]
    pub fn new(name: impl Into<String>, value: GraphSetValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    #[must_use]
    pub fn value(&self) -> &GraphSetValue {
        &self.value
    }

    /// The same row mechanism also supplies computed GroupAggregate inputs.
    /// The owner supplies its arithmetic error vocabulary; source/control
    /// failures pass through without wrapping or losing their original type.
    pub(crate) fn evaluate_row_with_control<E>(
        row: &GraphValueRow,
        projection: &[Self],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        arithmetic: impl FnOnce(usize, GraphIntegerError) -> E,
    ) -> Result<GraphValueRow, E> {
        Self::evaluate_row_with_control_and_elements(
            row,
            projection,
            control,
            &mut no_element_reads,
            arithmetic,
        )
    }

    pub(crate) fn evaluate_row_with_control_and_elements<E>(
        row: &GraphValueRow,
        projection: &[Self],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        read: &mut ElementRead<'_, E>,
        arithmetic: impl FnOnce(usize, GraphIntegerError) -> E,
    ) -> Result<GraphValueRow, E> {
        evaluate(row, projection, None, control, read).map_err(|error| match error {
            ProjectionFailure::Control(error) => error,
            ProjectionFailure::Arithmetic { column, error } => arithmetic(column, error),
        })
    }

    /// evaluate_row_with_control for a row at `ordinal` in the stage's
    /// deterministic input order, which keys rand() (fgdb-13l85). Every
    /// executor of the same stage numbers its rows the same way. `read` is the
    /// host's element property reader (fgdb-xnd7x).
    pub(crate) fn evaluate_numbered_row_with_control<E>(
        row: &GraphValueRow,
        ordinal: u64,
        projection: &[Self],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        read: &mut ElementRead<'_, E>,
        arithmetic: impl FnOnce(usize, GraphIntegerError) -> E,
    ) -> Result<GraphValueRow, E> {
        evaluate(row, projection, Some(ordinal), control, read).map_err(|error| match error {
            ProjectionFailure::Control(error) => error,
            ProjectionFailure::Arithmetic { column, error } => arithmetic(column, error),
        })
    }
}
impl core::fmt::Debug for GraphSetProjection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphSetProjection([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetProjectionError {
    Empty,
    TooManyColumns {
        limit: usize,
        observed: usize,
    },
    InvalidName {
        column: usize,
    },
    DuplicateName {
        column: usize,
    },
    UnknownInput {
        column: usize,
        input: usize,
    },
    IntegerInput {
        column: usize,
        input: usize,
    },
    ListInput {
        column: usize,
    },
    MapInput {
        column: usize,
    },
    ExpressionBounds {
        column: usize,
    },
    InvalidValue {
        column: usize,
    },
    SetBuild(GraphSetBuildError),
}
impl core::fmt::Display for GraphSetProjectionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("a projection requires at least one column"),
            Self::TooManyColumns { limit, observed } => {
                write!(f, "projection has {observed} columns, limit {limit}")
            }
            Self::InvalidName { column } => {
                write!(f, "projection column {column} has an invalid name")
            }
            Self::DuplicateName { column } => {
                write!(
                    f,
                    "projection column {column} repeats an earlier column name"
                )
            }
            Self::UnknownInput { column, input } => {
                write!(f, "projection column {column} reads unknown input {input}")
            }
            Self::IntegerInput { column, input } => write!(
                f,
                "projection column {column} reads input {input}, which has the wrong type for its expression"
            ),
            Self::ListInput { column } => write!(
                f,
                "projection column {column} uses a non-scalar value where a scalar is required"
            ),
            Self::MapInput { column } => write!(
                f,
                "projection column {column} reads a map from a value that is not a map"
            ),
            Self::ExpressionBounds { column } => {
                write!(
                    f,
                    "projection column {column} nests too deeply or has too many values"
                )
            }
            Self::InvalidValue { column } => {
                write!(
                    f,
                    "projection column {column} holds a value outside its bounds"
                )
            }
            Self::SetBuild(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphSetProjectionError {}

impl PreparedGraphSet {
    /// Project the completed input before this new scope's DISTINCT/order/page.
    /// The input retains its own page. All columns read the same frozen input;
    /// aliases never become temporary variable bindings. Numeric references are
    /// checked against scalar domains before a source can be executed, including
    /// columns in lazy branches. Individual scalar kinds are checked at runtime.
    /// A constant projection still emits once per input occurrence, not once
    /// per distinct hidden carrier. This is the bounded materialized profile,
    /// not spill-backed expression evaluation.
    pub fn project(
        self,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphSetProjectionError> {
        use GraphSetProjectionError as Error;
        if projection.is_empty() {
            return Err(Error::Empty);
        }
        if projection.len() > MAX_PATTERN_VERTICES {
            return Err(Error::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: projection.len(),
            });
        }
        let depth = self.depth + 1;
        check_depth(depth).map_err(Error::SetBuild)?;
        let mut names = BTreeSet::new();
        let mut types = Vec::new();
        for (column, output) in projection.iter().enumerate() {
            validate_name(&output.name, column)?;
            if !names.insert(output.name.as_str()) {
                return Err(Error::DuplicateName { column });
            }
            let mut nodes = 0;
            types.push(admit(&output.value, &self.types, column, 0, &mut nodes, 0)?);
        }
        Ok(Self {
            columns: projection
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            types,
            operands: self.operands,
            depth,
            node: SetNode::Project {
                input: Box::new(self),
                projection,
                quantifier,
                preserve_order: false,
            },
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    /// Remove compiler-private ranking cells while keeping the selected
    /// sequence. Ordinary RETURN projections retain their canonical-order
    /// contract; this method is only the output half of one ordering stage.
    pub(crate) fn project_ordered_output(
        self,
        projection: Vec<GraphSetProjection>,
    ) -> Result<Self, GraphSetProjectionError> {
        let mut result = self.project(projection, GraphSetQuantifier::All)?;
        let SetNode::Project { preserve_order, .. } = &mut result.node else {
            unreachable!("project constructs the checked projection node");
        };
        *preserve_order = true;
        Ok(result)
    }
}

pub(super) fn value_type(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
) -> Result<GraphSetColumnType, GraphSetProjectionError> {
    admit(value, types, column, 0, &mut 0, 0)
}

pub(super) fn validate_name(name: &str, column: usize) -> Result<(), GraphSetProjectionError> {
    if !crate::algebra::valid_column_name(name) {
        return Err(GraphSetProjectionError::InvalidName { column });
    }
    Ok(())
}

/// A name that introduces a row variable (an UNWIND or YIELD alias, a named
/// procedure argument) keeps the identifier shape; only a projected output
/// column may carry any [`validate_name`] text.
pub(super) fn validate_binding_name(
    name: &str,
    column: usize,
) -> Result<(), GraphSetProjectionError> {
    validate_name(name, column)?;
    let bytes = name.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(GraphSetProjectionError::InvalidName { column });
    }
    Ok(())
}

fn admit(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
    depth: usize,
    nodes: &mut usize,
    locals: usize,
) -> Result<GraphSetColumnType, GraphSetProjectionError> {
    use GraphSetProjectionError as Error;
    *nodes += 1;
    if depth > GraphValue::MAX_LIST_DEPTH || *nodes > GraphValue::MAX_LIST_NODES {
        return Err(Error::ExpressionBounds { column });
    }
    Ok(match value {
        GraphSetValue::Column(input) => *types.get(*input).ok_or(Error::UnknownInput {
            column,
            input: *input,
        })?,
        GraphSetValue::Literal(_) => GraphSetColumnType::Scalar,
        GraphSetValue::TypedNull(kind) => *kind,
        GraphSetValue::Integer(expression) => {
            for input in expression.scalar_columns() {
                let kind = types
                    .get(input)
                    .ok_or(Error::UnknownInput { column, input })?;
                if !kind.is_scalar_like() && *kind != GraphSetColumnType::Any {
                    return Err(Error::IntegerInput { column, input });
                }
            }
            // id()/elementId() read an element column's identity (fgdb-j687q).
            for input in expression.element_columns() {
                let kind = types
                    .get(input)
                    .ok_or(Error::UnknownInput { column, input })?;
                if !matches!(
                    kind,
                    GraphSetColumnType::Vertex | GraphSetColumnType::Edge | GraphSetColumnType::Any
                ) {
                    return Err(Error::IntegerInput { column, input });
                }
            }
            if let Some(input) = expression
                .referenced_locals()
                .find(|offset| *offset >= locals)
            {
                return Err(Error::UnknownInput { column, input });
            }
            GraphSetColumnType::Scalar
        }
        GraphSetValue::Value(value) => {
            if !value.validate_bounds() || value.canonical_byte_len().is_err() {
                return Err(Error::InvalidValue { column });
            }
            match value {
                GraphValue::Scalar(_) => GraphSetColumnType::Scalar,
                GraphValue::Vertex(_) => GraphSetColumnType::Vertex,
                GraphValue::Edge(_) => GraphSetColumnType::Edge,
                GraphValue::Path(_) => GraphSetColumnType::Path,
                GraphValue::Vertices(_) => GraphSetColumnType::Vertices,
                GraphValue::Edges(_) => GraphSetColumnType::Edges,
                GraphValue::List(_) => GraphSetColumnType::List,
                // A map is read by key at run time (fgdb-2jw3z).
                GraphValue::Map { .. } => GraphSetColumnType::Any,
            }
        }
        GraphSetValue::List(values) => {
            for value in values {
                admit(value, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::List
        }
        GraphSetValue::Index { list, index } => {
            admit_list(list, types, column, depth + 1, nodes, locals)?;
            let index = admit(index, types, column, depth + 1, nodes, locals)?;
            if !index.is_scalar_like() && index != GraphSetColumnType::Any {
                return Err(Error::ListInput { column });
            }
            GraphSetColumnType::Any
        }
        GraphSetValue::In { value, list } => {
            admit(value, types, column, depth + 1, nodes, locals)?;
            let kind = admit(list, types, column, depth + 1, nodes, locals)?;
            let nonnull_literal = match list.as_ref() {
                GraphSetValue::Literal(value) => !matches!(value.value(), CanonicalScalar::Null),
                GraphSetValue::Value(GraphValue::Scalar(value)) => {
                    !matches!(value, CanonicalScalar::Null)
                }
                _ => false,
            };
            // A scalar column can carry NULL. Every nonnull scalar RHS still
            // fails in the shared evaluator, never silently filters out a row.
            if nonnull_literal
                || !matches!(
                    kind,
                    GraphSetColumnType::List
                        | GraphSetColumnType::Any
                        | GraphSetColumnType::Scalar
                        | GraphSetColumnType::Stored
                        | GraphSetColumnType::Vertices
                        | GraphSetColumnType::Edges
                )
            {
                return Err(Error::ListInput { column });
            }
            GraphSetColumnType::Scalar
        }
        GraphSetValue::Equal { left, right } | GraphSetValue::Ordered { left, right, .. } => {
            admit(left, types, column, depth + 1, nodes, locals)?;
            admit(right, types, column, depth + 1, nodes, locals)?;
            GraphSetColumnType::Scalar
        }
        // A temporal value reads its fields like a map entry (`d.year`), so
        // it is dynamically typed for the operators after it.
        GraphSetValue::Temporal { argument, .. } => {
            admit(argument, types, column, depth + 1, nodes, locals)?;
            GraphSetColumnType::Any
        }
        // reverse() is a list or a text, and a sum with a list operand a list
        // or a scalar, so both are dynamically typed; split() is a list.
        GraphSetValue::Function { function, argument } => {
            admit(argument, types, column, depth + 1, nodes, locals)?;
            match function {
                GraphValueFunction::Reverse
                | GraphValueFunction::Add
                | GraphValueFunction::Subtract
                | GraphValueFunction::Coalesce
                | GraphValueFunction::PathNodes
                | GraphValueFunction::PathRelationships => GraphSetColumnType::Any,
                GraphValueFunction::Split => GraphSetColumnType::List,
                GraphValueFunction::PercentileDisc | GraphValueFunction::PercentileCont => {
                    GraphSetColumnType::Scalar
                }
            }
        }
        // openCypher size(): a list's length or a text's character count
        // (fgdb-xakp1). A literal that is neither refuses here; any other
        // scalar is a typed failure when evaluated.
        GraphSetValue::Size(value) => {
            let literal = matches!(value.as_ref(), GraphSetValue::Literal(scalar)
                if !matches!(scalar.value(), CanonicalScalar::Null | CanonicalScalar::Text(_)));
            if literal
                || !matches!(
                    admit(value, types, column, depth + 1, nodes, locals)?,
                    GraphSetColumnType::List
                        | GraphSetColumnType::Any
                        | GraphSetColumnType::Scalar
                        | GraphSetColumnType::Stored
                        | GraphSetColumnType::Vertices
                        | GraphSetColumnType::Edges
                )
            {
                return Err(Error::ListInput { column });
            }
            GraphSetColumnType::Scalar
        }
        // An element reference exists only inside its binding's scope.
        GraphSetValue::Local(offset) => {
            if *offset >= locals {
                return Err(Error::UnknownInput {
                    column,
                    input: *offset,
                });
            }
            GraphSetColumnType::Any
        }
        GraphSetValue::Comprehension { list, filter, map } => {
            admit_list(list, types, column, depth + 1, nodes, locals)?;
            if let Some(filter) = filter {
                admit(filter, types, column, depth + 1, nodes, locals + 1)?;
            }
            if let Some(map) = map {
                admit(map, types, column, depth + 1, nodes, locals + 1)?;
            }
            GraphSetColumnType::List
        }
        GraphSetValue::Quantifier {
            list, predicate, ..
        } => {
            admit_list(list, types, column, depth + 1, nodes, locals)?;
            admit(predicate, types, column, depth + 1, nodes, locals + 1)?;
            GraphSetColumnType::Scalar
        }
        GraphSetValue::Slice { list, from, to } => {
            admit_list(list, types, column, depth + 1, nodes, locals)?;
            for bound in [from, to].into_iter().flatten() {
                admit_scalar(bound, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::List
        }
        GraphSetValue::Range { start, end, step } => {
            for part in [Some(start), Some(end), step.as_ref()]
                .into_iter()
                .flatten()
            {
                admit_scalar(part, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::List
        }
        GraphSetValue::Reduce { init, list, expr } => {
            admit(init, types, column, depth + 1, nodes, locals)?;
            admit_list(list, types, column, depth + 1, nodes, locals)?;
            admit(expr, types, column, depth + 1, nodes, locals + 2)?;
            GraphSetColumnType::Any
        }
        GraphSetValue::MapLiteral {
            keys,
            values,
            guard,
        } => {
            if keys.len() != values.len()
                || keys
                    .windows(2)
                    .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
            {
                return Err(Error::InvalidValue { column });
            }
            if let Some(guard) = guard {
                admit(guard, types, column, depth + 1, nodes, locals)?;
            }
            for value in values {
                admit(value, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::Any
        }
        GraphSetValue::MapGet { map, .. } => {
            admit(map, types, column, depth + 1, nodes, locals)?;
            GraphSetColumnType::Any
        }
        GraphSetValue::Keys(map) => {
            admit(map, types, column, depth + 1, nodes, locals)?;
            GraphSetColumnType::List
        }
        GraphSetValue::MapOverlay { base, keys, values } => {
            if keys.len() != values.len()
                || keys
                    .windows(2)
                    .any(|pair| pair[0].as_bytes() >= pair[1].as_bytes())
            {
                return Err(Error::InvalidValue { column });
            }
            let kind = admit(base, types, column, depth + 1, nodes, locals)?;
            let nonnull_scalar = match base.as_ref() {
                GraphSetValue::Literal(value) => !matches!(value.value(), CanonicalScalar::Null),
                GraphSetValue::Value(GraphValue::Scalar(value)) => {
                    !matches!(value, CanonicalScalar::Null)
                }
                _ => false,
            };
            if nonnull_scalar || !(kind.is_scalar_like() || kind == GraphSetColumnType::Any) {
                return Err(Error::MapInput { column });
            }
            // NULL can skip execution, never binding or type admission.
            for value in values {
                admit(value, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::Any
        }
        GraphSetValue::Let { values, body } => {
            if values.is_empty() {
                return Err(Error::InvalidValue { column });
            }
            for value in values {
                admit(value, types, column, depth + 1, nodes, locals)?;
            }
            admit(body, types, column, depth + 1, nodes, locals + values.len())?
        }
        // Each arm's result may be a list, a map or a scalar.
        GraphSetValue::Case { arms, otherwise } => {
            if arms.is_empty() {
                return Err(Error::InvalidValue { column });
            }
            for (condition, value) in arms {
                admit(condition, types, column, depth + 1, nodes, locals)?;
                admit(value, types, column, depth + 1, nodes, locals)?;
            }
            if let Some(otherwise) = otherwise {
                admit(otherwise, types, column, depth + 1, nodes, locals)?;
            }
            GraphSetColumnType::Any
        }
        // The draw keys on exactly the element locals in force here.
        GraphSetValue::Random { scope, .. } => {
            if *scope as usize != locals {
                return Err(Error::InvalidValue { column });
            }
            GraphSetColumnType::Scalar
        }
    })
}

/// A value that must be a scalar (an integer bound or step) at run time.
fn admit_scalar(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
    depth: usize,
    nodes: &mut usize,
    locals: usize,
) -> Result<(), GraphSetProjectionError> {
    let kind = admit(value, types, column, depth, nodes, locals)?;
    if !kind.is_scalar_like() && kind != GraphSetColumnType::Any {
        return Err(GraphSetProjectionError::ListInput { column });
    }
    Ok(())
}

fn admit_list(
    value: &GraphSetValue,
    types: &[GraphSetColumnType],
    column: usize,
    depth: usize,
    nodes: &mut usize,
    locals: usize,
) -> Result<(), GraphSetProjectionError> {
    let kind = admit(value, types, column, depth, nodes, locals)?;
    let null = matches!(value, GraphSetValue::TypedNull(_))
        || matches!(value, GraphSetValue::Literal(v) if matches!(v.value(), CanonicalScalar::Null))
        || matches!(value, GraphSetValue::Value(v) if v.is_null());
    // A scalar column or expression can carry NULL, whose list operation is
    // NULL; any nonnull scalar fails in the shared evaluator, as for IN. A
    // nonnull scalar literal can never be a list, so it refuses here.
    let scalar_literal = matches!(value, GraphSetValue::Literal(_))
        || matches!(value, GraphSetValue::Value(GraphValue::Scalar(_)));
    let admitted = match kind {
        GraphSetColumnType::List
        | GraphSetColumnType::Any
        | GraphSetColumnType::Vertices
        | GraphSetColumnType::Edges => true,
        GraphSetColumnType::Scalar | GraphSetColumnType::Stored => !scalar_literal,
        _ => false,
    };
    if !admitted && !null {
        return Err(GraphSetProjectionError::ListInput { column });
    }
    Ok(())
}

pub(super) fn append_value_transcript(value: &GraphSetValue, bytes: &mut Vec<u8>) {
    match value {
        GraphSetValue::Column(input) => {
            bytes.push(0);
            bytes.extend_from_slice(&(*input as u64).to_be_bytes());
        }
        GraphSetValue::Literal(value) => {
            bytes.push(1);
            bytes.extend_from_slice(&(value.canonical_bytes().len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.canonical_bytes());
        }
        GraphSetValue::TypedNull(kind) => bytes.extend_from_slice(&[19, kind.tag()]),
        GraphSetValue::Integer(expression) => {
            bytes.push(2);
            let value = expression.canonical_bytes();
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&value);
        }
        GraphSetValue::List(values) => {
            bytes.push(3);
            bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                append_value_transcript(value, bytes);
            }
        }
        GraphSetValue::Index { list, index } => {
            bytes.push(4);
            append_value_transcript(list, bytes);
            append_value_transcript(index, bytes);
        }
        GraphSetValue::Size(list) => {
            bytes.push(5);
            append_value_transcript(list, bytes);
        }
        GraphSetValue::Value(value) => {
            bytes.push(6);
            let value = value
                .canonical_bytes()
                .expect("value encoding admitted during preparation");
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&value);
        }
        GraphSetValue::In { value, list } => {
            bytes.push(7);
            append_value_transcript(value, bytes);
            append_value_transcript(list, bytes);
        }
        GraphSetValue::Local(offset) => {
            bytes.push(8);
            bytes.extend_from_slice(&(*offset as u64).to_be_bytes());
        }
        GraphSetValue::Comprehension { list, filter, map } => {
            bytes.push(9);
            append_value_transcript(list, bytes);
            for part in [filter, map] {
                match part {
                    Some(part) => {
                        bytes.push(1);
                        append_value_transcript(part, bytes);
                    }
                    None => bytes.push(0),
                }
            }
        }
        GraphSetValue::Quantifier {
            kind,
            list,
            predicate,
        } => {
            bytes.extend_from_slice(&[10, kind.tag()]);
            append_value_transcript(list, bytes);
            append_value_transcript(predicate, bytes);
        }
        GraphSetValue::Slice { list, from, to } => {
            bytes.push(11);
            append_value_transcript(list, bytes);
            for bound in [from, to] {
                append_optional_transcript(bound.as_deref(), bytes);
            }
        }
        GraphSetValue::Range { start, end, step } => {
            bytes.push(12);
            append_value_transcript(start, bytes);
            append_value_transcript(end, bytes);
            append_optional_transcript(step.as_deref(), bytes);
        }
        GraphSetValue::Reduce { init, list, expr } => {
            bytes.push(13);
            append_value_transcript(init, bytes);
            append_value_transcript(list, bytes);
            append_value_transcript(expr, bytes);
        }
        GraphSetValue::MapLiteral {
            keys,
            values,
            guard,
        } => {
            // An unguarded literal keeps its original bytes.
            if let Some(guard) = guard {
                bytes.push(17);
                append_value_transcript(guard, bytes);
            } else {
                bytes.push(14);
            }
            bytes.extend_from_slice(&(keys.len() as u64).to_be_bytes());
            for (key, value) in keys.iter().zip(values) {
                bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
                bytes.extend_from_slice(key.as_bytes());
                append_value_transcript(value, bytes);
            }
        }
        GraphSetValue::MapGet { map, key, property } => {
            // An unbound read keeps its original bytes. A bound one names the
            // catalog answer, which decides what an element operand reads.
            bytes.push(if *property == GraphElementKey::Unbound {
                15
            } else {
                27
            });
            append_value_transcript(map, bytes);
            bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
            bytes.extend_from_slice(key.as_bytes());
            match property {
                GraphElementKey::Unbound => {}
                GraphElementKey::Missing => bytes.push(0),
                GraphElementKey::Property(PropertyKeyId(id)) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&id.to_be_bytes());
                }
            }
        }
        GraphSetValue::Keys(map) => {
            bytes.push(16);
            append_value_transcript(map, bytes);
        }
        GraphSetValue::MapOverlay { base, keys, values } => {
            bytes.push(18);
            append_value_transcript(base, bytes);
            bytes.extend_from_slice(&(keys.len() as u64).to_be_bytes());
            for (key, value) in keys.iter().zip(values) {
                bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
                bytes.extend_from_slice(key.as_bytes());
                append_value_transcript(value, bytes);
            }
        }
        GraphSetValue::Let { values, body } => {
            bytes.push(19);
            bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                append_value_transcript(value, bytes);
            }
            append_value_transcript(body, bytes);
        }
        GraphSetValue::Equal { left, right } => {
            bytes.push(20);
            append_value_transcript(left, bytes);
            append_value_transcript(right, bytes);
        }
        GraphSetValue::Temporal {
            function,
            argument,
            zones,
        } => {
            // A pinned tzdb is part of the plan: tag 26 and its identity.
            match zones {
                None => bytes.extend_from_slice(&[21, function.tag()]),
                Some(zones) => {
                    bytes.extend_from_slice(&[26, function.tag()]);
                    bytes.extend_from_slice(zones.tzdb().as_bytes());
                }
            }
            append_value_transcript(argument, bytes);
        }
        GraphSetValue::Function { function, argument } => {
            bytes.extend_from_slice(&[23, function.tag()]);
            append_value_transcript(argument, bytes);
        }
        GraphSetValue::Ordered {
            comparison,
            left,
            right,
        } => {
            bytes.extend_from_slice(&[22, comparison.tag()]);
            append_value_transcript(left, bytes);
            append_value_transcript(right, bytes);
        }
        GraphSetValue::Case { arms, otherwise } => {
            bytes.push(24);
            bytes.extend_from_slice(&(arms.len() as u64).to_be_bytes());
            for (condition, value) in arms {
                append_value_transcript(condition, bytes);
                append_value_transcript(value, bytes);
            }
            append_optional_transcript(otherwise.as_deref(), bytes);
        }
        GraphSetValue::Random { site, scope } => {
            bytes.push(25);
            bytes.extend_from_slice(&site.to_be_bytes());
            bytes.extend_from_slice(&scope.to_be_bytes());
        }
    }
}

fn append_optional_transcript(value: Option<&GraphSetValue>, bytes: &mut Vec<u8>) {
    match value {
        Some(value) => {
            bytes.push(1);
            append_value_transcript(value, bytes);
        }
        None => bytes.push(0),
    }
}

pub(super) fn append_transcript(projection: &[GraphSetProjection], bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&(projection.len() as u64).to_be_bytes());
    for column in projection {
        append_value_transcript(&column.value, bytes);
    }
}

#[derive(Debug)]
pub(crate) enum ProjectionFailure<E> {
    Control(E),
    Arithmetic {
        column: usize,
        error: GraphIntegerError,
    },
}

pub(super) fn copy_value<E>(
    value: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, E> {
    value.copy_with_control(control)
}

// The scalar leaf of GraphValue::copy_with_control, with the identical event
// sequence. Borrowing the scalar lets a cold host retain its record guard
// through admission instead of cloning an uncharged temporary GraphValue.
fn copy_property<E>(
    value: &CanonicalScalar,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, E> {
    control(GlaExecutionEvent::ScratchEntry)?;
    control(GlaExecutionEvent::Work)?;
    control(GlaExecutionEvent::ScratchEntry)?;
    let sizes = match value {
        CanonicalScalar::Text(text) => {
            [text.len(), text.canonical_sort_key().map_or(0, <[u8]>::len)]
        }
        CanonicalScalar::Bytes(bytes) => [bytes.as_slice().len(), 0],
        CanonicalScalar::Timestamp(timestamp) => [
            timestamp.zone().map_or(0, |zone| zone.identifier().len()),
            0,
        ],
        _ => [0, 0],
    };
    for bytes in sizes {
        for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    Ok(GraphValue::Scalar(value.clone()))
}

#[cfg(test)]
mod property_copy_tests {
    use super::*;

    #[test]
    fn borrowed_property_copies_preserve_scalar_events_and_every_refusal() {
        let mut values = vec![
            CanonicalScalar::Null,
            CanonicalScalar::Bool(true),
            CanonicalScalar::Int(i64::MIN),
        ];
        for size in [0, 1, 64, 65, 129, 513] {
            values.push(CanonicalScalar::bytes(vec![0xa5; size]).unwrap());
            values.push(CanonicalScalar::ucs_basic_text(&"x".repeat(size)).unwrap());
        }
        for value in values {
            let scalar = GraphValue::Scalar(value.clone());
            let mut original_events = Vec::new();
            let original = scalar
                .copy_with_control(&mut |event| {
                    original_events.push(event);
                    Ok::<_, &'static str>(())
                })
                .unwrap();
            let mut borrowed_events = Vec::new();
            let borrowed = copy_property(&value, &mut |event| {
                borrowed_events.push(event);
                Ok::<_, &'static str>(())
            })
            .unwrap();
            assert_eq!(borrowed, original);
            assert_eq!(borrowed_events, original_events);
            for stop in 1..=original_events.len() {
                let mut original_prefix = Vec::new();
                let original = scalar.copy_with_control(&mut |event| {
                    original_prefix.push(event);
                    if original_prefix.len() == stop {
                        Err("copy refused")
                    } else {
                        Ok(())
                    }
                });
                let mut borrowed_prefix = Vec::new();
                let borrowed = copy_property(&value, &mut |event| {
                    borrowed_prefix.push(event);
                    if borrowed_prefix.len() == stop {
                        Err("copy refused")
                    } else {
                        Ok(())
                    }
                });
                assert_eq!(original, Err("copy refused"));
                assert_eq!(borrowed, original);
                assert_eq!(original_prefix, original_events[..stop]);
                assert_eq!(borrowed_prefix, original_prefix);
            }
        }
    }
}

/// One evaluated UNWIND input, shared by resident, folded and external rows.
/// Evaluation owns the list once; each step moves one element and copies only
/// its original prefix. Consuming a step makes a control refusal terminal.
pub(super) struct UnwindRows {
    prefix: GraphValueRow,
    members: UnwindMembers,
}

/// What an UNWIND step moves next: an evaluated list's elements, or the
/// members of `range(start, end, step)`, generated in order (fgdb-85aal).
/// A range is never materialized, so the list-value bound does not cap the
/// rows it unwinds. Each member charges the scratch entry its list element
/// would have, and the query policy bounds the output.
enum UnwindMembers {
    List {
        values: std::vec::IntoIter<GraphValue>,
        capacity: usize,
    },
    Range {
        next: i128,
        step: i128,
        remaining: i128,
    },
}

impl UnwindRows {
    /// `read` is the host's element reader, as for a projection: an UNWIND
    /// list such as `[x IN items | x.p]` reads the properties of carried
    /// vertices and edges at the source's cut.
    pub(super) fn new<E>(
        row: GraphValueRow,
        value: &GraphSetValue,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        read: &mut ElementRead<'_, E>,
    ) -> Result<Self, ProjectionFailure<E>> {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        let column = row.len();
        let mut evaluation = Evaluation::new(None, read);
        if let GraphSetValue::Range { start, end, step } = value {
            // The range call's own node charge, as evaluate_value_at makes
            // it, and then the same bounds its Range arm reads.
            control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
            evaluation.nodes += 1;
            let bounds = synchronous::range_bounds(
                start,
                end,
                step.as_deref(),
                &row,
                column,
                control,
                0,
                &mut evaluation,
                &|_| Ok::<(), E>(()),
            )?;
            return Self::from_range(row, bounds, control);
        }
        let list = evaluate_value_at(value, &row, column, control, 0, &mut evaluation)?;
        Self::from_evaluated(row, list, control)
    }

    /// UNWIND over range() from its evaluated bounds (range_bounds). A NULL
    /// bound makes range() NULL, which unwinds nothing; otherwise a zero step
    /// is the typed error range() itself raises.
    pub(super) fn from_range<E>(
        row: GraphValueRow,
        bounds: Option<(i64, i64, i64)>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, ProjectionFailure<E>> {
        let Some((start, end, step)) = bounds else {
            // The scratch entry the NULL result of range() reserves.
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            return Ok(Self {
                prefix: row,
                members: UnwindMembers::List {
                    values: Vec::new().into_iter(),
                    capacity: 0,
                },
            });
        };
        let remaining =
            range_member_count(start, end, step).map_err(|kind| ProjectionFailure::Arithmetic {
                column: row.len(),
                error: GraphIntegerError {
                    instruction: 0,
                    kind,
                },
            })?;
        Ok(Self {
            prefix: row,
            members: UnwindMembers::Range {
                next: i128::from(start),
                step: i128::from(step),
                remaining,
            },
        })
    }

    pub(super) fn from_evaluated<E>(
        row: GraphValueRow,
        list: GraphValue,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, ProjectionFailure<E>> {
        let column = row.len();
        let values = match list {
            GraphValue::List(values) => values.into_vec(),
            value if value.is_null() => Vec::new(),
            // nodes(p), relationships(p) and a variable-length relationship
            // variable are compact element lists (fgdb-xnd7x): each member
            // unwinds as its Vertex or Edge value, one scratch entry each.
            value @ (GraphValue::Vertices(_) | GraphValue::Edges(_)) => {
                list_members(&value, control)
                    .map_err(ProjectionFailure::Control)?
                    .map(std::borrow::Cow::into_owned)
                    .unwrap_or_default()
            }
            _ => {
                return Err(ProjectionFailure::Arithmetic {
                    column,
                    error: GraphIntegerError {
                        instruction: 0,
                        kind: crate::GraphIntegerErrorKind::IncompatibleOperands,
                    },
                });
            }
        };
        let capacity = values.capacity();
        Ok(Self {
            prefix: row,
            members: UnwindMembers::List {
                values: values.into_iter(),
                capacity,
            },
        })
    }

    pub(super) fn visit_retained_values<E>(
        &self,
        visitor: &mut impl FnMut(&[GraphValue], usize) -> Result<(), E>,
    ) -> Result<(), E> {
        visitor(self.prefix.values(), self.prefix.len())?;
        match &self.members {
            // IntoIter keeps its original allocation after moving elements out.
            UnwindMembers::List { values, capacity } => visitor(values.as_slice(), *capacity),
            // A range retains no member: each is generated as it moves.
            UnwindMembers::Range { .. } => visitor(&[], 0),
        }
    }

    /// State allocations remain owned by the caller through the last emitted
    /// row's use. This control charges only the next output's new allocations;
    /// the moved element keeps the reservation made during list evaluation.
    pub(super) fn next_with_control<E>(
        mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(GraphValueRow, Self)>, E> {
        let value = match &mut self.members {
            UnwindMembers::List { values, .. } => match values.next() {
                Some(value) => value,
                None => return Ok(None),
            },
            UnwindMembers::Range {
                next,
                step,
                remaining,
            } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                // The reservation range_values makes for a listed member.
                control(GlaExecutionEvent::ScratchEntry)?;
                let member = i64::try_from(*next).expect("a range member lies within its bounds");
                *next += *step;
                *remaining -= 1;
                GraphValue::Scalar(CanonicalScalar::Int(member))
            }
        };
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut cells = Vec::new();
        for cell in self.prefix.values() {
            cells.push(copy_value(cell, control)?);
        }
        // The evaluated element already owns its metered payload; only its
        // newly retained output cell needs another reservation.
        control(GlaExecutionEvent::ScratchEntry)?;
        cells.push(value);
        Ok(Some((GraphValueRow::from_owned_values(cells), self)))
    }
}

/// The input is a private owned row whose complete schema was admitted by the
/// enclosing set executor. Every output cell and variable payload is reserved
/// before growth. A refusal drops the whole private row, never a short result.
/// Arithmetic is not elided for DISTINCT duplicates or a final LIMIT 0.
pub(super) fn evaluate<E>(
    row: &GraphValueRow,
    projection: &[GraphSetProjection],
    ordinal: Option<u64>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    read: &mut ElementRead<'_, E>,
) -> Result<GraphValueRow, ProjectionFailure<E>> {
    let mut control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E> = control;
    let control = &mut control;
    let mut values = Vec::new();
    for (column, output) in projection.iter().enumerate() {
        let value = evaluate_value_at(
            &output.value,
            row,
            column,
            control,
            0,
            &mut Evaluation::new(ordinal, read),
        )?;
        values.push(value);
    }
    Ok(GraphValueRow::from_owned_values(values))
}

pub(crate) fn evaluate_value<E>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    evaluate_value_reading(value, row, column, control, &mut no_element_reads)
}

/// `evaluate_value` with the host's element property reader (fgdb-xnd7x):
/// a mutation's right-hand side reads `x.key` of an element it computes at
/// the statement's frozen cut (fgdb-694ga).
pub(crate) fn evaluate_value_reading<E>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    read: &mut ElementRead<'_, E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    let mut control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E> = control;
    let control = &mut control;
    evaluate_value_at(
        value,
        row,
        column,
        control,
        0,
        &mut Evaluation::new(None, read),
    )
}

/// Reads one stored property of a vertex or edge value at the executing
/// source's cut (fgdb-xnd7x). `Ok(None)` is an absent property. `None` means
/// this host reads no element properties, so the read stays a typed error,
/// never a guessed NULL.
pub(crate) type ElementRead<'r, E> = dyn FnMut(ElementId, PropertyKeyId) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>>
    + 'r;

/// The element reader of a host that reads no element properties.
pub(crate) fn no_element_reads<E>(
    _: ElementId,
    _: PropertyKeyId,
) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
    None
}

/// One value evaluation's running state: the node count that bounds it, the
/// row's ordinal when its caller numbers the rows it projects, which keys
/// rand() (fgdb-13l85; without an ordinal, rand() is a typed error), and the
/// host's element property reader (fgdb-xnd7x).
struct EvaluationState<'a, R: ?Sized> {
    nodes: usize,
    ordinal: Option<u64>,
    read: &'a mut R,
}

type Evaluation<'a, 'r, E> = EvaluationState<'a, ElementRead<'r, E>>;

impl<'a, R: ?Sized> EvaluationState<'a, R> {
    fn new(ordinal: Option<u64>, read: &'a mut R) -> Self {
        Self {
            nodes: 0,
            ordinal,
            read,
        }
    }

    /// A fresh node bound for one element's own evaluation, on the same row
    /// and with the same reader.
    fn element(&mut self) -> EvaluationState<'_, R> {
        EvaluationState::new(self.ordinal, &mut *self.read)
    }
}

/// The members of a list operand. A list lends its own values. A compact node
/// or relationship list (`nodes(p)`, `relationships(p)`, a variable-length
/// relationship variable) reads as its Vertex or Edge values, one scratch
/// entry each. Any other value has no members.
fn list_members<'a, E>(
    value: &'a GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<std::borrow::Cow<'a, [GraphValue]>>, E> {
    let mut members = Vec::new();
    match value {
        GraphValue::List(values) => return Ok(Some(std::borrow::Cow::Borrowed(&values[..]))),
        GraphValue::Vertices(ids) => {
            for id in ids.iter() {
                control(GlaExecutionEvent::ScratchEntry)?;
                members.push(GraphValue::Vertex(*id));
            }
        }
        GraphValue::Edges(ids) => {
            for id in ids.iter() {
                control(GlaExecutionEvent::ScratchEntry)?;
                members.push(GraphValue::Edge(*id));
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(std::borrow::Cow::Owned(members)))
}

trait SynchronousElementSource {
    type Error;
    type Property: Borrow<fgdb_types::CanonicalPropertyValue>;

    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<Self::Property>, Self::Error>>;
}

impl<E, F: ?Sized> SynchronousElementSource for F
where
    F: FnMut(
        ElementId,
        PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>>,
{
    type Error = E;
    type Property = fgdb_types::CanonicalPropertyValue;

    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, E>> {
        self(element, key)
    }
}

// One evaluator body owns bounds, operand order, short-circuiting, native
// events, canonical copies and errors. Only its effect calls differ. Ordinary
// execution keeps plain recursion; async helpers live in the active frame.
macro_rules! projection_await {
    (synchronous, $call:expr) => {
        $call
    };
    (asynchronous, $call:expr) => {
        $call.await
    };
}

macro_rules! projection_recurse {
    (synchronous, $reserve:expr, $call:expr) => {
        $call
    };
    (asynchronous, $reserve:expr, $call:expr) => {{
        let mut frame = reserve_future($call, $reserve)?;
        let result = frame.0.as_mut().await;
        drop(frame);
        result
    }};
}

// Keep allocation outside the recursive async function's concrete type. The
// exact grant precedes Box::pin, and tuple order drops the future before the
// grant both on normal return and when a suspended evaluation is cancelled.
fn reserve_future<F, M, G, E>(
    future: F,
    reserve: &M,
) -> Result<(std::pin::Pin<Box<F>>, G), ProjectionFailure<E>>
where
    M: Fn(usize) -> Result<G, E>,
{
    let grant = reserve(core::mem::size_of::<F>()).map_err(ProjectionFailure::Control)?;
    Ok((Box::pin(future), grant))
}

macro_rules! projection_evaluator {
    ($mode:ident, [$($asyncness:tt)*], $reader:ident, [$($send:tt)*], [$($sync:tt)*]) => {
mod $mode {
use super::*;

$($asyncness)* fn operand<'a, R, C, M, G>(
    value: &'a GraphSetValue,
    row: &'a GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    nodes: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<std::borrow::Cow<'a, GraphValue>, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    let borrowed = match value {
        GraphSetValue::Column(input) => row.values().get(*input),
        GraphSetValue::Value(value) => Some(value),
        _ => None,
    };
    if let Some(value) = borrowed {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        Ok(std::borrow::Cow::Borrowed(value))
    } else {
        projection_recurse!(
            $mode,
            reserve,
            evaluate_value_at(value, row, column, control, depth, nodes, reserve)
        )
        .map(std::borrow::Cow::Owned)
    }
}

pub(super) $($asyncness)* fn evaluate_value_at<R, C, M, G>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    nodes: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<GraphValue, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    use crate::GraphIntegerErrorKind;
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
    nodes.nodes += 1;
    if depth > GraphValue::MAX_LIST_DEPTH || nodes.nodes > GraphValue::MAX_LIST_NODES {
        return Err(failure(GraphIntegerErrorKind::Overflow));
    }
    let result = match value {
        GraphSetValue::Column(input) => {
            let value = row
                .values()
                .get(*input)
                .ok_or_else(|| failure(GraphIntegerErrorKind::MissingColumn))?;
            copy_value(value, control).map_err(ProjectionFailure::Control)?
        }
        GraphSetValue::Literal(value) => {
            // Borrow the checked scalar while reserving its eventual copy.
            // The temporary GraphValue owns a clone, so use the shared row
            // copier only after the payload reservations below instead.
            let scalar = value.value();
            let sizes = match scalar {
                CanonicalScalar::Text(value) => [
                    value.len(),
                    value.canonical_sort_key().map_or(0, <[u8]>::len),
                ],
                CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
                CanonicalScalar::Timestamp(value) => {
                    [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
                }
                _ => [0, 0],
            };
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            for bytes in sizes {
                for _ in 0..bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                }
            }
            GraphValue::Scalar(scalar.clone())
        }
        GraphSetValue::TypedNull(_) => {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(CanonicalScalar::Null)
        }
        GraphSetValue::Integer(expression) => {
            let value = expression
                .evaluate_scalar_with_control(row.values(), control)
                .map_err(|error| match error {
                    GraphIntegerEvaluationError::Control(error) => {
                        ProjectionFailure::Control(error)
                    }
                    GraphIntegerEvaluationError::Value(error) => {
                        ProjectionFailure::Arithmetic { column, error }
                    }
                })?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(value)
        }
        GraphSetValue::Value(value) => {
            copy_value(value, control).map_err(ProjectionFailure::Control)?
        }
        GraphSetValue::List(expressions) => {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            let mut values = Vec::new();
            for expression in expressions {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                values.push(projection_recurse!(
                    $mode,
                    reserve,
                    evaluate_value_at(expression, row, column, control, depth + 1, nodes, reserve)
                )?);
            }
            GraphValue::List(values.into_boxed_slice())
        }
        GraphSetValue::Index { list, index } => {
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            let index = projection_recurse!(
                $mode,
                reserve,
                evaluate_value_at(index, row, column, control, depth + 1, nodes, reserve)
            )?;
            if list.is_null() || index.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else if let Some((keys, values)) = list.as_map() {
                // openCypher dynamic map access, `map[key]`: a text key reads
                // its entry, case-sensitively, and an absent key is NULL. Any
                // other key is an error, as for a list.
                let GraphValue::Scalar(CanonicalScalar::Text(key)) = &index else {
                    return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
                };
                let at =
                    GraphSetValue::map_subscript_index(keys, values.len(), key.as_str(), control)
                        .map_err(|error| match error {
                        GraphIntegerEvaluationError::Control(error) => {
                            ProjectionFailure::Control(error)
                        }
                        GraphIntegerEvaluationError::Value(error) => failure(error.kind),
                    })?;
                match at {
                    Some(at) => {
                        copy_value(&values[at], control).map_err(ProjectionFailure::Control)?
                    }
                    None => {
                        control(GlaExecutionEvent::ScratchEntry)
                            .map_err(ProjectionFailure::Control)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                }
            } else {
                let values = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let GraphValue::Scalar(CanonicalScalar::Int(index)) = index else {
                    return Err(failure(GraphIntegerErrorKind::NonInteger));
                };
                let at = if index < 0 {
                    (values.len() as i128) + i128::from(index)
                } else {
                    i128::from(index)
                };
                match usize::try_from(at).ok().and_then(|at| values.get(at)) {
                    Some(value) => {
                        copy_value(value, control).map_err(ProjectionFailure::Control)?
                    }
                    None => {
                        control(GlaExecutionEvent::ScratchEntry)
                            .map_err(ProjectionFailure::Control)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                }
            }
        }
        GraphSetValue::In { value, list } => {
            let value = projection_await!(
                $mode,
                operand(value, row, column, control, depth + 1, nodes, reserve)
            )?;
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            let truth = if list.is_null() {
                None
            } else {
                let members = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                membership::evaluate(value.as_ref(), &members, control)
                    .map_err(ProjectionFailure::Control)?
            };
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
        }
        GraphSetValue::Equal { left, right } => {
            let left = projection_await!(
                $mode,
                operand(left, row, column, control, depth + 1, nodes, reserve)
            )?;
            let right = projection_await!(
                $mode,
                operand(right, row, column, control, depth + 1, nodes, reserve)
            )?;
            let truth = membership::equal(left.as_ref(), right.as_ref(), control)
                .map_err(ProjectionFailure::Control)?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
        }
        GraphSetValue::Ordered {
            comparison,
            left,
            right,
        } => {
            let left = projection_await!(
                $mode,
                operand(left, row, column, control, depth + 1, nodes, reserve)
            )?;
            let right = projection_await!(
                $mode,
                operand(right, row, column, control, depth + 1, nodes, reserve)
            )?;
            let truth = membership::order(*comparison, left.as_ref(), right.as_ref(), control)
                .map_err(ProjectionFailure::Control)?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
        }
        GraphSetValue::Temporal {
            function,
            argument,
            zones,
        } => {
            let argument = projection_await!(
                $mode,
                operand(argument, row, column, control, depth + 1, nodes, reserve)
            )?;
            let value = temporal::construct(*function, argument.as_ref(), zones.as_ref())
                .map_err(failure)?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(value)
        }
        GraphSetValue::Function {
            function: GraphValueFunction::Coalesce,
            argument,
        } => projection_await!(
            $mode,
            coalesce_value(argument, row, column, control, depth, nodes, reserve)
        )?,
        GraphSetValue::Function { function, argument } => {
            let argument = projection_await!(
                $mode,
                operand(argument, row, column, control, depth + 1, nodes, reserve)
            )?;
            value_function(*function, argument.as_ref(), column, control)?
        }
        GraphSetValue::Size(list) => {
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            let size = if list.is_null() {
                None
            } else if let Some(values) = list.as_list() {
                Some(values.len())
            } else if let GraphValue::Vertices(ids) = list.as_ref() {
                Some(ids.len())
            } else if let GraphValue::Edges(ids) = list.as_ref() {
                Some(ids.len())
            } else if let GraphValue::Scalar(CanonicalScalar::Text(text)) = list.as_ref() {
                // CHAR_LENGTH's charge: one unit of work per payload unit read.
                let text = text.as_str();
                for _ in 0..text.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
                }
                Some(text.chars().count())
            } else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            let scalar = match size {
                None => CanonicalScalar::Null,
                Some(size) => CanonicalScalar::Int(
                    i64::try_from(size).map_err(|_| failure(GraphIntegerErrorKind::Overflow))?,
                ),
            };
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(scalar)
        }
        GraphSetValue::Local(offset) => {
            let value = row
                .len()
                .checked_sub(offset + 1)
                .and_then(|at| row.values().get(at))
                .ok_or_else(|| failure(GraphIntegerErrorKind::MissingColumn))?;
            copy_value(value, control).map_err(ProjectionFailure::Control)?
        }
        GraphSetValue::Comprehension { list, filter, map } => {
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            if list.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else {
                let members = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let mut scope = element_scope(row, 1, control)?;
                let mut kept = Vec::new();
                for member in members.iter() {
                    scope.set_from_end(
                        0,
                        copy_value(member, control).map_err(ProjectionFailure::Control)?,
                    );
                    if let Some(filter) = filter
                        && !projection_await!(
                            $mode,
                            element_truth(filter, &scope, column, control, depth, nodes, reserve)
                        )?
                            .unwrap_or(false)
                    {
                        continue;
                    }
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    kept.push(match map {
                        // Each element evaluation is bounded like a whole
                        // expression; total work is charged through control.
                        Some(map) => {
                            let mut element = nodes.element();
                            projection_recurse!(
                                $mode,
                                reserve,
                                evaluate_value_at(
                                    map, &scope, column, control, depth + 1, &mut element, reserve
                                )
                            )?
                        }
                        None => copy_value(member, control).map_err(ProjectionFailure::Control)?,
                    });
                }
                let value = GraphValue::List(kept.into_boxed_slice());
                if !value.validate_bounds() {
                    return Err(failure(GraphIntegerErrorKind::Overflow));
                }
                value
            }
        }
        GraphSetValue::Quantifier {
            kind,
            list,
            predicate,
        } => {
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            let truth = if list.is_null() {
                None
            } else {
                let members = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let mut scope = element_scope(row, 1, control)?;
                let mut results = Vec::new();
                for member in members.iter() {
                    scope.set_from_end(
                        0,
                        copy_value(member, control).map_err(ProjectionFailure::Control)?,
                    );
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    results.push(projection_await!(
                        $mode,
                        element_truth(predicate, &scope, column, control, depth, nodes, reserve)
                    )?);
                }
                kind.combine(results)
            };
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(truth.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
        }
        GraphSetValue::Slice { list, from, to } => {
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            let mut bounds = [None, None];
            let mut null = list.is_null();
            for (slot, bound) in bounds.iter_mut().zip([from, to]) {
                if let Some(bound) = bound {
                    match projection_await!(
                        $mode,
                        integer_part(bound, row, column, control, depth, nodes, reserve)
                    )? {
                        Some(value) => *slot = Some(i128::from(value)),
                        None => null = true,
                    }
                }
            }
            if null {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else {
                let members = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let mut kept = Vec::new();
                for member in &members[slice_bounds(members.len(), bounds[0], bounds[1])] {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    kept.push(copy_value(member, control).map_err(ProjectionFailure::Control)?);
                }
                GraphValue::List(kept.into_boxed_slice())
            }
        }
        GraphSetValue::Range { start, end, step } => {
            let bounds = projection_await!(
                $mode,
                range_bounds(start, end, step.as_deref(), row, column, control, depth, nodes, reserve)
            )?;
            match bounds {
                Some((start, end, step)) => {
                    let members =
                        range_values(start, end, step, control).map_err(|error| match error {
                            GraphIntegerEvaluationError::Control(error) => {
                                ProjectionFailure::Control(error)
                            }
                            GraphIntegerEvaluationError::Value(error) => failure(error.kind),
                        })?;
                    GraphValue::List(members.into_boxed_slice())
                }
                None => {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    GraphValue::Scalar(CanonicalScalar::Null)
                }
            }
        }
        GraphSetValue::Reduce { init, list, expr } => {
            let mut accumulator = projection_recurse!(
                $mode,
                reserve,
                evaluate_value_at(init, row, column, control, depth + 1, nodes, reserve)
            )?;
            let list = projection_await!(
                $mode,
                operand(list, row, column, control, depth + 1, nodes, reserve)
            )?;
            if list.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else {
                let members = list_members(&list, control)
                    .map_err(ProjectionFailure::Control)?
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                let mut scope = element_scope(row, 2, control)?;
                for member in members.iter() {
                    scope.set_from_end(1, accumulator);
                    scope.set_from_end(
                        0,
                        copy_value(member, control).map_err(ProjectionFailure::Control)?,
                    );
                    let mut element = nodes.element();
                    accumulator = projection_recurse!(
                        $mode,
                        reserve,
                        evaluate_value_at(
                            expr, &scope, column, control, depth + 1, &mut element, reserve
                        )
                    )?;
                }
                accumulator
            }
        }
        GraphSetValue::MapLiteral {
            keys,
            values,
            guard,
        } => {
            if let Some(guard) = guard
                && projection_await!(
                    $mode,
                    operand(guard, row, column, control, depth + 1, nodes, reserve)
                )?.is_null()
            {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                return Ok(GraphValue::Scalar(CanonicalScalar::Null));
            }
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            let mut entries = Vec::new();
            for value in values {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                entries.push(projection_recurse!(
                    $mode,
                    reserve,
                    evaluate_value_at(value, row, column, control, depth + 1, nodes, reserve)
                )?);
            }
            for key in keys.iter() {
                for _ in 0..key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                }
            }
            // Keys were sorted and deduplicated at preparation.
            let value = GraphValue::Map {
                keys: keys.clone(),
                values: entries.into_boxed_slice(),
            };
            if !value.validate_bounds() {
                return Err(failure(GraphIntegerErrorKind::Overflow));
            }
            value
        }
        GraphSetValue::MapGet { map, key, property } => {
            let map = projection_await!(
                $mode,
                operand(map, row, column, control, depth + 1, nodes, reserve)
            )?;
            let element = match map.as_ref() {
                GraphValue::Vertex(vid) => Some(ElementId::Vertex(*vid)),
                GraphValue::Edge(eid) => Some(ElementId::Edge(*eid)),
                _ => None,
            };
            if map.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else if let Some(element) = element {
                // A vertex or edge reads its stored property at the source's
                // cut (fgdb-xnd7x). The read is charged as work and its value
                // as a copy. A host without a reader keeps the typed error.
                let value = match property {
                    GraphElementKey::Unbound => None,
                    GraphElementKey::Missing => Some(Ok(None)),
                    GraphElementKey::Property(key) => {
                        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
                        projection_await!($mode, nodes.read.element_property(element, *key))
                    }
                };
                match value
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?
                    .map_err(ProjectionFailure::Control)?
                {
                    Some(value) => match value.borrow() {
                        // A stored scalar keeps the borrowed copy's charges.
                        fgdb_types::CanonicalPropertyValue::Scalar(scalar) => {
                            copy_property(scalar, control).map_err(ProjectionFailure::Control)?
                        }
                        // A stored List or Map reads as the value it is
                        // (fgdb-k3nub); copy_value charges every node.
                        stored => copy_value(
                            &GraphValue::from_property_value(stored).ok_or_else(|| {
                                failure(GraphIntegerErrorKind::StoredValueBounds)
                            })?,
                            control,
                        )
                        .map_err(ProjectionFailure::Control)?,
                    },
                    None => {
                        control(GlaExecutionEvent::ScratchEntry)
                            .map_err(ProjectionFailure::Control)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                }
            } else if let GraphValue::Scalar(scalar) = map.as_ref()
                && let Some(field) = temporal::field(scalar, key)
            {
                // A temporal value's fields (fgdb-txbg9); any other key is an
                // error. A text field charges its payload too.
                let field = field.map_err(failure)?;
                for _ in 0..temporal::field_scratch_entries(&field) {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                }
                GraphValue::Scalar(field)
            } else {
                let (keys, values) = map
                    .as_map()
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                match keys.binary_search_by(|probe| probe.as_bytes().cmp(key.as_bytes())) {
                    Ok(at) => {
                        copy_value(&values[at], control).map_err(ProjectionFailure::Control)?
                    }
                    Err(_) => {
                        control(GlaExecutionEvent::ScratchEntry)
                            .map_err(ProjectionFailure::Control)?;
                        GraphValue::Scalar(CanonicalScalar::Null)
                    }
                }
            }
        }
        GraphSetValue::Keys(map) => {
            let map = projection_await!(
                $mode,
                operand(map, row, column, control, depth + 1, nodes, reserve)
            )?;
            if map.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                GraphValue::Scalar(CanonicalScalar::Null)
            } else {
                let (keys, _) = map
                    .as_map()
                    .ok_or_else(|| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
                GraphValue::List(map_keys(keys, control).map_err(|error| match error {
                    Some(error) => ProjectionFailure::Control(error),
                    None => failure(GraphIntegerErrorKind::TextConstruction),
                })?)
            }
        }
        GraphSetValue::MapOverlay { base, keys, values } => {
            let base = projection_await!(
                $mode,
                operand(base, row, column, control, depth + 1, nodes, reserve)
            )?;
            if base.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                return Ok(GraphValue::Scalar(CanonicalScalar::Null));
            }
            if base.as_map().is_none() {
                return Err(failure(GraphIntegerErrorKind::NonMap));
            }
            let mut entries = Vec::new();
            for value in values {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                entries.push(projection_recurse!(
                    $mode,
                    reserve,
                    evaluate_value_at(value, row, column, control, depth + 1, nodes, reserve)
                )?);
            }
            overlay_map(&base, keys, entries, column, control)?
        }
        GraphSetValue::Let { values, body } => {
            // Every value reads the enclosing row, never another slot.
            let mut computed = Vec::with_capacity(values.len());
            for value in values {
                computed.push(projection_recurse!(
                    $mode,
                    reserve,
                    evaluate_value_at(value, row, column, control, depth + 1, nodes, reserve)
                )?);
            }
            let mut scope = element_scope(row, computed.len(), control)?;
            for (slot, value) in computed.into_iter().rev().enumerate() {
                scope.set_from_end(slot, value);
            }
            projection_recurse!(
                $mode,
                reserve,
                evaluate_value_at(body, &scope, column, control, depth + 1, nodes, reserve)
            )?
        }
        GraphSetValue::Case { arms, otherwise } => {
            let mut selected = otherwise.as_deref();
            for (condition, value) in arms {
                let truth =
                    match projection_recurse!(
                        $mode,
                        reserve,
                        evaluate_value_at(condition, row, column, control, depth + 1, nodes, reserve)
                    )? {
                        GraphValue::Scalar(CanonicalScalar::Bool(truth)) => truth,
                        GraphValue::Scalar(CanonicalScalar::Null) => false,
                        _ => return Err(failure(GraphIntegerErrorKind::NonBoolean)),
                    };
                if truth {
                    selected = Some(value);
                    break;
                }
            }
            match selected {
                Some(value) => projection_recurse!(
                    $mode,
                    reserve,
                    evaluate_value_at(value, row, column, control, depth + 1, nodes, reserve)
                )?,
                None => {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    GraphValue::Scalar(CanonicalScalar::Null)
                }
            }
        }
        GraphSetValue::Random { site, scope } => {
            let ordinal = nodes
                .ordinal
                .ok_or_else(|| failure(GraphIntegerErrorKind::MissingColumn))?;
            let elements = row
                .values()
                .len()
                .checked_sub(*scope as usize)
                .map(|start| &row.values()[start..])
                .ok_or_else(|| failure(GraphIntegerErrorKind::MissingColumn))?;
            let draw = random_draw(*site, ordinal, elements, column, control)?;
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(draw)))
        }
    };
    Ok(result)
}

/// openCypher coalesce over a composite argument: each argument in order
/// until one is not NULL, which is the answer; no later argument runs. A call
/// with one argument holds it directly, more hold the list of them.
$($asyncness)* fn coalesce_value<R, C, M, G>(
    arguments: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    nodes: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<GraphValue, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    let arguments = match arguments {
        GraphSetValue::List(arguments) => arguments.as_slice(),
        single => core::slice::from_ref(single),
    };
    for argument in arguments {
        let value = projection_await!(
            $mode,
            operand(argument, row, column, control, depth + 1, nodes, reserve)
        )?;
        if !value.is_null() {
            return match value {
                std::borrow::Cow::Owned(value) => Ok(value),
                std::borrow::Cow::Borrowed(value) => {
                    copy_value(value, control).map_err(ProjectionFailure::Control)
                }
            };
        }
    }
    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
    Ok(GraphValue::Scalar(CanonicalScalar::Null))
}

/// range()'s bounds over `row`, in order: Some((start, end, step)) when none
/// is NULL, the step 1 when absent. `depth` is the range call's own depth.
/// UNWIND reads these to stream the members it would otherwise materialize
/// (fgdb-85aal).
#[allow(clippy::too_many_arguments)]
pub(super) $($asyncness)* fn range_bounds<R, C, M, G>(
    start: &GraphSetValue,
    end: &GraphSetValue,
    step: Option<&GraphSetValue>,
    row: &GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    nodes: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<Option<(i64, i64, i64)>, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    let start = projection_await!(
        $mode,
        integer_part(start, row, column, control, depth, nodes, reserve)
    )?;
    let end = projection_await!(
        $mode,
        integer_part(end, row, column, control, depth, nodes, reserve)
    )?;
    let step = match step {
        Some(step) => projection_await!(
            $mode,
            integer_part(step, row, column, control, depth, nodes, reserve)
        )?,
        None => Some(1),
    };
    Ok(match (start, end, step) {
        (Some(start), Some(end), Some(step)) => Some((start, end, step)),
        _ => None,
    })
}

/// An integer bound, step or range endpoint: Some(value), None for NULL, and
/// any other value a typed NonInteger error.
$($asyncness)* fn integer_part<R, C, M, G>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    nodes: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<Option<i64>, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    match projection_recurse!(
        $mode,
        reserve,
        evaluate_value_at(value, row, column, control, depth + 1, nodes, reserve)
    )? {
        GraphValue::Scalar(CanonicalScalar::Int(value)) => Ok(Some(value)),
        GraphValue::Scalar(CanonicalScalar::Null) => Ok(None),
        _ => Err(ProjectionFailure::Arithmetic {
            column,
            error: GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::NonInteger,
            },
        }),
    }
}

/// A per-element predicate's three-valued result: TRUE, FALSE or UNKNOWN
/// (NULL). Any other value is a typed expression error, never a filter.
$($asyncness)* fn element_truth<R, C, M, G>(
    predicate: &GraphSetValue,
    scope: &GraphValueRow,
    column: usize,
    control: &mut C,
    depth: usize,
    outer: &mut EvaluationState<'_, R>,
    reserve: &M,
) -> Result<Option<bool>, ProjectionFailure<R::Error>>
where
    R: $reader + ?Sized,
    C: FnMut(GlaExecutionEvent) -> Result<(), R::Error> $($send)*,
    M: Fn(usize) -> Result<G, R::Error> $($sync)*,
    G: Sized $($send)*,
{
    let mut element = outer.element();
    match projection_recurse!(
        $mode,
        reserve,
        evaluate_value_at(predicate, scope, column, control, depth + 1, &mut element, reserve)
    )? {
        GraphValue::Scalar(CanonicalScalar::Bool(truth)) => Ok(Some(truth)),
        GraphValue::Scalar(CanonicalScalar::Null) => Ok(None),
        _ => Err(ProjectionFailure::Arithmetic {
            column,
            error: GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::NonBoolean,
            },
        }),
    }
}
}
    };
}

projection_evaluator!(synchronous, [], SynchronousElementSource, [], []);
projection_evaluator!(asynchronous, [async], AsyncSpillElementSource, [+ Send], [+ Sync]);

fn evaluate_value_at<E>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    depth: usize,
    nodes: &mut Evaluation<'_, '_, E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    synchronous::evaluate_value_at(value, row, column, control, depth, nodes, &|_| {
        Ok::<(), E>(())
    })
}

pub(super) async fn evaluate_async<E, S, M, G>(
    row: &GraphValueRow,
    projection: &[GraphSetProjection],
    ordinal: Option<u64>,
    control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), E> + Send),
    read: &mut S,
    reserve: &M,
) -> Result<GraphValueRow, ProjectionFailure<E>>
where
    E: Send,
    S: AsyncSpillElementSource<Error = E>,
    M: Fn(usize) -> Result<G, E> + Sync,
    G: Send,
{
    let mut values = Vec::new();
    for (column, output) in projection.iter().enumerate() {
        values.push(
            evaluate_value_async(&output.value, row, column, ordinal, control, read, reserve)
                .await?,
        );
    }
    Ok(GraphValueRow::from_owned_values(values))
}

pub(super) async fn evaluate_value_async<E, S, M, G>(
    value: &GraphSetValue,
    row: &GraphValueRow,
    column: usize,
    ordinal: Option<u64>,
    control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), E> + Send),
    read: &mut S,
    reserve: &M,
) -> Result<GraphValue, ProjectionFailure<E>>
where
    E: Send,
    S: AsyncSpillElementSource<Error = E>,
    M: Fn(usize) -> Result<G, E> + Sync,
    G: Send,
{
    let mut nodes = EvaluationState::new(ordinal, read);
    projection_recurse!(
        asynchronous,
        reserve,
        asynchronous::evaluate_value_at(value, row, column, control, 0, &mut nodes, reserve)
    )
}

/// How many members a row-independent range() unwinds (fgdb-85aal), counted
/// from its bounds, which evaluate as UnwindRows::new reads them. `column` is
/// the UNWIND's appended column, which expression errors name.
pub(super) fn constant_range_members<E>(
    (start, end, step): (&GraphSetValue, &GraphSetValue, Option<&GraphSetValue>),
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<u128, ProjectionFailure<E>> {
    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
    let mut read = no_element_reads;
    let mut evaluation = Evaluation::new(None, &mut read);
    evaluation.nodes += 1;
    let bounds = synchronous::range_bounds(
        start,
        end,
        step,
        &GraphValueRow::unit(),
        column,
        control,
        0,
        &mut evaluation,
        &|_| Ok::<(), E>(()),
    )?;
    let Some((start, end, step)) = bounds else {
        return Ok(0);
    };
    range_member_count(start, end, step)
        .map(i128::unsigned_abs)
        .map_err(|kind| ProjectionFailure::Arithmetic {
            column,
            error: GraphIntegerError {
                instruction: 0,
                kind,
            },
        })
}

/// UnwindRows::new over a range() value for the async spill evaluator: the
/// same node charge and bounds, then the same streamed members (fgdb-85aal).
pub(super) async fn unwind_range_async<E, S, M, G>(
    row: GraphValueRow,
    (start, end, step): (&GraphSetValue, &GraphSetValue, Option<&GraphSetValue>),
    control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), E> + Send),
    read: &mut S,
    reserve: &M,
) -> Result<UnwindRows, ProjectionFailure<E>>
where
    E: Send,
    S: AsyncSpillElementSource<Error = E>,
    M: Fn(usize) -> Result<G, E> + Sync,
    G: Send,
{
    control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
    let column = row.len();
    let mut nodes = EvaluationState::new(None, read);
    nodes.nodes += 1;
    let bounds = projection_recurse!(
        asynchronous,
        reserve,
        asynchronous::range_bounds(
            start, end, step, &row, column, control, 0, &mut nodes, reserve
        )
    )?;
    UnwindRows::from_range(row, bounds, control)
}

/// An openCypher value function (fgdb-13l85) over its evaluated argument:
/// the value, or the list of a longer call's arguments. Every element copied
/// and every text built is reserved before it grows the result. The row and
/// aggregate output evaluators share it.
pub(crate) fn value_function<E>(
    function: GraphValueFunction,
    argument: &GraphValue,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    use crate::GraphIntegerErrorKind;
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    let text = |value: &str,
                control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), E>|
     -> Result<GraphValue, ProjectionFailure<E>> {
        for _ in 0..=value.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        }
        CanonicalScalar::ucs_basic_text(value)
            .map(GraphValue::Scalar)
            .map_err(|_| failure(GraphIntegerErrorKind::TextConstruction))
    };
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    match function {
        GraphValueFunction::Reverse => {
            if argument.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                Ok(null)
            } else if let Some(values) =
                list_members(argument, control).map_err(ProjectionFailure::Control)?
            {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                let mut reversed = Vec::with_capacity(values.len());
                for value in values.iter().rev() {
                    reversed.push(copy_value(value, control).map_err(ProjectionFailure::Control)?);
                }
                Ok(GraphValue::List(reversed.into_boxed_slice()))
            } else if let GraphValue::Scalar(CanonicalScalar::Text(value)) = argument {
                let reversed: String = value.as_str().chars().rev().collect();
                text(&reversed, control)
            } else {
                Err(failure(GraphIntegerErrorKind::IncompatibleOperands))
            }
        }
        GraphValueFunction::Split => {
            let Some([value, delimiter]) = argument
                .as_list()
                .and_then(|arguments| <&[GraphValue; 2]>::try_from(arguments).ok())
            else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            if value.is_null() || delimiter.is_null() {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                return Ok(null);
            }
            let (
                GraphValue::Scalar(CanonicalScalar::Text(value)),
                GraphValue::Scalar(CanonicalScalar::Text(delimiter)),
            ) = (value, delimiter)
            else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            let (value, delimiter) = (value.as_str(), delimiter.as_str());
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            let mut pieces = Vec::new();
            if delimiter.is_empty() {
                for (at, scalar) in value.char_indices() {
                    pieces.push(text(&value[at..at + scalar.len_utf8()], control)?);
                }
            } else {
                for piece in value.split(delimiter) {
                    pieces.push(text(piece, control)?);
                }
            }
            Ok(GraphValue::List(pieces.into_boxed_slice()))
        }
        // The evaluators read a coalesce lazily; here its arguments are
        // already values, so the first non-NULL one is the answer.
        GraphValueFunction::Coalesce => {
            let first = match argument.as_list() {
                Some(arguments) => arguments.iter().find(|value| !value.is_null()),
                None => (!argument.is_null()).then_some(argument),
            };
            match first {
                Some(value) => copy_value(value, control).map_err(ProjectionFailure::Control),
                None => {
                    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                    Ok(null)
                }
            }
        }
        GraphValueFunction::Add | GraphValueFunction::Subtract => {
            let Some([left, right]) = argument
                .as_list()
                .and_then(|arguments| <&[GraphValue; 2]>::try_from(arguments).ok())
            else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            sum_value(
                function == GraphValueFunction::Add,
                left,
                right,
                column,
                control,
            )
        }
        GraphValueFunction::PercentileDisc | GraphValueFunction::PercentileCont => percentile(
            function == GraphValueFunction::PercentileDisc,
            argument,
            column,
            control,
        ),
        GraphValueFunction::PathNodes | GraphValueFunction::PathRelationships => match argument {
            GraphValue::Path(path) => {
                let value = if function == GraphValueFunction::PathNodes {
                    GraphValue::Vertices(path.nodes().collect())
                } else {
                    GraphValue::Edges(path.edges().collect())
                };
                copy_value(&value, control).map_err(ProjectionFailure::Control)
            }
            _ if argument.is_null() => {
                control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
                Ok(null)
            }
            _ => Err(failure(GraphIntegerErrorKind::IncompatibleOperands)),
        },
    }
}

/// openCypher percentileDisc and percentileCont over `[collected, p]`
/// (fgdb-qbaex). p is checked first: NULL is NULL, and outside [0, 1] or not
/// a number is a typed error whatever was collected. The numbers sort by exact
/// numeric orderability, equal ones by canonical identity, so the answer never
/// depends on the collection's order; NULL members are skipped and none left
/// is NULL. The index and the interpolation use checked binary64 operations in
/// Neo4j's order.
fn percentile<E>(
    discrete: bool,
    argument: &GraphValue,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    use crate::GraphIntegerErrorKind as Kind;
    use crate::algebra::OrderableNumber;
    use fgdb_types::CanonicalF64;
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    let arithmetic = |_| failure(Kind::Overflow);
    let Some([collected, p]) = argument
        .as_list()
        .and_then(|arguments| <&[GraphValue; 2]>::try_from(arguments).ok())
    else {
        return Err(failure(Kind::IncompatibleOperands));
    };
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let p = match p {
        GraphValue::Scalar(CanonicalScalar::Null) => {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
            return Ok(null);
        }
        GraphValue::Scalar(CanonicalScalar::Int(value)) if (0..=1).contains(value) => {
            CanonicalF64::from_i64_rounded(*value)
        }
        GraphValue::Scalar(CanonicalScalar::Float(value)) if (0.0..=1.0).contains(&value.get()) => {
            *value
        }
        GraphValue::Scalar(CanonicalScalar::Int(_) | CanonicalScalar::Float(_)) => {
            return Err(failure(Kind::NumberOutOfRange));
        }
        _ => return Err(failure(Kind::IncompatibleOperands)),
    };
    let Some(members) = list_members(collected, control).map_err(ProjectionFailure::Control)?
    else {
        return Err(failure(Kind::IncompatibleOperands));
    };
    // Each number beside its binary64 value, which interpolation reads.
    let mut numbers: Vec<(&CanonicalScalar, CanonicalF64)> = Vec::with_capacity(members.len());
    for member in members.iter() {
        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        match member {
            GraphValue::Scalar(CanonicalScalar::Null) => {}
            GraphValue::Scalar(number @ CanonicalScalar::Int(value)) => {
                numbers.push((number, CanonicalF64::from_i64_rounded(*value)));
            }
            GraphValue::Scalar(number @ CanonicalScalar::Float(value)) => {
                numbers.push((number, *value));
            }
            _ => return Err(failure(Kind::IncompatibleOperands)),
        }
    }
    let Some(last) = numbers.len().checked_sub(1) else {
        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        return Ok(null);
    };
    let mut comparisons = 0_u64;
    numbers.sort_by(|(left, _), (right, _)| {
        comparisons += 1;
        match (
            OrderableNumber::scalar(left),
            OrderableNumber::scalar(right),
        ) {
            (Some(a), Some(b)) => a.cmp(b),
            _ => core::cmp::Ordering::Equal,
        }
        .then_with(|| left.cmp(right))
    });
    for _ in 0..comparisons {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
    }
    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
    // The list bound keeps both counts far inside i64.
    let count = CanonicalF64::from_i64_rounded(numbers.len() as i64);
    if discrete {
        // ceil(p * n) - 1, at least 0; p <= 1 keeps it at most n - 1.
        let position = p
            .checked_mul(count)
            .and_then(CanonicalF64::checked_ceil)
            .and_then(CanonicalF64::checked_to_i64_truncated)
            .map_err(arithmetic)?;
        let index = usize::try_from(position.saturating_sub(1))
            .unwrap_or(0)
            .min(last);
        return Ok(GraphValue::Scalar(numbers[index].0.clone()));
    }
    // pos = p * (n - 1): v[floor] when floor = ceil or floor = n - 1, else
    // v[floor] * (ceil - pos) + v[ceil] * (pos - floor).
    let position = p
        .checked_mul(CanonicalF64::from_i64_rounded(last as i64))
        .map_err(arithmetic)?;
    let floor = position.checked_floor().map_err(arithmetic)?;
    let ceil = position.checked_ceil().map_err(arithmetic)?;
    let index = |bound: CanonicalF64| {
        bound
            .checked_to_i64_truncated()
            .map(|index| usize::try_from(index).unwrap_or(0).min(last))
            .map_err(arithmetic)
    };
    let (lower, upper) = (index(floor)?, index(ceil)?);
    let value = if lower == upper || lower == last {
        numbers[lower].1
    } else {
        let below = ceil
            .checked_sub(position)
            .and_then(|weight| numbers[lower].1.checked_mul(weight))
            .map_err(arithmetic)?;
        let above = position
            .checked_sub(floor)
            .and_then(|weight| numbers[upper].1.checked_mul(weight))
            .map_err(arithmetic)?;
        below.checked_add(above).map_err(arithmetic)?
    };
    Ok(GraphValue::Scalar(CanonicalScalar::Float(value)))
}

/// rand()'s draw (fgdb-13l85): FNV-1a over a domain tag, the call site, the
/// row ordinal and each in-scope element's canonical bytes, finished by the
/// SplitMix64 mixer; its top 53 bits over 2^53 are a float in [0, 1). A pure
/// function, so replaying a statement over the same rows draws the same.
fn random_draw<E>(
    site: u32,
    ordinal: u64,
    elements: &[GraphValue],
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<f64, ProjectionFailure<E>> {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    fn absorb(hash: &mut u64, bytes: &[u8]) {
        for &byte in bytes {
            *hash ^= u64::from(byte);
            *hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    let mut hash = FNV_OFFSET;
    absorb(&mut hash, b"fgdb:rand:v1");
    absorb(&mut hash, &site.to_be_bytes());
    absorb(&mut hash, &ordinal.to_be_bytes());
    for element in elements {
        let bytes = element
            .canonical_bytes()
            .map_err(|_| ProjectionFailure::Arithmetic {
                column,
                error: GraphIntegerError {
                    instruction: 0,
                    kind: crate::GraphIntegerErrorKind::IncompatibleOperands,
                },
            })?;
        for _ in 0..bytes.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        }
        absorb(&mut hash, &(bytes.len() as u64).to_be_bytes());
        absorb(&mut hash, &bytes);
    }
    let mut mixed = hash.wrapping_add(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    Ok((mixed >> 11) as f64 / (1_u64 << 53) as f64)
}

/// `left + right` (`add`) or `left - right` over values that may be lists
/// (fgdb-c4i0d). NULL in either operand is NULL. Two scalars run the scalar
/// VM's own `+` or `-`, with `||` when either is text, so numbers, texts and
/// temporal values keep exactly the scalar rules. Lists only concatenate.
fn sum_value<E>(
    add: bool,
    left: &GraphValue,
    right: &GraphValue,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    use crate::{GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerOp};
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
    if left.is_null() || right.is_null() {
        return Ok(GraphValue::Scalar(CanonicalScalar::Null));
    }
    let left_members = list_members(left, control).map_err(ProjectionFailure::Control)?;
    let right_members = list_members(right, control).map_err(ProjectionFailure::Control)?;
    let (prefix, suffix) = match (left_members.as_deref(), right_members.as_deref()) {
        (None, None) => {
            let (GraphValue::Scalar(a), GraphValue::Scalar(b)) = (left, right) else {
                return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
            };
            let text =
                matches!(a, CanonicalScalar::Text(_)) || matches!(b, CanonicalScalar::Text(_));
            let op = match (add, text) {
                (true, true) => GraphIntegerOp::Concat,
                (true, false) => GraphIntegerOp::Binary(GraphIntegerBinary::Add),
                (false, _) => GraphIntegerOp::Binary(GraphIntegerBinary::Subtract),
            };
            let program = GraphIntegerExpression::prepare_scalar(&[
                GraphIntegerOp::ScalarColumn(0),
                GraphIntegerOp::ScalarColumn(1),
                op,
            ])
            .map_err(|_| failure(GraphIntegerErrorKind::IncompatibleOperands))?;
            let operands = [
                copy_value(left, control).map_err(ProjectionFailure::Control)?,
                copy_value(right, control).map_err(ProjectionFailure::Control)?,
            ];
            return program
                .evaluate_scalar_with_control(&operands, control)
                .map(GraphValue::Scalar)
                .map_err(|error| match error {
                    GraphIntegerEvaluationError::Value(error) => {
                        ProjectionFailure::Arithmetic { column, error }
                    }
                    GraphIntegerEvaluationError::Control(error) => {
                        ProjectionFailure::Control(error)
                    }
                });
        }
        _ if !add => return Err(failure(GraphIntegerErrorKind::IncompatibleOperands)),
        (Some(left), Some(right)) => (left, right),
        (Some(left), None) => (left, core::slice::from_ref(right)),
        (None, Some(right)) => (core::slice::from_ref(left), right),
    };
    let mut values = Vec::with_capacity(prefix.len() + suffix.len());
    for value in prefix.iter().chain(suffix) {
        values.push(copy_value(value, control).map_err(ProjectionFailure::Control)?);
    }
    Ok(GraphValue::List(values.into_boxed_slice()))
}

/// Merge two canonical key sequences once. The row and exact aggregate
/// evaluators share this owner after evaluating their explicit values.
/// Unchanged base values and all key payloads are reserved before copying;
/// overridden base values are never copied. A refusal drops the private map.
pub(crate) fn overlay_map<E>(
    base: &GraphValue,
    keys: &[Box<str>],
    values: Vec<GraphValue>,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValue, ProjectionFailure<E>> {
    use crate::GraphIntegerErrorKind;
    let failure = |kind| ProjectionFailure::Arithmetic {
        column,
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
    if base.is_null() {
        return Ok(GraphValue::Scalar(CanonicalScalar::Null));
    }
    let (base_keys, base_values) = base
        .as_map()
        .ok_or_else(|| failure(GraphIntegerErrorKind::NonMap))?;
    if base_keys.len() != base_values.len() || keys.len() != values.len() {
        return Err(failure(GraphIntegerErrorKind::NonMap));
    }
    let mut overrides = keys.iter().zip(values).peekable();
    let mut at = 0;
    let mut output_keys = Vec::new();
    let mut output_values = Vec::new();
    while at < base_keys.len() || overrides.peek().is_some() {
        control(GlaExecutionEvent::Work).map_err(ProjectionFailure::Control)?;
        let explicit = match (base_keys.get(at), overrides.peek()) {
            (Some(base_key), Some((key, _))) => match base_key.as_bytes().cmp(key.as_bytes()) {
                core::cmp::Ordering::Less => false,
                core::cmp::Ordering::Equal => {
                    at += 1;
                    true
                }
                core::cmp::Ordering::Greater => true,
            },
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => unreachable!("a remaining map entry was checked"),
        };
        let (key, value) = if explicit {
            overrides.next().expect("an explicit entry was checked")
        } else {
            let key = &base_keys[at];
            let value =
                copy_value(&base_values[at], control).map_err(ProjectionFailure::Control)?;
            at += 1;
            (key, value)
        };
        for _ in 0..=key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        output_keys.push(key.clone());
        output_values.push(value);
    }
    let result = GraphValue::Map {
        keys: output_keys.into_boxed_slice(),
        values: output_values.into_boxed_slice(),
    };
    if !result.validate_bounds() {
        return Err(failure(GraphIntegerErrorKind::Overflow));
    }
    Ok(result)
}

/// A map's keys as UCS_BASIC text values, ascending, each reserved before it
/// is built. Err(None) is a key that is not canonical text.
pub(crate) fn map_keys<E>(
    keys: &[Box<str>],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Box<[GraphValue]>, Option<E>> {
    let mut texts = Vec::new();
    for key in keys {
        control(GlaExecutionEvent::ScratchEntry).map_err(Some)?;
        for _ in 0..key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
            control(GlaExecutionEvent::ScratchEntry).map_err(Some)?;
        }
        texts.push(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(key).map_err(|_| None)?,
        ));
    }
    Ok(texts.into_boxed_slice())
}

/// Copy `row` into a list-comprehension element scope, under the same
/// per-value reservations as any other row copy.
fn element_scope<E>(
    row: &GraphValueRow,
    slots: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValueRow, ProjectionFailure<E>> {
    let mut values = Vec::new();
    for value in row.values() {
        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
        values.push(copy_value(value, control).map_err(ProjectionFailure::Control)?);
    }
    for _ in 0..slots {
        control(GlaExecutionEvent::ScratchEntry).map_err(ProjectionFailure::Control)?;
    }
    Ok(GraphValueRow::element_scope(values, slots))
}

/// openCypher list-slice bounds over `len` members: a negative bound
/// counts from the end, both bounds clamp to the list, an absent lower
/// bound is 0 and an absent upper bound is the length (fgdb-20foe).
pub(crate) fn slice_bounds(
    len: usize,
    from: Option<i128>,
    to: Option<i128>,
) -> core::ops::Range<usize> {
    let len = len as i128;
    let resolve = |bound: i128| {
        let at = if bound < 0 { len + bound } else { bound };
        at.clamp(0, len) as usize
    };
    let start = from.map_or(0, resolve);
    let end = to.map_or(len as usize, resolve);
    start..end.max(start)
}

/// openCypher range(start, end, step), end inclusive. A zero step is a
/// typed error, and the member count is checked against the list bound
/// before anything is allocated (fgdb-20foe).
pub(crate) fn range_values<E>(
    start: i64,
    end: i64,
    step: i64,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<GraphValue>, GraphIntegerEvaluationError<E>> {
    use crate::GraphIntegerErrorKind as Kind;
    let failure = |kind| {
        GraphIntegerEvaluationError::Value(GraphIntegerError {
            instruction: 0,
            kind,
        })
    };
    let count = range_member_count(start, end, step).map_err(failure)?;
    if count > GraphValue::MAX_LIST_NODES as i128 {
        return Err(failure(Kind::Overflow));
    }
    let (start, step) = (i128::from(start), i128::from(step));
    let mut members = Vec::new();
    for at in 0..count {
        control(GlaExecutionEvent::ScratchEntry).map_err(GraphIntegerEvaluationError::Control)?;
        let value = i64::try_from(start + at * step).map_err(|_| failure(Kind::Overflow))?;
        members.push(GraphValue::Scalar(CanonicalScalar::Int(value)));
    }
    Ok(members)
}

/// How many members range(start, end, step) has: none when `end` lies
/// before `start` in the step's direction. A zero step is a typed error.
/// Every member lies between `start` and `end`, so each fits an i64.
pub(crate) fn range_member_count(
    start: i64,
    end: i64,
    step: i64,
) -> Result<i128, crate::GraphIntegerErrorKind> {
    if step == 0 {
        return Err(crate::GraphIntegerErrorKind::InvalidRangeStep);
    }
    let (start, end, step) = (i128::from(start), i128::from(end), i128::from(step));
    let span = end - start;
    Ok(if span == 0 || (span > 0) == (step > 0) {
        span / step + 1
    } else {
        0
    })
}
