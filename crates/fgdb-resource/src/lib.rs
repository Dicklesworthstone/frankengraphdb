//! Typed resource vectors and ceilings.
//!
//! The in-memory accounting types that permits, `FrameLimits`, and W10
//! admission control consume: a [`ResourceVector`] is a point in the
//! five-axis resource space, a [`ResourceCeiling`] is an inclusive upper
//! bound, and every combining operation is **checked** — overflow and
//! ceiling violations are typed rejections naming the exact axis, never
//! saturation or wraparound. B6's admission-controlled-by-construction
//! story starts here: an admission path that accounts work through these
//! types cannot silently exceed a budget.
//!
//! The [`ledger`] module owns the durable-accounting semantic algebra:
//! fixed charge vectors, quota paths, buckets, ownership entries, and atomic
//! transitions. Registry-generated field tags, durable bytes, and keyed object
//! identities remain in their separately owned format/identity layers.

#![forbid(unsafe_code)]

pub mod ledger;

/// The five accounting axes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ResourceAxis {
    CpuMicros,
    MemoryBytes,
    IoBytes,
    IoOps,
    NetworkBytes,
}

impl ResourceAxis {
    pub const ALL: [ResourceAxis; 5] = [
        ResourceAxis::CpuMicros,
        ResourceAxis::MemoryBytes,
        ResourceAxis::IoBytes,
        ResourceAxis::IoOps,
        ResourceAxis::NetworkBytes,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            ResourceAxis::CpuMicros => "cpu_micros",
            ResourceAxis::MemoryBytes => "memory_bytes",
            ResourceAxis::IoBytes => "io_bytes",
            ResourceAxis::IoOps => "io_ops",
            ResourceAxis::NetworkBytes => "network_bytes",
        }
    }
}

/// A point in resource space. `ZERO` is the additive identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ResourceVector {
    pub cpu_micros: u64,
    pub memory_bytes: u64,
    pub io_bytes: u64,
    pub io_ops: u64,
    pub network_bytes: u64,
}

/// Typed accounting failure: which axis, which operation, and both operands
/// — reproducible from the error alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResourceError {
    /// Checked addition overflowed an axis.
    Overflow {
        axis: ResourceAxis,
        lhs: u64,
        rhs: u64,
    },
    /// Checked subtraction underflowed an axis (releasing more than held).
    Underflow {
        axis: ResourceAxis,
        held: u64,
        released: u64,
    },
    /// A vector exceeded a ceiling on an axis.
    CeilingExceeded {
        axis: ResourceAxis,
        requested: u64,
        ceiling: u64,
    },
}

impl std::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResourceError::Overflow { axis, lhs, rhs } => {
                write!(f, "resource overflow on {}: {lhs} + {rhs}", axis.name())
            }
            ResourceError::Underflow {
                axis,
                held,
                released,
            } => {
                write!(
                    f,
                    "resource underflow on {}: held {held}, released {released}",
                    axis.name()
                )
            }
            ResourceError::CeilingExceeded {
                axis,
                requested,
                ceiling,
            } => {
                write!(
                    f,
                    "resource ceiling exceeded on {}: requested {requested}, ceiling {ceiling}",
                    axis.name()
                )
            }
        }
    }
}

impl std::error::Error for ResourceError {}

impl ResourceVector {
    pub const ZERO: ResourceVector = ResourceVector {
        cpu_micros: 0,
        memory_bytes: 0,
        io_bytes: 0,
        io_ops: 0,
        network_bytes: 0,
    };

    pub const fn axis(&self, axis: ResourceAxis) -> u64 {
        match axis {
            ResourceAxis::CpuMicros => self.cpu_micros,
            ResourceAxis::MemoryBytes => self.memory_bytes,
            ResourceAxis::IoBytes => self.io_bytes,
            ResourceAxis::IoOps => self.io_ops,
            ResourceAxis::NetworkBytes => self.network_bytes,
        }
    }

    fn map2(
        self,
        other: ResourceVector,
        mut f: impl FnMut(ResourceAxis, u64, u64) -> Result<u64, ResourceError>,
    ) -> Result<ResourceVector, ResourceError> {
        Ok(ResourceVector {
            cpu_micros: f(ResourceAxis::CpuMicros, self.cpu_micros, other.cpu_micros)?,
            memory_bytes: f(
                ResourceAxis::MemoryBytes,
                self.memory_bytes,
                other.memory_bytes,
            )?,
            io_bytes: f(ResourceAxis::IoBytes, self.io_bytes, other.io_bytes)?,
            io_ops: f(ResourceAxis::IoOps, self.io_ops, other.io_ops)?,
            network_bytes: f(
                ResourceAxis::NetworkBytes,
                self.network_bytes,
                other.network_bytes,
            )?,
        })
    }

    /// Checked component-wise addition.
    pub fn checked_add(self, other: ResourceVector) -> Result<ResourceVector, ResourceError> {
        self.map2(other, |axis, lhs, rhs| {
            lhs.checked_add(rhs)
                .ok_or(ResourceError::Overflow { axis, lhs, rhs })
        })
    }

