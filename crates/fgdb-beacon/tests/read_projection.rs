use fgdb_beacon::read::{Projection, ReadOptions, Search};
use fgdb_beacon::{BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, IndexConfig, TextMatch, VectorSearch, WorkBudget, WorkControl};
use fgdb_types::{CanonicalF64, CanonicalScalar, VId};

fn config() -> IndexConfig {
    IndexConfig { vector: Some(HnswConfig::new(2, DistanceMetric::SquaredEuclidean)), ..IndexConfig::default() }
}
fn projection() -> Projection<u8> { Projection { text: Some(0), vector: vec![1, 2] } }
fn budget() -> WorkBudget { WorkBudget::new(1_000_000) }
fn text(value: &str) -> CanonicalScalar { CanonicalScalar::ucs_basic_text(value).unwrap() }

#[test]
fn scalar_coordinates_keep_order_and_do_not_invent_missing_modalities() {
    let props = [text("search this"), CanonicalScalar::Int(7), CanonicalScalar::Float(CanonicalF64::new(0.5))];
    let p = Projection { text: Some(0_u8), vector: vec![2, 1] };
    let row = p.project(VId(8), &config(), |k| props.get(usize::from(k)), &mut budget()).unwrap();
    assert_eq!(row.id, VId(8));
    assert_eq!(row.text.as_deref(), Some("search this"));
    assert_eq!(row.vector, Some(vec![0.5, 7.0]));
    let row = p.project(VId(8), &config(), |k| (k != 1).then(|| &props[usize::from(k)]), &mut budget()).unwrap();
    assert_eq!(row.vector, None);
    assert_eq!(row.text.as_deref(), Some("search this"));
    let row = p.project(VId(8), &config(), |_| None, &mut budget()).unwrap();
    assert!(row.vector.is_none() && row.text.is_none());
}

#[test]
fn exact_f32_admission_rejects_rounding_nonfinite_and_wrong_scalar_types() {
    let mut c = config();
    c.text = None;
    c.vector.as_mut().unwrap().dimensions = 1;
    let p = Projection { text: None, vector: vec![1_u8] };
    for value in [
        CanonicalScalar::Int(i64::MAX), CanonicalScalar::Int(16_777_217),
        CanonicalScalar::Float(CanonicalF64::new(0.1)),
        CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
        CanonicalScalar::Float(CanonicalF64::new(f64::INFINITY)),
        CanonicalScalar::Float(CanonicalF64::new(f64::MAX)), text("1"),
    ] {
        assert!(matches!(p.project(VId(1), &c, |_| Some(&value), &mut budget()), Err(BeaconError::InvalidQuery(_))));
    }
    for value in [
        CanonicalScalar::Int(i64::MIN), CanonicalScalar::Int(16_777_216),
        CanonicalScalar::Float(CanonicalF64::new(f64::from(f32::from_bits(1)))),
        CanonicalScalar::Float(CanonicalF64::new(f64::from(f32::MAX))),
    ] {
        assert!(p.project(VId(1), &c, |_| Some(&value), &mut budget()).is_ok());
    }
}

#[test]
fn an_incomplete_vector_does_not_validate_the_other_coordinates() {
    let p = projection();
    let props = [text("visible"), CanonicalScalar::Float(CanonicalF64::new(f64::NAN)), CanonicalScalar::Null];
    let row = p.project(VId(1), &config(), |k| props.get(usize::from(k)), &mut budget()).unwrap();
    assert_eq!(row.vector, None);
    assert_eq!(row.text.as_deref(), Some("visible"));
    // A resolver which masks the key has exactly the same absence semantics.
    let row = p.project(VId(1), &config(), |k| (k != 2).then(|| &props[usize::from(k)]), &mut budget()).unwrap();
    assert_eq!(row.vector, None);
}

#[test]
fn unused_lanes_are_not_validated_or_resolved() {
    let mut options: ReadOptions<u8, u8> = ReadOptions::text(0);
    options.projection.vector = vec![1, 2];
    options.index.vector = Some(HnswConfig::new(0, DistanceMetric::Cosine));
    let query = Search::Text { query: "x", k: 1, mode: TextMatch::Any };
    let config = options.config_for(query).unwrap();
    let value = text("x");
    let row = options.projection.project(VId(1), &config, |key| {
        assert_eq!(key, 0, "unused vector was resolved"); Some(&value)
    }, &mut budget()).unwrap();
    assert_eq!(row.vector, None);
    let hybrid = Search::Hybrid(ExactHybridQuery {
        vector: &[f32::NAN], text: "x", k: 1, vector_candidates: 50, text_candidates: 1,
        vector_mode: VectorSearch::Approximate { ef_search: 0 }, text_mode: TextMatch::Any,
        profile: ExactRrfProfile::new(60, 0, 1).unwrap(),
    });
    hybrid.validate(&options.config_for(hybrid).unwrap(), &mut budget()).unwrap();
}

#[test]
fn projection_and_result_limits_refuse_instead_of_truncating() {
    let mut options: ReadOptions<u8, u8> = ReadOptions::text(0);
    options.policy.max_result_rows = 1;
    assert!(matches!(options.config_for(Search::Text { query: "x", k: 2, mode: TextMatch::Any }),
        Err(BeaconError::ResourceLimit { resource: "result rows", limit: 1 })));
    let value = text("12345");
    options.index.max_text_bytes = 4;
    assert!(matches!(options.projection.project(VId(1), &options.index, |_| Some(&value), &mut budget()),
        Err(BeaconError::ResourceLimit { resource: "staged text bytes", limit: 4 })));
    let mut c = config();
    c.max_vector_values = 1;
    assert!(matches!(projection().project(VId(1), &c, |_| None, &mut budget()),
        Err(BeaconError::ResourceLimit { resource: "staged vector values", limit: 1 })));
}

#[derive(Default)]
struct Count(usize);
impl WorkControl for Count {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> { self.0 += units; Ok(()) }
}

#[test]
fn exact_projection_budget_succeeds_but_every_shorter_allowance_refuses() {
    let props = [text("red blue"), CanonicalScalar::Int(3), CanonicalScalar::Int(4)];
    let p = projection();
    let mut count = Count::default();
    p.project(VId(1), &config(), |k| props.get(usize::from(k)), &mut count).unwrap();
    for units in 0..count.0 {
        assert!(matches!(p.project(VId(1), &config(), |k| props.get(usize::from(k)), &mut WorkBudget::new(units)),
            Err(BeaconError::WorkBudgetExceeded)));
    }
    p.project(VId(1), &config(), |k| props.get(usize::from(k)), &mut WorkBudget::new(count.0)).unwrap();
}
