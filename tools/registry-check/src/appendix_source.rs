//! Conservative, deterministic structural census for Appendix-style source.
//!
//! This module deliberately recognizes a small, closed grammar over Markdown
//! code spans.  It never treats capitalization or a type-name suffix as proof
//! of ownership.  Syntax outside that grammar is retained as an ambiguity with
//! an exact source span, so a caller can pin the resulting transcripts without
//! turning parser uncertainty into an accidental definition.

use crate::hash::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

/// A caller-supplied, inclusive source-line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSliceSpec<'a> {
    pub id: &'a str,
    pub start_line: usize,
    pub end_line: usize,
}

/// A one-based position in the original UTF-8 source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourcePosition {
    pub line: usize,
    pub column: usize,
}

/// A half-open span in the original source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceSpan {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaOwnerStatus {
    ConfirmedTopLevel,
    AmbiguousUnownedStructure,
    NamedConceptNoBody,
}

impl SchemaOwnerStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedTopLevel => "confirmed-top-level",
            Self::AmbiguousUnownedStructure => "ambiguous-unowned-structure",
            Self::NamedConceptNoBody => "named-concept-no-body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefinitionKind {
    FencedRecord,
    FencedUnbalanced,
    InlineRecord,
    InlineUnbalanced,
    InlineAlias,
    ProseLinkedStructural,
    ProseDefinitionNoBody,
    BoldOwnerStructural,
}

impl DefinitionKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FencedRecord => "fenced-record",
            Self::FencedUnbalanced => "fenced-unbalanced",
            Self::InlineRecord => "inline-record",
            Self::InlineUnbalanced => "inline-unbalanced",
            Self::InlineAlias => "inline-alias",
            Self::ProseLinkedStructural => "prose-linked-structural",
            Self::ProseDefinitionNoBody => "prose-definition-no-body",
            Self::BoldOwnerStructural => "bold-owner-structural",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cardinality {
    One,
    Optional,
    Many,
    ManyOrIndexed,
}

impl Cardinality {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Optional => "optional",
            Self::Many => "many",
            Self::ManyOrIndexed => "many-or-indexed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AmbiguityKind {
    AliasExpressionUnparsed,
    AmbiguousSchemaOwner,
    CompressedMemberToken,
    ConflictingCandidateEvidence,
    DefinitionWithoutStructuralBody,
    FieldTypeAmbiguous,
    MismatchedDelimiter,
    NestingLimitExceeded,
    UnbalancedDefinition,
    UnownedStructuralFragment,
    UnparsedRecordItem,
    UnparsedUnionArm,
    UnparsedTrailingTokens,
    UnterminatedCodeFence,
    UnterminatedInlineCode,
}

