use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CanonicalList, CanonicalPropertyValue, CanonicalScalar, PurposeContexts};

const WORKSPACE: usize = 2 * 1024 * 1024;

fn okay() -> Result<(), BufferedScanError<()>> {
    Ok(())
}

fn edge(props: EdgePropertyRow) -> BufferedEdge {
    BufferedEdge {
        entry: AdjacencyEntry {
            eid: EId(u128::MAX),
            src: VId(1),
            dst: VId(2),
            relation: RelationId(7),
            created_at: CommitSeq(1),
            retired_at: None,
        },
        props,
    }
}

fn scalar_row() -> EdgePropertyRow {
    let mut bytes = Vec::with_capacity(8192);
    bytes.extend_from_slice(b"small payload, large retained capacity");
    let mut props = Vec::with_capacity(32);
    props.push((
        PropertyKeyId(1),
        CanonicalScalar::bytes(bytes).unwrap().into(),
    ));
    props.push((
        PropertyKeyId(2),
        CanonicalScalar::ucs_basic_text("alpha\0β").unwrap().into(),
    ));
    props
}

#[test]
fn retained_edges_account_scalar_and_property_vector_capacities() {
    let ((), report) = run_async_under_lab(0x7c601, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let parent = MemoryPool::new(WORKSPACE, 0).unwrap();
        let pool = parent.child(WORKSPACE, 0).unwrap();
        let row = edge(scalar_row());
        let expected = size_of::<BufferedEdge>()
            + row.props.capacity() * size_of::<(PropertyKeyId, CanonicalPropertyValue)>()
            + row
                .props
                .iter()
                .map(|(_, value)| value.as_scalar().unwrap().owned_heap_bytes().unwrap())
                .sum::<usize>();
        assert!(expected > 8192);
        let value = BufferedValue::from_reserved(row, pool.reserve(&cx, WORKSPACE).unwrap());
        let value = value.retain_scan_edge(&mut okay).unwrap();
        assert_eq!(value.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        assert_eq!(parent.used(), expected);
        drop(pool);
        assert_eq!(value.entry.eid, EId(u128::MAX));
        assert_eq!(value.props.len(), 2);
        assert_eq!(parent.used(), expected);
        drop(value);
        assert_eq!(parent.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_vertices_include_unused_label_capacity() {
    let ((), report) = run_async_under_lab(0x7c602, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(WORKSPACE, 0).unwrap();
        let mut labels = Vec::with_capacity(64);
        labels.push(LabelId(1));
        let row = VertexRow {
            vid: VId(u128::MAX),
            birth_ordinal: 1,
            created_at: CommitSeq(2),
            retired_at: None,
            labels,
            props: scalar_row(),
        };
        let expected = size_of::<VertexRow>()
            + row.labels.capacity() * size_of::<LabelId>()
            + properties(&row.props, &mut okay).unwrap();
        let value = BufferedValue::from_reserved(row, pool.reserve(&cx, WORKSPACE).unwrap());
        let value = value.retain_scan_vertex(&mut okay).unwrap();
        assert_eq!(value.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        assert_eq!(value.vid, VId(u128::MAX));
        drop(value);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_blocks_include_all_nested_vectors_without_changing_locators() {
    let ((), report) = run_async_under_lab(0x7c603, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(WORKSPACE, 0).unwrap();
        let mut entries = Vec::with_capacity(16);
        entries.push(edge(Vec::new()).entry);
        let mut locators = Vec::with_capacity(16);
        locators.push(1);
        let mut rows = Vec::with_capacity(8);
        rows.push(scalar_row());
        let expected = size_of::<BufferedBlockImage>()
            + entries.capacity() * size_of::<AdjacencyEntry>()
            + locators.capacity()
            + rows.capacity() * size_of::<EdgePropertyRow>()
            + properties(&rows[0], &mut okay).unwrap();
        let image = (entries, Some(BlockProps { locators, rows }));
        let value = BufferedValue::from_reserved(image, pool.reserve(&cx, WORKSPACE).unwrap());
        let value = block(value, &mut okay).unwrap();
        assert_eq!(value.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        assert_eq!(value.1.as_ref().unwrap().locators, vec![1]);
        assert_eq!(value.1.as_ref().unwrap().props_ref(0).len(), 2);
        drop(value);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collection_images_retain_their_actual_capacity_instead_of_decoder_workspaces() {
    let ((), report) = run_async_under_lab(0x7c604, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(WORKSPACE, 0).unwrap();
        let mut children = Vec::with_capacity(128);
        children.push(CanonicalScalar::Int(7).into());
        let child_bytes = children.capacity() * size_of::<CanonicalPropertyValue>();
        let list = CanonicalPropertyValue::List(CanonicalList::try_new(children).unwrap());
        let props = vec![(PropertyKeyId(1), list)];
        let prop_bytes =
            props.capacity() * size_of::<(PropertyKeyId, CanonicalPropertyValue)>() + child_bytes;
        assert_eq!(properties(&props, &mut okay).unwrap(), prop_bytes);
        let image = (
            vec![edge(Vec::new()).entry],
            Some(BlockProps {
                locators: vec![1],
                rows: vec![props],
            }),
        );
        let expected = size_of::<BufferedBlockImage>()
            + image.0.capacity() * size_of::<AdjacencyEntry>()
            + image.1.as_ref().unwrap().locators.capacity()
            + image.1.as_ref().unwrap().rows.capacity() * size_of::<EdgePropertyRow>()
            + prop_bytes;
        let image = BufferedValue::from_reserved(image, pool.reserve(&cx, WORKSPACE).unwrap());
        let image = block(image, &mut okay).unwrap();
        assert_eq!(image.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        drop(image);
        let mut children = Vec::with_capacity(256);
        children.push(CanonicalScalar::Int(8).into());
        let child_bytes = children.capacity() * size_of::<CanonicalPropertyValue>();
        let props = vec![(
            PropertyKeyId(1),
            CanonicalPropertyValue::List(CanonicalList::try_new(children).unwrap()),
        )];
        let expected = size_of::<BufferedEdge>()
            + props.capacity() * size_of::<(PropertyKeyId, CanonicalPropertyValue)>()
            + child_bytes;
        let row = edge(props);
        let row = BufferedValue::from_reserved(row, pool.reserve(&cx, WORKSPACE).unwrap());
        let row = row.retain_scan_edge(&mut okay).unwrap();
        assert_eq!(row.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        drop(row);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_retention_control_failure_refunds_the_whole_candidate() {
    let ((), report) = run_async_under_lab(0x7c605, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(WORKSPACE, 0).unwrap();
        let mut count = 0;
        let value =
            BufferedValue::from_reserved(edge(scalar_row()), pool.reserve(&cx, WORKSPACE).unwrap());
        drop(
            value
                .retain_scan_edge(&mut || {
                    assert_eq!(pool.used(), WORKSPACE);
                    count += 1;
                    Ok::<_, BufferedScanError<usize>>(())
                })
                .unwrap(),
        );
        for stop in 1..=count {
            let value = BufferedValue::from_reserved(
                edge(scalar_row()),
                pool.reserve(&cx, WORKSPACE).unwrap(),
            );
            let mut at = 0;
            let result = value.retain_scan_edge(&mut || {
                assert_eq!(pool.used(), WORKSPACE);
                at += 1;
                if at == stop {
                    Err(BufferedScanError::Control(stop))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(BufferedScanError::Control(value)) if value == stop));
            assert_eq!(at, stop);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn undersized_prepaid_reservation_refuses_without_an_unaccounted_owner() {
    let ((), report) = run_async_under_lab(0x7c606, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64, 0).unwrap();
        // Inline test payload: no unadmitted heap allocation is introduced.
        let value = BufferedValue::from_reserved([0_u8; 128], pool.reserve(&cx, 64).unwrap());
        assert!(matches!(
            retain(value, 128),
            Err(BufferedReadError::SizeOverflow)
        ));
        assert_eq!(pool.used(), 0);
        assert!(matches!(
            add(usize::MAX, 1),
            Err(BufferedReadError::SizeOverflow)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_property_retention_keeps_exact_capacity_and_refunds_every_refusal() {
    let ((), report) = run_async_under_lab(0x7c607, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let parent = MemoryPool::new(WORKSPACE, 0).unwrap();
        let pool = parent.child(WORKSPACE, 0).unwrap();
        let property = || {
            let mut bytes = Vec::with_capacity(8192);
            bytes.extend_from_slice(b"selected value");
            let mut children = Vec::with_capacity(16);
            children.push(CanonicalScalar::bytes(bytes).unwrap().into());
            CanonicalPropertyValue::List(CanonicalList::try_new(children).unwrap())
        };
        let charge = pool.reserve(&cx, WORKSPACE).unwrap();
        let value = property();
        let expected = size_of::<CanonicalPropertyValue>()
            + value
                .owned_heap_bytes_controlled(&mut okay)
                .unwrap()
                .unwrap();
        assert!(expected > 8192);
        let mut count = 0;
        let value = BufferedValue::from_reserved(value, charge)
            .retain_scan_property(&mut || {
                assert_eq!(pool.used(), WORKSPACE);
                assert_eq!(parent.used(), WORKSPACE);
                count += 1;
                Ok::<_, BufferedScanError<usize>>(())
            })
            .unwrap();
        assert_eq!(value.charged_bytes(), expected);
        assert_eq!(pool.used(), expected);
        assert_eq!(parent.used(), expected);
        assert_eq!(value.as_ref(), &property());
        drop(value);
        assert_eq!(parent.used(), 0);

        for stop in 1..=count {
            let charge = pool.reserve(&cx, WORKSPACE).unwrap();
            let value = BufferedValue::from_reserved(property(), charge);
            let mut at = 0;
            let result = value.retain_scan_property(&mut || {
                assert_eq!(pool.used(), WORKSPACE);
                at += 1;
                if at == stop {
                    Err(BufferedScanError::Control(stop))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(BufferedScanError::Control(value)) if value == stop));
            assert_eq!(at, stop);
            assert_eq!(pool.used(), 0);
            assert_eq!(parent.used(), 0);
        }
        let charge = pool.reserve(&cx, 64).unwrap();
        let value = BufferedValue::from_reserved(property(), charge);
        assert!(matches!(
            value.retain_scan_property(&mut okay),
            Err(BufferedScanError::Read(BufferedReadError::SizeOverflow)),
        ));
        assert_eq!(pool.used(), 0);
        assert_eq!(parent.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
