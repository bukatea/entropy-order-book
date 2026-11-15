use orderbook::domain::{
    Cancel, Command, Modify, OrderId, Owner, Place, Price, Quantity, Side, TimeInForce, ValueError,
};
use orderbook::input::{
    Field, HeaderError, ParsedLine, PhysicalLines, RejectReason, SeqError, commands, parse_header,
    parse_line,
};

const HEADER: &str = "seq,cmd,id,owner,side,price,size,tif\n";

fn line(text: &[u8]) -> Result<ParsedLine, SeqError> {
    parse_line(text)
}

fn valid(text: &[u8]) -> (u64, Command) {
    match line(text) {
        Ok(ParsedLine {
            seq,
            result: Ok(command),
        }) => (seq, command),
        other => panic!("expected a valid command, got {other:?}"),
    }
}

fn rejected(text: &[u8]) -> (u64, RejectReason) {
    match line(text) {
        Ok(ParsedLine {
            seq,
            result: Err(reason),
        }) => (seq, reason),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

fn all(input: &[u8]) -> Vec<Result<ParsedLine, SeqError>> {
    commands(input).expect("valid header").collect()
}

fn place_id(text: &[u8]) -> Vec<u8> {
    match valid(text).1 {
        Command::Place(place) => place.id.as_bytes().to_vec(),
        other => panic!("expected a place, got {other:?}"),
    }
}

fn id(bytes: &[u8]) -> OrderId {
    OrderId::new(bytes).unwrap()
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn sample_place_parses_to_cents_and_lots() {
    let input = format!("{HEADER}1,place,a1,alice,buy,100.25,5,GTC");
    let expected = Command::Place(Place {
        id: id(b"a1"),
        owner: Owner::new(b"alice").unwrap(),
        side: Side::Buy,
        price: Price::from_cents(10025).unwrap(),
        size: Quantity::new(5).unwrap(),
        tif: TimeInForce::Gtc,
    });
    assert_eq!(
        all(input.as_bytes()),
        vec![Ok(ParsedLine {
            seq: 1,
            result: Ok(expected)
        })]
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn place_reads_every_side_and_tif() {
    let (_, command) = valid(b"7,place,b,bob,sell,0.01,1000000000,POST_ONLY");
    let Command::Place(place) = command else {
        panic!("not a place")
    };
    assert_eq!((place.side, place.tif), (Side::Sell, TimeInForce::PostOnly));
    assert_eq!((place.price.cents(), place.size.lots()), (1, 1_000_000_000));
    let (_, command) = valid(b"8,place,c,bob,buy,1,1,IOC");
    let Command::Place(place) = command else {
        panic!("not a place")
    };
    assert_eq!(place.tif, TimeInForce::Ioc);
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn cancel_needs_only_an_id() {
    assert_eq!(
        valid(b"2,cancel,a1,,,,,"),
        (2, Command::Cancel(Cancel { id: id(b"a1") }))
    );
    assert_eq!(
        valid(br#"2,cancel,a1,"","",,,"""#),
        (2, Command::Cancel(Cancel { id: id(b"a1") }))
    );
    assert_eq!(
        rejected(b"2,cancel,,,,,,"),
        (2, RejectReason::Invalid(Field::Id, ValueError::Empty))
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn cancel_rejects_each_unused_field() {
    let cases: [(&[u8], Field); 5] = [
        (b"2,cancel,a1,alice,,,,", Field::Owner),
        (b"2,cancel,a1,,buy,,,", Field::Side),
        (b"2,cancel,a1,,,1.00,,", Field::Price),
        (b"2,cancel,a1,,,,5,", Field::Size),
        (b"2,cancel,a1,,,,,GTC", Field::Tif),
    ];
    for (text, field) in cases {
        assert_eq!(rejected(text), (2, RejectReason::Unexpected(field)));
    }
    // A single space is not empty: nothing is trimmed.
    assert_eq!(
        rejected(b"2,cancel,a1, ,,,,"),
        (2, RejectReason::Unexpected(Field::Owner))
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn modify_carries_id_price_and_new_remaining_size() {
    let expected = Command::Modify(Modify {
        id: id(b"a1"),
        price: Price::from_cents(10040).unwrap(),
        size: Quantity::new(2).unwrap(),
    });
    assert_eq!(valid(b"3,modify,a1,,,100.40,2,"), (3, expected));
    let cases: [(&[u8], RejectReason); 6] = [
        (
            b"3,modify,a1,alice,,100.40,2,",
            RejectReason::Unexpected(Field::Owner),
        ),
        (
            b"3,modify,a1,,buy,100.40,2,",
            RejectReason::Unexpected(Field::Side),
        ),
        (
            b"3,modify,a1,,,100.40,2,GTC",
            RejectReason::Unexpected(Field::Tif),
        ),
        (
            b"3,modify,a1,,,,2,",
            RejectReason::Invalid(Field::Price, ValueError::Empty),
        ),
        (
            b"3,modify,a1,,,100.40,,",
            RejectReason::Invalid(Field::Size, ValueError::Empty),
        ),
        (
            b"3,modify,a1,,,100.40,0,",
            RejectReason::Invalid(Field::Size, ValueError::OutOfRange),
        ),
    ];
    for (text, reason) in cases {
        assert_eq!(rejected(text), (3, reason));
    }
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn place_requires_every_field() {
    let cases: [(&[u8], Field); 6] = [
        (b"1,place,,alice,buy,1,1,GTC", Field::Id),
        (b"1,place,a1,,buy,1,1,GTC", Field::Owner),
        (b"1,place,a1,alice,,1,1,GTC", Field::Side),
        (b"1,place,a1,alice,buy,,1,GTC", Field::Price),
        (b"1,place,a1,alice,buy,1,,GTC", Field::Size),
        (b"1,place,a1,alice,buy,1,1,", Field::Tif),
    ];
    for (text, field) in cases {
        assert_eq!(
            rejected(text),
            (1, RejectReason::Invalid(field, ValueError::Empty))
        );
    }
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn keywords_are_case_sensitive() {
    assert_eq!(
        rejected(b"1,PLACE,a1,alice,buy,1,1,GTC"),
        (1, RejectReason::UnknownCommand)
    );
    assert_eq!(
        rejected(b"1,,a1,alice,buy,1,1,GTC"),
        (1, RejectReason::UnknownCommand)
    );
    assert_eq!(
        rejected(b"1,place,a1,alice,Buy,1,1,GTC"),
        (1, RejectReason::Invalid(Field::Side, ValueError::Syntax))
    );
    assert_eq!(
        rejected(b"1,place,a1,alice,buy,1,1,gtc"),
        (1, RejectReason::Invalid(Field::Tif, ValueError::Syntax))
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn several_defects_follow_the_documented_order() {
    let cases: [(&[u8], RejectReason); 6] = [
        (b"1,bogus,a1", RejectReason::FieldCount),
        (b"1,bogus,,,,,,,", RejectReason::FieldCount),
        (b"1,bogus,,,,,,", RejectReason::UnknownCommand),
        (
            b"1,place,,,BUY,0,0,FOK",
            RejectReason::Invalid(Field::Id, ValueError::Empty),
        ),
        (
            b"1,place,a1,alice,BUY,0,0,FOK",
            RejectReason::Invalid(Field::Side, ValueError::Syntax),
        ),
        (
            b"1,place,a1,alice,buy,1.001,0,FOK",
            RejectReason::Invalid(Field::Price, ValueError::OffTick),
        ),
    ];
    for (text, reason) in cases {
        assert_eq!(rejected(text), (1, reason), "{}", text.escape_ascii());
    }
    assert_eq!(
        rejected(b"1,cancel,,x,,,,"),
        (1, RejectReason::Invalid(Field::Id, ValueError::Empty))
    );
    assert_eq!(
        rejected(b"1,modify,a1,,,0,0,x"),
        (
            1,
            RejectReason::Invalid(Field::Price, ValueError::OutOfRange)
        )
    );
    assert_eq!(
        rejected(b"1,place,\"broken,,,,,,"),
        (1, RejectReason::MalformedCsv)
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn quoted_fields_decode_commas_and_doubled_quotes() {
    assert_eq!(place_id(br#"1,place,"A,1",o,buy,1,1,GTC"#), b"A,1");
    assert_eq!(place_id(br#"1,place,"A""1",o,buy,1,1,GTC"#), b"A\"1");
    assert_eq!(place_id(br#"1,place,"a1",o,buy,1,1,GTC"#), b"a1");
    assert_eq!(place_id(br#"1,place,"""",o,buy,1,1,GTC"#), b"\"");
    // Backslash is an ordinary byte, not an escape.
    assert_eq!(place_id(br"1,place,a\b,o,buy,1,1,GTC"), b"a\\b");
    assert_eq!(place_id(br#"1,place,"a\b",o,buy,1,1,GTC"#), b"a\\b");
    // Quoting works for every field, including numbers and keywords.
    let (seq, command) = valid(br#""4","place","a1","o","buy","1.00","1","GTC""#);
    assert_eq!(seq, 4);
    assert!(matches!(command, Command::Place(_)));
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn identifiers_decode_losslessly_with_edge_bytes() {
    let text = b"1,place,\" a\0\t\x7f\\,\"\"b \",\x01o\x1f,buy,1,1,GTC";
    let Command::Place(place) = valid(text).1 else {
        panic!("not a place")
    };
    assert_eq!(place.id.as_bytes(), b" a\0\t\x7f\\,\"b ");
    assert_eq!(place.owner.as_bytes(), b"\x01o\x1f");
    let max = format!("1,place,{0},{0},buy,1,1,GTC", "x".repeat(32));
    let Command::Place(place) = valid(max.as_bytes()).1 else {
        panic!("not a place")
    };
    assert_eq!(place.id.as_bytes().len(), 32);
    assert_eq!(place.owner.as_bytes().len(), 32);
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn identifier_bounds_are_reported_per_field() {
    let long = "x".repeat(33);
    let long_id = format!("1,place,{long},o,buy,1,1,GTC");
    let long_owner = format!("1,place,a,{long},buy,1,1,GTC");
    assert_eq!(
        rejected(long_id.as_bytes()),
        (1, RejectReason::Invalid(Field::Id, ValueError::TooLong))
    );
    assert_eq!(
        rejected(long_owner.as_bytes()),
        (1, RejectReason::Invalid(Field::Owner, ValueError::TooLong))
    );
    // A quoted field's length counts decoded bytes: 31 bytes plus `""` is 32.
    let quoted = format!("1,place,\"{}\"\"\",o,buy,1,1,GTC", "x".repeat(31));
    assert_eq!(place_id(quoted.as_bytes()).len(), 32);
    assert_eq!(
        rejected("1,place,é,o,buy,1,1,GTC".as_bytes()),
        (1, RejectReason::Invalid(Field::Id, ValueError::NonAscii))
    );
    assert_eq!(
        rejected(b"1,cancel,\xff,,,,,"),
        (1, RejectReason::Invalid(Field::Id, ValueError::NonAscii))
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn malformed_quotes_reject_with_the_recovered_seq() {
    let cases: [&[u8]; 6] = [
        b"1,place,\"broken",
        b"1,place,a\"1,alice,buy,1,1,GTC",
        b"1,place,\"a1\"x,alice,buy,1,1,GTC",
        b"1,place,\"a\\\"1\",alice,buy,1,1,GTC",
        b"1,place,a1,alice,buy,1,1,GTC\"",
        b"1,place,\"a\r1\",alice,buy,1,1,GTC",
    ];
    for text in cases {
        assert_eq!(
            rejected(text),
            (1, RejectReason::MalformedCsv),
            "{}",
            text.escape_ascii()
        );
    }
    // A stray CR outside quotes is malformed too.
    assert_eq!(
        rejected(b"1,cancel,a\r1,,,,,"),
        (1, RejectReason::MalformedCsv)
    );
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn seq_is_recovered_with_the_same_quoting_rules() {
    assert_eq!(valid(b"\"1\",cancel,a1,,,,,").0, 1);
    assert_eq!(valid(b"0001,cancel,a1,,,,,").0, 1);
    assert_eq!(valid(b"0,cancel,a1,,,,,").0, 0);
    assert_eq!(valid(b"18446744073709551615,cancel,a1,,,,,").0, u64::MAX);
    assert_eq!(rejected(b"18446744073709551615,cancel,a1").0, u64::MAX);
    let long = format!("{}5,cancel,a1,,,,,", "0".repeat(1000));
    assert_eq!(valid(long.as_bytes()).0, 5);
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn unrecoverable_seq_is_terminal() {
    let cases: [(&[u8], SeqError); 15] = [
        (b"", SeqError::Blank),
        (b"\"1,place,a1,alice,buy,1,1,GTC", SeqError::Malformed),
        (b"\"1\"2,cancel,a1,,,,,", SeqError::Malformed),
        (b"1\",cancel,a1,,,,,", SeqError::Malformed),
        (b"1\r,cancel,a1,,,,,", SeqError::Malformed),
        (b"\r", SeqError::Malformed),
        (b",cancel,a1,,,,,", SeqError::Empty),
        (b"\"\",cancel,a1,,,,,", SeqError::Empty),
        (b",,,,,,,", SeqError::Empty),
        (b"x,place,\"broken", SeqError::Syntax),
        (b"-1,cancel,a1,,,,,", SeqError::Syntax),
        (b" 1,cancel,a1,,,,,", SeqError::Syntax),
        (b"1.0,cancel,a1,,,,,", SeqError::Syntax),
        (b"18446744073709551616,cancel,a1,,,,,", SeqError::Overflow),
        (
            b"99999999999999999999999999999999,cancel",
            SeqError::Overflow,
        ),
    ];
    for (text, error) in cases {
        assert_eq!(line(text), Err(error), "{}", text.escape_ascii());
    }
    assert_eq!(line("１,cancel,a1,,,,,".as_bytes()), Err(SeqError::Syntax));
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn an_unclosed_quote_never_swallows_the_next_line() {
    let input = format!("{HEADER}1,place,\"a1,alice,buy,1,1,GTC\n2,cancel,a1,,,,,\n");
    assert_eq!(
        all(input.as_bytes()),
        vec![
            Ok(ParsedLine {
                seq: 1,
                result: Err(RejectReason::MalformedCsv)
            }),
            Ok(ParsedLine {
                seq: 2,
                result: Ok(Command::Cancel(Cancel { id: id(b"a1") }))
            }),
        ]
    );
    // A quoted field cannot continue on the next physical line.
    let input = format!("{HEADER}1,place,\"a\n1\",alice,buy,1,1,GTC\n");
    assert_eq!(
        all(input.as_bytes()),
        vec![
            Ok(ParsedLine {
                seq: 1,
                result: Err(RejectReason::MalformedCsv)
            }),
            Err(SeqError::Malformed),
        ]
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn lf_crlf_and_missing_final_newline_agree() {
    let body = [
        "1,place,a1,alice,buy,100.25,5,GTC",
        "2,cancel,a1,,,,,",
        "3,modify,b,,,1,1,",
    ];
    let lf = format!("{HEADER}{}\n", body.join("\n"));
    let crlf = lf.replace('\n', "\r\n");
    let bare = lf.trim_end_matches('\n').to_owned();
    let mixed = format!(
        "{}{}\r\n{}\n{}",
        HEADER.replace('\n', "\r\n"),
        body[0],
        body[1],
        body[2]
    );
    let expected = all(lf.as_bytes());
    assert_eq!(expected.len(), 3);
    assert!(
        expected
            .iter()
            .all(|line| matches!(line, Ok(ParsedLine { result: Ok(_), .. })))
    );
    assert_eq!(all(crlf.as_bytes()), expected);
    assert_eq!(all(bare.as_bytes()), expected);
    assert_eq!(all(mixed.as_bytes()), expected);
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn physical_lines_strip_one_terminator() {
    let lines: Vec<&[u8]> = PhysicalLines::new(b"a\r\nb\n\r\n\nc\r").collect();
    assert_eq!(lines, [&b"a"[..], b"b", b"", b"", b"c\r"]);
    assert_eq!(PhysicalLines::new(b"").count(), 0);
    assert_eq!(PhysicalLines::new(b"\n").collect::<Vec<_>>(), [&b""[..]]);
    assert_eq!(
        PhysicalLines::new(b"a\r\r\n").collect::<Vec<_>>(),
        [&b"a\r"[..]]
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn header_must_match_exactly() {
    assert_eq!(commands(b"").err(), Some(HeaderError::Empty));
    let bad: [&[u8]; 9] = [
        b"\n",
        b"\xEF\xBB\xBFseq,cmd,id,owner,side,price,size,tif\n",
        b"seq,cmd,id,owner,side,price,tif,size\n",
        b"seq, cmd,id,owner,side,price,size,tif\n",
        b"seq,cmd,id,owner,side,price,size,tif \n",
        b"SEQ,CMD,ID,OWNER,SIDE,PRICE,SIZE,TIF\n",
        b"\"seq\",cmd,id,owner,side,price,size,tif\n",
        b"seq,cmd,id,owner,side,price,size,tif,\n",
        b"seq,cmd,id,owner,side,price,size,tif\r",
    ];
    for input in bad {
        assert_eq!(
            commands(input).err(),
            Some(HeaderError::Mismatch),
            "{}",
            input.escape_ascii()
        );
    }
    assert_eq!(
        parse_header(b"seq,cmd,id,owner,side,price,size,tif"),
        Ok(())
    );
    assert_eq!(all(b"seq,cmd,id,owner,side,price,size,tif").len(), 0);
    assert_eq!(all(HEADER.as_bytes()).len(), 0);
    assert_eq!(all(b"seq,cmd,id,owner,side,price,size,tif\r\n").len(), 0);
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn blank_command_lines_are_terminal() {
    let input = format!("{HEADER}1,cancel,a1,,,,,\n\n2,cancel,a1,,,,,\r\n\r\n");
    let lines = all(input.as_bytes());
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1], Err(SeqError::Blank));
    assert_eq!(lines[3], Err(SeqError::Blank));
    assert_eq!(
        all(format!("{HEADER}\n").as_bytes()),
        vec![Err(SeqError::Blank)]
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn errors_display_fixed_text_without_input() {
    assert_eq!(
        RejectReason::Invalid(Field::Price, ValueError::OffTick).to_string(),
        "invalid price: price is not a whole number of cents"
    );
    assert_eq!(
        RejectReason::Unexpected(Field::Owner).to_string(),
        "unexpected owner"
    );
    assert_eq!(
        SeqError::Overflow.to_string(),
        "sequence number is too large"
    );
    assert_eq!(HeaderError::Mismatch.to_string(), "header does not match");
}
