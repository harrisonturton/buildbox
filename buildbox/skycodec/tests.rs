use skycodec::{from_bytes, to_bytes, Decode, DecodeError, Decoder, Encode, Encoder, Fingerprint, PathId, Symbol};

fn round_trip<T: Encode + Decode + PartialEq + std::fmt::Debug>(value: T) {
    let bytes = to_bytes(&value);
    assert_eq!(from_bytes::<T>(&bytes).unwrap(), value);
}

#[test]
fn integers_round_trip() {
    for value in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
        round_trip(value);
    }
    for value in [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX] {
        round_trip(value);
    }
}

#[test]
fn small_integers_take_one_byte() {
    assert_eq!(to_bytes(&127u64).len(), 1);
    assert_eq!(to_bytes(&-64i64).len(), 1);
    assert_eq!(to_bytes(&128u64).len(), 2);
}

#[test]
fn compound_values_round_trip() {
    round_trip(String::from("hello"));
    round_trip(vec![Some(1u32), None, Some(3)]);
    round_trip((Symbol::intern("rules_rust"), PathId::parse("foo/bar").unwrap(), true));
    round_trip(Fingerprint::of(b"data"));
}

#[test]
fn sorted_u32s_round_trip_as_gaps() {
    let values = [3u32, 4, 10, 1000, 1001];
    let mut encoder = Encoder::new();
    encoder.sorted_u32s(&values);
    let bytes = encoder.into_bytes();
    assert_eq!(bytes.len(), 7);
    let mut decoder = Decoder::new(&bytes);
    assert_eq!(decoder.sorted_u32s().unwrap(), values);
}

#[test]
fn truncated_data_is_an_error() {
    let bytes = to_bytes(&String::from("hello"));
    let result = from_bytes::<String>(&bytes[..3]);
    assert!(matches!(result, Err(DecodeError::UnexpectedEnd(_))));
}

#[test]
fn trailing_data_is_an_error() {
    let mut bytes = to_bytes(&5u32);
    bytes.push(0);
    assert_eq!(from_bytes::<u32>(&bytes), Err(DecodeError::TrailingBytes(1)));
}

#[test]
fn huge_lengths_are_rejected_before_allocating() {
    let mut encoder = Encoder::new();
    encoder.u64(u32::MAX as u64);
    let bytes = encoder.into_bytes();
    assert!(matches!(from_bytes::<Vec<u8>>(&bytes), Err(DecodeError::Invalid { .. })));
}

#[test]
fn overlong_integers_are_rejected() {
    let bytes = [0xffu8; 11];
    assert!(matches!(from_bytes::<u64>(&bytes), Err(DecodeError::Overflow(0))));
}

#[test]
fn fingerprints_separate_parts() {
    let mut a = skycodec::Fingerprinter::new();
    a.bytes(b"ab").bytes(b"c");
    let mut b = skycodec::Fingerprinter::new();
    b.bytes(b"a").bytes(b"bc");
    assert_ne!(a.finish(), b.finish());
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct Named {
    name: Symbol,
    count: u32,
    tags: Vec<String>,
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct Tuple(u64, Option<bool>);

#[derive(Encode, Decode, Debug, PartialEq)]
enum Shape {
    Empty,
    Circle(u32),
    Rect { width: u32, height: u32 },
}

#[derive(Encode, Decode, Debug, PartialEq)]
struct Wrapper<T>(Vec<T>);

#[test]
fn derived_codecs_round_trip() {
    round_trip(Named {
        name: Symbol::intern("x"),
        count: 3,
        tags: vec!["a".into()],
    });
    round_trip(Tuple(9, Some(true)));
    for shape in [Shape::Empty, Shape::Circle(4), Shape::Rect { width: 1, height: 2 }] {
        round_trip(shape);
    }
    round_trip(Wrapper(vec![Shape::Empty]));
}

#[test]
fn unknown_variants_are_errors() {
    let bytes = to_bytes(&7u32);
    assert!(matches!(from_bytes::<Shape>(&bytes), Err(DecodeError::Invalid { .. })));
}
