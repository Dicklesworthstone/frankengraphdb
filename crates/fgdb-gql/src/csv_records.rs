//! Incremental CSV framing and decoding without a complete input buffer.
//!
//! A framer and decoder use the SAME byte state machine. Chunks may split UTF-8,
//! doubled quotes or CRLF. Only a complete record escapes the decoder. Neither
//! component reads files, binds a schema or executes a graph operation.
//!
//! The profile accepts comma separators, double-quoted fields with doubled quote
//! escapes, LF/CRLF records and embedded CR/LF in quoted fields. Whitespace is
//! data; bare CR terminators and quotes inside unquoted fields refuse. Empty
//! records are one empty field; an ending terminator invents no extra record.
//! A BOM is NOT stripped here: a file adapter may strip it only at byte zero.
//!
//! Limits are inclusive and checked before growing decoded buffers. Framing is
//! allocation-free but does not validate UTF-8; decoding validates every field.
//! The caller owns whole-source limits, cooperative I/O and execution admission.

/// Logical byte/count bounds for ONE record, not allocator-capacity accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvRecordLimits {
    /// All raw record bytes, including quotes, commas and LF/CRLF terminators.
    pub max_record_bytes: usize,
    /// Decoded UTF-8 bytes in each field (a doubled quote costs one byte).
    pub max_field_bytes: usize,
    pub max_columns: usize,
}
impl Default for CsvRecordLimits {
    fn default() -> Self {
        Self { max_record_bytes: 1024 * 1024, max_field_bytes: 1024 * 1024, max_columns: 256 }
    }
}

/// Position of the offending byte; records/columns/offsets are zero-based.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CsvPosition {
    pub record: u64,
    pub column: usize,
    pub offset: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvRecordErrorKind {
    RecordBytes { limit: usize },
    FieldBytes { limit: usize },
    Columns { limit: usize },
    UnexpectedQuote,
    TrailingCharacters,
    InvalidLineEnding,
    UnterminatedQuote,
    InvalidUtf8,
    Allocation,
    CounterOverflow,
    /// A prior error or explicit EOF has terminated this instance.
    Terminated,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvRecordError {
    pub position: CsvPosition,
    pub kind: CsvRecordErrorKind,
}
impl core::fmt::Display for CsvRecordError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CSV record {}, column {} at byte {}: {:?}",
            self.position.record, self.position.column, self.position.offset, self.kind)
    }
}
impl core::error::Error for CsvRecordError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State { Start, Bare, Quoted, Quote, Cr }
#[derive(Clone, Copy)]
enum Step { Nothing, Quoted, Byte(u8), Field, Record }

/// Allocation-free syntax/size preflight with no decoded payload retention.
/// A successful `push` returns true exactly at an LF/CRLF record boundary;
/// `finish` reports a final unterminated record, not an unterminated quote.
/// Any error is terminal. Cloning copies only the bounded framing state.
#[derive(Clone, Debug)]
pub struct CsvRecordFramer {
    limits: CsvRecordLimits,
    state: State,
    position: CsvPosition,
    record_bytes: usize,
    field_bytes: usize,
    ended: bool,
    failed: bool,
}
impl CsvRecordFramer {
    pub fn new(limits: CsvRecordLimits) -> Self {
        Self { limits, state: State::Start, position: CsvPosition::default(),
            record_bytes: 0, field_bytes: 0, ended: false, failed: false }
    }
    pub fn position(&self) -> CsvPosition { self.position }
    pub fn push(&mut self, byte: u8) -> Result<bool, CsvRecordError> {
        self.advance(byte).map(|step| matches!(step, Step::Record))
    }
    pub fn finish(&mut self) -> Result<bool, CsvRecordError> { self.finish_record() }
    fn refuse<T>(&mut self, kind: CsvRecordErrorKind) -> Result<T, CsvRecordError> {
        self.failed = true;
        Err(CsvRecordError { position: self.position, kind })
    }
    fn advance(&mut self, byte: u8) -> Result<Step, CsvRecordError> {
        use CsvRecordErrorKind as E;
        if self.ended || self.failed { return self.refuse(E::Terminated); }
        if self.limits.max_columns == 0 { return self.refuse(E::Columns { limit: 0 }); }
        // Refuse before either counter or caller-owned payload can grow.
        if self.record_bytes == self.limits.max_record_bytes {
            return self.refuse(E::RecordBytes { limit: self.limits.max_record_bytes });
        }
        if self.position.offset == u64::MAX { return self.refuse(E::CounterOverflow); }
        let (next, step) = match (self.state, byte) {
            (State::Start, b'"') => (State::Quoted, Step::Quoted),
            (State::Quoted, b'"') => (State::Quote, Step::Nothing),
            (State::Quoted, byte) => (State::Quoted, Step::Byte(byte)),
            (State::Quote, b'"') => (State::Quoted, Step::Byte(b'"')),
            (State::Cr, b'\n') => (State::Start, Step::Record),
            (State::Cr, _) => return self.refuse(E::InvalidLineEnding),
            (State::Start | State::Bare | State::Quote, b',') => (State::Start, Step::Field),
            (State::Start | State::Bare | State::Quote, b'\n') => (State::Start, Step::Record),
            (State::Start | State::Bare | State::Quote, b'\r') => (State::Cr, Step::Nothing),
            (State::Quote, _) => return self.refuse(E::TrailingCharacters),
            (State::Bare, b'"') => return self.refuse(E::UnexpectedQuote),
            (State::Start | State::Bare, byte) => (State::Bare, Step::Byte(byte)),
        };
        match step {
            Step::Byte(_) if self.field_bytes == self.limits.max_field_bytes =>
                return self.refuse(E::FieldBytes { limit: self.limits.max_field_bytes }),
            Step::Field if self.position.column == self.limits.max_columns - 1 =>
                return self.refuse(E::Columns { limit: self.limits.max_columns }),
            Step::Record if self.position.record == u64::MAX => return self.refuse(E::CounterOverflow),
            _ => {}
        }
        self.record_bytes += 1;
        self.position.offset += 1;
        self.state = next;
        match step {
            Step::Byte(_) => self.field_bytes += 1,
            Step::Field => { self.field_bytes = 0; self.position.column += 1; }
            Step::Record => self.completed_record(),
            _ => {}
        }
        Ok(step)
    }
    fn completed_record(&mut self) {
        self.state = State::Start;
        self.record_bytes = 0;
        self.field_bytes = 0;
        self.position.record += 1;
        self.position.column = 0;
    }
    fn finish_record(&mut self) -> Result<bool, CsvRecordError> {
        use CsvRecordErrorKind as E;
        if self.failed { return self.refuse(E::Terminated); }
        if self.ended { return Ok(false); }
        if self.state == State::Quoted { return self.refuse(E::UnterminatedQuote); }
        if self.state == State::Cr { return self.refuse(E::InvalidLineEnding); }
        let record = self.record_bytes != 0;
        if record {
            if self.position.record == u64::MAX { return self.refuse(E::CounterOverflow); }
            self.completed_record();
        }
        self.ended = true;
        Ok(record)
    }
}