impl AmbiguityKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AliasExpressionUnparsed => "alias-expression-unparsed",
            Self::AmbiguousSchemaOwner => "ambiguous-schema-owner",
            Self::CompressedMemberToken => "compressed-member-token",
            Self::ConflictingCandidateEvidence => "conflicting-candidate-evidence",
            Self::DefinitionWithoutStructuralBody => "definition-without-structural-body",
            Self::FieldTypeAmbiguous => "field-type-ambiguous",
            Self::MismatchedDelimiter => "mismatched-delimiter",
            Self::NestingLimitExceeded => "nesting-limit-exceeded",
            Self::UnbalancedDefinition => "unbalanced-definition",
            Self::UnownedStructuralFragment => "unowned-structural-fragment",
            Self::UnparsedRecordItem => "unparsed-record-item",
            Self::UnparsedUnionArm => "unparsed-union-arm",
            Self::UnparsedTrailingTokens => "unparsed-trailing-tokens",
            Self::UnterminatedCodeFence => "unterminated-code-fence",
            Self::UnterminatedInlineCode => "unterminated-inline-code",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchemaCandidateKey {
    pub family: String,
    pub generic_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldCandidateKey {
    pub schema_family: String,
    pub schema_owner: String,
    pub path: String,
    pub stable_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnionCandidateKey {
    pub schema_family: String,
    pub schema_owner: String,
    pub union_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ArmCandidateKey {
    pub schema_family: String,
    pub schema_owner: String,
    pub union_path: String,
    pub arm_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AmbiguityKey {
    pub kind: AmbiguityKind,
    pub schema_family: Option<String>,
    pub path: Option<String>,
    pub raw_sha256: String,
    pub affected_source_key_count: usize,
    pub affected_source_keys_sha256: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum StructuralCandidateKey {
    Schema(SchemaCandidateKey),
    Field(FieldCandidateKey),
    Union(UnionCandidateKey),
    Arm(ArmCandidateKey),
}

impl SchemaCandidateKey {
    /// Source-only key; identity classification is intentionally a separate
    /// catalog decision and is not inferred by this parser.
    pub fn source_key(&self) -> String {
        format!("top|{}{}", self.family, self.generic_signature)
    }
}

impl FieldCandidateKey {
    pub fn source_key(&self) -> String {
        format!(
            "field|{}|{}|{}",
            self.schema_owner, self.path, self.stable_name
        )
    }
}

impl UnionCandidateKey {
    pub fn source_key(&self) -> String {
        format!("union|{}|{}", self.schema_owner, self.union_path)
    }
}

impl ArmCandidateKey {
    pub fn source_key(&self) -> String {
        format!(
            "arm|{}|{}|{}",
            self.schema_owner, self.union_path, self.arm_name
        )
    }
}

impl AmbiguityKey {
    pub fn source_key(&self) -> String {
        format!(
            "ambiguity|{}|{}|{}|{}|{}|{}|{}",
            self.kind.as_str(),
            self.schema_family.as_deref().unwrap_or_default(),
            self.path.as_deref().unwrap_or_default(),
            self.raw_sha256,
            self.affected_source_key_count,
            self.affected_source_keys_sha256,
            self.reason
        )
    }
}

impl StructuralCandidateKey {
    fn source_key(&self) -> String {
        match self {
            Self::Schema(key) => key.source_key(),
            Self::Field(key) => key.source_key(),
            Self::Union(key) => key.source_key(),
            Self::Arm(key) => key.source_key(),
        }
    }

    fn schema_family(&self) -> &str {
        match self {
            Self::Schema(key) => &key.family,
            Self::Field(key) => &key.schema_family,
            Self::Union(key) => &key.schema_family,
            Self::Arm(key) => &key.schema_family,
        }
    }

    fn container_path(&self) -> String {
        match self {
            Self::Schema(key) => format!("{}{}", key.family, key.generic_signature),
            Self::Field(key) => key.path.clone(),
            Self::Union(key) => key.union_path.clone(),
            Self::Arm(key) => format!("{}.{}", key.union_path, key.arm_name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaCandidate {
    pub key: SchemaCandidateKey,
    pub owner_statuses: Vec<SchemaOwnerStatus>,
    pub definition_kinds: Vec<DefinitionKind>,
    pub expression_sha256s: Vec<String>,
    pub body_conflict: bool,
    pub locations: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldCandidate {
    pub key: FieldCandidateKey,
    pub exact_types: Vec<String>,
    pub cardinalities: Vec<Cardinality>,
    pub type_conflict: bool,
    pub ambiguous: bool,
    pub locations: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnionCandidate {
    pub key: UnionCandidateKey,
    pub occurrence_count: usize,
    pub arm_names: Vec<String>,
    pub arm_name_sets: Vec<Vec<String>>,
    pub arm_set_conflict: bool,
    pub parsed_arm_count: usize,
    pub unparsed_arm_count: usize,
    pub locations: Vec<SourceSpan>,
    pub evidence_lines: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmCandidate {
    pub key: ArmCandidateKey,
    pub payload_sha256s: Vec<String>,
    pub payload_conflict: bool,
    pub locations: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguityCandidate {
    pub key: AmbiguityKey,
    pub raw: String,
    pub affected_source_keys: Vec<String>,
    pub locations: Vec<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDigest {
    pub rows: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusTranscripts {
    pub schemas: TranscriptDigest,
    pub fields: TranscriptDigest,
    pub unions: TranscriptDigest,
    pub arms: TranscriptDigest,
    pub ambiguities: TranscriptDigest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CensusCounts {
    pub schema_occurrences: usize,
    pub schema_candidates: usize,
    pub field_occurrences: usize,
    pub field_candidates: usize,
    pub union_occurrences: usize,
    pub union_candidates: usize,
    pub unions_with_unparsed_arms: usize,
    pub arm_occurrences: usize,
    pub arm_candidates: usize,
    pub ambiguity_occurrences: usize,
    pub ambiguities: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceSourceCensus {
    pub slice_id: String,
    pub start_line: usize,
    pub end_line: usize,
    pub source_byte_count: usize,
    pub source_sha256: String,
    pub schemas: Vec<SchemaCandidate>,
    pub fields: Vec<FieldCandidate>,
    pub unions: Vec<UnionCandidate>,
    pub arms: Vec<ArmCandidate>,
    pub ambiguities: Vec<AmbiguityCandidate>,
    pub counts: CensusCounts,
    /// Hashes of sorted, unique canonical source keys. Exact locations remain
    /// available on each candidate and source movement changes `source_sha256`.
    pub transcripts: CensusTranscripts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendixSourceCensus {
    pub source_start_line: usize,
    pub source_end_line: usize,
    pub source_byte_count: usize,
    pub source_sha256: String,
    pub slices: Vec<SliceSourceCensus>,
    pub schemas: Vec<SchemaCandidate>,
    pub fields: Vec<FieldCandidate>,
    pub unions: Vec<UnionCandidate>,
    pub arms: Vec<ArmCandidate>,
    pub ambiguities: Vec<AmbiguityCandidate>,
    pub counts: CensusCounts,
    pub transcripts: CensusTranscripts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CensusErrorKind {
    CandidateAssignmentInvariant,
    CarriageReturn,
    EmptySlices,
    EmptySource,
    InvalidSliceId,
    InvalidSliceRange,
    InvalidUtf8,
    SourceCoordinateOverflow,
    SliceGap,
    SliceOverlap,
    SliceOutsideSource,
}

fn census_error(
    kind: CensusErrorKind,
    slice_id: Option<&str>,
    message: impl Into<String>,
) -> CensusError {
    CensusError {
        kind,
        slice_id: slice_id.map(str::to_owned),
        message: message.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CensusError {
    pub kind: CensusErrorKind,
    pub slice_id: Option<String>,
    pub message: String,
}

impl fmt::Display for CensusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CensusError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FragmentKind {
    Inline,
    Fence,
}

#[derive(Debug, Clone)]
struct MarkdownFragment {
    id: usize,
    kind: FragmentKind,
    text: String,
    source_range: Range<usize>,
    before: String,
    after: String,
}

#[derive(Debug, Clone)]
struct MappedSegment {
    synthetic_range: Range<usize>,
    source_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct MappedText {
    text: String,
    segments: Vec<MappedSegment>,
}

#[derive(Debug, Clone)]
struct SchemaOccurrence {
    key: SchemaCandidateKey,
    display_name: String,
    owner_status: SchemaOwnerStatus,
    definition_kind: DefinitionKind,
    complete_top_level_map_definition: bool,
    declaration_range: Range<usize>,
    expression: Option<MappedText>,
    supplemental_unions: Vec<MappedText>,
    expression_sha256: String,
}

#[derive(Debug, Clone)]
struct FieldOccurrence {
    key: FieldCandidateKey,
    exact_type: Option<String>,
    cardinality: Cardinality,
    raw: String,
    ambiguity: Option<String>,
    source_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct UnionOccurrence {
    key: UnionCandidateKey,
    source_range: Range<usize>,
    evidence_ranges: Vec<Range<usize>>,
    arm_names: BTreeSet<String>,
    unparsed_arm_count: usize,
}

#[derive(Debug, Clone)]
struct ArmOccurrence {
    key: ArmCandidateKey,
    payload: Option<String>,
    raw: String,
    source_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct AmbiguityOccurrence {
    kind: AmbiguityKind,
    schema_family: Option<String>,
    path: Option<String>,
    raw: String,
    reason: String,
    affected_source_keys: BTreeSet<StructuralCandidateKey>,
    source_range: Range<usize>,
}

#[derive(Debug, Clone)]
struct SourceMap<'a> {
    source: &'a str,
    start_line: usize,
    line_starts: Vec<usize>,
}

impl<'a> SourceMap<'a> {
    fn new(source: &'a str, start_line: usize) -> Self {
        let mut line_starts = vec![0];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' && index + 1 < source.len() {
                line_starts.push(index + 1);
            }
        }
        Self {
            source,
            start_line,
            line_starts,
        }
    }

    fn position(&self, offset: usize) -> SourcePosition {
        let offset = offset.min(self.source.len());
        let line_index = self
            .line_starts
            .partition_point(|candidate| *candidate <= offset)
            .saturating_sub(1);
        let line_start = self.line_starts[line_index];
        let column = self.source[line_start..offset].chars().count() + 1;
        SourcePosition {
            line: self.start_line + line_index,
            column,
        }
    }

    fn span(&self, range: &Range<usize>) -> SourceSpan {
        SourceSpan {
            start: self.position(range.start),
            end: self.position(range.end),
        }
    }

    fn byte_range_for_lines(&self, start_line: usize, end_line: usize) -> Range<usize> {
        let first = start_line - self.start_line;
        let last = end_line - self.start_line;
        let start = self.line_starts[first];
        let end = self
            .line_starts
            .get(last + 1)
            .copied()
            .unwrap_or(self.source.len());
        start..end
    }
}

impl MappedText {
    fn from_source(source: &str, range: Range<usize>) -> Self {
        let text = source[range.clone()].to_owned();
        let length = text.len();
        Self {
            text,
            segments: vec![MappedSegment {
                synthetic_range: 0..length,
                source_range: range,
            }],
        }
    }

    fn joined(source: &str, ranges: &[Range<usize>]) -> Self {
        let mut text = String::new();
        let mut segments = Vec::new();
        for (index, range) in ranges.iter().enumerate() {
            if index != 0 {
                text.push_str(" | ");
            }
            let start = text.len();
            text.push_str(&source[range.clone()]);
            let end = text.len();
            segments.push(MappedSegment {
                synthetic_range: start..end,
                source_range: range.clone(),
            });
        }
        Self { text, segments }
    }

    fn subrange(&self, range: Range<usize>) -> Self {
        let text = self.text[range.clone()].to_owned();
        let mut segments = Vec::new();
        for segment in &self.segments {
            let start = segment.synthetic_range.start.max(range.start);
            let end = segment.synthetic_range.end.min(range.end);
            if start >= end {
                continue;
            }
            let source_start = segment.source_range.start + (start - segment.synthetic_range.start);
            let source_end = source_start + (end - start);
            segments.push(MappedSegment {
                synthetic_range: (start - range.start)..(end - range.start),
                source_range: source_start..source_end,
            });
        }
        Self { text, segments }
    }

    fn source_range(&self, range: Range<usize>) -> Range<usize> {
        let start = self.map_offset(range.start, false);
        let end = self.map_offset(range.end, true);
        start..end.max(start)
    }

    fn map_offset(&self, offset: usize, end_bias: bool) -> usize {
        if let Some(segment) = self.segments.iter().find(|segment| {
            segment.synthetic_range.start <= offset && offset < segment.synthetic_range.end
        }) {
            return segment.source_range.start + (offset - segment.synthetic_range.start);
        }
        if end_bias {
            if let Some(segment) = self
                .segments
                .iter()
                .rev()
                .find(|segment| segment.synthetic_range.end <= offset)
            {
                return segment.source_range.end;
            }
        } else if let Some(segment) = self
            .segments
            .iter()
            .find(|segment| segment.synthetic_range.start >= offset)
        {
            return segment.source_range.start;
        }
        self.segments
            .last()
            .map(|segment| segment.source_range.end)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplitSpan {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DelimiterIssue {
    offset: usize,
    mismatched: bool,
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_upper_identifier_start(byte: u8) -> bool {
    byte.is_ascii_uppercase()
}

fn is_lower_identifier_start(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte == b'_'
}

fn skip_ascii_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn trim_range(text: &str, range: Range<usize>) -> Range<usize> {
    let bytes = text.as_bytes();
    let mut start = range.start;
    let mut end = range.end;
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    start..end
}

fn normalize_whitespace(value: &str) -> String {
    let mut normalized = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;
    for character in value.chars() {
        if let Some(active_quote) = quote {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.push(character);
            quote = Some(character);
        } else if character.is_whitespace() {
            pending_space = !normalized.is_empty();
        } else {
            if pending_space && !normalized.is_empty() {
                normalized.push(' ');
            }
            pending_space = false;
            normalized.push(character);
        }
    }
    normalized
}

fn parse_identifier(bytes: &[u8], start: usize) -> Option<usize> {
    if !bytes.get(start).copied().is_some_and(is_identifier_start) {
        return None;
    }
    let mut end = start + 1;
    while bytes.get(end).copied().is_some_and(is_identifier_continue) {
        end += 1;
    }
    Some(end)
}

fn parse_upper_identifier(bytes: &[u8], start: usize) -> Option<usize> {
    if !bytes
        .get(start)
        .copied()
        .is_some_and(is_upper_identifier_start)
    {
        return None;
    }
    let mut end = start + 1;
    while bytes.get(end).copied().is_some_and(is_identifier_continue) {
        end += 1;
    }
    Some(end)
}

fn is_generic_angle_open(text: &str, index: usize) -> bool {
    let bytes = text.as_bytes();
    if bytes.get(index) != Some(&b'<') {
        return false;
    }
    let mut before = index;
    while before > 0 && bytes[before - 1].is_ascii_whitespace() {
        before -= 1;
    }
    let mut after = index + 1;
    while bytes.get(after).is_some_and(u8::is_ascii_whitespace) {
        after += 1;
    }
    let Some(previous) = before.checked_sub(1).and_then(|at| bytes.get(at)).copied() else {
        return false;
    };
    let Some(next) = bytes.get(after).copied() else {
        return false;
    };
    (is_identifier_continue(previous) || matches!(previous, b'>' | b']'))
        && (is_identifier_start(next) || matches!(next, b'?' | b'[' | b'{'))
}

/// The four delimiters the Appendix A source grammar nests, and their closers
/// at the same index. Spelled ONCE: a second copy of this set is how the two
/// readers below drifted apart in the first place.
const OPENING_DELIMITERS: [u8; 4] = *b"{[(<";
const CLOSING_DELIMITERS: [u8; 4] = *b"}])>";

fn opening_delimiter_for(closer: u8) -> Option<u8> {
    CLOSING_DELIMITERS
        .iter()
        .position(|candidate| *candidate == closer)
        .map(|at| OPENING_DELIMITERS[at])
}

/// What one byte was, structurally, to [`DelimiterScan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimiterStep {
    /// Consumed by the quote machine, or an angle that is not generic syntax,
    /// or a `>` that closes nothing. NEVER a candidate separator — this is the
    /// distinction a caller cannot re-derive from the byte alone, and the one
    /// a second reader gets wrong.
    Skipped,
    /// An opening delimiter; the depth grew.
    Opened,
    /// A closing delimiter that matched its opener; the depth shrank.
    Closed,
    /// Structurally inert at the current depth. The only kind of byte a caller
    /// may treat as a separator.
    Plain,
}

/// THE delimiter reader for Appendix A source text: one stack, one quote
/// machine, one generic-angle rule.
///
/// There used to be TWO. `matching_delimiter` and `split_top_level` each
/// carried a private stack, private quote/escape handling, a private
/// `is_generic_angle_open` special case, and a private copy of the push/close
/// sets — the same balance test written out twice. That is the defect
/// fgdb-8kzt actually names, and it is why three separate repairs to
/// `matching_delimiter` alone (V1/V2/V3 on that bead) recovered NOTHING: the
/// twin still rejected the text. Where two pieces of code answer one question
/// they drift, and the weaker one wins by being the one that happens to run.
///
/// Both entry points below are now thin drivers over this scanner. They differ
/// only in where they start and what they do with the events — never in what
/// counts as balanced. A future grammar change (the half-open interval
/// `(a,b]` of fgdb-8kzt, say) is therefore ONE edit, and it cannot land in
/// half the readers.
///
/// The structural guard that keeps it that way is
/// `exactly_one_delimiter_reader_exists` in this file's test module: behaviour
/// tests can only exercise a reader they know the name of, so a third stack
/// would be invisible to every other test here.
struct DelimiterScan {
    stack: Vec<u8>,
    quote: Option<u8>,
    escaped: bool,
    /// Offset of the `]` that terminates a half-open interval literal opened at
    /// an earlier `(`. See [`half_open_interval_end`]: the pair is one TOKEN, so
    /// neither byte reaches the stack.
    interval_close: Option<usize>,
}

impl DelimiterScan {
    /// A scan that starts outside every delimiter.
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            quote: None,
            escaped: false,
            interval_close: None,
        }
    }

    /// A scan that starts having already consumed `opener`.
    fn inside(opener: u8) -> Self {
        Self {
            stack: vec![opener],
            quote: None,
            escaped: false,
            interval_close: None,
        }
    }

    fn depth(&self) -> usize {
        self.stack.len()
    }

    fn in_quote(&self) -> bool {
        self.quote.is_some()
    }

    /// Feed the byte at `index`. `text` is needed whole because the
    /// generic-angle rule looks both ways around a `<`.
    fn step(
        &mut self,
        text: &str,
        index: usize,
        byte: u8,
    ) -> Result<DelimiterStep, DelimiterIssue> {
        if let Some(active_quote) = self.quote {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == active_quote {
                self.quote = None;
            }
            return Ok(DelimiterStep::Skipped);
        }
        if matches!(byte, b'\'' | b'"') {
            self.quote = Some(byte);
            return Ok(DelimiterStep::Skipped);
        }
        // A half-open interval literal is ONE token, recognised at its `(` and
        // released at its `]`. Both bytes are inert, exactly as a quoted string's
        // contents are: the pair never enters the stack, so it can neither open a
        // depth nor close somebody else's. Placed after the quote machine so an
        // interval spelled inside a quoted string stays quoted text.
        if self.interval_close == Some(index) {
            self.interval_close = None;
            return Ok(DelimiterStep::Skipped);
        }
        if matches!(byte, b'(' | b'[')
            && let Some(end) = half_open_interval_end(text, index)
        {
            self.interval_close = Some(end);
            return Ok(DelimiterStep::Skipped);
        }
        if byte == b'<' && !is_generic_angle_open(text, index) {
            return Ok(DelimiterStep::Skipped);
        }
        if OPENING_DELIMITERS.contains(&byte) {
            self.stack.push(byte);
            return Ok(DelimiterStep::Opened);
        }
        if byte == b'>' && self.stack.last() != Some(&b'<') {
            return Ok(DelimiterStep::Skipped);
        }
        // Pop-and-compare, not peek-and-compare, and the lookup IS the
        // membership test — a byte with no opener is simply not a closer. Both
        // old readers instead tested membership and then re-derived the pair,
        // which needed an `unreachable!()` arm to convince the compiler the two
        // agreed. There is no unreachable arm here: this is total.
        //
        // The peek form the old `matching_delimiter` used is genuinely PARTIAL —
        // it reaches `unreachable!()` on an empty stack, and only its own
        // driver's invariant kept that safe. `split_top_level` reaches depth 0
        // routinely, and an unmatched closer there IS the mismatch. The two
        // forms agree wherever both were defined: `closer_of(top) != byte` and
        // `top != opener_of(byte)` are one test over a four-element bijection.
        let Some(expected_opener) = opening_delimiter_for(byte) else {
            return Ok(DelimiterStep::Plain);
        };
        if self.stack.pop() != Some(expected_opener) {
            return Err(DelimiterIssue {
                offset: index,
                mismatched: true,
            });
        }
        Ok(DelimiterStep::Closed)
    }
}

/// If the `(` at `open_index` begins a half-open interval literal — `(term,term]`,
/// the standard mathematical spelling Appendix A uses for a retained window —
/// return the offset of the `]` that ends it.
///
/// This is a TOKEN recognizer and deliberately NOT a relaxed balance rule. The
/// alternative measured on fgdb-8kzt (V4) let any `(` be closed by any `]`
/// anywhere, which makes a genuine mismatched-delimiter typo parse silently — a
/// fail-open reader inside a checker whose whole job is to be unfoolable. The
/// shape required here is two nonempty identifier terms and exactly one comma, so
/// `entries[foo)`, `(a,b)`, `(a]`, `(a,b,c]` and `(a b,c]` all stay mismatched.
///
/// Both orientations are recognised, because the census region spells both:
/// measured over plan lines 1388-2728, `(term,term]` appears twice (a10:1928, the
/// two delta indexes) and `[term,term)` once (line 1728, `[0,byte_count)`). Each
/// is closed by the bracket the interval's own openness demands, so the closer is
/// part of the token and never a structural closer.
fn half_open_interval_end(text: &str, open_index: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let terminator = match bytes.get(open_index).copied() {
        Some(b'(') => b']',
        Some(b'[') => b')',
        _ => return None,
    };
    let mut index = open_index + 1;
    let mut terms = 0usize;
    loop {
        let term_start = index;
        while matches!(bytes.get(index), Some(&byte) if byte.is_ascii_alphanumeric() || byte == b'_')
        {
            index += 1;
        }
        if index == term_start {
            return None;
        }
        terms += 1;
        match bytes.get(index).copied() {
            Some(b',') if terms == 1 => index += 1,
            Some(byte) if byte == terminator && terms == 2 => return Some(index),
            _ => return None,
        }
    }
}

fn matching_delimiter(text: &str, open_index: usize) -> Result<usize, DelimiterIssue> {
    let bytes = text.as_bytes();
    let Some(opener) = bytes.get(open_index).copied() else {
        return Err(DelimiterIssue {
            offset: open_index,
            mismatched: false,
        });
    };
    // The interval token is inert HERE TOO. `inside` seeds the stack with the
    // opener without routing it through `step`, so without this the same bracket
    // would be an inert token mid-scan and a structural opener at the entry point
    // — one rule spelled two ways, which is the defect fgdb-8kzt is about. A
    // caller that points at an interval's bracket is not pointing at a body, and
    // gets exactly what it gets for any other non-opener byte.
    if !OPENING_DELIMITERS.contains(&opener) || half_open_interval_end(text, open_index).is_some() {
        return Err(DelimiterIssue {
            offset: open_index,
            mismatched: true,
        });
    }
    let mut scan = DelimiterScan::inside(opener);
    for (index, byte) in bytes.iter().copied().enumerate().skip(open_index + 1) {
        if scan.step(text, index, byte)? == DelimiterStep::Closed && scan.depth() == 0 {
            return Ok(index);
        }
    }
    Err(DelimiterIssue {
        offset: text.len(),
        mismatched: false,
    })
}

fn split_top_level(text: &str, delimiters: &[u8]) -> Result<Vec<SplitSpan>, DelimiterIssue> {
    let mut spans = Vec::new();
    let mut scan = DelimiterScan::new();
    let mut start = 0;
    for (index, byte) in text.as_bytes().iter().copied().enumerate() {
        // Only a Plain byte may separate: a quoted comma, a nested comma, and
        // a closing delimiter must all fail this test, and only the scanner
        // knows which is which.
        if scan.step(text, index, byte)? == DelimiterStep::Plain
            && scan.depth() == 0
            && delimiters.contains(&byte)
        {
            spans.push(SplitSpan { start, end: index });
            start = index + 1;
        }
    }
    if scan.depth() != 0 || scan.in_quote() {
        return Err(DelimiterIssue {
            offset: text.len(),
            mismatched: false,
        });
    }
    spans.push(SplitSpan {
        start,
        end: text.len(),
    });
    Ok(spans)
}

fn top_level_arrow(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if matches!(byte, b'\'' | b'"') {
            let quote = byte;
            let mut escaped = false;
            cursor += 1;
            while cursor < bytes.len() {
                let quoted = bytes[cursor];
                cursor += 1;
                if escaped {
                    escaped = false;
                } else if quoted == b'\\' {
                    escaped = true;
                } else if quoted == quote {
                    break;
                }
            }
            continue;
        }
        if matches!(byte, b'{' | b'[' | b'(')
            || (byte == b'<' && is_generic_angle_open(text, cursor))
        {
            cursor = matching_delimiter(text, cursor).ok()? + 1;
            continue;
        }
        if bytes.get(cursor..cursor + 2) == Some(b"->") {
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn indexed_map_value(mapped: &MappedText) -> Option<MappedText> {
    let trimmed = trim_range(&mapped.text, 0..mapped.text.len());
    let open = trimmed.start;
    if mapped.text.as_bytes().get(open) != Some(&b'[') {
        return None;
    }
    let close = matching_delimiter(&mapped.text, open).ok()?;
    if close + 1 != trimmed.end {
        return None;
    }
    let interior_start = open + 1;
    let arrow = interior_start + top_level_arrow(&mapped.text[interior_start..close])?;
    let key = trim_range(&mapped.text, interior_start..arrow);
    let value = trim_range(&mapped.text, arrow + 2..close);
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some(mapped.subrange(value))
}

fn top_level_map_value(mapped: &MappedText) -> Option<MappedText> {
    let trimmed = trim_range(&mapped.text, 0..mapped.text.len());
    let arrow = trimmed.start + top_level_arrow(&mapped.text[trimmed.clone()])?;
    let key = trim_range(&mapped.text, trimmed.start..arrow);
    let value = trim_range(&mapped.text, arrow + 2..trimmed.end);
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some(mapped.subrange(value))
}

fn parse_type_display(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let start = skip_ascii_whitespace(bytes, 0);
    let family_end = parse_upper_identifier(bytes, start)?;
    let mut end = family_end;
    let after_name = skip_ascii_whitespace(bytes, end);
    if bytes.get(after_name) == Some(&b'<') && is_generic_angle_open(text, after_name) {
        end = matching_delimiter(text, after_name).ok()? + 1;
    }
    Some((normalize_whitespace(&text[start..end]), end))
}

fn family_and_generic(display: &str) -> Option<SchemaCandidateKey> {
    let bytes = display.as_bytes();
    let family_end = parse_upper_identifier(bytes, 0)?;
    let family = display[..family_end].to_owned();
    if is_schema_metavariable(&family) {
        return None;
    }
    Some(SchemaCandidateKey {
        family,
        generic_signature: display[family_end..].trim().to_owned(),
    })
}

fn is_schema_metavariable(family: &str) -> bool {
    matches!(
        family,
        "A" | "B" | "T" | "Role" | "Kind" | "Enum" | "Local" | "Meta" | "Shard"
    )
}

fn top_level_assignment(text: &str) -> Result<Option<(String, Range<usize>)>, DelimiterIssue> {
    let spans = split_top_level(text, b"=")?;
    if spans.len() != 2 {
        return Ok(None);
    }
    let left = trim_range(text, spans[0].start..spans[0].end);
    let Some((display, consumed)) = parse_type_display(&text[left.clone()]) else {
        return Ok(None);
    };
    if consumed != left.len() {
        return Ok(None);
    }
    let right = trim_range(text, spans[1].start..spans[1].end);
    Ok(Some((display, right)))
}

fn has_top_level_pipe(text: &str) -> Result<bool, DelimiterIssue> {
    Ok(split_top_level(text, b"|")?.len() > 1)
}

fn starts_with_word(value: &str, word: &str) -> bool {
    let lower = value.trim_start().to_ascii_lowercase();
    lower.starts_with(word)
        && lower
            .as_bytes()
            .get(word.len())
            .is_none_or(|byte| !is_identifier_continue(*byte))
}

/// The one connector across which a prose definition distributes to an earlier
/// conjunct.  Deliberately exact: measured over all 13 conjunction definition
/// sites in Appendix A, the trailing text of the first conjunct is " and " in
/// 13 of 13.  A looser predicate would start claiming list separators.
const CONJUNCTION_CONNECTOR: &str = "and";

fn has_definition_cue(value: &str) -> bool {
    const CUES: [&str; 22] = [
        "is",
        "are",
        "has",
        "exactly",
        "contains",
        "maps",
        "uses",
        "adds",
        "becomes",
        "remains",
        "means",
        "binds",
        "merely stores",
        "names",
        "carries",
        "defines",
        "commits",
        "records",
        "stores",
        "holds",
        "encodes",
        "owns",
    ];
    CUES.iter().any(|cue| starts_with_word(value, cue)) || starts_with_word(value, "selects")
}

fn cue_names_union(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    ["union", "tag", "arm", "one of"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn contains_continuation_separator(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b';' | b',' | b'/'))
        || value
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word.eq_ignore_ascii_case("or") || word.eq_ignore_ascii_case("and"))
}

fn sentence_ends(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b'.' | b'!' | b'?'))
}

fn prose_body_candidate(owner: &str, text: &str) -> bool {
    if let Some((assigned, _)) = leading_assignment(text) {
        return assigned == owner;
    }
    structural_fragment(text)
}

fn union_continuation_candidate(text: &str) -> bool {
    leading_assignment(text).is_none() && matches!(has_top_level_pipe(text), Ok(true))
}

fn complete_record_fragment(text: &str) -> bool {
    let trimmed = trim_range(text, 0..text.len());
    if text.as_bytes().get(trimmed.start) != Some(&b'{') {
        return false;
    }
    matching_delimiter(text, trimmed.start).is_ok_and(|close| close + 1 == trimmed.end)
}

fn complete_union_arm_fragment(text: &str) -> bool {
    let trimmed = trim_range(text, 0..text.len());
    if trimmed.is_empty() || matches!(has_top_level_pipe(&text[trimmed.clone()]), Ok(true)) {
        return false;
    }
    let candidate = &text[trimmed];
    let Some(token) = first_arm_token(candidate) else {
        return false;
    };
    let Some(open) = candidate.find('{') else {
        return token.end == candidate.len();
    };
    trim_range(candidate, token.end..open).is_empty()
        && matching_delimiter(candidate, open).is_ok_and(|close| close + 1 == candidate.len())
}

fn introduces_supplemental_union(value: &str) -> bool {
    let normalized = normalize_whitespace(value).to_ascii_lowercase();
    normalized.starts_with("and exactly one ")
        && normalized.ends_with(':')
        && normalized
            .split_ascii_whitespace()
            .any(|word| word.trim_end_matches(':') == "body")
}

fn is_exact_or_connector(value: &str) -> bool {
    normalize_whitespace(value).eq_ignore_ascii_case("or")
}

fn reference_wrapper_owner(family: &str) -> bool {
    matches!(
        family,
        "StrongRef"
            | "StrongMarkerRef"
            | "StrongCommandRef"
            | "ConditionalCoordinateRef"
            | "ConditionalMarkerRef"
            | "ConditionalCommandRef"
            | "WeakMarkerIdentity"
            | "WeakDigest"
    )
}

fn structural_fragment(text: &str) -> bool {
    let trimmed = text.trim_start();
    text.contains('{')
        || matches!(has_top_level_pipe(text), Ok(true))
        || matches!(top_level_assignment(text), Ok(Some(_)))
        || (trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && trimmed
                .bytes()
                .any(|byte| matches!(byte, b':' | b'=' | b'{' | b'|')))
}

fn source_line_ranges(source: &str) -> Vec<Range<usize>> {
    if source.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' && index + 1 < source.len() {
            starts.push(index + 1);
        }
    }
    starts
        .iter()
        .copied()
        .enumerate()
        .map(|(index, start)| start..starts.get(index + 1).copied().unwrap_or(source.len()))
        .collect()
}

fn line_content_end(source: &str, range: &Range<usize>) -> usize {
    range.end - usize::from(source.as_bytes().get(range.end.saturating_sub(1)) == Some(&b'\n'))
}

fn extract_markdown_fragments(
    source_map: &SourceMap<'_>,
) -> (Vec<MarkdownFragment>, Vec<AmbiguityOccurrence>) {
    let source = source_map.source;
    let lines = source_line_ranges(source);
    let mut fragments = Vec::new();
    let mut ambiguities = Vec::new();
    let mut line_index = 0;
    while line_index < lines.len() {
        let line = &lines[line_index];
        let content_end = line_content_end(source, line);
        let content = &source[line.start..content_end];
        if content.starts_with("```") {
            let body_start = lines
                .get(line_index + 1)
                .map(|next| next.start)
                .unwrap_or(content_end);
            let mut close_index = line_index + 1;
            while close_index < lines.len() {
                let close_line = &lines[close_index];
                let close_end = line_content_end(source, close_line);
                if source[close_line.start..close_end].starts_with("```") {
                    break;
                }
                close_index += 1;
            }
            let body_end = lines
                .get(close_index)
                .map(|close| close.start)
                .unwrap_or(source.len());
            let range = body_start..body_end;
            fragments.push(MarkdownFragment {
                id: fragments.len(),
                kind: FragmentKind::Fence,
                text: source[range.clone()].to_owned(),
                source_range: range,
                before: String::new(),
                after: String::new(),
            });
            if close_index == lines.len() {
                ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::UnterminatedCodeFence,
                    schema_family: None,
                    path: None,
                    raw: content.to_owned(),
                    reason: "Markdown code fence has no closing fence".to_owned(),
                    affected_source_keys: BTreeSet::new(),
                    source_range: line.start..content_end,
                });
                break;
            }
            line_index = close_index + 1;
            continue;
        }

        let bytes = content.as_bytes();
        let mut pairs = Vec::new();
        let mut cursor = 0;
        let mut unmatched = None;
        while cursor < bytes.len() {
            let Some(open_relative) = bytes[cursor..].iter().position(|byte| *byte == b'`') else {
                break;
            };
            let open = cursor + open_relative;
            let Some(close_relative) = bytes[open + 1..].iter().position(|byte| *byte == b'`')
            else {
                unmatched = Some(open);
                break;
            };
            let close = open + 1 + close_relative;
            pairs.push((open, close));
            cursor = close + 1;
        }
        for (pair_index, (open, close)) in pairs.iter().copied().enumerate() {
            let previous_end = pairs
                .get(pair_index.wrapping_sub(1))
                .map(|(_, previous_close)| previous_close + 1)
                .unwrap_or_default();
            let next_start = pairs
                .get(pair_index + 1)
                .map(|(next_open, _)| *next_open)
                .unwrap_or(content.len());
            let range = line.start + open + 1..line.start + close;
            fragments.push(MarkdownFragment {
                id: fragments.len(),
                kind: FragmentKind::Inline,
                text: source[range.clone()].to_owned(),
                source_range: range,
                before: content[previous_end..open].to_owned(),
                after: content[close + 1..next_start].to_owned(),
            });
        }
        if let Some(open) = unmatched {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnterminatedInlineCode,
                schema_family: None,
                path: None,
                raw: content[open + 1..].to_owned(),
                reason: "Markdown inline-code opener has no closing backtick on its physical line"
                    .to_owned(),
                affected_source_keys: BTreeSet::new(),
                source_range: line.start + open..content_end,
            });
        }
        line_index += 1;
    }
    (fragments, ambiguities)
}

#[derive(Debug, Clone)]
struct ProseLink {
    display_name: String,
    owner_fragment: usize,
    rhs_fragments: Vec<usize>,
    supplemental_union_fragments: Vec<usize>,
    cue: String,
}

#[derive(Debug, Clone)]
struct BoldLink {
    display_name: String,
    declaration_range: Range<usize>,
    expression_range: Range<usize>,
    rhs_fragment: usize,
}

fn simple_type_display(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (display, consumed) = parse_type_display(trimmed)?;
    (consumed == trimmed.len()).then_some(display)
}

/// The one reader for a backticked prose definition head.
///
/// Keep both discovery and continuation termination delegated here. A second
/// spelling of this predicate inside either scan is how one owner can consume
/// the next owner's structural body.
fn prose_definition_head(fragment: &MarkdownFragment) -> Option<String> {
    if fragment.kind != FragmentKind::Inline {
        return None;
    }
    let display_name = simple_type_display(&fragment.text)?;
    has_definition_cue(&fragment.after).then_some(display_name)
}

fn prose_schema_links(
    fragments: &[MarkdownFragment],
    source_map: &SourceMap<'_>,
) -> Vec<ProseLink> {
    let mut by_line: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, fragment) in fragments.iter().enumerate() {
        if fragment.kind == FragmentKind::Inline {
            by_line
                .entry(source_map.position(fragment.source_range.start).line)
                .or_default()
                .push(index);
        }
    }
    let mut links = Vec::new();
    for indexes in by_line.values_mut() {
        indexes.sort_by_key(|index| fragments[*index].source_range.start);
        for (position, fragment_index) in indexes.iter().copied().enumerate() {
            let fragment = &fragments[fragment_index];
            let Some(display_name) = prose_definition_head(fragment) else {
                continue;
            };
            let mut cue = normalize_whitespace(&fragment.after);
            let mut rhs_fragments = Vec::new();
            let mut scan = position;
            let mut connector_length = cue.len();
            let mut stopped = sentence_ends(&fragment.after);
            while !stopped && connector_length <= 300 {
                let Some(candidate_index) = indexes.get(scan + 1).copied() else {
                    break;
                };
                let candidate = &fragments[candidate_index];
                if prose_definition_head(candidate).is_some() {
                    break;
                }
                if prose_body_candidate(&display_name, &candidate.text) {
                    rhs_fragments.push(candidate_index);
                    scan += 1;
                    break;
                }
                connector_length += candidate.text.len() + candidate.after.len();
                let next_cue = normalize_whitespace(&candidate.after);
                if !next_cue.is_empty() {
                    if !cue.is_empty() {
                        cue.push(' ');
                    }
                    cue.push_str(&next_cue);
                }
                stopped = sentence_ends(&candidate.after);
                scan += 1;
            }
            if rhs_fragments.first().is_some_and(|index| {
                leading_assignment(&fragments[*index].text)
                    .is_some_and(|(assigned, _)| assigned == display_name)
            }) {
                continue;
            }
            let mut supplemental_union_fragments = Vec::new();
            if !rhs_fragments.is_empty() && cue_names_union(&cue) {
                let primary_is_record = complete_record_fragment(&fragments[rhs_fragments[0]].text);
                if primary_is_record
                    && introduces_supplemental_union(&fragments[indexes[scan]].after)
                    && let Some(candidate_index) = indexes.get(scan + 1).copied()
                {
                    let candidate = &fragments[candidate_index];
                    if prose_definition_head(candidate).is_none() {
                        if union_continuation_candidate(&candidate.text) {
                            supplemental_union_fragments.push(candidate_index);
                        } else if complete_union_arm_fragment(&candidate.text) {
                            let mut arms = vec![candidate_index];
                            let mut arm_scan = scan + 1;
                            while let Some(next_index) = indexes.get(arm_scan + 1).copied() {
                                let previous = &fragments[indexes[arm_scan]];
                                if sentence_ends(&previous.after)
                                    || !is_exact_or_connector(&previous.after)
                                {
                                    break;
                                }
                                let next = &fragments[next_index];
                                if prose_definition_head(next).is_some()
                                    || !complete_union_arm_fragment(&next.text)
                                {
                                    break;
                                }
                                arms.push(next_index);
                                arm_scan += 1;
                            }
                            if arms.len() >= 2 {
                                supplemental_union_fragments = arms;
                            }
                        }
                    }
                } else if !primary_is_record {
                    let mut separator_seen = false;
                    while let Some(candidate_index) = indexes.get(scan + 1).copied() {
                        let previous = &fragments[indexes[scan]];
                        if sentence_ends(&previous.after) {
                            break;
                        }
                        separator_seen |= contains_continuation_separator(&previous.after);
                        let candidate = &fragments[candidate_index];
                        if prose_definition_head(candidate).is_some() {
                            break;
                        }
                        if separator_seen && union_continuation_candidate(&candidate.text) {
                            rhs_fragments.push(candidate_index);
                            separator_seen = false;
                        }
                        scan += 1;
                    }
                }
            }
            // A conjunction distributes the definition.  In "`A` and `B` are
            // <definition>" only B's trailing text carries the cue, so
            // `prose_definition_head` never sees A as a head and A is dropped
            // from the census entirely (measured 13 of 13 such sites).  Walk
            // back from a CONFIRMED head across pure " and " connectors and give
            // every co-named conjunct the same right-hand side.
            //
            // Fail-closed by construction: this pass is anchored to a head that
            // was already confirmed by the unchanged reader above, so it can only
            // ATTACH names to an existing definition and can never invent one.
            // It stops at the first fragment that is not a bare type display, is
            // already a head in its own right, or is joined by anything other
            // than the bare conjunction.
            let mut co_heads = Vec::new();
            let mut back = position;
            while back > 0 {
                let previous_index = indexes[back - 1];
                let previous = &fragments[previous_index];
                if normalize_whitespace(&previous.after) != CONJUNCTION_CONNECTOR {
                    break;
                }
                if prose_definition_head(previous).is_some() {
                    break;
                }
                let Some(co_name) = simple_type_display(&previous.text) else {
                    break;
                };
                co_heads.push((co_name, previous_index));
                back -= 1;
            }
            for (co_name, co_index) in co_heads.into_iter().rev() {
                links.push(ProseLink {
                    display_name: co_name,
                    owner_fragment: co_index,
                    rhs_fragments: rhs_fragments.clone(),
                    supplemental_union_fragments: supplemental_union_fragments.clone(),
                    cue: cue.clone(),
                });
            }
            links.push(ProseLink {
                display_name,
                owner_fragment: fragment_index,
                rhs_fragments,
                supplemental_union_fragments,
                cue,
            });
        }
    }
    links
}

/// The range, within `before`, of the bold owner name that owns the structural
/// fragment `before` precedes. Two spellings, one reader.
///
/// A. `**Name**: {…}` — a bold owner immediately colon-introducing its body.
/// B. `**Name / Other / Third.** A <phrase> is {…}` — a bold heading listing the
///    types the paragraph defines, whose FIRST body is introduced by an anonymous
///    noun phrase instead of a backticked name. The phrase is a paraphrase of the
///    heading's first name, so the heading is the attribution.
///
/// B cannot collide with `prose_schema_links`, which binds a backticked intro
/// (`` `ControlCommand` is {…} ``). A backticked name is its own inline fragment,
/// so for a backtick-introduced body `before` is just " is " and never begins with
/// `**`. That is why B needs no explicit "reject a backticked phrase" guard —
/// one was measured and could not fire on any input. `bold_owner_name_range_b_
/// cannot_claim_a_backticked_intro` pins that rather than leaving it to comment.
///
/// B reads ONLY the appendix. The plan also spells a named `CommitCommand` body at
/// plan line 393, whose 21 members match 1912's in name and order but leave two
/// array element types unelaborated. That line is outside `[source_manifest]`
/// (1388-2728), so this reader never sees it, never compares the two spellings and
/// never normalises one to the other: it records exactly what the appendix spells,
/// element types included. Choosing between the spellings would be a durable-format
/// ruling, and it is not this reader's to make.
fn bold_owner_name_range(before: &str) -> Option<Range<usize>> {
    let before_range = trim_range(before, 0..before.len());
    let colon_index = before_range.end.checked_sub(1)?;
    if before.as_bytes().get(colon_index) == Some(&b':') {
        // Shape A.
        let prefix_range = trim_range(before, before_range.start..colon_index);
        if !before[prefix_range.clone()].ends_with("**") {
            return None;
        }
        let close_start = prefix_range.end - 2;
        let open = before[..close_start].rfind("**")?;
        return Some(trim_range(before, open + 2..close_start));
    }
    // Shape B. Anchor to the current source line: `before` reaches back to the
    // previous inline fragment, which is usually in an earlier paragraph.
    let line_start = before[..before_range.end]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_range = trim_range(before, line_start..before_range.end);
    let line = &before[line_range.clone()];
    if !line.starts_with("**") || !line.ends_with(" is") {
        return None;
    }
    let close = line[2..].find("**")? + 2;
    // The heading's FIRST name owns the first body. A heading lists the types its
    // paragraph defines in the order it defines them, so any later name is a
    // different type's; `heading_led_binding_takes_the_first_name_not_a_later_one`
    // is the control that keeps this honest.
    let first = line[2..close].split('/').next()?;
    let lead = first.len() - first.trim_start().len();
    let trimmed = first.trim();
    let name = trimmed.strip_suffix('.').unwrap_or(trimmed).trim_end();
    let start = line_range.start + 2 + lead;
    let end = start + name.len();
    (start < end).then_some(start..end)
}

fn bold_schema_links(fragments: &[MarkdownFragment]) -> Vec<BoldLink> {
    let mut links = Vec::new();
    for (index, fragment) in fragments.iter().enumerate() {
        if fragment.kind != FragmentKind::Inline || !structural_fragment(&fragment.text) {
            continue;
        }
        let before = &fragment.before;
        let Some(candidate_range) = bold_owner_name_range(before) else {
            continue;
        };
        let Some(display_name) = simple_type_display(&before[candidate_range.clone()]) else {
            continue;
        };
        let Some(before_source_start) = fragment
            .source_range
            .start
            .checked_sub(fragment.before.len() + 1)
        else {
            continue;
        };
        let expression_range = if let Some((assigned, rhs)) = leading_assignment(&fragment.text) {
            if assigned != display_name {
                continue;
            }
            fragment.source_range.start + rhs.start..fragment.source_range.start + rhs.end
        } else {
            fragment.source_range.clone()
        };
        links.push(BoldLink {
            display_name,
            declaration_range: before_source_start + candidate_range.start
                ..before_source_start + candidate_range.end,
            expression_range,
            rhs_fragment: index,
        });
    }
    links
}

fn make_schema_occurrence(
    display_name: String,
    owner_status: SchemaOwnerStatus,
    definition_kind: DefinitionKind,
    declaration_range: Range<usize>,
    expression: Option<MappedText>,
) -> Option<SchemaOccurrence> {
    let key = family_and_generic(&display_name)?;
    let supplemental_unions = Vec::new();
    let expression_sha256 = schema_expression_sha256(&expression, &supplemental_unions);
    Some(SchemaOccurrence {
        key,
        display_name,
        owner_status,
        definition_kind,
        complete_top_level_map_definition: matches!(
            definition_kind,
            DefinitionKind::InlineAlias | DefinitionKind::BoldOwnerStructural
        ),
        declaration_range,
        expression,
        supplemental_unions,
        expression_sha256,
    })
}

fn schema_expression_sha256(
    expression: &Option<MappedText>,
    supplemental_unions: &[MappedText],
) -> String {
    if supplemental_unions.is_empty() {
        return expression
            .as_ref()
            .map(|value| sha256_hex(normalize_whitespace(&value.text).as_bytes()))
            .unwrap_or_else(|| sha256_hex(b""));
    }
    let mut transcript = b"fgdb:appendix-source-schema-with-supplemental-unions:v1".to_vec();
    for part in expression.iter().chain(supplemental_unions) {
        let normalized = normalize_whitespace(&part.text);
        let length = normalized.len().to_string();
        transcript.extend_from_slice(length.as_bytes());
        transcript.push(b':');
        transcript.extend_from_slice(normalized.as_bytes());
    }
    sha256_hex(&transcript)
}

fn affected_source_key(key: StructuralCandidateKey) -> BTreeSet<StructuralCandidateKey> {
    BTreeSet::from([key])
}

fn delimiter_ambiguity(
    issue: DelimiterIssue,
    mapped: &MappedText,
    family: Option<String>,
    path: Option<String>,
    raw: String,
    reason: &str,
    affected_source_keys: BTreeSet<StructuralCandidateKey>,
) -> AmbiguityOccurrence {
    let offset = issue.offset.min(mapped.text.len());
    let range = if offset == mapped.text.len() {
        mapped.source_range(0..mapped.text.len())
    } else {
        mapped.source_range(offset..(offset + 1).min(mapped.text.len()))
    };
    AmbiguityOccurrence {
        kind: if issue.mismatched {
            AmbiguityKind::MismatchedDelimiter
        } else {
            AmbiguityKind::UnbalancedDefinition
        },
        schema_family: family,
        path,
        raw,
        reason: reason.to_owned(),
        affected_source_keys,
        source_range: range,
    }
}

fn leading_assignment(text: &str) -> Option<(String, Range<usize>)> {
    let (display, consumed) = parse_type_display(text)?;
    let bytes = text.as_bytes();
    let equals = skip_ascii_whitespace(bytes, consumed);
    if bytes.get(equals) != Some(&b'=') {
        return None;
    }
    Some((display, trim_range(text, equals + 1..text.len())))
}

fn leading_record(text: &str) -> Option<(String, usize)> {
    let (display, consumed) = parse_type_display(text)?;
    let bytes = text.as_bytes();
    let mut cursor = skip_ascii_whitespace(bytes, consumed);
    if bytes.get(cursor..cursor + 2) == Some(b"is")
        && bytes
            .get(cursor + 2)
            .is_none_or(|byte| !is_identifier_continue(*byte))
    {
        cursor = skip_ascii_whitespace(bytes, cursor + 2);
    }
    (bytes.get(cursor) == Some(&b'{')).then_some((display, cursor))
}

fn direct_schemas_from_inline(
    fragment: &MarkdownFragment,
    source: &str,
    claimed_by_link: bool,
    occurrences: &mut Vec<SchemaOccurrence>,
    ambiguities: &mut Vec<AmbiguityOccurrence>,
    claimed_ranges: &mut Vec<Range<usize>>,
) {
    let whole = MappedText::from_source(source, fragment.source_range.clone());
    if leading_assignment(&fragment.text).is_none()
        && matches!(has_top_level_pipe(&fragment.text), Ok(true))
    {
        return;
    }
    let segments = match split_top_level(&fragment.text, b";") {
        Ok(segments) => segments,
        Err(issue) => {
            if let Some((display, rhs)) = leading_assignment(&fragment.text) {
                let declaration_end = fragment.source_range.start
                    + parse_type_display(&fragment.text)
                        .map(|(_, consumed)| consumed)
                        .unwrap_or_default();
                let expression = whole.subrange(rhs);
                if let Some(occurrence) = make_schema_occurrence(
                    display.clone(),
                    SchemaOwnerStatus::ConfirmedTopLevel,
                    DefinitionKind::InlineAlias,
                    fragment.source_range.start..declaration_end,
                    Some(expression),
                ) {
                    ambiguities.push(delimiter_ambiguity(
                        issue,
                        &whole,
                        Some(occurrence.key.family.clone()),
                        Some(occurrence.display_name.clone()),
                        normalize_whitespace(&fragment.text),
                        "inline alias contains an unbalanced or mismatched delimiter",
                        affected_source_key(StructuralCandidateKey::Schema(occurrence.key.clone())),
                    ));
                    occurrences.push(occurrence);
                    claimed_ranges.push(fragment.source_range.clone());
                    return;
                }
            }
            if let Some((display, _open)) = leading_record(&fragment.text) {
                let declaration_end = fragment.source_range.start
                    + parse_type_display(&fragment.text)
                        .map(|(_, consumed)| consumed)
                        .unwrap_or_default();
                if let Some(occurrence) = make_schema_occurrence(
                    display.clone(),
                    if has_definition_cue(&fragment.after)
                        || family_and_generic(&display)
                            .is_some_and(|key| reference_wrapper_owner(&key.family))
                    {
                        SchemaOwnerStatus::ConfirmedTopLevel
                    } else {
                        SchemaOwnerStatus::AmbiguousUnownedStructure
                    },
                    DefinitionKind::InlineUnbalanced,
                    fragment.source_range.start..declaration_end,
                    Some(whole.clone()),
                ) {
                    ambiguities.push(delimiter_ambiguity(
                        issue,
                        &whole,
                        Some(occurrence.key.family.clone()),
                        Some(occurrence.display_name.clone()),
                        normalize_whitespace(&fragment.text),
                        "inline record has no balanced closing delimiter",
                        affected_source_key(StructuralCandidateKey::Schema(occurrence.key.clone())),
                    ));
                    occurrences.push(occurrence);
                    claimed_ranges.push(fragment.source_range.clone());
                    return;
                }
            }
            ambiguities.push(delimiter_ambiguity(
                issue,
                &whole,
                None,
                None,
                normalize_whitespace(&fragment.text),
                "structural inline-code fragment has invalid delimiter structure",
                BTreeSet::new(),
            ));
            return;
        }
    };

    for segment in segments {
        let trimmed = trim_range(&fragment.text, segment.start..segment.end);
        if trimmed.is_empty() {
            continue;
        }
        let text = &fragment.text[trimmed.clone()];
        let mapped = whole.subrange(trimmed.clone());
        if claimed_by_link {
            continue;
        }
        if let Some((display, rhs)) = leading_assignment(text) {
            let display_length = parse_type_display(text)
                .map(|(_, consumed)| consumed)
                .unwrap_or_default();
            if let Some(occurrence) = make_schema_occurrence(
                display,
                SchemaOwnerStatus::ConfirmedTopLevel,
                DefinitionKind::InlineAlias,
                mapped.source_range(0..display_length),
                Some(mapped.subrange(rhs)),
            ) {
                occurrences.push(occurrence);
                claimed_ranges.push(mapped.source_range(0..mapped.text.len()));
            }
            continue;
        }
        let Some((display, open_index)) = leading_record(text) else {
            continue;
        };
        let display_length = parse_type_display(text)
            .map(|(_, consumed)| consumed)
            .unwrap_or_default();
        match matching_delimiter(text, open_index) {
            Ok(_) => {
                let owner_status = if has_definition_cue(&fragment.after)
                    || family_and_generic(&display)
                        .is_some_and(|key| reference_wrapper_owner(&key.family))
                {
                    SchemaOwnerStatus::ConfirmedTopLevel
                } else {
                    SchemaOwnerStatus::AmbiguousUnownedStructure
                };
                if let Some(occurrence) = make_schema_occurrence(
                    display,
                    owner_status,
                    DefinitionKind::InlineRecord,
                    mapped.source_range(0..display_length),
                    Some(mapped.clone()),
                ) {
                    if owner_status == SchemaOwnerStatus::AmbiguousUnownedStructure {
                        ambiguities.push(AmbiguityOccurrence {
                            kind: AmbiguityKind::AmbiguousSchemaOwner,
                            schema_family: Some(occurrence.key.family.clone()),
                            path: Some(occurrence.display_name.clone()),
                            raw: normalize_whitespace(text),
                            reason: "leading named record has no explicit top-level ownership cue"
                                .to_owned(),
                            affected_source_keys: affected_source_key(
                                StructuralCandidateKey::Schema(occurrence.key.clone()),
                            ),
                            source_range: mapped.source_range(0..mapped.text.len()),
                        });
                    }
                    occurrences.push(occurrence);
                    claimed_ranges.push(mapped.source_range(0..mapped.text.len()));
                }
            }
            Err(issue) => {
                if let Some(occurrence) = make_schema_occurrence(
                    display,
                    SchemaOwnerStatus::AmbiguousUnownedStructure,
                    DefinitionKind::InlineUnbalanced,
                    mapped.source_range(0..display_length),
                    Some(mapped.clone()),
                ) {
                    ambiguities.push(delimiter_ambiguity(
                        issue,
                        &mapped,
                        Some(occurrence.key.family.clone()),
                        Some(occurrence.display_name.clone()),
                        normalize_whitespace(text),
                        "inline record has no balanced closing delimiter",
                        affected_source_key(StructuralCandidateKey::Schema(occurrence.key.clone())),
                    ));
                    occurrences.push(occurrence);
                    claimed_ranges.push(mapped.source_range(0..mapped.text.len()));
                }
            }
        }
    }
}

fn direct_schemas_from_fence(
    fragment: &MarkdownFragment,
    source: &str,
    occurrences: &mut Vec<SchemaOccurrence>,
    ambiguities: &mut Vec<AmbiguityOccurrence>,
    claimed_ranges: &mut Vec<Range<usize>>,
) {
    let whole = MappedText::from_source(source, fragment.source_range.clone());
    let mut cursor = 0;
    while cursor < fragment.text.len() {
        let line_start = cursor;
        let line_end = fragment.text[cursor..]
            .find('\n')
            .map(|relative| cursor + relative)
            .unwrap_or(fragment.text.len());
        let candidate_start = skip_ascii_whitespace(fragment.text.as_bytes(), line_start);
        if candidate_start < line_end {
            let candidate = &fragment.text[candidate_start..];
            if let Some((display, open_relative)) = leading_record(candidate) {
                let open_index = candidate_start + open_relative;
                let display_length = parse_type_display(candidate)
                    .map(|(_, consumed)| consumed)
                    .unwrap_or_default();
                match matching_delimiter(&fragment.text, open_index) {
                    Ok(close_index) => {
                        let expression_end = fragment.text[close_index + 1..]
                            .find('\n')
                            .map(|relative| close_index + 1 + relative)
                            .unwrap_or(fragment.text.len());
                        if let Some(occurrence) = make_schema_occurrence(
                            display,
                            SchemaOwnerStatus::ConfirmedTopLevel,
                            DefinitionKind::FencedRecord,
                            whole.source_range(candidate_start..candidate_start + display_length),
                            Some(whole.subrange(candidate_start..expression_end)),
                        ) {
                            occurrences.push(occurrence);
                            claimed_ranges
                                .push(whole.source_range(candidate_start..expression_end));
                        }
                        cursor = close_index + 1;
                        continue;
                    }
                    Err(issue) => {
                        let expression = whole.subrange(candidate_start..fragment.text.len());
                        if let Some(occurrence) = make_schema_occurrence(
                            display,
                            SchemaOwnerStatus::ConfirmedTopLevel,
                            DefinitionKind::FencedUnbalanced,
                            whole.source_range(candidate_start..candidate_start + display_length),
                            Some(expression.clone()),
                        ) {
                            ambiguities.push(delimiter_ambiguity(
                                issue,
                                &whole,
                                Some(occurrence.key.family.clone()),
                                Some(occurrence.display_name.clone()),
                                normalize_whitespace(&expression.text),
                                "fenced declaration has no balanced closing delimiter",
                                affected_source_key(StructuralCandidateKey::Schema(
                                    occurrence.key.clone(),
                                )),
                            ));
                            occurrences.push(occurrence);
                            claimed_ranges
                                .push(whole.source_range(candidate_start..fragment.text.len()));
                        }
                        break;
                    }
                }
            }
        }
        cursor = if line_end < fragment.text.len() {
            line_end + 1
        } else {
            fragment.text.len()
        };
    }
}

fn extract_schema_occurrences(
    source_map: &SourceMap<'_>,
    fragments: &[MarkdownFragment],
    mut ambiguities: Vec<AmbiguityOccurrence>,
) -> (Vec<SchemaOccurrence>, Vec<AmbiguityOccurrence>) {
    let prose_links = prose_schema_links(fragments, source_map);
    let bold_links = bold_schema_links(fragments);
    let mut linked_rhs = BTreeSet::new();
    for link in &prose_links {
        linked_rhs.extend(link.rhs_fragments.iter().map(|index| fragments[*index].id));
        linked_rhs.extend(
            link.supplemental_union_fragments
                .iter()
                .map(|index| fragments[*index].id),
        );
    }
    linked_rhs.extend(
        bold_links
            .iter()
            .map(|link| fragments[link.rhs_fragment].id),
    );

    let mut occurrences = Vec::new();
    let mut claimed_ranges = Vec::new();
    for fragment in fragments {
        match fragment.kind {
            FragmentKind::Fence => direct_schemas_from_fence(
                fragment,
                source_map.source,
                &mut occurrences,
                &mut ambiguities,
                &mut claimed_ranges,
            ),
            FragmentKind::Inline => direct_schemas_from_inline(
                fragment,
                source_map.source,
                linked_rhs.contains(&fragment.id),
                &mut occurrences,
                &mut ambiguities,
                &mut claimed_ranges,
            ),
        }
    }

    for link in bold_links {
        let fragment = &fragments[link.rhs_fragment];
        if let Some(occurrence) = make_schema_occurrence(
            link.display_name,
            SchemaOwnerStatus::ConfirmedTopLevel,
            DefinitionKind::BoldOwnerStructural,
            link.declaration_range,
            Some(MappedText::from_source(
                source_map.source,
                link.expression_range,
            )),
        ) {
            occurrences.push(occurrence);
            claimed_ranges.push(fragment.source_range.clone());
        }
    }

    for link in prose_links {
        let owner = &fragments[link.owner_fragment];
        if link.rhs_fragments.is_empty() {
            if let Some(occurrence) = make_schema_occurrence(
                link.display_name,
                SchemaOwnerStatus::NamedConceptNoBody,
                DefinitionKind::ProseDefinitionNoBody,
                owner.source_range.clone(),
                None,
            ) {
                ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::DefinitionWithoutStructuralBody,
                    schema_family: Some(occurrence.key.family.clone()),
                    path: None,
                    raw: owner.text.clone(),
                    reason: "definitional prose names a type but supplies no adjacent structural expression"
                        .to_owned(),
                    affected_source_keys: affected_source_key(
                        StructuralCandidateKey::Schema(occurrence.key.clone()),
                    ),
                    source_range: owner.source_range.clone(),
                });
                occurrences.push(occurrence);
            }
            continue;
        }
        let ranges: Vec<_> = link
            .rhs_fragments
            .iter()
            .map(|index| fragments[*index].source_range.clone())
            .collect();
        let expression = MappedText::joined(source_map.source, &ranges);
        if let Some(mut occurrence) = make_schema_occurrence(
            link.display_name,
            SchemaOwnerStatus::ConfirmedTopLevel,
            DefinitionKind::ProseLinkedStructural,
            owner.source_range.clone(),
            Some(expression),
        ) {
            occurrence.complete_top_level_map_definition =
                starts_with_word(&link.cue, "is") || starts_with_word(&link.cue, "are");
            if !link.supplemental_union_fragments.is_empty() {
                let supplemental_ranges: Vec<_> = link
                    .supplemental_union_fragments
                    .iter()
                    .map(|index| fragments[*index].source_range.clone())
                    .collect();
                occurrence
                    .supplemental_unions
                    .push(MappedText::joined(source_map.source, &supplemental_ranges));
                occurrence.expression_sha256 = schema_expression_sha256(
                    &occurrence.expression,
                    &occurrence.supplemental_unions,
                );
                claimed_ranges.extend(supplemental_ranges);
            }
            occurrences.push(occurrence);
            claimed_ranges.extend(ranges);
        }
        let _ = link.cue;
    }

    claimed_ranges.sort_by_key(|range| (range.start, range.end));
    for fragment in fragments {
        let mut cursor = fragment.source_range.start;
        let mut unclaimed = Vec::new();
        for claimed in claimed_ranges.iter().filter(|claimed| {
            claimed.start < fragment.source_range.end && claimed.end > fragment.source_range.start
        }) {
            let claimed_start = claimed.start.max(fragment.source_range.start);
            let claimed_end = claimed.end.min(fragment.source_range.end);
            if cursor < claimed_start {
                unclaimed.push(cursor..claimed_start);
            }
            cursor = cursor.max(claimed_end);
        }
        if cursor < fragment.source_range.end {
            unclaimed.push(cursor..fragment.source_range.end);
        }
        for range in unclaimed {
            let text = &source_map.source[range.clone()];
            if !structural_fragment(text) {
                continue;
            }
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnownedStructuralFragment,
                schema_family: None,
                path: None,
                raw: normalize_whitespace(text),
                reason: "schema-like notation has no owner under the conservative source grammar"
                    .to_owned(),
                affected_source_keys: BTreeSet::new(),
                source_range: range,
            });
        }
    }

    let mut unique = BTreeMap::new();
    for occurrence in occurrences {
        let line = source_map.position(occurrence.declaration_range.start).line;
        let key = (
            occurrence.key.family.clone(),
            occurrence.key.generic_signature.clone(),
            line,
            occurrence.declaration_range.start,
            occurrence.declaration_range.end,
            occurrence.expression_sha256.clone(),
        );
        unique.entry(key).or_insert(occurrence);
    }
    (unique.into_values().collect(), ambiguities)
}

const MAX_STRUCTURAL_NESTING: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpressionShape {
    Alias,
    Record,
}

fn outer_expression_body(
    schema: &SchemaOccurrence,
) -> Result<Option<(MappedText, ExpressionShape, Option<MappedText>)>, DelimiterIssue> {
    let Some(expression) = schema.expression.as_ref() else {
        return Ok(None);
    };
    let trimmed = trim_range(&expression.text, 0..expression.text.len());
    let mapped = expression.subrange(trimmed);
    if mapped.text.starts_with('{') {
        let close = matching_delimiter(&mapped.text, 0)?;
        let trailing = trim_range(&mapped.text, close + 1..mapped.text.len());
        return Ok(Some((
            mapped.subrange(1..close),
            ExpressionShape::Record,
            (!trailing.is_empty()).then(|| mapped.subrange(trailing)),
        )));
    }
    if matches!(
        schema.definition_kind,
        DefinitionKind::FencedRecord
            | DefinitionKind::FencedUnbalanced
            | DefinitionKind::InlineRecord
            | DefinitionKind::InlineUnbalanced
    ) && let Some(open) = mapped.text.find('{')
    {
        let close = matching_delimiter(&mapped.text, open)?;
        let trailing = trim_range(&mapped.text, close + 1..mapped.text.len());
        return Ok(Some((
            mapped.subrange(open + 1..close),
            ExpressionShape::Record,
            (!trailing.is_empty()).then(|| mapped.subrange(trailing)),
        )));
    }
    Ok(Some((mapped, ExpressionShape::Alias, None)))
}

fn qualified_identifier_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut end = parse_identifier(bytes, start)?;
    while bytes.get(end..end + 2) == Some(b"::") {
        let next = end + 2;
        let Some(next_end) = parse_identifier(bytes, next) else {
            break;
        };
        end = next_end;
    }
    Some(end)
}

fn first_arm_token(text: &str) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let start = skip_ascii_whitespace(bytes, 0);
    let first = bytes.get(start).copied()?;
    if first == b'*' {
        return Some(start..start + 1);
    }
    if first.is_ascii_digit() {
        let mut end = if bytes.get(start..start + 2) == Some(b"0x") {
            let mut cursor = start + 2;
            while bytes.get(cursor).is_some_and(u8::is_ascii_hexdigit) {
                cursor += 1;
            }
            (cursor > start + 2).then_some(cursor)?
        } else {
            let mut cursor = start + 1;
            while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
            }
            cursor
        };
        let label_start = skip_ascii_whitespace(bytes, end);
        if label_start > end
            && let Some(label_end) = qualified_identifier_end(bytes, label_start)
        {
            end = label_end;
        }
        return Some(start..end);
    }
    qualified_identifier_end(bytes, start).map(|end| start..end)
}

fn infer_cardinality(name: &str, remainder: &str) -> Cardinality {
    let stripped = remainder.trim();
    if name.ends_with('s') && stripped.starts_with('[') {
        Cardinality::Many
    } else if stripped.starts_with('[')
        || stripped
            .find('[')
            .zip(stripped.rfind(']'))
            .is_some_and(|(open, close)| open < close)
    {
        Cardinality::ManyOrIndexed
    } else if stripped.starts_with('?') || stripped.ends_with('?') || stripped.contains("Option<") {
        Cardinality::Optional
    } else {
        Cardinality::One
    }
}

fn trailing_upper_identifier(text: &str) -> Option<&str> {
    let trimmed = text.trim_end();
    let bytes = trimmed.as_bytes();
    let mut start = bytes.len();
    while start > 0 && is_identifier_continue(bytes[start - 1]) {
        start -= 1;
    }
    (start < bytes.len() && bytes[start].is_ascii_uppercase()).then_some(&trimmed[start..])
}

fn outermost_record_ranges(text: &str) -> Result<Vec<(usize, usize)>, DelimiterIssue> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == active_quote {
                quote = None;
            }
            cursor += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            cursor += 1;
            continue;
        }
        if byte != b'{' {
            cursor += 1;
            continue;
        }
        let close = matching_delimiter(text, cursor)?;
        ranges.push((cursor, close));
        cursor = close + 1;
    }
    Ok(ranges)
}

fn push_nesting_ambiguity(
    schema: &SchemaOccurrence,
    path: &str,
    mapped: &MappedText,
    owner_key: &StructuralCandidateKey,
    ambiguities: &mut Vec<AmbiguityOccurrence>,
) {
    ambiguities.push(AmbiguityOccurrence {
        kind: AmbiguityKind::NestingLimitExceeded,
        schema_family: Some(schema.key.family.clone()),
        path: Some(path.to_owned()),
        raw: normalize_whitespace(&mapped.text),
        reason: format!("structural nesting exceeds the limit of {MAX_STRUCTURAL_NESTING}"),
        affected_source_keys: affected_source_key(owner_key.clone()),
        source_range: mapped.source_range(0..mapped.text.len()),
    });
}

struct StructuralOccurrences {
    fields: Vec<FieldOccurrence>,
    unions: Vec<UnionOccurrence>,
    arms: Vec<ArmOccurrence>,
    ambiguities: Vec<AmbiguityOccurrence>,
}

fn parse_union(
    schema: &SchemaOccurrence,
    mapped: &MappedText,
    union_path: &str,
    owner_key: &StructuralCandidateKey,
    rows: &mut StructuralOccurrences,
    depth: usize,
    allow_typed_scalar_payload: bool,
) -> bool {
    if depth > MAX_STRUCTURAL_NESTING {
        push_nesting_ambiguity(schema, union_path, mapped, owner_key, &mut rows.ambiguities);
        return false;
    }
    let alternatives = match split_top_level(&mapped.text, b"|") {
        Ok(alternatives) => alternatives,
        Err(issue) => {
            rows.ambiguities.push(delimiter_ambiguity(
                issue,
                mapped,
                Some(schema.key.family.clone()),
                Some(union_path.to_owned()),
                normalize_whitespace(&mapped.text),
                "union expression contains an unbalanced or mismatched delimiter",
                affected_source_key(owner_key.clone()),
            ));
            return false;
        }
    };
    if alternatives.len() < 2 {
        return false;
    }
    let union_key = UnionCandidateKey {
        schema_family: schema.key.family.clone(),
        schema_owner: schema.display_name.clone(),
        union_path: union_path.to_owned(),
    };
    let union_index = rows.unions.len();
    rows.unions.push(UnionOccurrence {
        key: union_key.clone(),
        source_range: mapped.source_range(0..mapped.text.len()),
        evidence_ranges: Vec::new(),
        arm_names: BTreeSet::new(),
        unparsed_arm_count: 0,
    });
    let mut parsed = 0;
    for alternative in alternatives {
        let trimmed = trim_range(&mapped.text, alternative.start..alternative.end);
        if trimmed.is_empty() {
            let source_range = mapped.source_range(alternative.start..alternative.end);
            rows.unions[union_index].unparsed_arm_count += 1;
            rows.unions[union_index]
                .evidence_ranges
                .push(source_range.clone());
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnparsedUnionArm,
                schema_family: Some(schema.key.family.clone()),
                path: Some(union_path.to_owned()),
                raw: String::new(),
                reason: "top-level union contains an empty alternative".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Union(
                    union_key.clone(),
                )),
                source_range,
            });
            continue;
        }
        let alternative_mapped = mapped.subrange(trimmed);
        let Some(token) = first_arm_token(&alternative_mapped.text) else {
            rows.unions[union_index].unparsed_arm_count += 1;
            rows.unions[union_index]
                .evidence_ranges
                .push(alternative_mapped.source_range(0..alternative_mapped.text.len()));
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnparsedUnionArm,
                schema_family: Some(schema.key.family.clone()),
                path: Some(union_path.to_owned()),
                raw: normalize_whitespace(&alternative_mapped.text),
                reason: "top-level union alternative does not start with a stable arm token"
                    .to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Union(
                    union_key.clone(),
                )),
                source_range: alternative_mapped.source_range(0..alternative_mapped.text.len()),
            });
            continue;
        };
        let arm_name = normalize_whitespace(&alternative_mapped.text[token.clone()]);
        let arm_key = ArmCandidateKey {
            schema_family: schema.key.family.clone(),
            schema_owner: schema.display_name.clone(),
            union_path: union_path.to_owned(),
            arm_name: arm_name.clone(),
        };
        let new_arm = rows.unions[union_index].arm_names.insert(arm_name.clone());
        rows.unions[union_index]
            .evidence_ranges
            .push(alternative_mapped.source_range(0..alternative_mapped.text.len()));
        if !new_arm {
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(schema.key.family.clone()),
                path: Some(format!("{union_path}.{arm_name}")),
                raw: normalize_whitespace(&alternative_mapped.text),
                reason: "one union occurrence repeats the same arm name".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Arm(
                    arm_key.clone(),
                )),
                source_range: alternative_mapped.source_range(0..alternative_mapped.text.len()),
            });
        }
        let mut payload = None;
        let mut payload_body = None;
        if let Some(open) = alternative_mapped.text.find('{') {
            let prefix = trim_range(&alternative_mapped.text, token.end..open);
            if !prefix.is_empty() {
                let trailing = alternative_mapped.subrange(prefix);
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::UnparsedTrailingTokens,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(format!("{union_path}.{arm_name}")),
                    raw: normalize_whitespace(&trailing.text),
                    reason: "tokens between a union arm name and its record payload are not part of the closed source grammar"
                        .to_owned(),
                    affected_source_keys: affected_source_key(StructuralCandidateKey::Arm(
                        arm_key.clone(),
                    )),
                    source_range: trailing.source_range(0..trailing.text.len()),
                });
            }
            match matching_delimiter(&alternative_mapped.text, open) {
                Ok(close) => {
                    let body = alternative_mapped.subrange(open + 1..close);
                    payload = Some(normalize_whitespace(&body.text));
                    payload_body = Some(body);
                    let suffix = trim_range(
                        &alternative_mapped.text,
                        close + 1..alternative_mapped.text.len(),
                    );
                    if !suffix.is_empty() {
                        let trailing = alternative_mapped.subrange(suffix);
                        rows.ambiguities.push(AmbiguityOccurrence {
                            kind: AmbiguityKind::UnparsedTrailingTokens,
                            schema_family: Some(schema.key.family.clone()),
                            path: Some(format!("{union_path}.{arm_name}")),
                            raw: normalize_whitespace(&trailing.text),
                            reason: "tokens after a balanced union arm payload are not part of the closed source grammar"
                                .to_owned(),
                            affected_source_keys: affected_source_key(
                                StructuralCandidateKey::Arm(arm_key.clone()),
                            ),
                            source_range: trailing.source_range(0..trailing.text.len()),
                        });
                    }
                }
                Err(issue) => rows.ambiguities.push(delimiter_ambiguity(
                    issue,
                    &alternative_mapped,
                    Some(schema.key.family.clone()),
                    Some(format!("{union_path}.{arm_name}")),
                    normalize_whitespace(&alternative_mapped.text),
                    "union arm payload contains an unbalanced or mismatched delimiter",
                    affected_source_key(StructuralCandidateKey::Arm(arm_key.clone())),
                )),
            }
        } else {
            let trailing = trim_range(
                &alternative_mapped.text,
                token.end..alternative_mapped.text.len(),
            );
            if !trailing.is_empty() {
                let typed_payload = allow_typed_scalar_payload
                    && alternative_mapped.text.as_bytes().get(trailing.start) == Some(&b':')
                    && {
                        let payload_range =
                            trim_range(&alternative_mapped.text, trailing.start + 1..trailing.end);
                        if payload_range.is_empty() {
                            false
                        } else {
                            payload = Some(normalize_whitespace(
                                &alternative_mapped.text[payload_range],
                            ));
                            true
                        }
                    };
                if !typed_payload {
                    let trailing = alternative_mapped.subrange(trailing);
                    rows.ambiguities.push(AmbiguityOccurrence {
                        kind: AmbiguityKind::UnparsedTrailingTokens,
                        schema_family: Some(schema.key.family.clone()),
                        path: Some(format!("{union_path}.{arm_name}")),
                        raw: normalize_whitespace(&trailing.text),
                        reason:
                            "tokens after a union arm name are not part of the closed source grammar"
                                .to_owned(),
                        affected_source_keys: affected_source_key(StructuralCandidateKey::Arm(
                            arm_key.clone(),
                        )),
                        source_range: trailing.source_range(0..trailing.text.len()),
                    });
                }
            }
        }
        rows.arms.push(ArmOccurrence {
            key: arm_key.clone(),
            payload,
            raw: normalize_whitespace(&alternative_mapped.text),
            source_range: alternative_mapped.source_range(0..alternative_mapped.text.len()),
        });
        parsed += 1;
        if let Some(body) = payload_body {
            parse_record_fields(
                schema,
                &body,
                &format!("{union_path}.{arm_name}"),
                &StructuralCandidateKey::Arm(arm_key),
                rows,
                depth + 1,
            );
        }
    }
    parsed > 0
}

