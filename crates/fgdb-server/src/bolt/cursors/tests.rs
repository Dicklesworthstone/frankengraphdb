use super::*;
use asupersync::{Budget, runtime::RuntimeBuilder};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn interleaved_partial_results_keep_their_ids_and_last_execution() {
    let mut cursors = Cursors::new();
    assert_eq!(cursors.insert(vec![1, 2, 3], 30).unwrap(), 0);
    assert_eq!(cursors.insert(vec![4, 5], 20).unwrap(), 1);
    let (id, mut rows, charge) = cursors.take(0).unwrap();
    assert_eq!(rows.remove(0), 1);
    cursors.restore(id, rows, charge).unwrap();
    assert_eq!(cursors.last_qid, Some(1));
    assert_eq!(cursors.retained_bytes, 50);
    assert_eq!(cursors.take(-1).unwrap().1, vec![4, 5]);
    // Consuming the newest result cannot retarget the -1 selector.
    assert!(cursors.take(-1).is_err());
    assert_eq!(cursors.take(0).unwrap().1, vec![2, 3]);
    assert!(cursors.is_empty());
    assert_eq!(cursors.retained_bytes, 0);
    assert_eq!(cursors.insert(vec![6], 10).unwrap(), 2);
}

#[test]
fn empty_results_still_have_distinct_consumable_ids() {
    let mut cursors = Cursors::new();
    assert_eq!(cursors.insert(Vec::<u8>::new(), 1).unwrap(), 0);
    assert_eq!(cursors.insert(Vec::<u8>::new(), 1).unwrap(), 1);
    assert!(!cursors.is_empty());
    assert_eq!(cursors.take(0).unwrap().0, 0);
    assert_eq!(cursors.take(-1).unwrap().0, 1);
    assert!(cursors.take(1).is_err());
    assert!(cursors.is_empty());
}

#[test]
fn invalid_selectors_never_take_a_different_result() {
    let mut cursors = Cursors::new();
    cursors.insert(42, 5).unwrap();
    for qid in [i64::MIN, -2, 1, i64::MAX] {
        assert!(cursors.take(qid).is_err());
        assert_eq!(cursors.results.len(), 1);
        assert_eq!(cursors.retained_bytes, 5);
        assert_eq!(cursors.last_qid, Some(0));
    }
    assert_eq!(cursors.take(0).unwrap().1, 42);
}

#[test]
fn count_limit_refuses_before_advancing_the_id_and_recovers_after_consumption() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut cursors = Cursors::new();
    for id in 0..MAX_CURSORS {
        assert_eq!(
            cursors.insert(DropProbe(Arc::clone(&drops)), 1).unwrap(),
            i64::try_from(id).unwrap()
        );
    }
    assert!(cursors.check_admission().is_err());
    assert!(cursors.insert(DropProbe(Arc::clone(&drops)), 1).is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(cursors.retained_bytes, MAX_CURSORS);
    drop(cursors.take(0).unwrap());
    assert_eq!(
        cursors.insert(DropProbe(Arc::clone(&drops)), 1).unwrap(),
        i64::try_from(MAX_CURSORS).unwrap()
    );
    drop(cursors);
    assert_eq!(drops.load(Ordering::SeqCst), MAX_CURSORS + 2);
}

#[test]
fn exact_byte_limit_and_overflow_refuse_atomically() {
    let mut cursors = Cursors::new();
    assert_eq!(cursors.insert(1, MAX_RETAINED_BYTES).unwrap(), 0);
    for charge in [1, MAX_RETAINED_BYTES, usize::MAX] {
        assert!(cursors.insert(2, charge).is_err());
        assert_eq!(cursors.last_qid, Some(0));
        assert_eq!(cursors.retained_bytes, MAX_RETAINED_BYTES);
        assert_eq!(cursors.results.len(), 1);
    }
    cursors.take(0).unwrap();
    assert_eq!(cursors.retained_bytes, 0);
    assert_eq!(cursors.insert(3, MAX_RETAINED_BYTES).unwrap(), 1);
}

