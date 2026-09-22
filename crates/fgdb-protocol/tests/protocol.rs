#![forbid(unsafe_code)]

use fgdb_protocol::*;
use std::cell::Cell;

fn limits() -> FrameLimits { FrameLimits::new(4096).unwrap() }
fn session() -> SessionBinding { SessionBinding { transcript: [7; 32], auth_generation: 1 } }
fn ready() -> ReadyBinding {
    ReadyBinding {
        session: session(), namespace: [2; 32], incarnation: [3; 32],
        service_epoch: 9, posture: Posture::Local, authority_commitment: [4; 32],
    }
}
fn ready_connection() -> Connection {
    let mut connection = Connection::new(2, 4).unwrap();
    connection.negotiated().unwrap();
    connection.authenticated(session()).unwrap();
    connection.selected(ready()).unwrap();
    connection
}
fn frame(kind: FrameKind, binding: Binding, stream: StreamId, payload: &[u8]) -> Frame {
    Frame::new(kind, 1, stream, binding, payload.to_vec(), limits()).unwrap()
}

#[test]
fn all_header_classes_round_trip_at_every_fragment_boundary() {
    for binding in [Binding::Transport, Binding::Session(session()), Binding::Ready(ready())] {
        let original = frame(FrameKind::Error, binding, StreamId::CONTROL, b"payload\0bytes");
        let bytes = original.encode(limits()).unwrap();
        assert_eq!(bytes.len(), original.header().frame_len());
        for split in 0..=bytes.len() {
            let calls = Cell::new(0);
            let mut decoder = Decoder::new(limits());
            let first = decoder.decode(&bytes[..split], |_| { calls.set(calls.get() + 1); Ok(()) }).unwrap();
            assert_eq!(first.consumed, split);
            let decoded = if let Some(decoded) = first.frame {
                assert_eq!(split, bytes.len());
                decoded
            } else {
                let second = decoder.decode(&bytes[split..], |_| { calls.set(calls.get() + 1); Ok(()) }).unwrap();
                assert_eq!(second.consumed, bytes.len() - split);
                second.frame.unwrap()
            };
            assert_eq!(decoded, original, "split={split}");
            assert_eq!(calls.get(), 1);
            decoder.finish_eof().unwrap();
        }
    }
}

#[test]
fn one_byte_feeds_and_coalesced_frames_do_not_read_ahead() {
    let original = frame(FrameKind::Hello, Binding::Transport, StreamId::CONTROL, b"hello");
    let bytes = original.encode(limits()).unwrap();
    let mut decoder = Decoder::new(limits());
    for (index, byte) in bytes.iter().enumerate() {
        let progress = decoder.decode(&[*byte], |_| Ok(())).unwrap();
        assert_eq!(progress.consumed, 1);
        if index + 1 == bytes.len() { assert_eq!(progress.frame, Some(original.clone())); }
        else { assert!(progress.frame.is_none()); }
    }
    let coalesced = [bytes.as_slice(), bytes.as_slice()].concat();
    let first = decoder.decode(&coalesced, |_| Ok(())).unwrap();
    assert_eq!(first.consumed, bytes.len());
    assert_eq!(first.frame, Some(original.clone()));
    let second = decoder.decode(&coalesced[first.consumed..], |_| Ok(())).unwrap();
    assert_eq!(second.frame, Some(original));
}

#[test]
fn oversized_length_is_rejected_from_four_bytes_without_header_callback() {
    let mut decoder = Decoder::new(limits());
    assert_eq!(decoder.decode(&4097u32.to_be_bytes(), |_| panic!("must not authorize")),
        Err(ProtocolError::FrameTooLarge));
    assert_eq!(decoder.buffered_payload_bytes(), 0);
    assert_eq!(decoder.decode(&[], |_| Ok(())), Err(ProtocolError::DecoderPoisoned));
}

#[test]
fn stale_binding_is_rejected_before_payload_reservation() {
    let connection = ready_connection();
    let mut stale = ready();
    stale.service_epoch += 1;
    let bytes = frame(FrameKind::Execute, Binding::Ready(stale), StreamId::CONTROL, &[0; 3000])
        .encode(limits()).unwrap();
    let mut decoder = Decoder::new(limits());
    assert_eq!(decoder.decode(&bytes, |header| connection.validate_client_header(header)),
        Err(ProtocolError::InvalidBinding));
    assert_eq!(decoder.buffered_payload_bytes(), 0);
}

#[test]
fn codec_rejects_reserved_flags_unknown_tags_short_extended_header_and_version() {
    let original = frame(FrameKind::Hello, Binding::Transport, StreamId::CONTROL, &[])
        .encode(limits()).unwrap();
    for (offset, replacement, expected) in [
        (4, 2u16, ProtocolError::UnsupportedVersion),
        (6, 0xffff, ProtocolError::UnknownFrameKind),
        (8, 4, ProtocolError::UnsupportedFlags),
        (8, 2, ProtocolError::InvalidLength),
    ] {
        let mut bytes = original.clone();
        bytes[offset..offset + 2].copy_from_slice(&replacement.to_be_bytes());
        let mut decoder = Decoder::new(limits());
        assert_eq!(decoder.decode(&bytes, |_| Ok(())), Err(expected));
        assert_eq!(decoder.buffered_payload_bytes(), 0);
    }
}