fn parse_record_fields(
    schema: &SchemaOccurrence,
    mapped: &MappedText,
    path: &str,
    owner_key: &StructuralCandidateKey,
    rows: &mut StructuralOccurrences,
    depth: usize,
) {
    if depth > MAX_STRUCTURAL_NESTING {
        push_nesting_ambiguity(schema, path, mapped, owner_key, &mut rows.ambiguities);
        return;
    }
    let pieces = match split_top_level(&mapped.text, b",") {
        Ok(pieces) => pieces,
        Err(issue) => {
            rows.ambiguities.push(delimiter_ambiguity(
                issue,
                mapped,
                Some(schema.key.family.clone()),
                Some(path.to_owned()),
                normalize_whitespace(&mapped.text),
                "record body contains an unbalanced or mismatched delimiter",
                affected_source_key(owner_key.clone()),
            ));
            return;
        }
    };
    let comma_separated = pieces.len() > 1;
    let mut seen_fields: BTreeMap<String, (String, Range<usize>)> = BTreeMap::new();
    let mut reported_duplicates = BTreeSet::new();
    for piece in pieces {
        let trimmed = trim_range(&mapped.text, piece.start..piece.end);
        if trimmed.is_empty() {
            if comma_separated {
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::UnparsedRecordItem,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(path.to_owned()),
                    raw: String::new(),
                    reason: "record body contains an empty comma-delimited item".to_owned(),
                    affected_source_keys: affected_source_key(owner_key.clone()),
                    source_range: mapped.source_range(piece.start..piece.end),
                });
            }
            continue;
        }
        let field_mapped = mapped.subrange(trimmed);
        let bytes = field_mapped.text.as_bytes();
        let start = skip_ascii_whitespace(bytes, 0);
        if !bytes
            .get(start)
            .copied()
            .is_some_and(is_lower_identifier_start)
        {
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnparsedRecordItem,
                schema_family: Some(schema.key.family.clone()),
                path: Some(path.to_owned()),
                raw: normalize_whitespace(&field_mapped.text),
                reason: "record item does not begin with a lowercase stable field name".to_owned(),
                affected_source_keys: affected_source_key(owner_key.clone()),
                source_range: field_mapped.source_range(0..field_mapped.text.len()),
            });
            continue;
        }
        let Some(name_end) = parse_identifier(bytes, start) else {
            continue;
        };
        let name = field_mapped.text[start..name_end].to_owned();
        let optional_marker = bytes.get(name_end) == Some(&b'?');
        let remainder_start = skip_ascii_whitespace(bytes, name_end + usize::from(optional_marker));
        let remainder = &field_mapped.text[remainder_start..];
        // A member-position slash token (`a/b/c_suffix`) compresses several
        // member names into one. Splitting at the first slash would name the
        // member `a` and call the rest its type, which is what this parser used
        // to do and is wrong in both halves; expanding it would invent names the
        // source does not spell, and the compression rule is demonstrably not
        // uniform (a10:1920 against its uncompressed sibling at a08:1804).
        // Refuse the member and record the raw token for per-token adjudication.
        if bytes.get(name_end + usize::from(optional_marker)) == Some(&b'/') {
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::CompressedMemberToken,
                schema_family: Some(schema.key.family.clone()),
                path: Some(path.to_owned()),
                raw: normalize_whitespace(&field_mapped.text),
                reason: "member position compresses several names with '/' and the source does not spell them".to_owned(),
                affected_source_keys: affected_source_key(owner_key.clone()),
                source_range: field_mapped.source_range(0..field_mapped.text.len()),
            });
            continue;
        }
        let mut ambiguity = None;
        let exact_range = if matches!(bytes.get(remainder_start), Some(b':') | Some(b'=')) {
            let range = trim_range(
                &field_mapped.text,
                remainder_start + 1..field_mapped.text.len(),
            );
            if range.is_empty() {
                ambiguity = Some("field separator has no exact type".to_owned());
                None
            } else {
                Some(range)
            }
        } else if matches!(bytes.get(remainder_start), Some(b'[') | Some(b'?')) {
            Some(trim_range(
                &field_mapped.text,
                remainder_start..field_mapped.text.len(),
            ))
        } else if remainder.trim().is_empty() {
            ambiguity = Some("shorthand field has no exact type".to_owned());
            None
        } else {
            ambiguity = Some("noncanonical field separator".to_owned());
            Some(trim_range(
                &field_mapped.text,
                remainder_start..field_mapped.text.len(),
            ))
        };
        let mut exact_type = exact_range
            .as_ref()
            .map(|range| normalize_whitespace(&field_mapped.text[range.clone()]));
        if optional_marker && let Some(value) = exact_type.as_mut() {
            value.push('?');
        }
        let field_path = format!("{path}.{name}");
        let field_key = FieldCandidateKey {
            schema_family: schema.key.family.clone(),
            schema_owner: schema.display_name.clone(),
            path: field_path.clone(),
            stable_name: name.clone(),
        };
        let structural_field_key = StructuralCandidateKey::Field(field_key.clone());
        let raw = normalize_whitespace(&field_mapped.text);
        let source_range = field_mapped.source_range(0..field_mapped.text.len());
        if let Some((first_raw, first_range)) = seen_fields.get(&name) {
            if reported_duplicates.insert(name.clone()) {
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::ConflictingCandidateEvidence,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(field_path.clone()),
                    raw: first_raw.clone(),
                    reason: "one record occurrence repeats the same field name".to_owned(),
                    affected_source_keys: affected_source_key(structural_field_key.clone()),
                    source_range: first_range.clone(),
                });
            }
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(schema.key.family.clone()),
                path: Some(field_path.clone()),
                raw: raw.clone(),
                reason: "one record occurrence repeats the same field name".to_owned(),
                affected_source_keys: affected_source_key(structural_field_key.clone()),
                source_range: source_range.clone(),
            });
        } else {
            seen_fields.insert(name.clone(), (raw.clone(), source_range.clone()));
        }
        rows.fields.push(FieldOccurrence {
            key: field_key,
            exact_type,
            cardinality: infer_cardinality(
                &name,
                &format!("{remainder}{}", if optional_marker { "?" } else { "" }),
            ),
            raw: raw.clone(),
            ambiguity: ambiguity.clone(),
            source_range: source_range.clone(),
        });
        if let Some(reason) = ambiguity {
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::FieldTypeAmbiguous,
                schema_family: Some(schema.key.family.clone()),
                path: Some(field_path.clone()),
                raw,
                reason,
                affected_source_keys: affected_source_key(structural_field_key.clone()),
                source_range,
            });
        }
        let Some(exact_range) = exact_range else {
            continue;
        };
        let exact_mapped = field_mapped.subrange(exact_range);
        if let Some(value_mapped) = indexed_map_value(&exact_mapped)
            && parse_union(
                schema,
                &value_mapped,
                &field_path,
                &structural_field_key,
                rows,
                depth + 1,
                true,
            )
        {
            continue;
        }
        if parse_union(
            schema,
            &exact_mapped,
            &field_path,
            &structural_field_key,
            rows,
            depth + 1,
            false,
        ) {
            continue;
        }
        match outermost_record_ranges(&exact_mapped.text) {
            Ok(record_ranges) => {
                let multiple_records = record_ranges.len() > 1;
                for (record_index, (open, close)) in record_ranges.into_iter().enumerate() {
                    let nested_name = trailing_upper_identifier(&exact_mapped.text[..open]);
                    let nested_path = nested_name
                        .map(|name| format!("{field_path}.{name}"))
                        .unwrap_or_else(|| {
                            if multiple_records {
                                format!("{field_path}.record[{}]", record_index + 1)
                            } else {
                                format!("{field_path}.record")
                            }
                        });
                    parse_record_fields(
                        schema,
                        &exact_mapped.subrange(open + 1..close),
                        &nested_path,
                        &structural_field_key,
                        rows,
                        depth + 1,
                    );
                }
            }
            Err(issue) => rows.ambiguities.push(delimiter_ambiguity(
                issue,
                &exact_mapped,
                Some(schema.key.family.clone()),
                Some(field_path),
                normalize_whitespace(&exact_mapped.text),
                "nested record type contains an unbalanced or mismatched delimiter",
                affected_source_key(structural_field_key),
            )),
        }
    }
}

