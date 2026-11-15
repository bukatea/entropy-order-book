use orderbook::domain::{OrderId, Owner, Price, Quantity, Side, TimeInForce, ValueError};

fn price(text: &str) -> Result<u32, ValueError> {
    Price::parse(text.as_bytes()).map(Price::cents)
}

fn size(text: &str) -> Result<u32, ValueError> {
    Quantity::parse(text.as_bytes()).map(Quantity::lots)
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_bounds_are_inclusive_cents() {
    assert_eq!(price("0"), Err(ValueError::OutOfRange));
    assert_eq!(price("0.00"), Err(ValueError::OutOfRange));
    assert_eq!(price("0.01"), Ok(1));
    assert_eq!(price("1000000.00"), Ok(100_000_000));
    assert_eq!(price("1000000"), Ok(100_000_000));
    assert_eq!(price("1000000.01"), Err(ValueError::OutOfRange));
    assert_eq!(price("1000001"), Err(ValueError::OutOfRange));
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_accepts_zero_digits_beyond_cents_and_rejects_off_tick() {
    assert_eq!(price("100.25"), Ok(10025));
    assert_eq!(price("100.250"), Ok(10025));
    assert_eq!(price("100.2"), Ok(10020));
    assert_eq!(price("100.251"), Err(ValueError::OffTick));
    assert_eq!(price("0.001"), Err(ValueError::OffTick));
    let long_zeros = format!("100.25{}", "0".repeat(1000));
    assert_eq!(price(&long_zeros), Ok(10025));
    let late_digit = format!("100.25{}1", "0".repeat(1000));
    assert_eq!(price(&late_digit), Err(ValueError::OffTick));
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_rejects_non_decimal_syntax() {
    assert_eq!(price(""), Err(ValueError::Empty));
    for text in [
        "1.", ".01", ".", "+1", "-1", "1e2", " 1", "1 ", "1,000", "1_000", "1.2.3", "0x10", "１",
        "1.0é",
    ] {
        assert_eq!(price(text), Err(ValueError::Syntax), "{text:?}");
    }
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_handles_leading_zeros_and_huge_digit_strings() {
    assert_eq!(price("000100.25"), Ok(10025));
    assert_eq!(price(&format!("{}1", "0".repeat(1000))), Ok(100));
    assert_eq!(price(&"9".repeat(1000)), Err(ValueError::OutOfRange));
    assert_eq!(price("4294967296"), Err(ValueError::OutOfRange));
    assert_eq!(
        price("18446744073709551616.00"),
        Err(ValueError::OutOfRange)
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_fault_order_is_syntax_then_tick_then_range() {
    assert_eq!(price("99999999999.001"), Err(ValueError::OffTick));
    assert_eq!(price("99999999999.0x1"), Err(ValueError::Syntax));
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn price_from_cents_enforces_bounds() {
    assert_eq!(Price::from_cents(0), None);
    assert_eq!(Price::from_cents(1).map(Price::cents), Some(1));
    assert_eq!(
        Price::from_cents(100_000_000).map(Price::cents),
        Some(100_000_000)
    );
    assert_eq!(Price::from_cents(100_000_001), None);
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn size_bounds_and_syntax() {
    assert_eq!(size(""), Err(ValueError::Empty));
    assert_eq!(size("0"), Err(ValueError::OutOfRange));
    assert_eq!(size("1"), Ok(1));
    assert_eq!(size("1000000000"), Ok(1_000_000_000));
    assert_eq!(size("1000000001"), Err(ValueError::OutOfRange));
    assert_eq!(size("4294967296"), Err(ValueError::OutOfRange));
    assert_eq!(size(&"9".repeat(1000)), Err(ValueError::OutOfRange));
    assert_eq!(size("0007"), Ok(7));
    for text in ["-1", "+1", "5.0", "1e9", " 5", "５"] {
        assert_eq!(size(text), Err(ValueError::Syntax), "{text:?}");
    }
    assert_eq!(Quantity::new(0), None);
    assert_eq!(Quantity::new(1_000_000_001), None);
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn identifiers_are_one_to_32_ascii_bytes() {
    assert_eq!(OrderId::new(b""), Err(ValueError::Empty));
    assert_eq!(Owner::new(b""), Err(ValueError::Empty));
    let max = [b'x'; 32];
    assert_eq!(OrderId::new(&max).unwrap().as_bytes(), &max);
    assert_eq!(Owner::new(&max).unwrap().as_bytes(), &max);
    assert_eq!(OrderId::new(&[b'x'; 33]), Err(ValueError::TooLong));
    assert_eq!(Owner::new(&[b'x'; 33]), Err(ValueError::TooLong));
    assert_eq!(OrderId::new("é".as_bytes()), Err(ValueError::NonAscii));
    assert_eq!(OrderId::new(&[0xFF]), Err(ValueError::NonAscii));
    // Non-ASCII is reported before length.
    assert_eq!(
        OrderId::new("é".repeat(20).as_bytes()),
        Err(ValueError::NonAscii)
    );
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn identifiers_keep_every_ascii_byte_unchanged() {
    for byte in 0u8..=0x7F {
        let id = OrderId::new(&[byte]).unwrap();
        assert_eq!(id.as_bytes(), &[byte]);
    }
    let edge = b" a\0\t\x7f\\,\" ";
    assert_eq!(Owner::new(edge).unwrap().as_bytes(), edge);
    assert_eq!(OrderId::new(b"A1").unwrap().as_str(), "A1");
}

// exkg-verifies: fea-ky6m7m#ac-input
#[test]
fn keywords_are_case_sensitive() {
    assert_eq!(Side::parse(b"buy"), Ok(Side::Buy));
    assert_eq!(Side::parse(b"sell"), Ok(Side::Sell));
    assert_eq!(Side::parse(b"BUY"), Err(ValueError::Syntax));
    assert_eq!(Side::parse(b""), Err(ValueError::Empty));
    assert_eq!(TimeInForce::parse(b"GTC"), Ok(TimeInForce::Gtc));
    assert_eq!(TimeInForce::parse(b"IOC"), Ok(TimeInForce::Ioc));
    assert_eq!(TimeInForce::parse(b"POST_ONLY"), Ok(TimeInForce::PostOnly));
    assert_eq!(TimeInForce::parse(b"gtc"), Err(ValueError::Syntax));
    assert_eq!(TimeInForce::parse(b"FOK"), Err(ValueError::Syntax));
    assert_eq!(TimeInForce::parse(b""), Err(ValueError::Empty));
}