#[test]
fn every_nonempty_proper_prefix_is_truncated_not_success() {
    let bytes = frame(FrameKind::Error, Binding::Ready(ready()), StreamId::CONTROL, b"secret")
        .encode(limits()).unwrap();
    for end in 1..bytes.len() {
        let mut decoder = Decoder::new(limits());
        assert!(decoder.decode(&bytes[..end], |_| Ok(())).unwrap().frame.is_none());
        assert_eq!(decoder.finish_eof(), Err(ProtocolError::TruncatedFrame), "prefix={end}");
    }
}

#[test]
fn debug_output_never_contains_auth_payloads() {
    let value = frame(FrameKind::Auth, Binding::Transport, StreamId::CONTROL, b"BEARER_SECRET");
    assert!(!format!("{value:?}").contains("BEARER_SECRET"));
    let bytes = value.encode(limits()).unwrap();
    let mut decoder = Decoder::new(limits());
    decoder.decode(&bytes[..bytes.len() - 1], |_| Ok(())).unwrap();
    assert!(!format!("{decoder:?}").contains("BEARER_SECRET"));
}

#[test]
fn handshake_separates_mechanics_authentication_and_selection() {
    let mut connection = Connection::new(2, 4).unwrap();
    let hello = frame(FrameKind::Hello, Binding::Transport, StreamId::CONTROL, &[]);
    connection.validate_client_header(hello.header()).unwrap();
    assert_eq!(connection.selected(ready()), Err(ProtocolError::InvalidState));
    assert_eq!(connection.authenticated(session()), Err(ProtocolError::InvalidState));
    connection.negotiated().unwrap();
    assert_eq!(connection.validate_client_header(hello.header()), Err(ProtocolError::InvalidState));
    connection.authenticated(session()).unwrap();
    let select = frame(FrameKind::SelectDatabase, Binding::Session(session()), StreamId::CONTROL, &[]);
    connection.validate_client_header(select.header()).unwrap();
    let mut other = ready(); other.session.transcript[0] ^= 1;
    assert_eq!(connection.selected(other), Err(ProtocolError::InvalidBinding));
    assert_eq!(connection.phase(), Phase::Authenticated);
    connection.selected(ready()).unwrap();
    assert_eq!(connection.selected(other), Err(ProtocolError::InvalidState));
}

#[test]
fn authority_refresh_changes_only_generation_and_fences_prior_headers() {
    let mut connection = ready_connection();
    let old = frame(FrameKind::Execute, connection.binding(), StreamId::CONTROL, &[]);
    let new = connection.authority_narrowed().unwrap();
    assert_eq!(new.auth_generation, 2);
    assert_eq!(new.transcript, session().transcript);
    assert_eq!(connection.validate_client_header(old.header()), Err(ProtocolError::InvalidBinding));
    let mut expected = ready(); expected.session = new;
    assert_eq!(connection.binding(), Binding::Ready(expected));
    let current = frame(FrameKind::Execute, connection.binding(), StreamId::CONTROL, &[]);
    connection.validate_client_header(current.header()).unwrap();
}