fn extract_fields_and_arms(
    schemas: &[SchemaOccurrence],
    ambiguities: Vec<AmbiguityOccurrence>,
) -> (
    Vec<FieldOccurrence>,
    Vec<UnionOccurrence>,
    Vec<ArmOccurrence>,
    Vec<AmbiguityOccurrence>,
) {
    let mut rows = StructuralOccurrences {
        fields: Vec::new(),
        unions: Vec::new(),
        arms: Vec::new(),
        ambiguities,
    };
    for schema in schemas {
        let structural_schema_key = StructuralCandidateKey::Schema(schema.key.clone());
        let body = match outer_expression_body(schema) {
            Ok(body) => body,
            Err(issue) => {
                if let Some(expression) = schema.expression.as_ref() {
                    rows.ambiguities.push(delimiter_ambiguity(
                        issue,
                        expression,
                        Some(schema.key.family.clone()),
                        Some(schema.display_name.clone()),
                        normalize_whitespace(&expression.text),
                        "schema expression has an unbalanced or mismatched outer delimiter",
                        affected_source_key(structural_schema_key.clone()),
                    ));
                }
                continue;
            }
        };
        let Some((mapped, shape, trailing)) = body else {
            continue;
        };
        if let Some(trailing) = trailing {
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnparsedTrailingTokens,
                schema_family: Some(schema.key.family.clone()),
                path: Some(schema.display_name.clone()),
                raw: normalize_whitespace(&trailing.text),
                reason: "tokens after a balanced schema record are not part of the closed source grammar"
                    .to_owned(),
                affected_source_keys: affected_source_key(structural_schema_key.clone()),
                source_range: trailing.source_range(0..trailing.text.len()),
            });
        }
        if shape == ExpressionShape::Alias {
            if schema.complete_top_level_map_definition
                && let Some(value_mapped) = top_level_map_value(&mapped)
                && parse_union(
                    schema,
                    &value_mapped,
                    &schema.display_name,
                    &structural_schema_key,
                    &mut rows,
                    0,
                    false,
                )
            {
                continue;
            }
            if parse_union(
                schema,
                &mapped,
                &schema.display_name,
                &structural_schema_key,
                &mut rows,
                0,
                false,
            ) {
                continue;
            }
            let empty = mapped.text.trim().is_empty();
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::AliasExpressionUnparsed,
                schema_family: Some(schema.key.family.clone()),
                path: Some(schema.display_name.clone()),
                raw: normalize_whitespace(&mapped.text),
                reason: if empty {
                    "alias declaration has an empty right-hand side"
                } else {
                    "alias body is neither a top-level pipe union nor a record body"
                }
                .to_owned(),
                affected_source_keys: affected_source_key(structural_schema_key.clone()),
                source_range: mapped.source_range(0..mapped.text.len()),
            });
            continue;
        }
        parse_record_fields(
            schema,
            &mapped,
            &schema.display_name,
            &structural_schema_key,
            &mut rows,
            0,
        );
        for supplemental in &schema.supplemental_unions {
            if !parse_union(
                schema,
                supplemental,
                &schema.display_name,
                &structural_schema_key,
                &mut rows,
                0,
                false,
            ) {
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::AliasExpressionUnparsed,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(schema.display_name.clone()),
                    raw: normalize_whitespace(&supplemental.text),
                    reason:
                        "supplemental union body is not a top-level pipe union under its confirmed owner"
                            .to_owned(),
                    affected_source_keys: affected_source_key(structural_schema_key.clone()),
                    source_range: supplemental.source_range(0..supplemental.text.len()),
                });
            }
        }
    }
    rows.fields.sort_by(|left, right| {
        (
            &left.key.schema_family,
            &left.key.path,
            &left.source_range.start,
            &left.raw,
        )
            .cmp(&(
                &right.key.schema_family,
                &right.key.path,
                &right.source_range.start,
                &right.raw,
            ))
    });
    rows.arms.sort_by(|left, right| {
        (
            &left.key.schema_family,
            &left.key.union_path,
            &left.key.arm_name,
            &left.source_range.start,
            &left.raw,
        )
            .cmp(&(
                &right.key.schema_family,
                &right.key.union_path,
                &right.key.arm_name,
                &right.source_range.start,
                &right.raw,
            ))
    });
    rows.ambiguities.sort_by(|left, right| {
        (
            &left.source_range.start,
            left.kind,
            left.path.as_deref().unwrap_or_default(),
            &left.raw,
        )
            .cmp(&(
                &right.source_range.start,
                right.kind,
                right.path.as_deref().unwrap_or_default(),
                &right.raw,
            ))
    });
    rows.unions.sort_by(|left, right| {
        (
            &left.key.schema_family,
            &left.key.union_path,
            &left.source_range.start,
        )
            .cmp(&(
                &right.key.schema_family,
                &right.key.union_path,
                &right.source_range.start,
            ))
    });
    (rows.fields, rows.unions, rows.arms, rows.ambiguities)
}

fn candidate_conflict_ambiguities(
    source_map: &SourceMap<'_>,
    schemas: &[SchemaOccurrence],
    fields: &[FieldOccurrence],
    unions: &[UnionOccurrence],
    arms: &[ArmOccurrence],
) -> Vec<AmbiguityOccurrence> {
    let mut ambiguities = Vec::new();

    let mut schema_groups: BTreeMap<&SchemaCandidateKey, Vec<&SchemaOccurrence>> = BTreeMap::new();
    for row in schemas {
        schema_groups.entry(&row.key).or_default().push(row);
    }
    for rows in schema_groups.into_values() {
        let expressions: BTreeSet<_> = rows
            .iter()
            .filter(|row| row.expression.is_some())
            .map(|row| row.expression_sha256.as_str())
            .collect();
        if expressions.len() < 2 {
            continue;
        }
        for row in rows {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(row.key.family.clone()),
                path: Some(row.display_name.clone()),
                raw: row
                    .expression
                    .as_ref()
                    .map(|expression| normalize_whitespace(&expression.text))
                    .unwrap_or_default(),
                reason: "the same schema source key has divergent structural bodies".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Schema(
                    row.key.clone(),
                )),
                source_range: row.declaration_range.clone(),
            });
        }
    }

    let mut field_groups: BTreeMap<&FieldCandidateKey, Vec<&FieldOccurrence>> = BTreeMap::new();
    for row in fields {
        field_groups.entry(&row.key).or_default().push(row);
    }
    for rows in field_groups.into_values() {
        let exact_types: BTreeSet<_> = rows
            .iter()
            .filter_map(|row| row.exact_type.as_deref())
            .collect();
        if exact_types.len() < 2 {
            continue;
        }
        for row in rows {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(row.key.schema_family.clone()),
                path: Some(row.key.path.clone()),
                raw: row.raw.clone(),
                reason: "the same field source key has divergent exact types".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Field(
                    row.key.clone(),
                )),
                source_range: row.source_range.clone(),
            });
        }
    }

    let mut union_groups: BTreeMap<&UnionCandidateKey, Vec<&UnionOccurrence>> = BTreeMap::new();
    for row in unions {
        union_groups.entry(&row.key).or_default().push(row);
    }
    for rows in union_groups.into_values() {
        let arm_sets: BTreeSet<_> = rows.iter().map(|row| &row.arm_names).collect();
        if arm_sets.len() < 2 {
            continue;
        }
        for row in rows {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(row.key.schema_family.clone()),
                path: Some(row.key.union_path.clone()),
                raw: normalize_whitespace(&source_map.source[row.source_range.clone()]),
                reason: "the same union source key has divergent arm sets".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Union(
                    row.key.clone(),
                )),
                source_range: row.source_range.clone(),
            });
        }
    }

    let mut arm_groups: BTreeMap<&ArmCandidateKey, Vec<&ArmOccurrence>> = BTreeMap::new();
    for row in arms {
        arm_groups.entry(&row.key).or_default().push(row);
    }
    for rows in arm_groups.into_values() {
        let payloads: BTreeSet<_> = rows.iter().map(|row| row.payload.as_deref()).collect();
        if payloads.len() < 2 {
            continue;
        }
        for row in rows {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(row.key.schema_family.clone()),
                path: Some(format!("{}.{}", row.key.union_path, row.key.arm_name)),
                raw: row.raw.clone(),
                reason: "the same arm source key has divergent payloads".to_owned(),
                affected_source_keys: affected_source_key(StructuralCandidateKey::Arm(
                    row.key.clone(),
                )),
                source_range: row.source_range.clone(),
            });
        }
    }

    ambiguities
}

fn sorted_unique<T: Ord>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn canonical_schemas(
    rows: &[&SchemaOccurrence],
    source_map: &SourceMap<'_>,
) -> Vec<SchemaCandidate> {
    let mut grouped: BTreeMap<SchemaCandidateKey, Vec<&SchemaOccurrence>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.key.clone()).or_default().push(*row);
    }
    grouped
        .into_iter()
        .map(|(key, rows)| {
            let expression_sha256s = sorted_unique(rows.iter().filter_map(|row| {
                row.expression
                    .as_ref()
                    .map(|_| row.expression_sha256.clone())
            }));
            SchemaCandidate {
                key,
                owner_statuses: sorted_unique(rows.iter().map(|row| row.owner_status)),
                definition_kinds: sorted_unique(rows.iter().map(|row| row.definition_kind)),
                body_conflict: expression_sha256s.len() > 1,
                expression_sha256s,
                locations: sorted_unique(
                    rows.iter()
                        .map(|row| source_map.span(&row.declaration_range)),
                ),
            }
        })
        .collect()
}

fn canonical_fields(rows: &[&FieldOccurrence], source_map: &SourceMap<'_>) -> Vec<FieldCandidate> {
    let mut grouped: BTreeMap<FieldCandidateKey, Vec<&FieldOccurrence>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.key.clone()).or_default().push(*row);
    }
    grouped
        .into_iter()
        .map(|(key, rows)| {
            let exact_types = sorted_unique(rows.iter().filter_map(|row| row.exact_type.clone()));
            FieldCandidate {
                key,
                type_conflict: exact_types.len() > 1,
                exact_types,
                cardinalities: sorted_unique(rows.iter().map(|row| row.cardinality)),
                ambiguous: rows.iter().any(|row| row.ambiguity.is_some()),
                locations: sorted_unique(rows.iter().map(|row| source_map.span(&row.source_range))),
            }
        })
        .collect()
}

fn canonical_unions(rows: &[&UnionOccurrence], source_map: &SourceMap<'_>) -> Vec<UnionCandidate> {
    let mut grouped: BTreeMap<UnionCandidateKey, Vec<&UnionOccurrence>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.key.clone()).or_default().push(*row);
    }
    grouped
        .into_iter()
        .map(|(key, rows)| {
            let arm_name_sets = sorted_unique(
                rows.iter()
                    .map(|row| row.arm_names.iter().cloned().collect::<Vec<_>>()),
            );
            let arm_names =
                sorted_unique(arm_name_sets.iter().flat_map(|names| names.iter().cloned()));
            UnionCandidate {
                key,
                occurrence_count: rows.len(),
                parsed_arm_count: arm_names.len(),
                arm_names,
                arm_set_conflict: arm_name_sets.len() > 1,
                arm_name_sets,
                unparsed_arm_count: rows.iter().map(|row| row.unparsed_arm_count).sum(),
                locations: sorted_unique(rows.iter().map(|row| source_map.span(&row.source_range))),
                evidence_lines: sorted_unique(rows.iter().flat_map(|row| {
                    row.evidence_ranges
                        .iter()
                        .map(|range| source_map.position(range.start).line)
                })),
            }
        })
        .collect()
}

fn canonical_arms(rows: &[&ArmOccurrence], source_map: &SourceMap<'_>) -> Vec<ArmCandidate> {
    let mut grouped: BTreeMap<ArmCandidateKey, Vec<&ArmOccurrence>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.key.clone()).or_default().push(*row);
    }
    grouped
        .into_iter()
        .map(|(key, rows)| {
            let payload_forms = sorted_unique(rows.iter().map(|row| row.payload.clone()));
            let payload_sha256s = sorted_unique(
                rows.iter()
                    .filter_map(|row| row.payload.as_ref())
                    .map(|payload| sha256_hex(payload.as_bytes())),
            );
            ArmCandidate {
                key,
                payload_conflict: payload_forms.len() > 1,
                payload_sha256s,
                locations: sorted_unique(rows.iter().map(|row| source_map.span(&row.source_range))),
            }
        })
        .collect()
}