/// Quote provenance lets schema adapters distinguish an empty/missing value or
/// a literal quoted null token without reverse engineering the decoded string.
#[derive(Clone, PartialEq, Eq)]
pub struct CsvField { text: String, quoted: bool }
impl CsvField {
    pub fn text(&self) -> &str { &self.text }
    pub fn is_quoted(&self) -> bool { self.quoted }
    pub fn into_text(self) -> String { self.text }
}
impl core::fmt::Debug for CsvField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CsvField([REDACTED])")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvRecord { fields: Vec<CsvField> }
impl CsvRecord {
    pub fn fields(&self) -> &[CsvField] { &self.fields }
    pub fn into_fields(self) -> Vec<CsvField> { self.fields }
}

/// Streaming decoder retaining only the current record, never all input rows.
/// Feed bounded input chunks, resume with their unconsumed suffix, then call
/// `finish` exactly when the source reports EOF. A read error is NOT EOF.
/// Completed earlier records remain the caller's responsibility on a later
/// error; this type grants neither whole-source atomicity nor write authority.
pub struct CsvRecordDecoder {
    framer: CsvRecordFramer,
    fields: Vec<CsvField>,
    field: Vec<u8>,
    quoted: bool,
    field_start: CsvPosition,
}
impl core::fmt::Debug for CsvRecordDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CsvRecordDecoder").field("position", &self.position())
            .field("payload", &"[REDACTED]").finish()
    }
}
impl CsvRecordDecoder {
    pub fn new(limits: CsvRecordLimits) -> Self {
        Self { framer: CsvRecordFramer::new(limits), fields: Vec::new(),
            field: Vec::new(), quoted: false, field_start: CsvPosition::default() }
    }
    pub fn position(&self) -> CsvPosition { self.framer.position() }
    /// Return consumed bytes and at most one complete record. A nonempty chunk
    /// always makes progress or refuses. Never consume bytes of the next record.
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<CsvRecord>), CsvRecordError> {
        let result = self.push_inner(input);
        if result.is_err() { self.discard(); }
        result
    }
    fn push_inner(&mut self, input: &[u8]) -> Result<(usize, Option<CsvRecord>), CsvRecordError> {
        if self.framer.failed || self.framer.ended {
            return self.framer.refuse(CsvRecordErrorKind::Terminated);
        }
        for (index, &byte) in input.iter().enumerate() {
            match self.framer.advance(byte)? {
                Step::Nothing => {},
                Step::Quoted => self.quoted = true,
                Step::Byte(byte) => {
                    if self.field.try_reserve(1).is_err() {
                        return self.framer.refuse(CsvRecordErrorKind::Allocation);
                    }
                    self.field.push(byte);
                }
                Step::Field => self.end_field()?,
                Step::Record => {
                    self.end_field()?;
                    return Ok((index + 1, Some(CsvRecord { fields: core::mem::take(&mut self.fields) })));
                }
            }
        }
        Ok((input.len(), None))
    }
    /// Successful EOF may finish one record without a trailing line ending.
    /// EOF inside quotes/CRLF refuses and discards the entire unfinished record.
    /// Repeated successful EOF returns None; a failed instance stays failed.
    pub fn finish(&mut self) -> Result<Option<CsvRecord>, CsvRecordError> {
        let result = (|| {
            if !self.framer.finish_record()? { return Ok(None); }
            self.end_field()?;
            Ok(Some(CsvRecord { fields: core::mem::take(&mut self.fields) }))
        })();
        if result.is_err() { self.discard(); }
        result
    }
    fn end_field(&mut self) -> Result<(), CsvRecordError> {
        let text = String::from_utf8(core::mem::take(&mut self.field)).map_err(|_| {
            self.framer.failed = true;
            CsvRecordError { position: self.field_start, kind: CsvRecordErrorKind::InvalidUtf8 }
        })?;
        if self.fields.try_reserve(1).is_err() {
            return self.framer.refuse(CsvRecordErrorKind::Allocation);
        }
        self.fields.push(CsvField { text, quoted: self.quoted });
        self.quoted = false;
        self.field_start = self.framer.position();
        Ok(())
    }
    fn discard(&mut self) {
        self.framer.failed = true;
        // Release payload storage; failure is not an implicit replay/reset.
        self.fields = Vec::new();
        self.field = Vec::new();
        self.quoted = false;
    }
}

#[cfg(test)]
mod tests;
