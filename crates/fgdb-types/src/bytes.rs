//! Length-bounded byte containers.
//!
//! Appendix A's encoding preamble requires "length-before-allocation" and
//! per-kind maximum sizes. `BoundedBytes<MAX>` is the type-level form of that
//! rule: the bound is part of the type, construction checks it, and no
//! constructor can allocate past it.

/// Owned bytes whose length is statically bounded by `MAX`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BoundedBytes<const MAX: usize> {
    data: Vec<u8>,
}

/// Typed rejection from bounded byte admission or copying.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BoundedBytesError {
    /// The requested byte length exceeds the type-level bound.
    LengthExceedsBound { declared_len: usize, max: usize },
    /// The declared length is in bounds, but the input is shorter.
    TruncatedInput {
        declared_len: usize,
        available_len: usize,
    },
    /// Reserving storage for an otherwise-valid copy failed.
    AllocationFailed { requested: usize },
}

impl std::fmt::Display for BoundedBytesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LengthExceedsBound { declared_len, max } => write!(
                f,
                "byte string of length {declared_len} exceeds declared bound {max}"
            ),
            Self::TruncatedInput {
                declared_len,
                available_len,
            } => write!(
                f,
                "declared byte length {declared_len} exceeds available input length {available_len}"
            ),
            Self::AllocationFailed { requested } => {
                write!(
                    f,
                    "unable to allocate {requested} bytes for bounded byte copy"
                )
            }
        }
    }
}

impl std::error::Error for BoundedBytesError {}

impl<const MAX: usize> BoundedBytes<MAX> {
    /// Takes ownership of `data` if it fits the bound.
    pub fn new(data: Vec<u8>) -> Result<Self, BoundedBytesError> {
        if data.len() > MAX {
            return Err(BoundedBytesError::LengthExceedsBound {
                declared_len: data.len(),
                max: MAX,
            });
        }
        Ok(BoundedBytes { data })
    }

    /// Length-before-allocation construction: validates `declared_len`
    /// against both the bound and the actually-available input *before*
    /// copying, then copies exactly `declared_len` bytes.
    pub fn from_declared_len(declared_len: usize, input: &[u8]) -> Result<Self, BoundedBytesError> {
        if declared_len > MAX {
            return Err(BoundedBytesError::LengthExceedsBound {
                declared_len,
                max: MAX,
            });
        }
        if declared_len > input.len() {
            return Err(BoundedBytesError::TruncatedInput {
                declared_len,
                available_len: input.len(),
            });
        }

        let mut data = Vec::new();
        data.try_reserve_exact(declared_len)
            .map_err(|_| BoundedBytesError::AllocationFailed {
                requested: declared_len,
            })?;
        data.extend_from_slice(&input[..declared_len]);
        Ok(BoundedBytes { data })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub const fn max_len() -> usize {
        MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_admission_is_zero_copy_and_bound_checked() -> Result<(), BoundedBytesError> {
        let input = vec![1, 2, 3, 4];
        let original_allocation = input.as_ptr();
        let admitted = BoundedBytes::<4>::new(input)?;
        assert_eq!(admitted.as_slice().as_ptr(), original_allocation);

        assert_eq!(
            BoundedBytes::<4>::new(vec![0; 5]),
            Err(BoundedBytesError::LengthExceedsBound {
                declared_len: 5,
                max: 4
            })
        );
        Ok(())
    }

    #[test]
    fn declared_len_errors_distinguish_bound_from_truncated_input() {
        // Declared length past the bound: rejected even though input is short.
        assert_eq!(
            BoundedBytes::<4>::from_declared_len(usize::MAX, &[1, 2]),
            Err(BoundedBytesError::LengthExceedsBound {
                declared_len: usize::MAX,
                max: 4,
            })
        );
        // Declared length past the available input: rejected (no partial read).
        assert_eq!(
            BoundedBytes::<8>::from_declared_len(3, &[1, 2]),
            Err(BoundedBytesError::TruncatedInput {
                declared_len: 3,
                available_len: 2,
            })
        );
    }

    #[test]
    fn declared_len_copies_exact_prefix() -> Result<(), BoundedBytesError> {
        // Exact prefix taken otherwise.
        let ok = BoundedBytes::<8>::from_declared_len(2, &[1, 2, 3])?;
        assert_eq!(ok.as_slice(), &[1, 2]);
        Ok(())
    }
}
