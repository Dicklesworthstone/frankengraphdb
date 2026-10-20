//! Explicit lossy coordinate projection; exact and packed encodings are unchanged.

use crate::{BeaconError, WorkControl};
use fgdb_types::CanonicalScalar;

pub(super) fn rounded(
    value: &CanonicalScalar,
    work: &mut dyn WorkControl,
) -> Result<f32, BeaconError> {
    let invalid =
        || BeaconError::InvalidQuery("rounded vector coordinate must be finite numeric f32");
    let projected = match value {
        CanonicalScalar::Int(value) => *value as f32,
        CanonicalScalar::Float(value) if value.get().is_finite() => value.get() as f32,
        CanonicalScalar::Decimal(value) => {
            // CanonicalDecimal is bounded fixed-scale storage. Reserve work for
            // its entire short spelling BEFORE allocating it. Parse to f32
            // directly: a binary64 intermediate can round a near-midpoint twice.
            work.charge(64)?;
            value.to_string().parse::<f32>().map_err(|_| invalid())?
        }
        _ => return Err(invalid()),
    };
    if !projected.is_finite() {
        return Err(invalid());
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{Projection, VectorEncoding};
    use crate::{DistanceMetric, HnswConfig, IndexConfig, IndexDocument, WorkBudget};
    use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalPropertyValue, VId};

    fn float(value: f64) -> CanonicalScalar {
        CanonicalScalar::Float(CanonicalF64::new(value))
    }

    fn decimal(coefficient: i128) -> CanonicalScalar {
        CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(coefficient).unwrap())
    }

    fn project(
        values: &[Option<CanonicalScalar>],
        encoding: VectorEncoding,
        work: &mut dyn WorkControl,
    ) -> Result<IndexDocument, BeaconError> {
        let values: Vec<Option<CanonicalPropertyValue>> = values
            .iter()
            .cloned()
            .map(|value| value.map(CanonicalPropertyValue::from))
            .collect();
        Projection {
            text: None,
            vector: (0..values.len()).collect(),
            encoding,
        }
        .project(
            VId(1),
            &IndexConfig {
                text: None,
                vector: Some(HnswConfig::new(
                    values.len(),
                    DistanceMetric::SquaredEuclidean,
                )),
                ..IndexConfig::default()
            },
            |key| values[key].as_ref(),
            work,
        )
    }

    #[test]
    fn rounding_is_explicit_and_default_exact_admission_does_not_change() {
        assert_eq!(VectorEncoding::default(), VectorEncoding::Coordinates);
        for (value, bits) in [
            (float(0.1), 0x3dcc_cccd),
            (CanonicalScalar::Int(16_777_217), 0x4b80_0000),
            (CanonicalScalar::Int(16_777_219), 0x4b80_0002),
            (decimal(100_000_000_000_000_000), 0x3dcc_cccd),
        ] {
            let values = [Some(value)];
            assert!(
                project(
                    &values,
                    VectorEncoding::Coordinates,
                    &mut WorkBudget::new(1000)
                )
                .is_err()
            );
            let rounded = project(
                &values,
                VectorEncoding::CoordinatesRoundedF32,
                &mut WorkBudget::new(1000),
            )
            .unwrap();
            assert_eq!(rounded.vector.unwrap()[0].to_bits(), bits);
        }
        for value in [float(0.125), CanonicalScalar::Int(16_777_216)] {
            let values = [Some(value)];
            let exact = project(
                &values,
                VectorEncoding::Coordinates,
                &mut WorkBudget::new(1000),
            )
            .unwrap();
            let rounded = project(
                &values,
                VectorEncoding::CoordinatesRoundedF32,
                &mut WorkBudget::new(1000),
            )
            .unwrap();
            assert_eq!(exact.vector, rounded.vector);
        }
    }

    #[test]
    fn decimal_rounding_avoids_the_binary64_midpoint_trap() {
        // 1 + 2^-24 is the exact midpoint between 1f32 and its successor.
        // The decimal is just above it but first rounding to f64 loses that bit.
        let value = decimal(1_000_000_059_604_644_776);
        let projected = rounded(&value, &mut WorkBudget::new(1000)).unwrap();
        assert_eq!(projected.to_bits(), 0x3f80_0001);
        let CanonicalScalar::Decimal(value) = value else {
            unreachable!()
        };
        let double_rounded = value.to_string().parse::<f64>().unwrap() as f32;
        assert_eq!(double_rounded.to_bits(), 0x3f80_0000);
    }

    #[test]
    fn finite_extremes_underflow_and_overflow_have_defined_outcomes() {
        for (value, expected) in [
            (float(f64::from(f32::MAX)), f32::MAX.to_bits()),
            (float(f64::from(f32::from_bits(1))), 1),
            (float(f64::from_bits(1)), 0),
            (float(-f64::from_bits(1)), 0x8000_0000),
            (CanonicalScalar::Int(i64::MAX), 0x5f00_0000),
            (CanonicalScalar::Int(i64::MIN), 0xdf00_0000),
        ] {
            assert_eq!(
                rounded(&value, &mut WorkBudget::new(1000))
                    .unwrap()
                    .to_bits(),
                expected
            );
        }
        for value in [
            float(f64::MAX),
            float(-f64::MAX),
            float(f64::INFINITY),
            float(f64::NEG_INFINITY),
            float(f64::NAN),
            CanonicalScalar::Bool(true),
            CanonicalScalar::Null,
            CanonicalScalar::ucs_basic_text("0.1").unwrap(),
        ] {
            assert!(rounded(&value, &mut WorkBudget::new(1000)).is_err());
        }
    }

    #[test]
    fn missing_coordinates_precede_type_checks_and_disabled_vectors_are_unread() {
        let bad = CanonicalScalar::ucs_basic_text("not a coordinate").unwrap();
        for missing in [None, Some(CanonicalScalar::Null)] {
            for values in [
                vec![Some(bad.clone()), missing.clone()],
                vec![missing, Some(bad.clone())],
            ] {
                assert!(
                    project(
                        &values,
                        VectorEncoding::CoordinatesRoundedF32,
                        &mut WorkBudget::new(1000),
                    )
                    .unwrap()
                    .vector
                    .is_none()
                );
            }
        }
        let projection = Projection {
            text: None::<usize>,
            vector: vec![0],
            encoding: VectorEncoding::CoordinatesRoundedF32,
        };
        let config = IndexConfig {
            text: None,
            vector: None,
            ..IndexConfig::default()
        };
        let result = projection
            .project(
                VId(1),
                &config,
                |_| panic!("disabled vector read"),
                &mut WorkBudget::new(1000),
            )
            .unwrap();
        assert!(result.vector.is_none());
    }

    #[test]
    fn packed_vectors_remain_byte_exact_and_never_use_numeric_rounding() {
        let bytes = CanonicalScalar::bytes(0.1_f32.to_le_bytes().to_vec()).unwrap();
        let packed = project(
            &[Some(bytes.clone())],
            VectorEncoding::PackedF32Le,
            &mut WorkBudget::new(1000),
        )
        .unwrap();
        assert_eq!(packed.vector.unwrap()[0].to_bits(), 0x3dcc_cccd);
        assert!(
            project(
                &[Some(bytes)],
                VectorEncoding::CoordinatesRoundedF32,
                &mut WorkBudget::new(1000),
            )
            .is_err()
        );
        assert!(
            project(
                &[Some(float(0.1))],
                VectorEncoding::PackedF32Le,
                &mut WorkBudget::new(1000),
            )
            .is_err()
        );
    }

    #[test]
    fn every_projection_work_cut_refuses_without_a_partial_document() {
        struct Count(usize);
        impl WorkControl for Count {
            fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
                self.0 += units;
                Ok(())
            }
        }
        let values = [
            Some(float(0.1)),
            Some(decimal(1)),
            Some(CanonicalScalar::Int(16_777_217)),
        ];
        let mut count = Count(0);
        let expected = project(&values, VectorEncoding::CoordinatesRoundedF32, &mut count).unwrap();
        for limit in 0..count.0 {
            assert!(matches!(
                project(
                    &values,
                    VectorEncoding::CoordinatesRoundedF32,
                    &mut WorkBudget::new(limit),
                ),
                Err(BeaconError::WorkBudgetExceeded)
            ));
        }
        let result = project(
            &values,
            VectorEncoding::CoordinatesRoundedF32,
            &mut WorkBudget::new(count.0),
        )
        .unwrap();
        assert_eq!(result.vector, expected.vector);
    }
}