#[test]
fn query_id_exhaustion_fails_closed_without_integer_wraparound() {
    let mut cursors = Cursors::new();
    cursors.insert(7, 1).unwrap();
    cursors.last_qid = Some(i64::MAX);
    assert!(cursors.check_admission().is_err());
    assert!(cursors.insert(8, 1).is_err());
    assert_eq!(cursors.retained_bytes, 1);
    assert_eq!(cursors.take(0).unwrap().1, 7);
}

#[test]
fn restoring_an_existing_cursor_cannot_replace_its_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut cursors = Cursors::new();
    cursors.insert(DropProbe(Arc::clone(&drops)), 3).unwrap();
    assert!(
        cursors
            .restore(0, DropProbe(Arc::clone(&drops)), 4)
            .is_err()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(cursors.retained_bytes, 3);
    assert!(
        cursors
            .restore(1, DropProbe(Arc::clone(&drops)), 4)
            .is_err()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(cursors);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn partial_delivery_retains_charge_until_the_cursor_closes() {
    let mut cursors = Cursors::new();
    cursors.insert(vec![1, 2, 3], MAX_RETAINED_BYTES).unwrap();
    for expected in [1, 2] {
        let (id, mut rows, bytes) = cursors.take(-1).unwrap();
        assert_eq!(rows.remove(0), expected);
        cursors.restore(id, rows, bytes).unwrap();
        assert!(cursors.insert(vec![99], 1).is_err());
    }
    assert_eq!(cursors.take(-1).unwrap().1, vec![3]);
    assert_eq!(cursors.insert(vec![4], 1).unwrap(), 1);
}

fn with_cx(test: impl FnOnce(&Cx)) {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    test(&cx);
}

#[test]
fn retained_payload_counts_nested_maps_lists_bytes_and_graph_structures() {
    with_cx(|cx| {
        let mut key = String::with_capacity(64);
        key.push('k');
        let mut payload = Vec::with_capacity(128);
        payload.extend_from_slice(&[1, 2, 3]);
        let mut fields = Vec::with_capacity(4);
        fields.push(Value::Bytes(payload));
        let mut entries = Vec::with_capacity(3);
        entries.push((key, Value::Struct { tag: 0x4e, fields }));
        let mut values = Vec::with_capacity(2);
        values.push(Value::Map(entries));
        let expected = values.capacity() * size_of::<Value>()
            + match &values[0] {
                Value::Map(entries) => {
                    entries.capacity() * size_of::<(String, Value)>()
                        + entries[0].0.capacity()
                        + match &entries[0].1 {
                            Value::Struct { fields, .. } => {
                                fields.capacity() * size_of::<Value>()
                                    + match &fields[0] {
                                        Value::Bytes(bytes) => bytes.capacity(),
                                        _ => unreachable!(),
                                    }
                            }
                            _ => unreachable!(),
                        }
                }
                _ => unreachable!(),
            };
        let mut total = 0;
        value_bytes(cx, &Value::List(values), 0, &mut total).unwrap();
        assert_eq!(total, expected);
    });
}

#[test]
fn scalar_values_do_not_double_charge_their_enclosing_slots() {
    with_cx(|cx| {
        let mut total = 17;
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::Int(4),
            Value::Float(0.5),
        ] {
            value_bytes(cx, &value, 0, &mut total).unwrap();
        }
        assert_eq!(total, 17);
    });
}

#[test]
fn deep_results_and_capacity_arithmetic_fail_at_explicit_bounds() {
    with_cx(|cx| {
        let mut value = Value::Null;
        for _ in 0..fgdb_bolt::packstream::MAX_DEPTH {
            value = Value::List(vec![value]);
        }
        value_bytes(cx, &value, 0, &mut 0).unwrap();
        let value = Value::List(vec![value]);
        assert!(value_bytes(cx, &value, 0, &mut 0).is_err());
    });
    let mut total = MAX_RETAINED_BYTES;
    assert!(add(&mut total, 1).is_err());
    assert_eq!(total, MAX_RETAINED_BYTES);
    assert!(slots::<Value>(&mut 0, usize::MAX).is_err());
}
