use std::collections::HashSet;
use std::io::{self, Write};

use orderbook::canonical::{
    Digest, EVENTS_HEADER, write_digest_trailer, write_event, write_events_header, write_state,
};
use orderbook::domain::{
    MAX_PRICE_CENTS, MAX_QUANTITY, OrderId, Owner, Price, Quantity, Side, TimeInForce, ValueError,
};
use orderbook::engine::{Engine, replay};
use orderbook::event::{
    CancelReason, CommandKind, Cursor, Event, HaltReason, Priority, RejectCode,
};
use orderbook::input::{Field, HeaderError, RejectReason, SeqError};

const SAMPLE_CSV: &[u8] = include_bytes!("fixtures/sample.csv");
const SAMPLE_EVENTS: &[u8] = include_bytes!("fixtures/sample.events");
const SAMPLE_STATE: &[u8] = include_bytes!("fixtures/sample.state");
const SAMPLE_BLAKE3: &str = include_str!("fixtures/sample.blake3");
const EMPTY_STATE: &[u8] = include_bytes!("fixtures/empty.state");
const EMPTY_BLAKE3: &str = include_str!("fixtures/empty.blake3");

const FIELD_NAMES: [(Field, &str); 6] = [
    (Field::Id, "id"),
    (Field::Owner, "owner"),
    (Field::Side, "side"),
    (Field::Price, "price"),
    (Field::Size, "size"),
    (Field::Tif, "tif"),
];

const VALUE_NAMES: [(ValueError, &str); 6] = [
    (ValueError::Empty, "empty"),
    (ValueError::Syntax, "syntax"),
    (ValueError::NonAscii, "non_ascii"),
    (ValueError::TooLong, "too_long"),
    (ValueError::OffTick, "off_tick"),
    (ValueError::OutOfRange, "out_of_range"),
];

fn id(text: &str) -> OrderId {
    OrderId::new(text.as_bytes()).expect("test IDs are valid")
}

fn owner(text: &str) -> Owner {
    Owner::new(text.as_bytes()).expect("test owners are valid")
}

fn price(cents: u32) -> Price {
    Price::from_cents(cents).expect("test prices are in range")
}

fn qty(lots: u32) -> Quantity {
    Quantity::new(lots).expect("test quantities are in range")
}

fn encode_event(event: &Event) -> String {
    let mut out = Vec::new();
    write_event(&mut out, event).expect("writing to a Vec cannot fail");
    String::from_utf8(out).expect("canonical records are ASCII")
}

fn encode_events(events: &[Event]) -> Vec<u8> {
    let mut out = Vec::new();
    write_events_header(&mut out).expect("writing to a Vec cannot fail");
    for event in events {
        write_event(&mut out, event).expect("writing to a Vec cannot fail");
    }
    out
}

fn encode_state(engine: &Engine) -> Vec<u8> {
    let mut out = Vec::new();
    write_state(&mut out, engine).expect("writing to a Vec cannot fail");
    out
}

/// A sink that accepts `limit` bytes, then fails every later write.
struct FailAfter {
    limit: usize,
    accepted: Vec<u8>,
}

impl Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let room = self.limit - self.accepted.len();
        if room == 0 {
            return Err(io::Error::other("sink is full"));
        }
        let taken = room.min(buf.len());
        self.accepted.extend_from_slice(&buf[..taken]);
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Runs `write` against sinks that fail at every byte offset of `full`.
/// Each failing run must return an error after writing a prefix of `full`.
fn assert_every_cut_fails(full: &[u8], write: impl Fn(&mut FailAfter) -> io::Result<()>) {
    for limit in 0..full.len() {
        let mut sink = FailAfter {
            limit,
            accepted: Vec::new(),
        };
        assert!(write(&mut sink).is_err(), "limit {limit} did not fail");
        assert!(
            full.starts_with(&sink.accepted),
            "limit {limit} wrote bytes that are not a prefix of the output"
        );
    }
    let mut sink = FailAfter {
        limit: full.len(),
        accepted: Vec::new(),
    };
    assert!(write(&mut sink).is_ok(), "a sink of full length failed");
    assert_eq!(sink.accepted, full);
}

// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn empty_engine_matches_its_golden_state_and_digest() {
    let engine = Engine::new();
    let state = encode_state(&engine);
    assert_eq!(state, EMPTY_STATE);
    let digest = engine.digest();
    assert_eq!(digest.to_string(), EMPTY_BLAKE3.trim());
    assert_eq!(blake3::hash(&state).as_bytes(), digest.as_bytes());
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn event_header_is_the_golden_header_alone() {
    assert_eq!(EVENTS_HEADER, b"eob-events,1\n");
    assert_eq!(encode_events(&[]), b"eob-events,1\n");
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn sample_replay_matches_golden_events() {
    let (_, events) = replay(SAMPLE_CSV);
    assert_eq!(encode_events(&events), SAMPLE_EVENTS);
}

// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn sample_replay_matches_golden_state_and_digest() {
    let (engine, _) = replay(SAMPLE_CSV);
    let state = encode_state(&engine);
    assert_eq!(state, SAMPLE_STATE);
    let digest = engine.digest();
    assert_eq!(digest.to_string(), SAMPLE_BLAKE3.trim());
    assert_eq!(blake3::hash(&state).as_bytes(), digest.as_bytes());
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn accepted_events_encode_every_field() {
    let max = Event::Accepted {
        seq: u64::MAX,
        id: id("a1"),
        owner: owner("alice"),
        side: Side::Buy,
        price: price(1),
        size: qty(MAX_QUANTITY),
        tif: TimeInForce::Gtc,
    };
    assert_eq!(
        encode_event(&max),
        "18446744073709551615,accepted,a1,alice,buy,0.01,1000000000,GTC\n"
    );
    let top = Event::Accepted {
        seq: 2,
        id: id("s9"),
        owner: owner("bob"),
        side: Side::Sell,
        price: price(MAX_PRICE_CENTS),
        size: qty(1),
        tif: TimeInForce::PostOnly,
    };
    assert_eq!(
        encode_event(&top),
        "2,accepted,s9,bob,sell,1000000.00,1,POST_ONLY\n"
    );
    let ioc = Event::Accepted {
        seq: 3,
        id: id("x"),
        owner: owner("y"),
        side: Side::Sell,
        price: price(10025),
        size: qty(5),
        tif: TimeInForce::Ioc,
    };
    assert_eq!(encode_event(&ioc), "3,accepted,x,y,sell,100.25,5,IOC\n");
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn input_rejections_name_their_field_and_value() {
    let input = |reason: RejectReason| Event::Rejected {
        seq: 3,
        command: CommandKind::Input,
        id: None,
        reason: RejectCode::Input(reason),
    };
    assert_eq!(
        encode_event(&input(RejectReason::MalformedCsv)),
        "3,rejected,input,,malformed_csv\n"
    );
    assert_eq!(
        encode_event(&input(RejectReason::FieldCount)),
        "3,rejected,input,,field_count\n"
    );
    assert_eq!(
        encode_event(&input(RejectReason::UnknownCommand)),
        "3,rejected,input,,unknown_command\n"
    );
    for (field, field_name) in FIELD_NAMES {
        assert_eq!(
            encode_event(&input(RejectReason::Unexpected(field))),
            format!("3,rejected,input,,unexpected_{field_name}\n")
        );
        for (value, value_name) in VALUE_NAMES {
            assert_eq!(
                encode_event(&input(RejectReason::Invalid(field, value))),
                format!("3,rejected,input,,invalid_{field_name}_{value_name}\n")
            );
        }
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn command_rejections_name_their_command_id_and_reason() {
    let cases = [
        (CommandKind::Place, RejectCode::DuplicateId, "duplicate_id"),
        (CommandKind::Cancel, RejectCode::UnknownId, "unknown_order"),
        (CommandKind::Cancel, RejectCode::ClosedId, "order_closed"),
        (CommandKind::Modify, RejectCode::ClosedId, "order_closed"),
        (
            CommandKind::Place,
            RejectCode::PostOnlyWouldCross,
            "post_only_would_cross",
        ),
    ];
    for (command, reason, token) in cases {
        let event = Event::Rejected {
            seq: 6,
            command,
            id: Some(id("a1")),
            reason,
        };
        let command_name = match command {
            CommandKind::Input => "input",
            CommandKind::Place => "place",
            CommandKind::Cancel => "cancel",
            CommandKind::Modify => "modify",
        };
        assert_eq!(
            encode_event(&event),
            format!("6,rejected,{command_name},a1,{token}\n")
        );
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn trade_events_encode_prices_quantities_and_remainders() {
    let filled = Event::Trade {
        seq: u64::MAX,
        taker: id("t1"),
        maker: id("m1"),
        price: price(10025),
        quantity: qty(3),
        taker_left: 0,
        maker_left: MAX_QUANTITY,
    };
    assert_eq!(
        encode_event(&filled),
        "18446744073709551615,trade,t1,m1,100.25,3,0,1000000000\n"
    );
    let open = Event::Trade {
        seq: 2,
        taker: id("t2"),
        maker: id("m2"),
        price: price(1),
        quantity: qty(MAX_QUANTITY),
        taker_left: MAX_QUANTITY,
        maker_left: 0,
    };
    assert_eq!(
        encode_event(&open),
        "2,trade,t2,m2,0.01,1000000000,1000000000,0\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn rested_events_encode_side_price_remainder_and_rank() {
    let top = Event::Rested {
        seq: 7,
        id: id("r1"),
        side: Side::Sell,
        price: price(MAX_PRICE_CENTS),
        remaining: qty(1),
        rank: u64::MAX,
    };
    assert_eq!(
        encode_event(&top),
        "7,rested,r1,sell,1000000.00,1,18446744073709551615\n"
    );
    let bottom = Event::Rested {
        seq: 7,
        id: id("r2"),
        side: Side::Buy,
        price: price(1),
        remaining: qty(MAX_QUANTITY),
        rank: 1,
    };
    assert_eq!(encode_event(&bottom), "7,rested,r2,buy,0.01,1000000000,1\n");
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn cancelled_events_encode_every_reason() {
    let cases = [
        (CancelReason::User, "user"),
        (CancelReason::IocRemainder, "ioc_remainder"),
        (CancelReason::SelfTrade, "self_trade"),
    ];
    for (reason, token) in cases {
        let event = Event::Cancelled {
            seq: 8,
            id: id("c1"),
            remaining: qty(MAX_QUANTITY),
            reason,
        };
        assert_eq!(
            encode_event(&event),
            format!("8,cancelled,c1,1000000000,{token}\n")
        );
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn modified_events_encode_priority_and_rank() {
    let kept = Event::Modified {
        seq: 9,
        id: id("m1"),
        price: price(10040),
        remaining: qty(2),
        priority: Priority::Kept,
        rank: 3,
    };
    assert_eq!(encode_event(&kept), "9,modified,m1,100.40,2,kept,3\n");
    let lost = Event::Modified {
        seq: u64::MAX,
        id: id("m1"),
        price: price(MAX_PRICE_CENTS),
        remaining: qty(MAX_QUANTITY),
        priority: Priority::Lost,
        rank: u64::MAX,
    };
    assert_eq!(
        encode_event(&lost),
        "18446744073709551615,modified,m1,1000000.00,1000000000,lost,18446744073709551615\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn halted_events_encode_the_cursor_and_observed_sequence() {
    let gap = Event::Halted {
        expected: Cursor::Next(u64::MAX),
        reason: HaltReason::Gap,
        observed: Some(u64::MAX),
    };
    assert_eq!(
        encode_event(&gap),
        "18446744073709551615,halted,seq_gap,18446744073709551615\n"
    );
    let exhausted = Event::Halted {
        expected: Cursor::Exhausted,
        reason: HaltReason::Exhausted,
        observed: None,
    };
    assert_eq!(
        encode_event(&exhausted),
        "exhausted,halted,seq_exhausted,none\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn every_halt_reason_has_its_stable_token() {
    let cases = [
        (HaltReason::Header(HeaderError::Empty), "header_empty"),
        (HaltReason::Header(HeaderError::Mismatch), "header_mismatch"),
        (HaltReason::Seq(SeqError::Blank), "seq_blank"),
        (HaltReason::Seq(SeqError::Malformed), "seq_malformed"),
        (HaltReason::Seq(SeqError::Empty), "seq_empty"),
        (HaltReason::Seq(SeqError::Syntax), "seq_syntax"),
        (HaltReason::Seq(SeqError::Overflow), "seq_overflow"),
        (HaltReason::Gap, "seq_gap"),
        (HaltReason::Stale, "seq_stale"),
        (HaltReason::Exhausted, "seq_exhausted"),
    ];
    for (reason, token) in cases {
        let event = Event::Halted {
            expected: Cursor::Next(7),
            reason,
            observed: Some(9),
        };
        assert_eq!(encode_event(&event), format!("7,halted,{token},9\n"));
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn every_ascii_byte_in_an_id_is_escaped_as_specified() {
    for byte in 0x00..=0x7F_u8 {
        let order_id = OrderId::new(&[byte]).expect("every ASCII byte is a valid ID");
        let event = Event::Cancelled {
            seq: 1,
            id: order_id,
            remaining: qty(1),
            reason: CancelReason::User,
        };
        let expected = format!("1,cancelled,{},1,user\n", expected_escape(byte));
        assert_eq!(encode_event(&event), expected, "byte {byte:#04x}");
    }
}

/// The escaping rule, restated independently of the encoder.
fn expected_escape(byte: u8) -> String {
    let plain = (0x20..=0x7E).contains(&byte) && !matches!(byte, b',' | b'"' | b'\\');
    if plain {
        char::from(byte).to_string()
    } else {
        format!("\\x{byte:02X}")
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn separator_bytes_and_controls_have_fixed_escapes() {
    let cases = [
        (0x2C, r"\x2C"),
        (0x22, r"\x22"),
        (0x5C, r"\x5C"),
        (0x09, r"\x09"),
        (0x00, r"\x00"),
        (0x1F, r"\x1F"),
        (0x7F, r"\x7F"),
        (0x20, " "),
        (0x7E, "~"),
        (0x41, "A"),
    ];
    for (byte, text) in cases {
        let order_id = OrderId::new(&[byte]).expect("ASCII byte");
        let event = Event::Cancelled {
            seq: 1,
            id: order_id,
            remaining: qty(1),
            reason: CancelReason::User,
        };
        assert_eq!(
            encode_event(&event),
            format!("1,cancelled,{text},1,user\n"),
            "byte {byte:#04x}"
        );
    }
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn literal_escape_text_is_distinct_from_the_byte_it_names() {
    let literal = Event::Cancelled {
        seq: 1,
        id: id(r"\x41"),
        remaining: qty(1),
        reason: CancelReason::User,
    };
    let letter = Event::Cancelled {
        seq: 1,
        id: id("A"),
        remaining: qty(1),
        reason: CancelReason::User,
    };
    assert_eq!(encode_event(&literal), "1,cancelled,\\x5Cx41,1,user\n");
    assert_eq!(encode_event(&letter), "1,cancelled,A,1,user\n");
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn owners_escape_their_bytes_too() {
    let event = Event::Accepted {
        seq: 4,
        id: id("a,1"),
        owner: owner("\"o\"\\\x7f"),
        side: Side::Buy,
        price: price(10000),
        size: qty(2),
        tif: TimeInForce::Gtc,
    };
    assert_eq!(
        encode_event(&event),
        "4,accepted,a\\x2C1,\\x22o\\x22\\x5C\\x7F,buy,100.00,2,GTC\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn reserved_ids_are_listed_in_raw_byte_order() {
    let (engine, _) = replay(
        b"seq,cmd,id,owner,side,price,size,tif\n\
          1,place,-,o,buy,1.00,1,GTC\n\
          2,place,\",\",o,buy,2.00,1,GTC\n",
    );
    let state = String::from_utf8(encode_state(&engine)).expect("state is ASCII");
    // Raw bytes: 0x2C sorts before 0x2D, though `\x2C` would sort after `-`.
    assert!(state.ends_with("ids,2\nid,\\x2C\nid,-\n"), "{state}");
}

// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn hidden_state_changes_the_bytes_and_digest_at_equal_depth() {
    let header = "seq,cmd,id,owner,side,price,size,tif\n";
    let scenarios = [
        ("base", "1,place,a1,alice,buy,100.00,5,GTC\n"),
        ("owner", "1,place,a1,bob,buy,100.00,5,GTC\n"),
        ("tif", "1,place,a1,alice,buy,100.00,5,POST_ONLY\n"),
        (
            "fifo",
            "1,place,a1,alice,buy,100.00,2,GTC\n2,place,a2,bob,buy,100.00,3,GTC\n",
        ),
        (
            "fifo reversed",
            "1,place,a2,bob,buy,100.00,3,GTC\n2,place,a1,alice,buy,100.00,2,GTC\n",
        ),
        (
            "cursor",
            "1,place,a1,alice,buy,100.00,5,GTC\n2,cancel,zz,,,,,\n",
        ),
        (
            "clock",
            "1,place,a1,alice,buy,100.00,3,GTC\n2,modify,a1,,,100.00,5,\n",
        ),
        (
            "reserved",
            "1,place,a1,alice,buy,100.00,5,GTC\n2,place,p1,carol,sell,99.00,1,POST_ONLY\n",
        ),
        (
            "halted",
            "1,place,a1,alice,buy,100.00,5,GTC\n9,cancel,a1,,,,,\n",
        ),
    ];

    let mut states: Vec<(&str, Vec<u8>, Digest)> = Vec::new();
    for (label, body) in scenarios {
        let csv = format!("{header}{body}");
        let (engine, _) = replay(csv.as_bytes());
        // Every scenario shows the same visible bid depth: 5 lots at 100.00.
        assert_eq!(
            engine
                .bids()
                .map(|order| order.remaining.lots())
                .sum::<u32>(),
            5,
            "{label}"
        );
        assert!(
            engine.bids().all(|order| order.price == price(10000)),
            "{label}"
        );
        assert_eq!(engine.asks().count(), 0, "{label}");
        states.push((label, encode_state(&engine), engine.digest()));
    }

    // Replaying the same input again gives the same bytes and digest.
    let (again, _) = replay(format!("{header}{}", scenarios[0].1).as_bytes());
    assert_eq!(encode_state(&again), states[0].1);
    assert_eq!(again.digest(), states[0].2);

    for (i, (label_a, state_a, digest_a)) in states.iter().enumerate() {
        for (label_b, state_b, digest_b) in &states[i + 1..] {
            assert_ne!(
                state_a, state_b,
                "{label_a} and {label_b} share state bytes"
            );
            assert_ne!(digest_a, digest_b, "{label_a} and {label_b} share a digest");
        }
    }
    let distinct: HashSet<Digest> = states.iter().map(|(_, _, digest)| *digest).collect();
    assert_eq!(distinct.len(), scenarios.len());
}

#[test]
fn writers_accept_unsized_sinks() {
    let mut bytes = Vec::new();
    let sink: &mut dyn Write = &mut bytes;
    write_state(sink, &Engine::new()).expect("writing to a Vec cannot fail");
    assert_eq!(bytes, EMPTY_STATE);
}

// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn digest_trailer_is_one_lowercase_hex_record() {
    let (engine, _) = replay(SAMPLE_CSV);
    let mut out = Vec::new();
    write_digest_trailer(&mut out, &engine.digest()).expect("writing to a Vec cannot fail");
    assert_eq!(
        out,
        format!("digest,blake3,{}\n", SAMPLE_BLAKE3.trim()).as_bytes()
    );
}

// exkg-verifies: fea-ky6m7m#ac-events
#[test]
fn writers_propagate_every_io_error() {
    let (engine, events) = replay(SAMPLE_CSV);
    assert_every_cut_fails(EVENTS_HEADER, write_events_header);
    assert_every_cut_fails(&encode_state(&engine), |out| write_state(out, &engine));
    for event in &events {
        assert_every_cut_fails(encode_event(event).as_bytes(), |out| {
            write_event(out, event)
        });
    }
    let digest = engine.digest();
    let trailer = format!("digest,blake3,{digest}\n");
    assert_every_cut_fails(trailer.as_bytes(), |out| write_digest_trailer(out, &digest));
}