fn ambiguity_affinity_is_valid(row: &AmbiguityOccurrence) -> bool {
    let single_key = (row.affected_source_keys.len() == 1)
        .then(|| row.affected_source_keys.iter().next())
        .flatten();
    let family_matches =
        |key: &StructuralCandidateKey| row.schema_family.as_deref() == Some(key.schema_family());
    let path_matches = |key: &StructuralCandidateKey| {
        let Some(path) = row.path.as_deref() else {
            return false;
        };
        let container_path = key.container_path();
        path == container_path
            || path
                .strip_prefix(&container_path)
                .is_some_and(|suffix| suffix.starts_with('.'))
    };
    let exact_key = |predicate: fn(&StructuralCandidateKey) -> bool| {
        single_key.is_some_and(|key| family_matches(key) && path_matches(key) && predicate(key))
    };

    match row.kind {
        AmbiguityKind::UnterminatedCodeFence
        | AmbiguityKind::UnterminatedInlineCode
        | AmbiguityKind::UnownedStructuralFragment => row.affected_source_keys.is_empty(),
        AmbiguityKind::AliasExpressionUnparsed | AmbiguityKind::AmbiguousSchemaOwner => {
            exact_key(|key| matches!(key, StructuralCandidateKey::Schema(_)))
        }
        AmbiguityKind::DefinitionWithoutStructuralBody => {
            single_key.is_some_and(|key| {
                family_matches(key) && matches!(key, StructuralCandidateKey::Schema(_))
            }) && row.path.is_none()
        }
        AmbiguityKind::FieldTypeAmbiguous => {
            exact_key(|key| matches!(key, StructuralCandidateKey::Field(_)))
        }
        AmbiguityKind::UnparsedUnionArm => {
            exact_key(|key| matches!(key, StructuralCandidateKey::Union(_)))
        }
        AmbiguityKind::UnparsedTrailingTokens => exact_key(|key| {
            matches!(
                key,
                StructuralCandidateKey::Schema(_) | StructuralCandidateKey::Arm(_)
            )
        }),
        AmbiguityKind::ConflictingCandidateEvidence => exact_key(|_| true),
        AmbiguityKind::CompressedMemberToken
        | AmbiguityKind::NestingLimitExceeded
        | AmbiguityKind::UnparsedRecordItem => exact_key(|key| {
            matches!(
                key,
                StructuralCandidateKey::Schema(_)
                    | StructuralCandidateKey::Field(_)
                    | StructuralCandidateKey::Arm(_)
            )
        }),
        AmbiguityKind::MismatchedDelimiter | AmbiguityKind::UnbalancedDefinition => {
            if row.schema_family.is_none() && row.path.is_none() {
                row.affected_source_keys.is_empty()
            } else {
                exact_key(|key| {
                    matches!(
                        key,
                        StructuralCandidateKey::Schema(_)
                            | StructuralCandidateKey::Field(_)
                            | StructuralCandidateKey::Arm(_)
                    )
                })
            }
        }
    }
}

fn canonical_ambiguities(
    rows: &[&AmbiguityOccurrence],
    source_map: &SourceMap<'_>,
) -> Result<Vec<AmbiguityCandidate>, CensusError> {
    type AmbiguityIdentity = (
        AmbiguityKind,
        Option<String>,
        Option<String>,
        String,
        String,
    );
    let mut grouped: BTreeMap<AmbiguityIdentity, (Vec<String>, Vec<&AmbiguityOccurrence>)> =
        BTreeMap::new();
    for row in rows {
        if !ambiguity_affinity_is_valid(row) {
            return Err(census_error(
                CensusErrorKind::CandidateAssignmentInvariant,
                None,
                format!(
                    "ambiguity kind {:?} at family {:?} path {:?} has invalid affected structural source keys {:?}",
                    row.kind, row.schema_family, row.path, row.affected_source_keys
                ),
            ));
        }
        let affected_source_keys = row
            .affected_source_keys
            .iter()
            .map(StructuralCandidateKey::source_key)
            .collect::<Vec<_>>();
        let identity = (
            row.kind,
            row.schema_family.clone(),
            row.path.clone(),
            sha256_hex(row.raw.as_bytes()),
            row.reason.clone(),
        );
        if let Some((expected_source_keys, grouped_rows)) = grouped.get_mut(&identity) {
            if expected_source_keys != &affected_source_keys {
                return Err(census_error(
                    CensusErrorKind::CandidateAssignmentInvariant,
                    None,
                    format!(
                        "ambiguity identity {:?} has inconsistent affected structural source keys: {:?} versus {:?}",
                        identity, expected_source_keys, affected_source_keys
                    ),
                ));
            }
            grouped_rows.push(*row);
        } else {
            grouped.insert(identity, (affected_source_keys, vec![*row]));
        }
    }
    Ok(grouped
        .into_iter()
        .map(
            |((kind, schema_family, path, raw_sha256, reason), (affected_source_keys, rows))| {
                // Structural source keys come from the closed parser grammar and cannot
                // contain LF, so the sorted LF-terminated relation transcript is injective.
                let relation =
                    source_key_transcript(affected_source_keys.iter().map(String::as_str));
                AmbiguityCandidate {
                    raw: rows[0].raw.clone(),
                    key: AmbiguityKey {
                        kind,
                        schema_family,
                        path,
                        raw_sha256,
                        affected_source_key_count: relation.rows,
                        affected_source_keys_sha256: relation.sha256,
                        reason,
                    },
                    affected_source_keys,
                    locations: sorted_unique(
                        rows.iter().map(|row| source_map.span(&row.source_range)),
                    ),
                }
            },
        )
        .collect())
}

fn transcript_digest(transcript: String, rows: usize) -> TranscriptDigest {
    TranscriptDigest {
        rows,
        sha256: sha256_hex(transcript.as_bytes()),
    }
}

fn source_key_transcript<'a>(keys: impl IntoIterator<Item = &'a str>) -> TranscriptDigest {
    let keys = sorted_unique(keys.into_iter().map(str::to_owned));
    let mut transcript = keys.join("\n");
    if !transcript.is_empty() {
        transcript.push('\n');
    }
    transcript_digest(transcript, keys.len())
}

/// Candidate-key transcript grammar is one UTF-8 key per LF-terminated row:
/// `top|Family<generic>`, `field|Family|path|name`,
/// `union|Family|path`, `arm|Family|path|arm`, and
/// `ambiguity|kind|family-or-empty|path-or-empty|raw-sha256|affected-key-count|`
/// `affected-keys-sha256|reason`.
/// Rows are sorted and duplicate-free. Exact source movement is deliberately
/// excluded because each slice already pins its complete source bytes.
fn candidate_transcripts(
    schemas: &[SchemaCandidate],
    fields: &[FieldCandidate],
    unions: &[UnionCandidate],
    arms: &[ArmCandidate],
    ambiguities: &[AmbiguityCandidate],
) -> CensusTranscripts {
    let schema_keys: Vec<_> = schemas.iter().map(|row| row.key.source_key()).collect();
    let field_keys: Vec<_> = fields.iter().map(|row| row.key.source_key()).collect();
    let union_keys: Vec<_> = unions.iter().map(|row| row.key.source_key()).collect();
    let arm_keys: Vec<_> = arms.iter().map(|row| row.key.source_key()).collect();
    let ambiguity_keys: Vec<_> = ambiguities.iter().map(|row| row.key.source_key()).collect();
    CensusTranscripts {
        schemas: source_key_transcript(schema_keys.iter().map(String::as_str)),
        fields: source_key_transcript(field_keys.iter().map(String::as_str)),
        unions: source_key_transcript(union_keys.iter().map(String::as_str)),
        arms: source_key_transcript(arm_keys.iter().map(String::as_str)),
        ambiguities: source_key_transcript(ambiguity_keys.iter().map(String::as_str)),
    }
}

fn counts(
    occurrence_counts: [usize; 5],
    candidate_counts: [usize; 5],
    unions: &[UnionCandidate],
) -> CensusCounts {
    CensusCounts {
        schema_occurrences: occurrence_counts[0],
        schema_candidates: candidate_counts[0],
        field_occurrences: occurrence_counts[1],
        field_candidates: candidate_counts[1],
        union_occurrences: occurrence_counts[2],
        union_candidates: candidate_counts[2],
        unions_with_unparsed_arms: unions
            .iter()
            .filter(|row| row.unparsed_arm_count > 0)
            .count(),
        arm_occurrences: occurrence_counts[3],
        arm_candidates: candidate_counts[3],
        ambiguity_occurrences: occurrence_counts[4],
        ambiguities: candidate_counts[4],
    }
}

fn transcript_rows(transcripts: &CensusTranscripts) -> [usize; 5] {
    [
        transcripts.schemas.rows,
        transcripts.fields.rows,
        transcripts.unions.rows,
        transcripts.arms.rows,
        transcripts.ambiguities.rows,
    ]
}

fn line_in_slice(line: usize, slice: &SourceSliceSpec<'_>) -> bool {
    (slice.start_line..=slice.end_line).contains(&line)
}

fn candidate_in_slice(locations: &[SourceSpan], slice: &SourceSliceSpec<'_>) -> bool {
    locations
        .first()
        .is_some_and(|location| line_in_slice(location.start.line, slice))
}