    /// Checked component-wise subtraction (releasing charges).
    pub fn checked_sub(self, other: ResourceVector) -> Result<ResourceVector, ResourceError> {
        self.map2(other, |axis, held, released| {
            held.checked_sub(released).ok_or(ResourceError::Underflow {
                axis,
                held,
                released,
            })
        })
    }

    /// True iff every axis of `self` is ≤ the same axis of `other`
    /// (the component-wise partial order).
    pub fn fits_within(&self, other: &ResourceVector) -> bool {
        ResourceAxis::ALL
            .iter()
            .all(|&a| self.axis(a) <= other.axis(a))
    }
}

/// An inclusive upper bound in resource space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ResourceCeiling(ResourceVector);

impl ResourceCeiling {
    pub const fn new(bound: ResourceVector) -> Self {
        ResourceCeiling(bound)
    }

    pub const fn bound(&self) -> &ResourceVector {
        &self.0
    }

    /// Admits `requested` iff it fits; the rejection names the first
    /// violated axis in `ResourceAxis::ALL` order (deterministic).
    pub fn admit(&self, requested: ResourceVector) -> Result<Admitted, ResourceError> {
        for &axis in &ResourceAxis::ALL {
            let (req, ceil) = (requested.axis(axis), self.0.axis(axis));
            if req > ceil {
                return Err(ResourceError::CeilingExceeded {
                    axis,
                    requested: req,
                    ceiling: ceil,
                });
            }
        }
        Ok(Admitted { vector: requested })
    }
}

/// Proof token that a vector was admitted under some ceiling; constructible
/// only through [`ResourceCeiling::admit`]. Downstream permit machinery
/// takes `Admitted`, not raw vectors, so "forgot to check the budget" is
/// unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Admitted {
    vector: ResourceVector,
}

impl Admitted {
    pub const fn vector(&self) -> &ResourceVector {
        &self.vector
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(c: u64, m: u64, ib: u64, io: u64, n: u64) -> ResourceVector {
        ResourceVector {
            cpu_micros: c,
            memory_bytes: m,
            io_bytes: ib,
            io_ops: io,
            network_bytes: n,
        }
    }

    #[test]
    fn zero_is_the_additive_identity() {
        let a = v(1, 2, 3, 4, 5);
        assert_eq!(a.checked_add(ResourceVector::ZERO).unwrap(), a);
        assert_eq!(a.checked_sub(ResourceVector::ZERO).unwrap(), a);
        assert_eq!(a.checked_sub(a).unwrap(), ResourceVector::ZERO);
    }

    #[test]
    fn overflow_and_underflow_name_the_exact_axis() {
        let near_max = v(u64::MAX, 0, 0, 0, 0);
        let err = near_max.checked_add(v(1, 0, 0, 0, 0)).unwrap_err();
        assert_eq!(
            err,
            ResourceError::Overflow {
                axis: ResourceAxis::CpuMicros,
                lhs: u64::MAX,
                rhs: 1
            }
        );

        let err = v(0, 5, 0, 0, 0).checked_sub(v(0, 6, 0, 0, 0)).unwrap_err();
        assert_eq!(
            err,
            ResourceError::Underflow {
                axis: ResourceAxis::MemoryBytes,
                held: 5,
                released: 6
            }
        );
        assert_eq!(
            err.to_string(),
            "resource underflow on memory_bytes: held 5, released 6"
        );
    }

    #[test]
    fn ceilings_admit_exactly_the_component_wise_order() {
        let ceiling = ResourceCeiling::new(v(100, 100, 100, 100, 100));
        assert!(ceiling.admit(v(100, 100, 100, 100, 100)).is_ok());
        assert!(ceiling.admit(ResourceVector::ZERO).is_ok());
        // First violated axis in declared order wins the diagnostic.
        let err = ceiling.admit(v(200, 300, 0, 0, 0)).unwrap_err();
        assert_eq!(
            err,
            ResourceError::CeilingExceeded {
                axis: ResourceAxis::CpuMicros,
                requested: 200,
                ceiling: 100
            }
        );
        // fits_within agrees with admit.
        assert!(v(1, 1, 1, 1, 1).fits_within(ceiling.bound()));
        assert!(!v(1, 101, 1, 1, 1).fits_within(ceiling.bound()));
    }

    #[test]
    fn admitted_token_carries_the_admitted_vector() {
        let ceiling = ResourceCeiling::new(v(10, 10, 10, 10, 10));
        let admitted = ceiling.admit(v(1, 2, 3, 4, 5)).unwrap();
        assert_eq!(*admitted.vector(), v(1, 2, 3, 4, 5));
    }

    #[test]
    fn accumulation_across_admissions_stays_checked() {
        // A simple admission loop: accumulate admitted work, stop exactly at
        // the first rejection.
        let ceiling = ResourceCeiling::new(v(10, 1000, 1000, 1000, 1000));
        let mut used = ResourceVector::ZERO;
        let mut admitted_count = 0;
        for _ in 0..5 {
            let request = v(3, 10, 10, 10, 10);
            let next = used.checked_add(request).unwrap();
            if ceiling.admit(next).is_err() {
                break;
            }
            used = next;
            admitted_count += 1;
        }
        // 3*3=9 fits, 4th (12) exceeds cpu ceiling 10.
        assert_eq!(admitted_count, 3);
        assert_eq!(used.cpu_micros, 9);
    }
}