#[test]
fn drain_rejects_new_children_and_waits_for_typed_handoff_and_sends() {
    let mut connection = ready_connection();
    let stream = StreamId([1; 16]);
    let generation = connection.admit_child(stream, ChildKind::Query).unwrap();
    let send = connection.queue_send().unwrap();
    connection.begin_drain().unwrap();
    assert_eq!(connection.drain_cutoff(), Some(generation));
    connection.begin_drain().unwrap();
    assert_eq!(connection.admit_child(StreamId([2; 16]), ChildKind::Query), Err(ProtocolError::InvalidState));
    assert_eq!(connection.complete_drain(), Err(ProtocolError::DrainIncomplete));
    assert_eq!(connection.child_terminal(stream, generation + 1, ChildTerminus::ResultDurablyDetached), Err(ProtocolError::InvalidStream));
    assert_eq!(connection.child_terminal(stream, generation, ChildTerminus::TransactionOwnershipDetached), Err(ProtocolError::InvalidState));
    connection.child_terminal(stream, generation, ChildTerminus::ResultDurablyDetached).unwrap();
    assert_eq!(connection.complete_drain(), Err(ProtocolError::DrainIncomplete));
    connection.send_terminal(&send, SendTerminus::Sent).unwrap();
    connection.complete_drain().unwrap();
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn stale_child_completion_cannot_remove_a_reused_stream() {
    let mut connection = ready_connection();
    let id = StreamId([5; 16]);
    let first = connection.admit_child(id, ChildKind::Query).unwrap();
    connection.child_terminal(id, first, ChildTerminus::SemanticTerminalDurable).unwrap();
    let second = connection.admit_child(id, ChildKind::Query).unwrap();
    assert!(second > first);
    assert_eq!(connection.child_terminal(id, first, ChildTerminus::SemanticTerminalDurable), Err(ProtocolError::InvalidStream));
    assert_eq!(connection.children_in_flight(), 1);
}

#[test]
fn admission_bounds_and_server_frame_direction_are_checked() {
    let mut connection = ready_connection();
    for byte in 1..=2 { connection.admit_child(StreamId([byte; 16]), ChildKind::Query).unwrap(); }
    assert_eq!(connection.admit_child(StreamId([3; 16]), ChildKind::Query), Err(ProtocolError::StreamLimit));
    for _ in 0..4 { connection.queue_send().unwrap(); }
    assert!(matches!(connection.queue_send(), Err(ProtocolError::SendLimit)));
    let spoof = frame(FrameKind::ResultEnd, connection.binding(), StreamId([1; 16]), &[]);
    assert_eq!(connection.validate_client_header(spoof.header()), Err(ProtocolError::InvalidState));
}

#[test]
fn cancelled_before_write_refunds_but_uncertain_writes_never_do() {
    let initial = SendCost { bytes: 100, rows: 10 };
    let cost = SendCost { bytes: 20, rows: 2 };
    let mut window = FlowWindow::new(initial, initial).unwrap();
    drop(window.reserve(cost).unwrap());
    assert_eq!(window.available(), initial);
    window.reserve(cost).unwrap().cancel_before_write().unwrap();
    assert_eq!(window.available(), initial);
    let mut write = window.reserve(cost).unwrap();
    write.begin_write().unwrap();
    drop(write);
    assert_eq!(window.available(), SendCost { bytes: 80, rows: 8 });
    assert_eq!(window.failed_after_write(), 1);
    assert_eq!(window.sent(), SendCost { bytes: 0, rows: 0 });
    let mut write = window.reserve(cost).unwrap();
    write.begin_write().unwrap();
    write.sent().unwrap();
    assert_eq!(window.sent(), cost);
    assert_eq!(window.available(), SendCost { bytes: 60, rows: 6 });
}

#[test]
fn duplicate_credit_updates_do_not_inflate_either_dimension() {
    let mut window = FlowWindow::new(SendCost { bytes: 10, rows: 1 }, SendCost { bytes: 100, rows: 10 }).unwrap();
    let first = CreditUpdate { sequence: 1, bytes: 20, rows: 2 };
    assert_eq!(window.grant(first), Ok(WindowStatus::Applied));
    assert_eq!(window.grant(first), Ok(WindowStatus::Replayed));
    assert_eq!(window.available(), SendCost { bytes: 30, rows: 3 });
    assert_eq!(window.grant(CreditUpdate { bytes: 21, ..first }), Err(ProtocolError::InvalidCreditUpdate));
    assert_eq!(window.grant(CreditUpdate { sequence: 3, ..first }), Err(ProtocolError::InvalidCreditUpdate));
    assert_eq!(window.grant(CreditUpdate { sequence: 2, bytes: 10, rows: 8 }), Err(ProtocolError::CreditOverflow));
    assert_eq!(window.available(), SendCost { bytes: 30, rows: 3 });
    assert_eq!(window.grant(CreditUpdate { sequence: 2, bytes: 10, rows: 1 }), Ok(WindowStatus::Applied));
}

#[test]
fn empty_frames_complete_and_zero_row_control_still_charges_bytes() {
    let original = frame(FrameKind::Hello, Binding::Transport, StreamId::CONTROL, &[]);
    let mut decoder = Decoder::new(limits());
    assert_eq!(decoder.decode(&original.encode(limits()).unwrap(), |_| Ok(())).unwrap().frame, Some(original));
    let mut window = FlowWindow::new(SendCost { bytes: 10, rows: 0 }, SendCost { bytes: 10, rows: 0 }).unwrap();
    assert!(window.reserve(SendCost { bytes: 11, rows: 0 }).is_err());
    let mut reservation = window.reserve(SendCost { bytes: 10, rows: 0 }).unwrap();
    reservation.begin_write().unwrap();
    reservation.sent().unwrap();
    assert_eq!(window.available().bytes, 0);
}

#[test]
fn duplicate_or_foreign_send_completion_cannot_discharge_another_obligation() {
    let mut first = ready_connection();
    let mut second = ready_connection();
    let one = first.queue_send().unwrap();
    let two = first.queue_send().unwrap();
    let foreign = second.queue_send().unwrap();
    assert_eq!(first.send_terminal(&foreign, SendTerminus::Sent), Err(ProtocolError::InvalidState));
    assert_eq!(first.sends_in_flight(), 2);
    first.send_terminal(&one, SendTerminus::Sent).unwrap();
    assert_eq!(first.send_terminal(&one, SendTerminus::Sent), Err(ProtocolError::InvalidState));
    first.begin_drain().unwrap();
    assert_eq!(first.complete_drain(), Err(ProtocolError::DrainIncomplete));
    first.send_terminal(&two, SendTerminus::Failed).unwrap();
    first.complete_drain().unwrap();
    assert_eq!(second.sends_in_flight(), 1);
}
