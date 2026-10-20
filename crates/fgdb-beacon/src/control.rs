use fgdb_types::VId;

/// Failures never imply that a durable graph commit was rolled back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeaconError {
    InvalidConfig(&'static str),
    InvalidQuery(&'static str),
    Dimension {
        expected: usize,
        actual: usize,
    },
    NonFinite {
        coordinate: usize,
    },
    ZeroVector,
    DuplicateVertex(VId),
    ResourceLimit {
        resource: &'static str,
        limit: usize,
    },
    WorkBudgetExceeded,
    Cancelled,
    GenerationExhausted,
    Disabled(&'static str),
    Invariant(&'static str),
}

impl core::fmt::Display for BeaconError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidConfig(why) => write!(f, "invalid Beacon configuration: {why}"),
            Self::InvalidQuery(why) => write!(f, "invalid Beacon query: {why}"),
            Self::Dimension { expected, actual } => {
                write!(f, "vector dimension {actual}; expected {expected}")
            }
            Self::NonFinite { coordinate } => {
                write!(f, "non-finite vector coordinate {coordinate}")
            }
            Self::ZeroVector => f.write_str("cosine distance is undefined for a zero vector"),
            Self::DuplicateVertex(id) => write!(f, "duplicate index document for vertex {}", id.0),
            Self::ResourceLimit { resource, limit } => {
                write!(f, "Beacon {resource} limit exceeded ({limit})")
            }
            Self::WorkBudgetExceeded => f.write_str("Beacon work budget exhausted"),
            Self::Cancelled => f.write_str("Beacon operation cancelled"),
            Self::GenerationExhausted => f.write_str("Beacon derived generation exhausted"),
            Self::Disabled(kind) => write!(f, "{kind} indexing is disabled"),
            Self::Invariant(why) => write!(f, "Beacon invariant failed: {why}"),
        }
    }
}

impl std::error::Error for BeaconError {}

/// A surface adapter can combine resource accounting with a purpose-typed Cx
/// cancellation checkpoint. An error aborts the operation; it is not a signal
/// to return an incomplete answer. Work units are scalar coordinates, analyzed
/// characters, postings, or visited bookkeeping entries, not wall-clock time.
pub trait WorkControl {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError>;

    /// Admit logical scratch entries BEFORE a kernel retains them. Expansion
    /// emits two entries per new visit (distance and queue), then one per new
    /// ranking-heap slot and output row. Graph fusion emits its validation,
    /// rank, evidence, heap and result entries separately. Replacing a heap
    /// occupant reuses its slot; dropping entries does not refund admissions.
    ///
    /// The default is work-only: existing callers retain their independent
    /// count limits, not a new shared-memory promise. A shared-scratch host
    /// must override this hook and forward it through any control wrappers.
    /// Its implementation owns cancellation, work charges and the cumulative
    /// allowance. Source/index construction is accounted by the host, not by
    /// this hook. Entries are not allocator bytes or a process RSS bound.
    fn reserve_scratch(&mut self, _entries: usize) -> Result<(), BeaconError> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct WorkBudget {
    remaining: usize,
}

impl WorkBudget {
    #[must_use]
    pub fn new(units: usize) -> Self {
        Self { remaining: units }
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.remaining
    }
}

impl WorkControl for WorkBudget {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        match self.remaining.checked_sub(units) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(())
            }
            None => {
                self.remaining = 0;
                Err(BeaconError::WorkBudgetExceeded)
            }
        }
    }
}