/// Extract a structural census from exact Appendix bytes.
///
/// `source_start_line` is the source coordinate of the first supplied byte;
/// `slices` must be unique, sorted or unsorted, nonoverlapping, and together
/// cover every supplied physical line exactly once.  The function performs no
/// I/O and carries no baked-in Appendix line or hash pin.
pub fn census_appendix_source(
    source: &[u8],
    source_start_line: usize,
    slices: &[SourceSliceSpec<'_>],
) -> Result<AppendixSourceCensus, CensusError> {
    let source = std::str::from_utf8(source).map_err(|error| {
        census_error(
            CensusErrorKind::InvalidUtf8,
            None,
            format!("Appendix source is not UTF-8: {error}"),
        )
    })?;
    if source.contains('\r') {
        return Err(census_error(
            CensusErrorKind::CarriageReturn,
            None,
            "Appendix source must use LF line endings and contains a CR byte",
        ));
    }
    if source.is_empty() || source_start_line == 0 {
        return Err(census_error(
            CensusErrorKind::EmptySource,
            None,
            "Appendix source must be nonempty and use a one-based start line",
        ));
    }
    if slices.is_empty() {
        return Err(census_error(
            CensusErrorKind::EmptySlices,
            None,
            "Appendix source census requires at least one slice",
        ));
    }
    let source_line_count = 1 + source
        .bytes()
        .enumerate()
        .filter(|(index, byte)| *byte == b'\n' && index + 1 < source.len())
        .count();
    let Some(checked_source_end_line) = source_start_line.checked_add(source_line_count - 1) else {
        return Err(census_error(
            CensusErrorKind::SourceCoordinateOverflow,
            None,
            "Appendix source line coordinates exceed usize",
        ));
    };
    let source_map = SourceMap::new(source, source_start_line);
    let source_end_line = checked_source_end_line;
    let mut ordered_slices = slices.to_vec();
    ordered_slices.sort_by_key(|slice| (slice.start_line, slice.end_line, slice.id));
    let mut seen_ids = BTreeSet::new();
    for slice in &ordered_slices {
        if slice.id.trim().is_empty() || !seen_ids.insert(slice.id) {
            return Err(census_error(
                CensusErrorKind::InvalidSliceId,
                Some(slice.id),
                format!("slice id {:?} is blank or duplicated", slice.id),
            ));
        }
        if slice.start_line == 0 || slice.start_line > slice.end_line {
            return Err(census_error(
                CensusErrorKind::InvalidSliceRange,
                Some(slice.id),
                format!(
                    "slice {:?} has invalid inclusive range {}-{}",
                    slice.id, slice.start_line, slice.end_line
                ),
            ));
        }
        if slice.start_line < source_start_line || slice.end_line > source_end_line {
            return Err(census_error(
                CensusErrorKind::SliceOutsideSource,
                Some(slice.id),
                format!(
                    "slice {:?} range {}-{} is outside supplied source range {}-{}",
                    slice.id, slice.start_line, slice.end_line, source_start_line, source_end_line
                ),
            ));
        }
    }
    let mut expected_start = source_start_line;
    for slice in &ordered_slices {
        if slice.start_line < expected_start {
            return Err(census_error(
                CensusErrorKind::SliceOverlap,
                Some(slice.id),
                format!("slice {:?} overlaps an earlier slice", slice.id),
            ));
        }
        if slice.start_line > expected_start {
            return Err(census_error(
                CensusErrorKind::SliceGap,
                Some(slice.id),
                format!(
                    "slice {:?} begins at {}, leaving line {} uncovered",
                    slice.id, slice.start_line, expected_start
                ),
            ));
        }
        expected_start = slice.end_line.saturating_add(1);
    }
    if expected_start != source_end_line.saturating_add(1) {
        return Err(census_error(
            CensusErrorKind::SliceGap,
            None,
            format!("slice coverage ends before source line {source_end_line}"),
        ));
    }

    let (fragments, initial_ambiguities) = extract_markdown_fragments(&source_map);
    let (schemas, ambiguities) =
        extract_schema_occurrences(&source_map, &fragments, initial_ambiguities);
    let (fields, unions, arms, mut ambiguities) = extract_fields_and_arms(&schemas, ambiguities);
    ambiguities.extend(candidate_conflict_ambiguities(
        &source_map,
        &schemas,
        &fields,
        &unions,
        &arms,
    ));

    let all_schema_rows: Vec<_> = schemas.iter().collect();
    let all_field_rows: Vec<_> = fields.iter().collect();
    let all_union_rows: Vec<_> = unions.iter().collect();
    let all_arm_rows: Vec<_> = arms.iter().collect();
    let all_ambiguity_rows: Vec<_> = ambiguities.iter().collect();
    let canonical_schema_rows = canonical_schemas(&all_schema_rows, &source_map);
    let canonical_field_rows = canonical_fields(&all_field_rows, &source_map);
    let canonical_union_rows = canonical_unions(&all_union_rows, &source_map);
    let canonical_arm_rows = canonical_arms(&all_arm_rows, &source_map);
    let canonical_ambiguity_rows = canonical_ambiguities(&all_ambiguity_rows, &source_map)?;
    let structural_source_keys = canonical_schema_rows
        .iter()
        .map(|row| row.key.source_key())
        .chain(canonical_field_rows.iter().map(|row| row.key.source_key()))
        .chain(canonical_union_rows.iter().map(|row| row.key.source_key()))
        .chain(canonical_arm_rows.iter().map(|row| row.key.source_key()))
        .collect::<BTreeSet<_>>();
    for ambiguity in &canonical_ambiguity_rows {
        if let Some(unknown_source_key) = ambiguity
            .affected_source_keys
            .iter()
            .find(|source_key| !structural_source_keys.contains(*source_key))
        {
            return Err(census_error(
                CensusErrorKind::CandidateAssignmentInvariant,
                None,
                format!(
                    "ambiguity source key {:?} names unknown affected structural source key {:?}",
                    ambiguity.key.source_key(),
                    unknown_source_key
                ),
            ));
        }
    }
    let global_counts = counts(
        [
            all_schema_rows.len(),
            all_field_rows.len(),
            all_union_rows.len(),
            all_arm_rows.len(),
            all_ambiguity_rows.len(),
        ],
        [
            canonical_schema_rows.len(),
            canonical_field_rows.len(),
            canonical_union_rows.len(),
            canonical_arm_rows.len(),
            canonical_ambiguity_rows.len(),
        ],
        &canonical_union_rows,
    );
    let global_transcripts = candidate_transcripts(
        &canonical_schema_rows,
        &canonical_field_rows,
        &canonical_union_rows,
        &canonical_arm_rows,
        &canonical_ambiguity_rows,
    );

    let mut slice_results = Vec::with_capacity(ordered_slices.len());
    for slice in ordered_slices {
        let schema_rows: Vec<_> = schemas
            .iter()
            .filter(|row| {
                line_in_slice(
                    source_map.position(row.declaration_range.start).line,
                    &slice,
                )
            })
            .collect();
        let field_rows: Vec<_> = fields
            .iter()
            .filter(|row| line_in_slice(source_map.position(row.source_range.start).line, &slice))
            .collect();
        let union_rows: Vec<_> = unions
            .iter()
            .filter(|row| line_in_slice(source_map.position(row.source_range.start).line, &slice))
            .collect();
        let arm_rows: Vec<_> = arms
            .iter()
            .filter(|row| line_in_slice(source_map.position(row.source_range.start).line, &slice))
            .collect();
        let ambiguity_rows: Vec<_> = ambiguities
            .iter()
            .filter(|row| line_in_slice(source_map.position(row.source_range.start).line, &slice))
            .collect();
        let slice_schema_candidates: Vec<_> = canonical_schema_rows
            .iter()
            .filter(|row| candidate_in_slice(&row.locations, &slice))
            .cloned()
            .collect();
        let slice_field_candidates: Vec<_> = canonical_field_rows
            .iter()
            .filter(|row| candidate_in_slice(&row.locations, &slice))
            .cloned()
            .collect();
        let slice_union_candidates: Vec<_> = canonical_union_rows
            .iter()
            .filter(|row| candidate_in_slice(&row.locations, &slice))
            .cloned()
            .collect();
        let slice_arm_candidates: Vec<_> = canonical_arm_rows
            .iter()
            .filter(|row| candidate_in_slice(&row.locations, &slice))
            .cloned()
            .collect();
        let slice_ambiguity_candidates: Vec<_> = canonical_ambiguity_rows
            .iter()
            .filter(|row| candidate_in_slice(&row.locations, &slice))
            .cloned()
            .collect();
        let slice_counts = counts(
            [
                schema_rows.len(),
                field_rows.len(),
                union_rows.len(),
                arm_rows.len(),
                ambiguity_rows.len(),
            ],
            [
                slice_schema_candidates.len(),
                slice_field_candidates.len(),
                slice_union_candidates.len(),
                slice_arm_candidates.len(),
                slice_ambiguity_candidates.len(),
            ],
            &slice_union_candidates,
        );
        let transcripts = candidate_transcripts(
            &slice_schema_candidates,
            &slice_field_candidates,
            &slice_union_candidates,
            &slice_arm_candidates,
            &slice_ambiguity_candidates,
        );
        if transcript_rows(&transcripts)
            != [
                slice_counts.schema_candidates,
                slice_counts.field_candidates,
                slice_counts.union_candidates,
                slice_counts.arm_candidates,
                slice_counts.ambiguities,
            ]
        {
            return Err(census_error(
                CensusErrorKind::CandidateAssignmentInvariant,
                Some(slice.id),
                "candidate counts do not match canonical transcript row counts",
            ));
        }
        let source_range = source_map.byte_range_for_lines(slice.start_line, slice.end_line);
        slice_results.push(SliceSourceCensus {
            slice_id: slice.id.to_owned(),
            start_line: slice.start_line,
            end_line: slice.end_line,
            source_byte_count: source_range.len(),
            source_sha256: sha256_hex(source[source_range].as_bytes()),
            schemas: slice_schema_candidates,
            fields: slice_field_candidates,
            unions: slice_union_candidates,
            arms: slice_arm_candidates,
            ambiguities: slice_ambiguity_candidates,
            counts: slice_counts,
            transcripts,
        });
    }

    let mut occurrence_sums = [0; 5];
    let mut candidate_sums = [0; 5];
    for slice in &slice_results {
        let occurrences = [
            slice.counts.schema_occurrences,
            slice.counts.field_occurrences,
            slice.counts.union_occurrences,
            slice.counts.arm_occurrences,
            slice.counts.ambiguity_occurrences,
        ];
        let candidates = [
            slice.counts.schema_candidates,
            slice.counts.field_candidates,
            slice.counts.union_candidates,
            slice.counts.arm_candidates,
            slice.counts.ambiguities,
        ];
        for index in 0..5 {
            occurrence_sums[index] += occurrences[index];
            candidate_sums[index] += candidates[index];
        }
    }
    let expected_occurrences = [
        global_counts.schema_occurrences,
        global_counts.field_occurrences,
        global_counts.union_occurrences,
        global_counts.arm_occurrences,
        global_counts.ambiguity_occurrences,
    ];
    let expected_candidates = [
        global_counts.schema_candidates,
        global_counts.field_candidates,
        global_counts.union_candidates,
        global_counts.arm_candidates,
        global_counts.ambiguities,
    ];
    if occurrence_sums != expected_occurrences || candidate_sums != expected_candidates {
        return Err(census_error(
            CensusErrorKind::CandidateAssignmentInvariant,
            None,
            "per-slice source rows are not an exact disjoint partition of the global census",
        ));
    }
    if transcript_rows(&global_transcripts) != expected_candidates {
        return Err(census_error(
            CensusErrorKind::CandidateAssignmentInvariant,
            None,
            "global candidate counts do not match canonical transcript row counts",
        ));
    }

    Ok(AppendixSourceCensus {
        source_start_line,
        source_end_line,
        source_byte_count: source.len(),
        source_sha256: sha256_hex(source.as_bytes()),
        slices: slice_results,
        schemas: canonical_schema_rows,
        fields: canonical_field_rows,
        unions: canonical_union_rows,
        arms: canonical_arm_rows,
        ambiguities: canonical_ambiguity_rows,
        counts: global_counts,
        transcripts: global_transcripts,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AmbiguityCandidate, AmbiguityKind, AmbiguityOccurrence, CensusErrorKind, DelimiterIssue,
        FieldCandidateKey, OPENING_DELIMITERS, SchemaCandidateKey, SourceMap, SourceSliceSpec,
        SplitSpan, StructuralCandidateKey, affected_source_key, bold_owner_name_range,
        canonical_ambiguities, census_appendix_source, half_open_interval_end,
        is_generic_angle_open, matching_delimiter, normalize_whitespace, opening_delimiter_for,
        sha256_hex, source_key_transcript, split_top_level, top_level_arrow,
    };

    // ===================================================================
    // fgdb-8kzt — the one-reader collapse, mutation-proved.
    //
    // `matching_delimiter` and `split_top_level` used to carry one delimiter
    // stack each. The two bodies below are those PRE-COLLAPSE bodies, copied
    // VERBATIM and frozen. They are the oracle: the collapse is a refactor
    // only if the surviving reader answers exactly as they did, on every input
    // either of them could see.
    //
    // DO NOT "FIX" THESE COPIES. If a grammar change (the half-open interval
    // `(a,b]` this bead reports, say) is ever ruled in, it goes into
    // `DelimiterScan` and these copies are RETIRED with the tests that use
    // them — not edited to agree. An edited oracle proves nothing.
    // ===================================================================

    /// `matching_delimiter` exactly as it stood at b7bfd44, before the collapse.
    fn legacy_matching_delimiter(text: &str, open_index: usize) -> Result<usize, DelimiterIssue> {
        let bytes = text.as_bytes();
        let Some(opener) = bytes.get(open_index).copied() else {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: false,
            });
        };
        if !matches!(opener, b'{' | b'[' | b'(' | b'<') {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: true,
            });
        }
        let mut stack = vec![opener];
        let mut quote = None;
        let mut escaped = false;
        for (index, byte) in bytes.iter().copied().enumerate().skip(open_index + 1) {
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                continue;
            }
            if byte == b'<' && !is_generic_angle_open(text, index) {
                continue;
            }
            if matches!(byte, b'{' | b'[' | b'(' | b'<') {
                stack.push(byte);
                continue;
            }
            if !matches!(byte, b'}' | b']' | b')' | b'>') {
                continue;
            }
            if byte == b'>' && stack.last() != Some(&b'<') {
                continue;
            }
            let expected = match stack.last().copied() {
                Some(b'{') => b'}',
                Some(b'[') => b']',
                Some(b'(') => b')',
                Some(b'<') => b'>',
                _ => unreachable!("the delimiter stack only contains opening delimiters"),
            };
            if byte != expected {
                return Err(DelimiterIssue {
                    offset: index,
                    mismatched: true,
                });
            }
            stack.pop();
            if stack.is_empty() {
                return Ok(index);
            }
        }
        Err(DelimiterIssue {
            offset: text.len(),
            mismatched: false,
        })
    }

    /// `split_top_level` exactly as it stood at b7bfd44 — the twin, with its
    /// own stack, its own quote machine, and its own copy of the push/close
    /// sets.
    fn legacy_split_top_level(
        text: &str,
        delimiters: &[u8],
    ) -> Result<Vec<SplitSpan>, DelimiterIssue> {
        let bytes = text.as_bytes();
        let mut spans = Vec::new();
        let mut stack = Vec::new();
        let mut quote = None;
        let mut escaped = false;
        let mut start = 0;
        for (index, byte) in bytes.iter().copied().enumerate() {
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                continue;
            }
            if byte == b'<' && !is_generic_angle_open(text, index) {
                continue;
            }
            if matches!(byte, b'{' | b'[' | b'(' | b'<') {
                stack.push(byte);
                continue;
            }
            if matches!(byte, b'}' | b']' | b')' | b'>') {
                if byte == b'>' && stack.last() != Some(&b'<') {
                    continue;
                }
                let expected_opener = match byte {
                    b'}' => b'{',
                    b']' => b'[',
                    b')' => b'(',
                    b'>' => b'<',
                    _ => unreachable!(),
                };
                if stack.pop() != Some(expected_opener) {
                    return Err(DelimiterIssue {
                        offset: index,
                        mismatched: true,
                    });
                }
                continue;
            }
            if stack.is_empty() && delimiters.contains(&byte) {
                spans.push(SplitSpan { start, end: index });
                start = index + 1;
            }
        }
        if !stack.is_empty() || quote.is_some() {
            return Err(DelimiterIssue {
                offset: text.len(),
                mismatched: false,
            });
        }
        spans.push(SplitSpan {
            start,
            end: text.len(),
        });
        Ok(spans)
    }

    /// Seeds shaped like real Appendix A bodies, including the exact a10:1928
    /// interval that fgdb-8kzt is about and the sibling on the same line that
    /// parses fine — the control that proves the `Name {...}` form is not the
    /// cause.
    const DELIMITER_SEEDS: &[&str] = &[
        "{a:u8,b:u16}",
        "{left:Vec<A,B>,literal:\"}])>\",arrow:A->B,compare:x>=y}",
        "Outer<Inner<A|B>,[C,D]>",
        "{value:[u8;4)}",
        "entries[every commit_seq in (after,frontier] -> StrongRef<T>]",
        // The mirror orientation, which the census region also spells (plan line
        // 1728, `[0,byte_count)`), and the near-miss that the interval SHAPE does
        // accept — `[u8,4)` — so the widening-surface measurement below has to
        // account for it rather than never meeting it.
        "{descriptors cover [0,byte_count) exactly}",
        "{value:[u8,4)}",
        "{fragments[shard_id -> StrongRef<Frag>],count:u32}",
        "Left{x:u8}|Middle<Vec<A|B>>|Right{y:u16}",
        "{s:'a\\'b',t:\"c\\\"d\"}",
        "{unterminated:\"abc}",
        "{nested:{deep:{deeper:[1,2,(3)]}}}",
        "a<b,c>d",
        "{}",
        "",
        ")",
        "{a}}",
    ];

    /// The alphabet a mutant may substitute or insert. Every delimiter, both
    /// quotes, the escape, and the three separator bytes the real callers pass.
    const MUTATION_ALPHABET: &[u8] = b"{}[]()<>\"'\\,|=; aA";

    /// The separator sets `split_top_level` is actually called with, plus one
    /// it is not, so the differential is not accidentally scoped to the
    /// separators that happen to appear in the seeds.
    const SEPARATOR_SETS: &[&[u8]] = &[b",", b"|", b"=", b";", b">"];

    /// Every mutant: each seed, each position, each substitution, plus each
    /// insertion and each deletion. Order is deterministic.
    fn delimiter_mutants() -> Vec<String> {
        let mut mutants: Vec<String> = DELIMITER_SEEDS.iter().map(|s| (*s).to_owned()).collect();
        for seed in DELIMITER_SEEDS {
            let bytes = seed.as_bytes();
            for position in 0..=bytes.len() {
                for replacement in MUTATION_ALPHABET {
                    let mut inserted = bytes.to_vec();
                    inserted.insert(position, *replacement);
                    mutants.push(String::from_utf8(inserted).expect("ascii"));
                    if position < bytes.len() {
                        let mut substituted = bytes.to_vec();
                        substituted[position] = *replacement;
                        mutants.push(String::from_utf8(substituted).expect("ascii"));
                    }
                }
                if position < bytes.len() {
                    let mut deleted = bytes.to_vec();
                    deleted.remove(position);
                    mutants.push(String::from_utf8(deleted).expect("ascii"));
                }
            }
        }
        mutants.sort();
        mutants.dedup();
        mutants
    }

    /// Does this text contain a half-open interval literal anywhere? This is the
    /// FOOTPRINT of the interval rule, computed by the production recogniser
    /// itself rather than by a hand list, and it is what partitions the
    /// differential below.
    ///
    /// A text transformation was tried first and is NOT sound: there is no
    /// length-preserving substitution for the two bracket bytes that leaves the
    /// pre-collapse oracles' answers alone, because `is_generic_angle_open` reads
    /// the raw byte before a `<` and accepts `]` and identifier bytes while
    /// rejecting `)`. Any filler is therefore observable, and the resulting
    /// "drift" would be an artifact of the model, not of the reader. Hence a
    /// partition, whose predicate is exact, instead of a rewrite.
    fn carries_interval_token(text: &str) -> bool {
        text.as_bytes().iter().enumerate().any(|(index, byte)| {
            matches!(byte, b'(' | b'[') && half_open_interval_end(text, index).is_some()
        })
    }

    /// THE MUTATION PROOF. For every mutant and every entry point, the one
    /// reader answers exactly what the two frozen pre-collapse readers answered
    /// on the same text with its interval literals erased. Stated the other way,
    /// which is the point: the one reader IS the pre-collapse reader plus the
    /// single rule "a half-open interval is an inert token", and nothing else
    /// about it moved.
    ///
    /// The oracles are deliberately NOT re-frozen for the grammar change. A
    /// re-frozen oracle proves only that the new code agrees with itself. Keeping
    /// them pre-interval and quantifying the difference through `interval_erased`
    /// keeps the differential TOTAL — there is no carve-out in which a second,
    /// unrelated drift could hide.
    ///
    /// The vacuity controls are not decoration. "Every mutant agrees" is
    /// worthless if the corpus never produces disagreement-capable inputs, so
    /// this test proves its corpus reaches all three outcome classes, proves the
    /// erasure is load-bearing on it, and proves a deliberately wrong reader IS
    /// caught by it.
    #[test]
    fn one_delimiter_reader_answers_exactly_as_the_two_it_replaced() {
        let mutants = delimiter_mutants();
        assert!(
            mutants.len() > 2_000,
            "corpus collapsed to {} mutants; the differential below would be \
             a weak claim on a small corpus",
            mutants.len()
        );

        let mut balanced = 0usize;
        let mut mismatched = 0usize;
        let mut unclosed = 0usize;
        let mut split_ok = 0usize;
        let mut split_err = 0usize;
        let mut multi_span = 0usize;
        let mut carrying_interval = 0usize;
        let mut erasure_load_bearing = 0usize;

        for mutant in &mutants {
            // OUTSIDE the interval rule's own footprint the agreement is total.
            // INSIDE it the oracles are pre-interval by construction, so they are
            // the wrong yardstick; those mutants are counted, their divergence is
            // measured as the vacuity control, and their behaviour is pinned
            // case-by-case in `half_open_interval_is_a_token_not_a_relaxed_closer`
            // and at corpus scale by the a10 census count.
            if carries_interval_token(mutant) {
                carrying_interval += 1;
                for open_index in 0..=mutant.len() {
                    if matching_delimiter(mutant, open_index)
                        != legacy_matching_delimiter(mutant, open_index)
                    {
                        erasure_load_bearing += 1;
                    }
                }
                continue;
            }
            for open_index in 0..=mutant.len() {
                let expected = legacy_matching_delimiter(mutant, open_index);
                let actual = matching_delimiter(mutant, open_index);
                assert_eq!(
                    actual, expected,
                    "matching_delimiter drifted from its pre-collapse oracle at \
                     open_index {open_index} of {mutant:?}, which carries no \
                     interval literal and so must be untouched by that rule"
                );
                match expected {
                    Ok(_) => balanced += 1,
                    Err(issue) if issue.mismatched => mismatched += 1,
                    Err(_) => unclosed += 1,
                }
            }
            for separators in SEPARATOR_SETS {
                let expected = legacy_split_top_level(mutant, separators);
                let actual = split_top_level(mutant, separators);
                assert_eq!(
                    actual, expected,
                    "split_top_level drifted from its pre-collapse oracle on \
                     separators {separators:?} of {mutant:?}, which carries no \
                     interval literal"
                );
                match &expected {
                    Ok(spans) => {
                        split_ok += 1;
                        if spans.len() > 1 {
                            multi_span += 1;
                        }
                    }
                    Err(_) => split_err += 1,
                }
            }
        }

        // VACUITY CONTROL 1 — the corpus reaches every outcome class. A run
        // that never produced a mismatch, or never produced a successful
        // split, would report "no drift" without having asked the question.
        assert!(
            balanced > 0 && mismatched > 0 && unclosed > 0,
            "corpus did not reach all three matching outcomes: \
             balanced={balanced} mismatched={mismatched} unclosed={unclosed}"
        );
        assert!(
            split_ok > 0 && split_err > 0 && multi_span > 0,
            "corpus did not reach all three split outcomes: \
             ok={split_ok} err={split_err} multi_span={multi_span}"
        );

        // VACUITY CONTROL 2 — the corpus can SEE a wrong reader. Drop the
        // generic-angle rule, which is the subtlest thing the two old readers
        // shared, and the corpus must catch it. If this control ever stops
        // firing, the differential above has stopped proving anything.
        let mut caught = 0usize;
        for mutant in &mutants {
            for open_index in 0..=mutant.len() {
                if broken_matching_delimiter(mutant, open_index)
                    != legacy_matching_delimiter(mutant, open_index)
                {
                    caught += 1;
                }
            }
        }
        assert!(
            caught > 0,
            "the wrong-reader control was not caught by any of {} mutants; \
             the differential above is vacuous",
            mutants.len()
        );

        // VACUITY CONTROL 3 — the partition is LOAD-BEARING IN BOTH DIRECTIONS.
        //
        // If no mutant carried an interval, the differential above would be the
        // pre-interval one and would prove nothing about the rule this increment
        // landed. If interval-bearing mutants existed but the new reader answered
        // exactly as the pre-collapse oracle on all of them, then the rule would
        // be inert and the twelve recovered a10 field candidates could not have
        // come from it. Both halves must be non-zero.
        assert!(
            carrying_interval > 0,
            "no mutant carried an interval literal; the partition below is empty \
             and the interval rule is untested"
        );
        assert!(
            erasure_load_bearing > 0,
            "{carrying_interval} mutants carried an interval literal but the new \
             reader agreed with the pre-collapse oracle on every one of them; the \
             interval rule is inert"
        );
        // And the footprint must stay a MINORITY. The differential's strength is
        // the size of the set on which agreement is total; if the footprint ever
        // swallowed the corpus, "total agreement outside it" would be a claim
        // about almost nothing.
        assert!(
            carrying_interval * 4 < mutants.len(),
            "the interval footprint grew to {carrying_interval} of {} mutants; \
             the total-agreement claim now covers too little to be evidence",
            mutants.len()
        );

        // Printed so `--nocapture` reports the size of the claim rather than
        // just its verdict: "no drift" over 9 inputs and over 9000 are very
        // different sentences.
        println!(
            "one-reader differential: {} mutants; matching balanced={balanced} \
             mismatched={mismatched} unclosed={unclosed}; split ok={split_ok} \
             err={split_err} multi_span={multi_span}; wrong-reader control \
             caught on {caught} probes; interval footprint {carrying_interval} \
             mutants, on which the rule changed {erasure_load_bearing} oracle \
             answers",
            mutants.len()
        );
    }

    /// THE RULING fgdb-8kzt was parked on, as a measurement rather than a
    /// preference: the half-open interval is a TOKEN, not a relaxed closer.
    ///
    /// Two candidate grammars recover the same twelve a10 field candidates and
    /// produce a byte-identical census (11705 fields, 8433 ambiguities, all five
    /// transcript digests equal, measured):
    ///
    ///   TOKEN    — `(term,term]` / `[term,term)` is one inert token. Landed.
    ///   RELAXED  — any `(` may be closed by any `]` and any `[` by any `)`
    ///              ("V4" on the bead).
    ///
    /// They are indistinguishable on the real appendix and distinguishable on
    /// typos, which is the only place it matters for a checker whose job is to be
    /// unfoolable. This test pins that difference so nobody can quietly swap one
    /// for the other: the relaxed form must be CAUGHT by the same corpus that the
    /// landed form passes.
    #[test]
    fn half_open_interval_is_a_token_not_a_relaxed_closer() {
        // Every interval spelling the census region actually contains is
        // recognised AT ITS OPENING BRACKET, and consumes through its closer.
        for (text, opener, close) in [
            ("(retained_after_commit_seq,frontier]", 0usize, 35usize),
            ("(retained_after_global_commit_seq,frontier]", 0, 42),
            ("[0,byte_count)", 0, 13),
        ] {
            assert_eq!(
                half_open_interval_end(text, opener),
                Some(close),
                "{text:?} is an interval literal spelled by the plan"
            );
        }

        // The two real a10:1928 bodies, entered at the `[` of `entries[`: the
        // interval inside no longer breaks the enclosing map, which IS the twelve
        // recovered field candidates.
        for body in [
            "entries[every commit_seq in (retained_after_commit_seq,frontier] -> StrongRef<B>]",
            "entries[every global_commit_seq in (retained_after_global_commit_seq,frontier] -> StrongRef<G>]",
        ] {
            let open = body.find('[').expect("seed has a map bracket");
            assert_eq!(
                matching_delimiter(body, open),
                Ok(body.len() - 1),
                "{body:?}: an interior interval must not break the enclosing map"
            );
        }

        // A whole record carrying the mirror orientation is balanced too.
        let mirror = "{descriptors cover [0,byte_count) exactly}";
        assert_eq!(
            matching_delimiter(mirror, 0),
            Ok(mirror.len() - 1),
            "the mirror interval must not break its enclosing record"
        );

        // An interval bracket is NOT a body opener. `matching_delimiter` seeds its
        // stack directly from the byte at `open_index`, so the token has to be
        // inert there as well or the rule exists twice.
        let issue = matching_delimiter("[0,byte_count)", 0)
            .expect_err("an interval bracket does not open a record body");
        assert!(issue.mismatched);
        assert_eq!(issue.offset, 0);

        // Genuine mismatched-delimiter typos that must STAY mismatched. Each one
        // fails the token shape for a different, stated reason.
        const TYPOS: &[(&str, &str)] = &[
            ("{value:[u8;4)}", "separator is `;`, not the interval comma"),
            ("{entries[foo)}", "one term, no comma"),
            ("{x:(a]}", "one term, no comma"),
            ("{x:(a,b,c]}", "three terms"),
            ("{x:(a b,c]}", "a term contains a space"),
            ("{x:(a,]}", "second term is empty"),
            ("{x:([a],b]}", "a term is itself bracketed"),
        ];
        let mut relaxed_would_accept = 0usize;
        for (typo, why) in TYPOS {
            let issue = matching_delimiter(typo, 0)
                .expect_err(&format!("{typo:?} must stay mismatched ({why})"));
            assert!(
                issue.mismatched,
                "{typo:?} must be mismatched, not unclosed"
            );
            // INJECTED FAULT / VACUITY CONTROL: the relaxed grammar. If this
            // corpus stopped being able to see the difference, the ruling above
            // would be unenforced and this test would be decoration.
            if relaxed_closer_matching_delimiter(typo, 0).is_ok() {
                relaxed_would_accept += 1;
            }
        }
        assert!(
            relaxed_would_accept > 0,
            "the relaxed-closer control was accepted by none of the {} typos; \
             this test can no longer tell the two grammars apart",
            TYPOS.len()
        );
        println!(
            "interval-token ruling: {} typos stay mismatched under the token \
             grammar; the relaxed grammar would silently accept {relaxed_would_accept} \
             of them",
            TYPOS.len()
        );
    }

    /// fgdb-ihtt — the heading-led attribution, and the two ways it could be wrong.
    ///
    /// The appendix spells four bodies as `**A / B / C.** <anonymous phrase> is
    /// {…}`: CommitCommand@1912, LogicalDeltaTemplate@1924, RecoveryCheckpoint@1964
    /// and BranchManifest@2000. Before this rule all four bound nothing. The rule
    /// is that the heading's FIRST name owns that first body, and the whole-corpus
    /// numbers it moves are pinned in the catalog (a10 150->180, a12 197->213,
    /// a13 228->241; whole corpus 11712->11771 fields, the other 18 slices
    /// byte-identical). This test pins the RULE itself, where those pins cannot:
    /// a reader that attributed all four for the wrong reason would satisfy every
    /// count above and still be wrong.
    #[test]
    fn heading_led_body_binds_the_headings_first_name() {
        // One paragraph carrying both spellings at once: an anonymous first body,
        // then a backticked one — exactly the shape of plan line 1912.
        let source = concat!(
            "**Alpha / Beta / Gamma.** A transaction order unit is ",
            "`{id:u64,tag:u8}`. A `Beta` is `{other:u16}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "heading",
            start_line: 80,
            end_line: 80,
        }];
        let census = census_appendix_source(source.as_bytes(), 80, &slices)
            .expect("a heading-led paragraph must census");
        let paths = |family: &str| {
            let mut found = census
                .fields
                .iter()
                .filter(|field| field.key.schema_family == family)
                .map(|field| field.key.path.clone())
                .collect::<Vec<_>>();
            found.sort();
            found
        };

        // POSITIVE. The anonymous body belongs to the heading's first name.
        assert_eq!(
            paths("Alpha"),
            ["Alpha.id", "Alpha.tag"],
            "the anonymous body must bind to the heading's FIRST name"
        );

        // CONTROL 1 — WRONG NAME, and it FIRES. Measured: mutating `.next()` to
        // `.nth(1)` turns exactly these two tests red and leaves the module's other
        // 19 green.
        //
        // Why OWNERSHIP is asserted here and not a count. The catalog's census pins
        // lock how MANY candidates a slice has (a10 180, a12 213, a13 241); they
        // never lock WHICH schema owns them. Binding all four appendix bodies to
        // the wrong name of the right heading is precisely the defect those pins
        // cannot see, so it has to be caught here. `Beta` is a real type name that
        // owns a body of its own, which makes it the specific wrong answer this
        // fixture exists to rule out.
        assert_eq!(
            paths("Beta"),
            ["Beta.other"],
            "a later heading name owns only its own backticked body, never the \
             anonymous one; if this holds Beta.id the reader picked the wrong name"
        );
        assert_eq!(
            census.counts.field_candidates, 3,
            "fixture shape: the anonymous body's two members plus Beta's own"
        );
        assert!(
            paths("Gamma").is_empty(),
            "a heading name with no body of its own must bind nothing"
        );

        // CONTROL 2 — B CANNOT CLAIM A BACKTICKED INTRO, which is why shape B
        // needs no explicit guard against one. `Beta`'s body is introduced by a
        // backticked name, so it is `prose_schema_links`' to bind; B never sees it
        // because a backtick opens its own fragment and `before` is then " is ".
        // A guard rejecting a backtick inside the phrase was written and MEASURED
        // VACUOUS on the whole corpus (identical census with it disabled), so it
        // was dropped rather than shipped as if it earned its place.
        assert_eq!(
            bold_owner_name_range(" is "),
            None,
            "a backtick-introduced body leaves `before` as \" is \", which shape B \
             must not match — this is the guard, structurally"
        );

        // And shape A, the spelling that already worked, is untouched.
        assert_eq!(bold_owner_name_range("**Alpha**: "), Some(2..7));
    }

    /// fgdb-ihtt — the reader records the APPENDIX spelling, and does not reconcile
    /// it with the plan's prose copy.
    ///
    /// Plan line 393 spells a NAMED `CommitCommand` body with the same 21 members
    /// in the same order as the appendix body at 1912, but leaves two array element
    /// types bare. 393 sits outside `[source_manifest]` (1388-2728), so this reader
    /// never sees it, never compares the spellings and never normalises one to the
    /// other. That is deliberate: the appendix is the normative contract, and
    /// choosing between two spellings of a durable field is a format ruling, not a
    /// reader's call.
    ///
    /// The difference is LOAD-BEARING, not cosmetic, which is why silently
    /// normalising would be a defect rather than a tidy-up: the elaborated spelling
    /// yields a concrete element type, a `Many` cardinality, and four additional
    /// interior field candidates. On the real appendix that is the difference
    /// between CommitCommand's 25 recovered fields and the 21 the bare prose copy
    /// would give.
    #[test]
    fn heading_led_binding_records_the_appendix_spelling_verbatim() {
        let elaborated = concat!(
            "**Alpha / Beta.** A unit is `{expected_branch_heads[WeakMarkerIdentity],",
            "expected_branch_key_epochs[{graph,branch}]}`.\n",
        );
        let bare =
            "**Alpha / Beta.** A unit is `{expected_branch_heads,expected_branch_key_epochs}`.\n";
        let slices = [SourceSliceSpec {
            id: "spelling",
            start_line: 90,
            end_line: 90,
        }];
        let census =
            |text: &str| census_appendix_source(text.as_bytes(), 90, &slices).expect("must census");

        let rich = census(elaborated);
        let heads = rich
            .fields
            .iter()
            .find(|field| field.key.path == "Alpha.expected_branch_heads")
            .expect("the elaborated array member is a field candidate");
        assert_eq!(
            heads.exact_types,
            ["[WeakMarkerIdentity]"],
            "the element type must be recorded exactly as the appendix spells it"
        );
        assert_eq!(heads.cardinalities, [super::Cardinality::Many]);

        // The inline record's interior members are candidates in their own right.
        let interior = rich
            .fields
            .iter()
            .filter(|field| {
                field
                    .key
                    .path
                    .starts_with("Alpha.expected_branch_key_epochs.")
            })
            .count();
        assert_eq!(
            interior, 2,
            "an elaborated inline element type contributes its interior members"
        );

        // THE COUNTERFACTUAL, measured rather than asserted: the bare prose
        // spelling of the same two members yields strictly less. If the reader ever
        // normalised the elaborated form to this one it would silently discard
        // durable field rows.
        let plain = census(bare);
        assert_eq!(rich.counts.field_candidates, 4);
        assert_eq!(plain.counts.field_candidates, 2);
        let plain_heads = plain
            .fields
            .iter()
            .find(|field| field.key.path == "Alpha.expected_branch_heads")
            .expect("the bare member is still a field candidate");
        assert!(
            plain_heads.exact_types.is_empty()
                && plain_heads.cardinalities == [super::Cardinality::One],
            "the bare spelling carries no element type and no Many cardinality, \
             which is exactly what normalising the two would throw away"
        );
    }

    /// The REJECTED grammar from fgdb-8kzt ("V4"), kept only so
    /// `half_open_interval_is_a_token_not_a_relaxed_closer` can prove the corpus
    /// discriminates against it. Identical to the one reader except that the
    /// closer test is relaxed instead of the interval being tokenised.
    fn relaxed_closer_matching_delimiter(
        text: &str,
        open_index: usize,
    ) -> Result<usize, DelimiterIssue> {
        let bytes = text.as_bytes();
        let Some(opener) = bytes.get(open_index).copied() else {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: false,
            });
        };
        if !OPENING_DELIMITERS.contains(&opener) {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: true,
            });
        }
        let mut stack = vec![opener];
        let mut quote = None;
        let mut escaped = false;
        for (index, byte) in bytes.iter().copied().enumerate().skip(open_index + 1) {
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                continue;
            }
            if byte == b'<' && !is_generic_angle_open(text, index) {
                continue;
            }
            if OPENING_DELIMITERS.contains(&byte) {
                stack.push(byte);
                continue;
            }
            if byte == b'>' && stack.last() != Some(&b'<') {
                continue;
            }
            let Some(expected_opener) = opening_delimiter_for(byte) else {
                continue;
            };
            let popped = stack.pop();
            // THE REJECTED RELAXATION: any `(`/`]` or `[`/`)` pair passes.
            let interval = matches!((popped, byte), (Some(b'('), b']') | (Some(b'['), b')'));
            if popped != Some(expected_opener) && !interval {
                return Err(DelimiterIssue {
                    offset: index,
                    mismatched: true,
                });
            }
            if stack.is_empty() {
                return Ok(index);
            }
        }
        Err(DelimiterIssue {
            offset: text.len(),
            mismatched: false,
        })
    }

    /// A deliberately WRONG reader: identical to the real one except that it
    /// treats every `<` as an opening delimiter. Exists only so
    /// `one_delimiter_reader_answers_exactly_as_the_two_it_replaced` can prove
    /// its corpus is capable of catching a difference.
    fn broken_matching_delimiter(text: &str, open_index: usize) -> Result<usize, DelimiterIssue> {
        let bytes = text.as_bytes();
        let Some(opener) = bytes.get(open_index).copied() else {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: false,
            });
        };
        if !matches!(opener, b'{' | b'[' | b'(' | b'<') {
            return Err(DelimiterIssue {
                offset: open_index,
                mismatched: true,
            });
        }
        let mut stack = vec![opener];
        let mut quote = None;
        let mut escaped = false;
        for (index, byte) in bytes.iter().copied().enumerate().skip(open_index + 1) {
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            if matches!(byte, b'\'' | b'"') {
                quote = Some(byte);
                continue;
            }
            // THE INJECTED FAULT: no is_generic_angle_open test.
            if matches!(byte, b'{' | b'[' | b'(' | b'<') {
                stack.push(byte);
                continue;
            }
            if !matches!(byte, b'}' | b']' | b')' | b'>') {
                continue;
            }
            if byte == b'>' && stack.last() != Some(&b'<') {
                continue;
            }
            let expected = match stack.last().copied() {
                Some(b'{') => b'}',
                Some(b'[') => b']',
                Some(b'(') => b')',
                Some(b'<') => b'>',
                _ => unreachable!(),
            };
            if byte != expected {
                return Err(DelimiterIssue {
                    offset: index,
                    mismatched: true,
                });
            }
            stack.pop();
            if stack.is_empty() {
                return Ok(index);
            }
        }
        Err(DelimiterIssue {
            offset: text.len(),
            mismatched: false,
        })
    }

    /// COMPLETENESS GUARD for the collapse.
    ///
    /// Every other test in this file can only exercise a reader it knows the
    /// name of, so a THIRD delimiter stack would be invisible to all of them —
    /// the law would fail open in exactly the way fgdb-8kzt describes, where a
    /// fix lands in whichever reader the fixer happens to open. This guard is
    /// therefore over the SOURCE: one stack, one push, one pop, one copy of
    /// each delimiter set.
    ///
    /// It reads only the production half of the file. The frozen pre-collapse
    /// oracles above are second and third stacks BY DESIGN, and must not count.
    #[test]
    fn exactly_one_delimiter_reader_exists() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/appendix_source.rs"
        ));
        let marker = "\n#[cfg(test)]\nmod tests {";
        assert_eq!(
            source.matches(marker).count(),
            1,
            "the production/test split marker must occur exactly once, or this \
             guard is reading the wrong half of the file"
        );
        let production = &source[..source.find(marker).expect("split marker")];

        // Controls: the extractor must see something real and must not see
        // something fabricated. Without these a typo'd needle reports 0 and
        // every assertion below passes for the wrong reason.
        assert!(
            production.contains("struct DelimiterScan"),
            "known-present control missing: the extractor is not reading the \
             production source"
        );
        assert!(
            !production.contains("struct DefinitelyFabricatedScanner"),
            "fabricated control found: the extractor matches anything"
        );

        for (needle, expected, why) in [
            (
                "stack: Vec<u8>",
                1usize,
                "exactly one type carries a delimiter stack",
            ),
            (
                "self.stack.push(",
                1,
                "exactly one site decides that a byte opens a nesting level",
            ),
            (
                "self.stack.pop(",
                1,
                "exactly one site decides that a byte closes one",
            ),
            (
                "let mut stack",
                0,
                "a local delimiter stack is how the twin was spelled; there \
                 must be none left outside DelimiterScan",
            ),
            (
                "const OPENING_DELIMITERS",
                1,
                "the opening set is spelled once",
            ),
            (
                "const CLOSING_DELIMITERS",
                1,
                "the closing set is spelled once",
            ),
        ] {
            assert_eq!(
                production.matches(needle).count(),
                expected,
                "{needle:?} appears {} times in the production source, expected \
                 {expected} — {why}",
                production.matches(needle).count()
            );
        }

        // The residual, pinned rather than hidden. `top_level_arrow` and
        // `outermost_record_ranges` each still run their own quote/escape
        // machine before delegating the nesting to `matching_delimiter`. They
        // are NOT foldable into DelimiterScan: both deliberately tolerate a
        // stray top-level closer that the strict reader rejects — see
        // `partial_scanners_tolerate_a_stray_closer_the_one_reader_rejects`.
        // Pinning the count is what stops a third one appearing unnoticed.
        assert_eq!(
            production.matches("escaped = true").count(),
            4,
            "expected exactly four escape machines in production: \
             DelimiterScan (the one reader), normalize_whitespace (a text \
             emitter, not a structural reader), and the two partial scanners \
             top_level_arrow and outermost_record_ranges. A fifth is a new \
             duplicate and needs its own justification."
        );
    }

    /// Why the two partial scanners were left alone, stated as a test rather
    /// than a claim: folding them into the one reader would CHANGE BEHAVIOUR,
    /// because they ignore a stray top-level closer that `DelimiterScan`
    /// treats as a mismatch. Recording the divergence keeps a future collapse
    /// honest about what it would be changing.
    #[test]
    fn partial_scanners_tolerate_a_stray_closer_the_one_reader_rejects() {
        let stray = "a}b->c";
        assert_eq!(
            top_level_arrow(stray),
            Some(3),
            "top_level_arrow walks past a stray top-level closer"
        );
        let strict = split_top_level(stray, b",").expect_err("the one reader rejects it");
        assert!(
            strict.mismatched,
            "the one reader must call a stray top-level closer a mismatch"
        );
    }

    fn assert_ambiguity_relation_digest(candidate: &AmbiguityCandidate) {
        let expected =
            source_key_transcript(candidate.affected_source_keys.iter().map(String::as_str));
        assert_eq!(candidate.key.affected_source_key_count, expected.rows);
        assert_eq!(candidate.key.affected_source_keys_sha256, expected.sha256);
    }

    #[test]
    fn canonical_ambiguity_affinity_is_typed_and_consistent() {
        let source_map = SourceMap::new("x", 1);
        let schema_key = StructuralCandidateKey::Schema(SchemaCandidateKey {
            family: "Same".to_owned(),
            generic_signature: String::new(),
        });
        let field_key = StructuralCandidateKey::Field(FieldCandidateKey {
            schema_family: "Same".to_owned(),
            schema_owner: "Same".to_owned(),
            path: "Same.x".to_owned(),
            stable_name: "x".to_owned(),
        });
        let invalid_type = AmbiguityOccurrence {
            kind: AmbiguityKind::UnparsedUnionArm,
            schema_family: Some("Same".to_owned()),
            path: Some("Same".to_owned()),
            raw: "x".to_owned(),
            reason: "fixture".to_owned(),
            affected_source_keys: affected_source_key(schema_key.clone()),
            source_range: 0..1,
        };
        let error = canonical_ambiguities(&[&invalid_type], &source_map)
            .expect_err("an unparsed union arm cannot affect a top-level schema key");
        assert_eq!(error.kind, CensusErrorKind::CandidateAssignmentInvariant);

        let first = AmbiguityOccurrence {
            kind: AmbiguityKind::NestingLimitExceeded,
            schema_family: Some("Same".to_owned()),
            path: Some("Same.x".to_owned()),
            raw: "x".to_owned(),
            reason: "fixture".to_owned(),
            affected_source_keys: affected_source_key(schema_key),
            source_range: 0..1,
        };
        let second = AmbiguityOccurrence {
            affected_source_keys: affected_source_key(field_key),
            ..first.clone()
        };
        let error = canonical_ambiguities(&[&first, &second], &source_map)
            .expect_err("one ambiguity identity cannot broaden to two affected key sets");
        assert_eq!(error.kind, CensusErrorKind::CandidateAssignmentInvariant);
    }

    #[test]
    fn balanced_delimiters_ignore_quotes_and_non_generic_angles() {
        let value = r#"{ left: Vec<A,B>, literal: "}])>", arrow: A->B, compare: x>=y }"#;
        assert_eq!(matching_delimiter(value, 0), Ok(value.len() - 1));

        let generic = "Outer<Inner<A|B>, [C,D]>";
        assert_eq!(matching_delimiter(generic, 5), Ok(generic.len() - 1));

        let malformed = "{ value: [u8; 4) }";
        let issue = matching_delimiter(malformed, 0).expect_err("mismatched delimiter must fail");
        assert!(issue.mismatched);

        assert_eq!(
            normalize_whitespace("  Type < \"a  b\" ,  'c\\'  d' >  "),
            "Type < \"a  b\" , 'c\\'  d' >"
        );
        assert_ne!(
            normalize_whitespace("\"a  b\""),
            normalize_whitespace("\"a b\"")
        );
    }

    #[test]
    fn top_level_split_respects_nested_structures_and_quotes() {
        let value = r#"alpha, nested: Box<A,B>, record: R{x:1,y:"a,b"}, omega"#;
        let pieces = split_top_level(value, b",").expect("balanced source must split");
        let rendered: Vec<_> = pieces
            .into_iter()
            .map(|span| value[span.start..span.end].trim())
            .collect();
        assert_eq!(
            rendered,
            [
                "alpha",
                "nested: Box<A,B>",
                "record: R{x:1,y:\"a,b\"}",
                "omega"
            ]
        );

        let union = "Left{x:u8}|Middle<Vec<A|B>>|Right{y:u16}";
        assert_eq!(
            split_top_level(union, b"|")
                .expect("balanced union must split")
                .len(),
            3
        );
    }

    #[test]
    fn census_and_transcripts_are_deterministic() {
        let source = concat!(
            "`Thing` is `{ id:u64, state: Ready{code:u16}|Done, child: Child{name:String} }`.\n",
            "`Choice = Left{x:u8} | Right{y:Vec<A,B>}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "sample",
            start_line: 40,
            end_line: 41,
        }];
        let first = census_appendix_source(source.as_bytes(), 40, &slices)
            .expect("well-formed sample must census");
        let second = census_appendix_source(source.as_bytes(), 40, &slices)
            .expect("same sample must census twice");
        assert_eq!(first, second);
        assert_eq!(first.counts.schema_candidates, 2);
        assert_eq!(first.counts.field_candidates, 7);
        assert_eq!(first.counts.union_candidates, 2);
        assert_eq!(first.counts.arm_candidates, 4);
        assert_eq!(first.counts.ambiguities, 0);
        assert_eq!(first.slices[0].transcripts, first.transcripts);
        assert_ne!(
            first.transcripts.schemas.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(
            first
                .fields
                .iter()
                .all(|candidate| !candidate.locations.is_empty())
        );
    }

    #[test]
    fn indexed_map_value_unions_are_first_class_without_reclassifying_plain_maps() {
        let source = concat!(
            "`Indexed = {entries:[u16 -> Empty|Record{value:u8}|",
            "typed_ref:StrongRef<T>],plain:[u8 -> u16],",
            "nested:[u8 -> Wrapper{state:Open|Closed}]}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "indexed",
            start_line: 60,
            end_line: 60,
        }];
        let census = census_appendix_source(source.as_bytes(), 60, &slices)
            .expect("indexed map value unions must census");

        let entries = census
            .unions
            .iter()
            .find(|union| union.key.union_path == "Indexed.entries")
            .expect("the indexed-map value is a first-class union");
        assert_eq!(
            entries.arm_names,
            [
                "Empty".to_owned(),
                "Record".to_owned(),
                "typed_ref".to_owned()
            ]
        );
        assert!(
            census
                .fields
                .iter()
                .any(|field| field.key.path == "Indexed.entries.Record.value"),
            "record-arm field paths remain unchanged"
        );

        let typed = census
            .arms
            .iter()
            .find(|arm| arm.key.arm_name == "typed_ref")
            .expect("a typed scalar value arm remains source-visible");
        assert_eq!(
            typed.payload_sha256s,
            [sha256_hex(b"StrongRef<T>")],
            "the exact type is committed as the arm payload"
        );

        assert!(
            census
                .unions
                .iter()
                .all(|union| union.key.union_path != "Indexed.plain"),
            "a plain indexed map is not a union"
        );
        assert!(
            census
                .unions
                .iter()
                .all(|union| union.key.union_path != "Indexed.nested"),
            "a map-to-record with a nested union is not a map-value union"
        );
        assert!(
            census
                .unions
                .iter()
                .any(|union| union.key.union_path == "Indexed.nested.Wrapper.state"),
            "the nested record union remains visible at its existing path"
        );
        assert_eq!(census.counts.field_candidates, 5);
        assert_eq!(census.counts.union_candidates, 2);
        assert_eq!(census.counts.arm_candidates, 5);
        assert_eq!(census.counts.ambiguities, 0);
    }

    #[test]
    fn top_level_map_value_unions_exclude_the_map_key_from_the_arm_census() {
        let source = concat!(
            "`TrustRoot` is the trust map `authority_domain:ConsensusDomain -> ",
            "CurrentEvidence{evidence_ref:StrongRef<Evidence>}|",
            "ValidatedAnchor{anchor_ref:StrongRef<Anchor>}`.\n",
            "`RetentionRoot` is the retention map `(authority_domain,grant_id) -> ",
            "Active{record_ref:StrongRef<Record>}|",
            "ReleaseApplied{tombstone_ref:StrongRef<Tombstone>}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "top-level-map",
            start_line: 70,
            end_line: 71,
        }];
        let census = census_appendix_source(source.as_bytes(), 70, &slices)
            .expect("top-level map-value unions must census");

        for (union_path, expected_arms) in [
            (
                "TrustRoot",
                ["CurrentEvidence", "ValidatedAnchor"].as_slice(),
            ),
            ("RetentionRoot", ["Active", "ReleaseApplied"].as_slice()),
        ] {
            let union = census
                .unions
                .iter()
                .find(|union| union.key.union_path == union_path)
                .expect("map value is a first-class union");
            assert_eq!(
                union.arm_names,
                expected_arms
                    .iter()
                    .map(|arm| (*arm).to_owned())
                    .collect::<Vec<_>>()
            );
            assert_eq!(union.unparsed_arm_count, 0);
        }
        assert!(
            census
                .arms
                .iter()
                .all(|arm| arm.key.arm_name != "authority_domain"),
            "the typed map key must never be substituted for the first value arm"
        );
        assert_eq!(census.counts.union_candidates, 2);
        assert_eq!(census.counts.arm_candidates, 4);
        assert_eq!(census.counts.ambiguities, 0);
    }

    #[test]
    fn union_candidate_survives_when_every_arm_is_unparseable() {
        let source = "`Odd = ? | !`.\n";
        let slices = [SourceSliceSpec {
            id: "odd",
            start_line: 3,
            end_line: 3,
        }];
        let census = census_appendix_source(source.as_bytes(), 3, &slices)
            .expect("unparseable arms are census ambiguities, not fatal errors");
        assert_eq!(census.counts.union_occurrences, 1);
        assert_eq!(census.counts.union_candidates, 1);
        assert_eq!(census.counts.arm_candidates, 0);
        assert_eq!(census.unions[0].unparsed_arm_count, 2);
        assert_eq!(census.transcripts.unions.rows, 1);
        assert_eq!(
            census
                .ambiguities
                .iter()
                .filter(|row| row.key.kind == AmbiguityKind::UnparsedUnionArm)
                .count(),
            2
        );
        assert!(census.ambiguities.iter().all(|row| {
            row.key.kind != AmbiguityKind::UnparsedUnionArm
                || row.affected_source_keys == ["union|Odd|Odd"]
        }));
        for ambiguity in &census.ambiguities {
            assert_ambiguity_relation_digest(ambiguity);
        }
    }

    #[test]
    fn arm_tokens_preserve_hex_tags_and_qualified_paths() {
        let source = concat!(
            "`Tagged = 0x0001 Local{x:u8} | 0x0002 Meta | ",
            "OperationAuditAdmission::Claimed | *`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "tagged",
            start_line: 12,
            end_line: 12,
        }];
        let census = census_appendix_source(source.as_bytes(), 12, &slices)
            .expect("tagged and qualified arms must be source-censusable");
        let names: Vec<_> = census
            .arms
            .iter()
            .map(|arm| arm.key.arm_name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "*",
                "0x0001 Local",
                "0x0002 Meta",
                "OperationAuditAdmission::Claimed",
            ]
        );
        assert_eq!(census.counts.ambiguities, 0);
    }

    #[test]
    fn supplemental_body_union_is_owned_for_joined_and_split_arm_spellings() {
        for (label, source) in [
            (
                "joined",
                concat!(
                    "`Manifest` is the closed union with common `{id:u64}` ",
                    "and exactly one posture body: ",
                    "`Local{local_value:u8}|Sharded{sharded_value:u16}`.\n",
                ),
            ),
            (
                "split",
                concat!(
                    "`Manifest` is the closed union with common `{id:u64}` ",
                    "and exactly one body: `Local{local_value:u8}` or ",
                    "`Sharded{sharded_value:u16}`.\n",
                ),
            ),
        ] {
            let slices = [SourceSliceSpec {
                id: label,
                start_line: 20,
                end_line: 20,
            }];
            let census = census_appendix_source(source.as_bytes(), 20, &slices)
                .expect("a confirmed owner must retain its supplemental union");

            for path in [
                "Manifest.id",
                "Manifest.Local.local_value",
                "Manifest.Sharded.sharded_value",
            ] {
                assert!(
                    census
                        .fields
                        .iter()
                        .any(|candidate| candidate.key.path == path),
                    "{label}: missing {path}"
                );
            }
            let union = census
                .unions
                .iter()
                .find(|candidate| {
                    candidate.key.schema_family == "Manifest"
                        && candidate.key.union_path == "Manifest"
                })
                .expect("the posture body must be a union owned by Manifest");
            assert_eq!(union.arm_names, ["Local".to_owned(), "Sharded".to_owned()]);
            assert!(
                census
                    .schemas
                    .iter()
                    .all(|candidate| !matches!(candidate.key.family.as_str(), "Local" | "Sharded")),
                "{label}: an arm was misclassified as a top-level schema"
            );
            assert!(
                census
                    .ambiguities
                    .iter()
                    .all(|candidate| candidate.key.kind != AmbiguityKind::UnparsedTrailingTokens),
                "{label}: the supplemental union was left as trailing tokens"
            );
        }
    }

    #[test]
    fn one_braced_fragment_does_not_invent_a_supplemental_union() {
        let source = concat!(
            "`Manifest` is the closed union with common `{id:u64}` ",
            "and exactly one body: `Local{local_value:u8}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "one-arm",
            start_line: 30,
            end_line: 30,
        }];
        let census = census_appendix_source(source.as_bytes(), 30, &slices)
            .expect("an incomplete body union must remain explicit");
        assert!(
            census
                .unions
                .iter()
                .all(|candidate| candidate.key.schema_family != "Manifest"),
            "one braced fragment is not evidence for a closed union"
        );
        assert!(
            census
                .fields
                .iter()
                .all(|candidate| candidate.key.path != "Manifest.Local.local_value"),
            "the unlicensed arm was silently attached to Manifest"
        );
    }

    #[test]
    fn appendix_supplemental_body_unions_recover_a18_and_a20_members() {
        let plan = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
        ));
        let source = plan
            .lines()
            .skip(1387)
            .take(2728 - 1388 + 1)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let slices = [SourceSliceSpec {
            id: "appendix-a",
            start_line: 1388,
            end_line: 2728,
        }];
        let census = census_appendix_source(source.as_bytes(), 1388, &slices)
            .expect("the pinned Appendix A source must census");

        for path in [
            "RestoreServicePromotionManifest.Local.local_prepare_evidence",
            "RestoreServicePromotionManifest.Local.local_configuration_and_endpoint_commitment",
            "RestoreAbandonmentManifest.Local.target_tombstone_skeleton_recipe",
            "RestoreAbandonmentManifest.Local.no_target_observation_proof_public_commitment",
            "RestoreAbandonmentManifest.Local.local_pending_owner_public_commitment",
        ] {
            assert!(
                census
                    .fields
                    .iter()
                    .any(|candidate| candidate.key.path == path),
                "the real-source control {path} is still invisible"
            );
        }
        for owner in [
            "RestoreAbandonmentManifest",
            "RestoreServicePromotionManifest",
        ] {
            let union = census
                .unions
                .iter()
                .find(|candidate| {
                    candidate.key.schema_family == owner && candidate.key.union_path == owner
                })
                .expect("the real Appendix owner must retain its posture body union");
            assert_eq!(union.arm_names, ["Local".to_owned(), "Sharded".to_owned()]);
        }
        assert!(
            census
                .schemas
                .iter()
                .all(|candidate| candidate.key.family != "Sharded"),
            "the a20 body arm remains misclassified as a top-level schema"
        );
        assert!(
            census
                .fields
                .iter()
                .all(|candidate| candidate.key.stable_name != "fabricated_qh3r_control"),
            "fabricated-absent control unexpectedly matched"
        );
    }

    #[test]
    fn skipped_incidental_spans_do_not_steal_prose_ownership() {
        let source = concat!(
            "`Evidence` is stable with `Other=7`, then stops.\n",
            "`SecurityBasis` is the exact embedded `u16` union ",
            "`0x0001 Local{x:u8}|0x0002 Meta{y:u16}`.\n",
            "`Thing` is exactly `Thing = {x:u8}`.\n",
            "`First` is stable; `Second` is `{y:u16}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "prose",
            start_line: 30,
            end_line: 33,
        }];
        let census = census_appendix_source(source.as_bytes(), 30, &slices)
            .expect("prose ownership must be conservative and deterministic");
        let evidence = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "Evidence")
            .expect("the named owner remains visible without a body");
        assert!(
            evidence
                .owner_statuses
                .contains(&super::SchemaOwnerStatus::NamedConceptNoBody)
        );
        let security = census
            .unions
            .iter()
            .find(|union| union.key.schema_family == "SecurityBasis")
            .expect("the scanner skips incidental u16 and finds the tagged union");
        assert_eq!(
            security.arm_names,
            ["0x0001 Local".to_owned(), "0x0002 Meta".to_owned()]
        );
        assert!(
            census
                .schemas
                .iter()
                .any(|schema| schema.key.family == "Other")
        );
        let thing = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "Thing")
            .expect("same-owner explicit assignment is parsed once by direct grammar");
        assert_eq!(thing.locations.len(), 1);
        assert!(!thing.body_conflict);
        assert!(
            census
                .schemas
                .iter()
                .find(|schema| schema.key.family == "First")
                .is_some_and(|schema| {
                    schema
                        .owner_statuses
                        .contains(&super::SchemaOwnerStatus::NamedConceptNoBody)
                })
        );
        assert!(
            census
                .fields
                .iter()
                .any(|field| field.key.schema_family == "Second")
        );
    }

    #[test]
    fn malformed_structural_remainders_are_explicit_ambiguities() {
        let source = concat!(
            "`Record = {field:,other=,,field:u8}`.\n",
            "`Choice = Left{x:u8} junk | Right | `.\n",
            "`Unowned{value:u8}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "ambiguous",
            start_line: 50,
            end_line: 52,
        }];
        let census = census_appendix_source(source.as_bytes(), 50, &slices)
            .expect("uncertain structural source must census as ambiguity");
        let kinds: std::collections::BTreeSet<_> =
            census.ambiguities.iter().map(|row| row.key.kind).collect();
        assert!(kinds.contains(&AmbiguityKind::FieldTypeAmbiguous));
        assert!(kinds.contains(&AmbiguityKind::UnparsedRecordItem));
        assert!(kinds.contains(&AmbiguityKind::UnparsedTrailingTokens));
        assert!(kinds.contains(&AmbiguityKind::UnparsedUnionArm));
        assert!(kinds.contains(&AmbiguityKind::AmbiguousSchemaOwner));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::FieldTypeAmbiguous
                && row.raw == "field:"
                && row.affected_source_keys == ["field|Record|Record.field|field"]
        }));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::UnparsedRecordItem
                && row.raw.is_empty()
                && row.affected_source_keys == ["top|Record"]
        }));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::UnparsedTrailingTokens
                && row.raw == "junk"
                && row.affected_source_keys == ["arm|Choice|Choice|Left"]
        }));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::UnparsedUnionArm
                && row.affected_source_keys == ["union|Choice|Choice"]
        }));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::AmbiguousSchemaOwner
                && row.affected_source_keys == ["top|Unowned"]
        }));
        for ambiguity in &census.ambiguities {
            assert_ambiguity_relation_digest(ambiguity);
        }
    }

    #[test]
    fn residual_structure_empty_alias_and_same_line_duplicates_stay_visible() {
        let source = concat!(
            "`Empty =`.\n",
            "`Good={x:u8}; ?|!`.\n",
            "```text\nFence{x:u8}\n?|!\n```\n",
            "`Twin={x:u8}; Twin={x:u8}`.\n",
            "`Nested={pair:Pair<{x:u8},{x:u16}>,maybe?}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "coverage",
            start_line: 90,
            end_line: 97,
        }];
        let census = census_appendix_source(source.as_bytes(), 90, &slices)
            .expect("every residual structural region must be claimed or ambiguous");
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::AliasExpressionUnparsed
                && row.key.schema_family.as_deref() == Some("Empty")
        }));
        assert_eq!(
            census
                .ambiguities
                .iter()
                .filter(|row| row.key.kind == AmbiguityKind::UnownedStructuralFragment)
                .count(),
            2
        );
        assert!(
            census
                .ambiguities
                .iter()
                .filter(|row| row.key.kind == AmbiguityKind::UnownedStructuralFragment)
                .all(|row| row.affected_source_keys.is_empty())
        );
        let twin = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "Twin")
            .expect("same-line declarations share a candidate but retain occurrences");
        assert_eq!(twin.locations.len(), 2);
        assert_eq!(
            census
                .fields
                .iter()
                .filter(|field| field.key.schema_family == "Nested" && field.key.stable_name == "x")
                .count(),
            2
        );
        let maybe = census
            .fields
            .iter()
            .find(|field| field.key.stable_name == "maybe")
            .expect("optional shorthand field remains visible");
        assert!(maybe.exact_types.is_empty());
    }

    #[test]
    fn bold_owner_location_and_conflicts_retain_exact_evidence() {
        let source = concat!(
            "**BoldOwner**: `{x:u8}`.\n",
            "`Same = Left{x:u8}|Right`.\n",
            "`Same = Left{x:u16}|Other`.\n",
            "**Assigned**: `Assigned={z:u8}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "evidence",
            start_line: 70,
            end_line: 73,
        }];
        let census = census_appendix_source(source.as_bytes(), 70, &slices)
            .expect("bold owners and divergent candidates must retain evidence");
        let bold = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "BoldOwner")
            .expect("bold owner must be captured");
        assert_eq!(bold.locations[0].start.line, 70);
        assert_eq!(bold.locations[0].start.column, 3);
        let assigned = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "Assigned")
            .expect("bold same-owner assignment must normalize to its RHS");
        assert_eq!(assigned.locations.len(), 1);
        assert!(!assigned.body_conflict);
        assert!(
            census
                .fields
                .iter()
                .any(|field| field.key.schema_family == "Assigned")
        );

        let same = census
            .schemas
            .iter()
            .find(|schema| schema.key.family == "Same")
            .expect("duplicate schema key must canonicalize");
        assert!(same.body_conflict);
        let union = census
            .unions
            .iter()
            .find(|union| union.key.schema_family == "Same")
            .expect("duplicate union key must canonicalize");
        assert!(union.arm_set_conflict);
        assert_eq!(union.arm_name_sets.len(), 2);
        assert!(census.fields.iter().any(|field| field.type_conflict));
        assert!(census.arms.iter().any(|arm| arm.payload_conflict));
        let conflicting: Vec<_> = census
            .ambiguities
            .iter()
            .filter(|row| row.key.kind == AmbiguityKind::ConflictingCandidateEvidence)
            .collect();
        assert!(!conflicting.is_empty());
        assert!(conflicting.iter().any(|row| {
            row.key.reason == "the same schema source key has divergent structural bodies"
                && row.affected_source_keys == ["top|Same"]
        }));
        assert!(conflicting.iter().any(|row| {
            row.key.reason == "the same field source key has divergent exact types"
                && row.affected_source_keys == ["field|Same|Same.Left.x|x"]
        }));
        assert!(conflicting.iter().any(|row| {
            row.key.reason == "the same union source key has divergent arm sets"
                && row.affected_source_keys == ["union|Same|Same"]
        }));
        assert!(conflicting.iter().any(|row| {
            row.key.reason == "the same arm source key has divergent payloads"
                && row.affected_source_keys == ["arm|Same|Same|Left"]
        }));
        for ambiguity in &census.ambiguities {
            assert_ambiguity_relation_digest(ambiguity);
        }
    }

    #[test]
    fn canonical_candidates_belong_only_to_their_earliest_slice() {
        let source = concat!(
            "`Same = Left{x:u8,z} | Right{y:u16}`.\n",
            "`Same = Left{x:u8,z} | Right{y:u16}`.\n",
        );
        let slices = [
            SourceSliceSpec {
                id: "early",
                start_line: 20,
                end_line: 20,
            },
            SourceSliceSpec {
                id: "late",
                start_line: 21,
                end_line: 21,
            },
        ];
        let census = census_appendix_source(source.as_bytes(), 20, &slices)
            .expect("duplicate evidence across slices must canonicalize");
        assert_eq!(census.counts.schema_occurrences, 2);
        assert_eq!(census.counts.schema_candidates, 1);
        assert_eq!(census.counts.union_candidates, 1);
        assert_eq!(census.counts.field_candidates, 3);
        assert_eq!(census.counts.arm_candidates, 2);
        assert_eq!(census.counts.ambiguities, 1);
        assert_eq!(census.schemas[0].locations.len(), 2);

        let early = &census.slices[0];
        let late = &census.slices[1];
        assert_eq!(early.counts.schema_candidates, 1);
        assert_eq!(late.counts.schema_candidates, 0);
        assert_eq!(early.counts.union_candidates, 1);
        assert_eq!(late.counts.union_candidates, 0);
        assert_eq!(early.counts.field_candidates, 3);
        assert_eq!(late.counts.field_candidates, 0);
        assert_eq!(early.counts.arm_candidates, 2);
        assert_eq!(late.counts.arm_candidates, 0);
        assert_eq!(early.counts.ambiguities, 1);
        assert_eq!(late.counts.ambiguities, 0);

        assert_eq!(
            census
                .slices
                .iter()
                .map(|slice| slice.counts.schema_candidates)
                .sum::<usize>(),
            census.counts.schema_candidates
        );
        assert_eq!(
            census
                .slices
                .iter()
                .map(|slice| slice.counts.field_candidates)
                .sum::<usize>(),
            census.counts.field_candidates
        );
        assert_eq!(
            census
                .slices
                .iter()
                .map(|slice| slice.counts.union_candidates)
                .sum::<usize>(),
            census.counts.union_candidates
        );
        assert_eq!(
            census
                .slices
                .iter()
                .map(|slice| slice.counts.arm_candidates)
                .sum::<usize>(),
            census.counts.arm_candidates
        );
        assert_eq!(
            census
                .slices
                .iter()
                .map(|slice| slice.counts.ambiguities)
                .sum::<usize>(),
            census.counts.ambiguities
        );
    }

    #[test]
    fn malformed_and_unbalanced_source_becomes_ambiguity() {
        let source = "`Broken{field:u64`.\n```text\nFence { value:u8\n";
        let slices = [SourceSliceSpec {
            id: "broken",
            start_line: 7,
            end_line: 9,
        }];
        let census = census_appendix_source(source.as_bytes(), 7, &slices)
            .expect("structural uncertainty must be represented, not fatal");
        let kinds: std::collections::BTreeSet<_> = census
            .ambiguities
            .iter()
            .map(|candidate| candidate.key.kind)
            .collect();
        assert!(kinds.contains(&AmbiguityKind::UnbalancedDefinition));
        assert!(kinds.contains(&AmbiguityKind::UnterminatedCodeFence));
        assert!(census.ambiguities.iter().any(|row| {
            row.key.kind == AmbiguityKind::UnbalancedDefinition
                && row.key.path.as_deref() == Some("Broken")
                && row.affected_source_keys == ["top|Broken"]
        }));
        assert!(
            census
                .ambiguities
                .iter()
                .filter(|row| row.key.kind == AmbiguityKind::UnterminatedCodeFence)
                .all(|row| row.affected_source_keys.is_empty())
        );
        assert!(
            census
                .ambiguities
                .iter()
                .all(|candidate| !candidate.locations.is_empty())
        );
        for ambiguity in &census.ambiguities {
            assert_ambiguity_relation_digest(ambiguity);
        }
    }

    #[test]
    fn slice_coverage_is_input_driven_and_must_be_exact() {
        let source = "first\nsecond\n";
        let gap = [SourceSliceSpec {
            id: "late",
            start_line: 11,
            end_line: 11,
        }];
        let error = census_appendix_source(source.as_bytes(), 10, &gap)
            .expect_err("a source-line gap must fail");
        assert_eq!(error.kind, CensusErrorKind::SliceGap);

        let complete = [
            SourceSliceSpec {
                id: "first",
                start_line: 10,
                end_line: 10,
            },
            SourceSliceSpec {
                id: "second",
                start_line: 11,
                end_line: 11,
            },
        ];
        let census = census_appendix_source(source.as_bytes(), 10, &complete)
            .expect("caller-defined contiguous slices must work");
        assert_eq!(census.slices.len(), 2);
        assert_eq!(census.slices[0].source_byte_count, 6);
        assert_eq!(census.slices[1].source_byte_count, 7);

        let overflowing = [SourceSliceSpec {
            id: "overflow",
            start_line: usize::MAX,
            end_line: usize::MAX,
        }];
        let error = census_appendix_source(b"first\nsecond", usize::MAX, &overflowing)
            .expect_err("unrepresentable source coordinates must not panic");
        assert_eq!(error.kind, CensusErrorKind::SourceCoordinateOverflow);
    }

    fn census_801o_fixture(source: &str) -> super::AppendixSourceCensus {
        let slices = [SourceSliceSpec {
            id: "801o",
            start_line: 1,
            end_line: 1,
        }];
        census_appendix_source(source.as_bytes(), 1, &slices)
            .expect("the two-union fixture must census")
    }

    /// The metamorphic contract in both directions. Joining two definitions
    /// into one sentence and splitting that sentence again must preserve the
    /// union and arm transcripts.
    #[test]
    fn two_union_sentence_join_and_split_are_transcript_invariant() {
        const ORDER_JOINED: &str = concat!(
            "`LocalOrderSubject` is ",
            "`Terminal{reservation_ref:StrongRef<LocalReservation>}|",
            "Control{typed_payload_ref:StrongRef<SequenceNeutralSpec>}`; ",
            "`MetaOrderSubject` is the corresponding ",
            "`Terminal{reservation_ref:StrongRef<GlobalReservation>}|",
            "Control{typed_payload_ref:StrongRef<GlobalSequenceNeutralSpec>}`.\n",
        );
        const ORDER_SPLIT: &str = concat!(
            "`LocalOrderSubject` is ",
            "`Terminal{reservation_ref:StrongRef<LocalReservation>}|",
            "Control{typed_payload_ref:StrongRef<SequenceNeutralSpec>}`. ",
            "`MetaOrderSubject` is the corresponding ",
            "`Terminal{reservation_ref:StrongRef<GlobalReservation>}|",
            "Control{typed_payload_ref:StrongRef<GlobalSequenceNeutralSpec>}`.\n",
        );
        const KEY_JOINED: &str = concat!(
            "`KeyDestroyFloorRef` is ",
            "`Checkpoint{checkpoint_ref:StrongRef<RecoveryCheckpoint>}|",
            "Configuration{floor_ref:StrongRef<ConfigPayloadFloor>}` and ",
            "`KeyDestroyExternalAckRef` is ",
            "`Backup{ack_ref:StrongRef<BackupKeyReleaseAck>}|",
            "LegalHold{ack_ref:StrongRef<LegalHoldReleaseAck>}|",
            "RemoteConsumer{ack_ref:StrongRef<RemoteKeyConsumerReleaseAck>}`.\n",
        );
        const KEY_SPLIT: &str = concat!(
            "`KeyDestroyFloorRef` is ",
            "`Checkpoint{checkpoint_ref:StrongRef<RecoveryCheckpoint>}|",
            "Configuration{floor_ref:StrongRef<ConfigPayloadFloor>}`. ",
            "`KeyDestroyExternalAckRef` is ",
            "`Backup{ack_ref:StrongRef<BackupKeyReleaseAck>}|",
            "LegalHold{ack_ref:StrongRef<LegalHoldReleaseAck>}|",
            "RemoteConsumer{ack_ref:StrongRef<RemoteKeyConsumerReleaseAck>}`.\n",
        );
        const READY_JOINED: &str = concat!(
            "`ReadyChannelSurface` has exactly ",
            "`0x0001 NativeFgp|0x0002 Http2|0x0003 Grpc|0x0004 WebSocket`, and ",
            "`BoltBookmarkRequestKind` exactly `0x0001 Begin|0x0002 Run`; ",
            "all other nested tags are reserved-invalid.\n",
        );
        const READY_SPLIT: &str = concat!(
            "`ReadyChannelSurface` has exactly ",
            "`0x0001 NativeFgp|0x0002 Http2|0x0003 Grpc|0x0004 WebSocket`. ",
            "`BoltBookmarkRequestKind` exactly `0x0001 Begin|0x0002 Run`; ",
            "all other nested tags are reserved-invalid.\n",
        );
        const RETRY_JOINED: &str = concat!(
            "`SubscriptionClosePrecondition` has exactly the stable `u16` tags ",
            "`0x0001 NoOutstanding{current_cursor_state_digest}|",
            "0x0002 Outstanding{current_cursor_state_digest,lease_identity,",
            "output_kind_and_id,public_digest}`, and ",
            "`CapabilityMigrationRetrySelector` exactly ",
            "`0x0001 Initial|0x0002 ExactLiveSuccessorRetry{entry_digest}`; ",
            "every other nested tag is reserved-invalid.\n",
        );
        const RETRY_SPLIT: &str = concat!(
            "`SubscriptionClosePrecondition` has exactly the stable `u16` tags ",
            "`0x0001 NoOutstanding{current_cursor_state_digest}|",
            "0x0002 Outstanding{current_cursor_state_digest,lease_identity,",
            "output_kind_and_id,public_digest}`. ",
            "`CapabilityMigrationRetrySelector` exactly ",
            "`0x0001 Initial|0x0002 ExactLiveSuccessorRetry{entry_digest}`; ",
            "every other nested tag is reserved-invalid.\n",
        );

        let cases = [
            (
                "local/meta order subjects",
                ORDER_JOINED,
                ORDER_SPLIT,
                "; `MetaOrderSubject` is",
                ". `MetaOrderSubject` is",
                "`MetaOrderSubject` is",
                "`MetaOrderSubject` resembles",
                "LocalOrderSubject",
                ["Control", "Terminal"].as_slice(),
                4usize,
            ),
            (
                "key-destroy reference selectors",
                KEY_JOINED,
                KEY_SPLIT,
                " and `KeyDestroyExternalAckRef` is",
                ". `KeyDestroyExternalAckRef` is",
                "`KeyDestroyExternalAckRef` is",
                "`KeyDestroyExternalAckRef` resembles",
                "KeyDestroyFloorRef",
                ["Checkpoint", "Configuration"].as_slice(),
                5,
            ),
            (
                "ready-channel selectors",
                READY_JOINED,
                READY_SPLIT,
                ", and `BoltBookmarkRequestKind` exactly",
                ". `BoltBookmarkRequestKind` exactly",
                "`BoltBookmarkRequestKind` exactly",
                "`BoltBookmarkRequestKind` nominally",
                "ReadyChannelSurface",
                [
                    "0x0001 NativeFgp",
                    "0x0002 Http2",
                    "0x0003 Grpc",
                    "0x0004 WebSocket",
                ]
                .as_slice(),
                6usize,
            ),
            (
                "close/retry selectors",
                RETRY_JOINED,
                RETRY_SPLIT,
                ", and `CapabilityMigrationRetrySelector` exactly",
                ". `CapabilityMigrationRetrySelector` exactly",
                "`CapabilityMigrationRetrySelector` exactly",
                "`CapabilityMigrationRetrySelector` nominally",
                "SubscriptionClosePrecondition",
                [
                    "0x0001 Initial",
                    "0x0001 NoOutstanding",
                    "0x0002 ExactLiveSuccessorRetry",
                    "0x0002 Outstanding",
                ]
                .as_slice(),
                4,
            ),
        ];

        let mut split_mutations = 0usize;
        let mut join_mutations = 0usize;
        let mut fault_controls_fired = 0usize;
        let mut correct_unions = 0usize;
        let mut correct_arms = 0usize;
        for (
            label,
            joined,
            split,
            joined_separator,
            split_boundary,
            second_head,
            faulted_head,
            first_owner,
            faulted_first_arms,
            expected_arm_count,
        ) in cases
        {
            let resplit = joined.replacen(joined_separator, split_boundary, 1);
            assert_eq!(resplit, split, "{label}: the split mutation did not fire");
            split_mutations += 1;

            let rejoined = split.replacen(split_boundary, joined_separator, 1);
            assert_eq!(
                rejoined, joined,
                "{label}: the inverse join mutation did not fire"
            );
            join_mutations += 1;

            let joined_census = census_801o_fixture(joined);
            let split_census = census_801o_fixture(&resplit);
            let rejoined_census = census_801o_fixture(&rejoined);
            assert_eq!(
                joined_census.transcripts.unions, split_census.transcripts.unions,
                "{label}: splitting changed the union transcript"
            );
            assert_eq!(
                joined_census.transcripts.arms, split_census.transcripts.arms,
                "{label}: splitting changed the arm transcript"
            );
            assert_eq!(
                joined_census.transcripts.unions, rejoined_census.transcripts.unions,
                "{label}: joining changed the union transcript"
            );
            assert_eq!(
                joined_census.transcripts.arms, rejoined_census.transcripts.arms,
                "{label}: joining changed the arm transcript"
            );
            assert_eq!(joined_census.counts.union_candidates, 2, "{label}");
            assert_eq!(
                joined_census.counts.arm_candidates, expected_arm_count,
                "{label}"
            );
            correct_unions += joined_census.counts.union_candidates;
            correct_arms += joined_census.counts.arm_candidates;

            // INJECTED FAULT / VACUITY CONTROL: erase only the second
            // definition cue. This reconstructs the defect's two real shapes:
            // the second union vanishes in both; the first either stays intact
            // or absorbs the foreign arms depending on whether its cue enables
            // continuation.
            let faulted = joined.replacen(second_head, faulted_head, 1);
            assert_ne!(faulted, joined, "{label}: fault injection was inert");
            let faulted_census = census_801o_fixture(&faulted);
            if faulted_census.transcripts.unions != joined_census.transcripts.unions
                && faulted_census.transcripts.arms != joined_census.transcripts.arms
            {
                fault_controls_fired += 1;
            }
            assert_eq!(
                faulted_census.counts.union_candidates, 1,
                "{label}: the erased second head must make one union vanish"
            );
            let first = faulted_census
                .unions
                .iter()
                .find(|row| row.key.schema_owner == first_owner)
                .expect("the first owner remains visible under the injected fault");
            assert_eq!(
                first.arm_names,
                faulted_first_arms
                    .iter()
                    .map(|arm| (*arm).to_owned())
                    .collect::<Vec<_>>(),
                "{label}: the injected fault did not reproduce its named shape"
            );
        }

        assert_eq!(split_mutations, 4, "all source sentences must be split");
        assert_eq!(join_mutations, 4, "all split sentences must be rejoined");
        assert_eq!(
            fault_controls_fired, 4,
            "the erased-head control must change both union and arm transcripts \
             on all source shapes"
        );
        assert_eq!(correct_unions, 8);
        assert_eq!(correct_arms, 19);
        println!(
            "801o metamorphic: split 4/4; join 4/4; correct union/arm population \
             8/19; erased-head control fired 4/4"
        );
    }

    /// The complete real-source partition. The Appendix A population is small
    /// enough to name: four sentences, eight owners.
    #[test]
    fn appendix_a_two_union_sentence_population_is_exact_and_source_ordered() {
        let plan = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"
        ));
        let source = plan
            .lines()
            .skip(1387)
            .take(2728 - 1388 + 1)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        // This guard did its job: 65a21da (fgdb-ymqm) edited Appendix A and the guard
        // fired, refusing to let the population assertion below be read as current.
        // Re-measured on the new source, the population is BYTE-FOR-BYTE UNCHANGED --
        // same four sentences, same lines 1758/2057/2645/2651, same owners, same order.
        // So only the "measured on" sha moves here; the law below is untouched, and this
        // is a re-measurement rather than a re-pin.
        assert_eq!(
            sha256_hex(source.as_bytes()),
            "d9a3dfad58deaf2a82796d6c4cc867f2ec33ec7273dcc0f533dbaa99e46a8a7d",
            "the two-union population was measured on a different Appendix A source"
        );

        let source_map = SourceMap::new(&source, 1388);
        let (fragments, _) = super::extract_markdown_fragments(&source_map);
        let links = super::prose_schema_links(&fragments, &source_map);
        assert!(
            links
                .iter()
                .all(|link| link.display_name != "DefinitelyFabricatedTwoUnionOwner"),
            "fabricated-absent control matched a prose definition"
        );

        let mut by_line: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, fragment) in fragments.iter().enumerate() {
            if fragment.kind == super::FragmentKind::Inline {
                by_line
                    .entry(source_map.position(fragment.source_range.start).line)
                    .or_default()
                    .push(index);
            }
        }
        let mut sentence_by_fragment = std::collections::BTreeMap::new();
        for (line, indexes) in &mut by_line {
            indexes.sort_by_key(|index| fragments[*index].source_range.start);
            let mut sentence = 0usize;
            for index in indexes {
                sentence_by_fragment.insert(*index, (*line, sentence));
                if super::sentence_ends(&fragments[*index].after) {
                    sentence += 1;
                }
            }
        }

        let mut union_links_by_sentence: std::collections::BTreeMap<
            (usize, usize),
            Vec<(usize, String)>,
        > = std::collections::BTreeMap::new();
        for link in &links {
            if !link
                .rhs_fragments
                .iter()
                .any(|index| matches!(super::has_top_level_pipe(&fragments[*index].text), Ok(true)))
            {
                continue;
            }
            let sentence = sentence_by_fragment[&link.owner_fragment];
            union_links_by_sentence.entry(sentence).or_default().push((
                fragments[link.owner_fragment].source_range.start,
                link.display_name.clone(),
            ));
        }
        let mut population = Vec::new();
        for ((line, _), mut owners) in union_links_by_sentence {
            if owners.len() < 2 {
                continue;
            }
            owners.sort_by_key(|(source_start, _)| *source_start);
            population.push((
                line,
                owners
                    .into_iter()
                    .map(|(_, owner)| owner)
                    .collect::<Vec<_>>(),
            ));
        }
        assert_eq!(
            population,
            [
                (
                    1758,
                    vec![
                        "LocalOrderSubject".to_owned(),
                        "MetaOrderSubject".to_owned(),
                    ],
                ),
                (
                    2057,
                    vec![
                        "KeyDestroyFloorRef".to_owned(),
                        "KeyDestroyExternalAckRef".to_owned(),
                    ],
                ),
                (
                    2645,
                    vec![
                        "ReadyChannelSurface".to_owned(),
                        "BoltBookmarkRequestKind".to_owned(),
                    ],
                ),
                (
                    2651,
                    vec![
                        "SubscriptionClosePrecondition".to_owned(),
                        "CapabilityMigrationRetrySelector".to_owned(),
                    ],
                ),
            ],
            "every real sentence that spells two unions must be named in source order"
        );
        for tagged_source_spelling in [
            concat!(
                "`ReadyChannelSurface` has exactly ",
                "`0x0001 NativeFgp|0x0002 Http2|0x0003 Grpc|0x0004 WebSocket`",
            ),
            concat!(
                "`BoltBookmarkRequestKind` exactly ",
                "`0x0001 Begin|0x0002 Run`",
            ),
            concat!(
                "`SubscriptionClosePrecondition` has exactly the stable `u16` tags ",
                "`0x0001 NoOutstanding{current_cursor_state_digest}|",
                "0x0002 Outstanding{current_cursor_state_digest,lease_identity,",
                "output_kind_and_id,public_digest}`",
            ),
            concat!(
                "`CapabilityMigrationRetrySelector` exactly ",
                "`0x0001 Initial|0x0002 ExactLiveSuccessorRetry{entry_digest}`",
            ),
        ] {
            assert!(
                source.contains(tagged_source_spelling),
                "tag assignment must be read from this exact source-order spelling: \
                 {tagged_source_spelling}"
            );
        }

        let slices = crate::appendix_a::SLICE_PINS
            .iter()
            .map(|slice| SourceSliceSpec {
                id: slice.id,
                start_line: usize::try_from(slice.start_line).expect("positive start line"),
                end_line: usize::try_from(slice.end_line).expect("positive end line"),
            })
            .collect::<Vec<_>>();
        let census = census_appendix_source(source.as_bytes(), 1388, &slices)
            .expect("the committed Appendix A source must census");
        for (owner, expected_arms) in [
            ("LocalOrderSubject", ["Control", "Terminal"].as_slice()),
            ("MetaOrderSubject", ["Control", "Terminal"].as_slice()),
            (
                "KeyDestroyFloorRef",
                ["Checkpoint", "Configuration"].as_slice(),
            ),
            (
                "KeyDestroyExternalAckRef",
                ["Backup", "LegalHold", "RemoteConsumer"].as_slice(),
            ),
            (
                "ReadyChannelSurface",
                [
                    "0x0001 NativeFgp",
                    "0x0002 Http2",
                    "0x0003 Grpc",
                    "0x0004 WebSocket",
                ]
                .as_slice(),
            ),
            (
                "BoltBookmarkRequestKind",
                ["0x0001 Begin", "0x0002 Run"].as_slice(),
            ),
            (
                "SubscriptionClosePrecondition",
                ["0x0001 NoOutstanding", "0x0002 Outstanding"].as_slice(),
            ),
            (
                "CapabilityMigrationRetrySelector",
                ["0x0001 Initial", "0x0002 ExactLiveSuccessorRetry"].as_slice(),
            ),
        ] {
            let rows = census
                .unions
                .iter()
                .filter(|row| row.key.schema_owner == owner)
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1, "{owner} must own exactly one union");
            assert_eq!(rows[0].key.union_path, owner);
            assert_eq!(
                rows[0].arm_names,
                expected_arms
                    .iter()
                    .map(|arm| (*arm).to_owned())
                    .collect::<Vec<_>>(),
                "{owner}: the canonical census set must contain exactly the \
                 source-spelled arm identities; this alphabetical set order is \
                 not a tag-assignment order"
            );
            assert_eq!(rows[0].unparsed_arm_count, 0);
            assert!(!rows[0].arm_set_conflict);
        }
        assert!(
            census.fields.iter().any(|field| {
                field.key.path
                    == concat!(
                        "CapabilityMigrationRetrySelector.0x0002 ",
                        "ExactLiveSuccessorRetry.entry_digest"
                    )
            }),
            "the retry selector payload field must belong to its source owner"
        );
        assert!(
            census.fields.iter().all(|field| {
                field.key.path
                    != concat!(
                        "SubscriptionClosePrecondition.0x0002 ",
                        "ExactLiveSuccessorRetry.entry_digest"
                    )
            }),
            "the retry selector payload field must not remain on the preceding union"
        );
        println!(
            "801o source partition: 4 sentences, 8 unions, 19 arms; global \
             schemas={} fields={} unions={} arms={} ambiguities={}",
            census.counts.schema_candidates,
            census.counts.field_candidates,
            census.counts.union_candidates,
            census.counts.arm_candidates,
            census.counts.ambiguities
        );
    }
}
