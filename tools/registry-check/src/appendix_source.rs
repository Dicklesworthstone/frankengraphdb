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
    pub reason: String,
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
            "ambiguity|{}|{}|{}|{}|{}",
            self.kind.as_str(),
            self.schema_family.as_deref().unwrap_or_default(),
            self.path.as_deref().unwrap_or_default(),
            self.raw_sha256,
            self.reason
        )
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
    declaration_range: Range<usize>,
    expression: Option<MappedText>,
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

    fn line_count(&self) -> usize {
        if self.source.is_empty() {
            0
        } else {
            self.line_starts.len()
        }
    }

    fn end_line(&self) -> usize {
        self.start_line + self.line_count().saturating_sub(1)
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
        && (is_identifier_start(next) || matches!(next, b'?' | b'['))
}

fn matching_delimiter(text: &str, open_index: usize) -> Result<usize, DelimiterIssue> {
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

fn split_top_level(text: &str, delimiters: &[u8]) -> Result<Vec<SplitSpan>, DelimiterIssue> {
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

fn has_definition_cue(value: &str) -> bool {
    const CUES: [&str; 21] = [
        "is",
        "are",
        "has",
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
    cue: String,
}

#[derive(Debug, Clone)]
struct BoldLink {
    display_name: String,
    declaration_range: Range<usize>,
    rhs_fragment: usize,
}

fn simple_type_display(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (display, consumed) = parse_type_display(trimmed)?;
    (consumed == trimmed.len()).then_some(display)
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
            let Some(display_name) = simple_type_display(&fragment.text) else {
                continue;
            };
            if !has_definition_cue(&fragment.after) {
                continue;
            }
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
            if !rhs_fragments.is_empty() && cue_names_union(&cue) {
                let mut separator_seen = false;
                while let Some(candidate_index) = indexes.get(scan + 1).copied() {
                    let previous = &fragments[indexes[scan]];
                    if sentence_ends(&previous.after) {
                        break;
                    }
                    separator_seen |= contains_continuation_separator(&previous.after);
                    let candidate = &fragments[candidate_index];
                    if separator_seen && union_continuation_candidate(&candidate.text) {
                        rhs_fragments.push(candidate_index);
                        separator_seen = false;
                    }
                    scan += 1;
                }
            }
            links.push(ProseLink {
                display_name,
                owner_fragment: fragment_index,
                rhs_fragments,
                cue,
            });
        }
    }
    links
}

fn bold_schema_links(fragments: &[MarkdownFragment]) -> Vec<BoldLink> {
    let mut links = Vec::new();
    for (index, fragment) in fragments.iter().enumerate() {
        if fragment.kind != FragmentKind::Inline || !structural_fragment(&fragment.text) {
            continue;
        }
        let before = &fragment.before;
        let before_range = trim_range(before, 0..before.len());
        let Some(colon_index) = before_range.end.checked_sub(1) else {
            continue;
        };
        if before.as_bytes().get(colon_index) != Some(&b':') {
            continue;
        }
        let prefix_range = trim_range(before, before_range.start..colon_index);
        if !before[prefix_range.clone()].ends_with("**") {
            continue;
        }
        let close_start = prefix_range.end - 2;
        let Some(open) = before[..close_start].rfind("**") else {
            continue;
        };
        let candidate_range = trim_range(before, open + 2..close_start);
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
        links.push(BoldLink {
            display_name,
            declaration_range: before_source_start + candidate_range.start
                ..before_source_start + candidate_range.end,
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
    let expression_sha256 = expression
        .as_ref()
        .map(|value| sha256_hex(normalize_whitespace(&value.text).as_bytes()))
        .unwrap_or_else(|| sha256_hex(b""));
    Some(SchemaOccurrence {
        key,
        display_name,
        owner_status,
        definition_kind,
        declaration_range,
        expression,
        expression_sha256,
    })
}

fn delimiter_ambiguity(
    issue: DelimiterIssue,
    mapped: &MappedText,
    family: Option<String>,
    path: Option<String>,
    raw: String,
    reason: &str,
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
    claimed_fragments: &mut BTreeSet<usize>,
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
                    ));
                    occurrences.push(occurrence);
                    claimed_fragments.insert(fragment.id);
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
                    ));
                    occurrences.push(occurrence);
                    claimed_fragments.insert(fragment.id);
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
                claimed_fragments.insert(fragment.id);
            }
            continue;
        }
        if claimed_by_link && !matches!(top_level_assignment(text), Ok(Some(_))) {
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
                            source_range: mapped.source_range(0..mapped.text.len()),
                        });
                    }
                    occurrences.push(occurrence);
                    claimed_fragments.insert(fragment.id);
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
                    ));
                    occurrences.push(occurrence);
                    claimed_fragments.insert(fragment.id);
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
    claimed_fragments: &mut BTreeSet<usize>,
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
                            claimed_fragments.insert(fragment.id);
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
                            ));
                            occurrences.push(occurrence);
                            claimed_fragments.insert(fragment.id);
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
    }
    linked_rhs.extend(
        bold_links
            .iter()
            .map(|link| fragments[link.rhs_fragment].id),
    );

    let mut occurrences = Vec::new();
    let mut claimed_fragments = BTreeSet::new();
    for fragment in fragments {
        match fragment.kind {
            FragmentKind::Fence => direct_schemas_from_fence(
                fragment,
                source_map.source,
                &mut occurrences,
                &mut ambiguities,
                &mut claimed_fragments,
            ),
            FragmentKind::Inline => direct_schemas_from_inline(
                fragment,
                source_map.source,
                linked_rhs.contains(&fragment.id),
                &mut occurrences,
                &mut ambiguities,
                &mut claimed_fragments,
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
                fragment.source_range.clone(),
            )),
        ) {
            occurrences.push(occurrence);
            claimed_fragments.insert(fragment.id);
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
        if let Some(occurrence) = make_schema_occurrence(
            link.display_name,
            SchemaOwnerStatus::ConfirmedTopLevel,
            DefinitionKind::ProseLinkedStructural,
            owner.source_range.clone(),
            Some(expression),
        ) {
            occurrences.push(occurrence);
            claimed_fragments.extend(link.rhs_fragments.iter().map(|index| fragments[*index].id));
        }
        let _ = link.cue;
    }

    for fragment in fragments {
        if structural_fragment(&fragment.text) && !claimed_fragments.contains(&fragment.id) {
            ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::UnownedStructuralFragment,
                schema_family: None,
                path: None,
                raw: normalize_whitespace(&fragment.text),
                reason: "schema-like notation has no owner under the conservative source grammar"
                    .to_owned(),
                source_range: fragment.source_range.clone(),
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
    ambiguities: &mut Vec<AmbiguityOccurrence>,
) {
    ambiguities.push(AmbiguityOccurrence {
        kind: AmbiguityKind::NestingLimitExceeded,
        schema_family: Some(schema.key.family.clone()),
        path: Some(path.to_owned()),
        raw: normalize_whitespace(&mapped.text),
        reason: format!("structural nesting exceeds the limit of {MAX_STRUCTURAL_NESTING}"),
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
    rows: &mut StructuralOccurrences,
    depth: usize,
) -> bool {
    if depth > MAX_STRUCTURAL_NESTING {
        push_nesting_ambiguity(schema, union_path, mapped, &mut rows.ambiguities);
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
            ));
            return false;
        }
    };
    if alternatives.len() < 2 {
        return false;
    }
    let union_index = rows.unions.len();
    rows.unions.push(UnionOccurrence {
        key: UnionCandidateKey {
            schema_family: schema.key.family.clone(),
            schema_owner: schema.display_name.clone(),
            union_path: union_path.to_owned(),
        },
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
                source_range: alternative_mapped.source_range(0..alternative_mapped.text.len()),
            });
            continue;
        };
        let arm_name = normalize_whitespace(&alternative_mapped.text[token.clone()]);
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
                )),
            }
        } else {
            let trailing = trim_range(
                &alternative_mapped.text,
                token.end..alternative_mapped.text.len(),
            );
            if !trailing.is_empty() {
                let trailing = alternative_mapped.subrange(trailing);
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::UnparsedTrailingTokens,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(format!("{union_path}.{arm_name}")),
                    raw: normalize_whitespace(&trailing.text),
                    reason:
                        "tokens after a union arm name are not part of the closed source grammar"
                            .to_owned(),
                    source_range: trailing.source_range(0..trailing.text.len()),
                });
            }
        }
        rows.arms.push(ArmOccurrence {
            key: ArmCandidateKey {
                schema_family: schema.key.family.clone(),
                schema_owner: schema.display_name.clone(),
                union_path: union_path.to_owned(),
                arm_name: arm_name.clone(),
            },
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
    rows: &mut StructuralOccurrences,
    depth: usize,
) {
    if depth > MAX_STRUCTURAL_NESTING {
        push_nesting_ambiguity(schema, path, mapped, &mut rows.ambiguities);
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
        if optional_marker {
            exact_type = Some(format!("{}?", exact_type.unwrap_or_default()));
        }
        let field_path = format!("{path}.{name}");
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
                    source_range: first_range.clone(),
                });
            }
            rows.ambiguities.push(AmbiguityOccurrence {
                kind: AmbiguityKind::ConflictingCandidateEvidence,
                schema_family: Some(schema.key.family.clone()),
                path: Some(field_path.clone()),
                raw: raw.clone(),
                reason: "one record occurrence repeats the same field name".to_owned(),
                source_range: source_range.clone(),
            });
        } else {
            seen_fields.insert(name.clone(), (raw.clone(), source_range.clone()));
        }
        rows.fields.push(FieldOccurrence {
            key: FieldCandidateKey {
                schema_family: schema.key.family.clone(),
                schema_owner: schema.display_name.clone(),
                path: field_path.clone(),
                stable_name: name.clone(),
            },
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
                source_range,
            });
        }
        let Some(exact_range) = exact_range else {
            continue;
        };
        let exact_mapped = field_mapped.subrange(exact_range);
        if parse_union(schema, &exact_mapped, &field_path, rows, depth + 1) {
            continue;
        }
        match outermost_record_ranges(&exact_mapped.text) {
            Ok(record_ranges) => {
                for (open, close) in record_ranges {
                    let nested_name = trailing_upper_identifier(&exact_mapped.text[..open]);
                    let nested_path = nested_name
                        .map(|name| format!("{field_path}.{name}"))
                        .unwrap_or_else(|| format!("{field_path}.record"));
                    parse_record_fields(
                        schema,
                        &exact_mapped.subrange(open + 1..close),
                        &nested_path,
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
                source_range: trailing.source_range(0..trailing.text.len()),
            });
        }
        if shape == ExpressionShape::Alias {
            if parse_union(schema, &mapped, &schema.display_name, &mut rows, 0) {
                continue;
            }
            if !mapped.text.trim().is_empty() {
                rows.ambiguities.push(AmbiguityOccurrence {
                    kind: AmbiguityKind::AliasExpressionUnparsed,
                    schema_family: Some(schema.key.family.clone()),
                    path: Some(schema.display_name.clone()),
                    raw: normalize_whitespace(&mapped.text),
                    reason: "alias body is neither a top-level pipe union nor a record body"
                        .to_owned(),
                    source_range: mapped.source_range(0..mapped.text.len()),
                });
            }
            continue;
        }
        parse_record_fields(schema, &mapped, &schema.display_name, &mut rows, 0);
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

fn canonical_ambiguities(
    rows: &[&AmbiguityOccurrence],
    source_map: &SourceMap<'_>,
) -> Vec<AmbiguityCandidate> {
    let mut grouped: BTreeMap<AmbiguityKey, Vec<&AmbiguityOccurrence>> = BTreeMap::new();
    for row in rows {
        let key = AmbiguityKey {
            kind: row.kind,
            schema_family: row.schema_family.clone(),
            path: row.path.clone(),
            raw_sha256: sha256_hex(row.raw.as_bytes()),
            reason: row.reason.clone(),
        };
        grouped.entry(key).or_default().push(*row);
    }
    grouped
        .into_iter()
        .map(|(key, rows)| AmbiguityCandidate {
            raw: rows[0].raw.clone(),
            key,
            locations: sorted_unique(rows.iter().map(|row| source_map.span(&row.source_range))),
        })
        .collect()
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
/// `ambiguity|kind|family-or-empty|path-or-empty|raw-sha256|reason`.
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
    let source_map = SourceMap::new(source, source_start_line);
    let source_end_line = source_map.end_line();
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
    let canonical_ambiguity_rows = canonical_ambiguities(&all_ambiguity_rows, &source_map);
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
        AmbiguityKind, CensusErrorKind, SourceSliceSpec, census_appendix_source,
        matching_delimiter, normalize_whitespace, split_top_level,
    };

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
    fn skipped_incidental_spans_do_not_steal_prose_ownership() {
        let source = concat!(
            "`Evidence` is stable with `Other=7`, then stops.\n",
            "`SecurityBasis` is the exact embedded `u16` union ",
            "`0x0001 Local{x:u8}|0x0002 Meta{y:u16}`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "prose",
            start_line: 30,
            end_line: 31,
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
    }

    #[test]
    fn bold_owner_location_and_conflicts_retain_exact_evidence() {
        let source = concat!(
            "**BoldOwner**: `{x:u8}`.\n",
            "`Same = Left{x:u8}|Right`.\n",
            "`Same = Left{x:u16}|Other`.\n",
        );
        let slices = [SourceSliceSpec {
            id: "evidence",
            start_line: 70,
            end_line: 72,
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
        assert!(
            census
                .ambiguities
                .iter()
                .any(|row| row.key.kind == AmbiguityKind::ConflictingCandidateEvidence)
        );
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
        assert!(
            census
                .ambiguities
                .iter()
                .all(|candidate| !candidate.locations.is_empty())
        );
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
    }
}
