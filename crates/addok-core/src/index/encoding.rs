//! How documents and house numbers are encoded in the index's sections, for
//! the builder and the reader alike. Every value reads back as written.

use crate::document::{Document, Number, Text};

/// A document's text fields, in their order in `DocTexts`.
pub(super) fn texts(doc: &Document) -> [&Text; 8] {
    [
        &doc.name,
        &doc.postcode,
        &doc.citycode,
        &doc.city,
        &doc.context,
        &doc.oldcitycode,
        &doc.oldcity,
        &doc.district,
    ]
}

pub(super) fn texts_mut(doc: &mut Document) -> [&mut Text; 8] {
    [
        &mut doc.name,
        &mut doc.postcode,
        &mut doc.citycode,
        &mut doc.city,
        &mut doc.context,
        &mut doc.oldcitycode,
        &mut doc.oldcity,
        &mut doc.district,
    ]
}

/// Places in `texts_mut`.
pub(super) const NAME: usize = 0;
pub(super) const POSTCODE: usize = 1;
pub(super) const CITY: usize = 3;
/// `importance`'s place in `numbers_mut`.
pub(super) const IMPORTANCE: usize = 4;

/// A document's numbers, in their order in `DocNumbers`.
pub(super) fn numbers(doc: &Document) -> [Option<Number>; 6] {
    [
        doc.x,
        doc.y,
        doc.lon,
        doc.lat,
        doc.importance,
        doc.population,
    ]
}

pub(super) fn numbers_mut(doc: &mut Document) -> [&mut Option<Number>; 6] {
    [
        &mut doc.x,
        &mut doc.y,
        &mut doc.lon,
        &mut doc.lat,
        &mut doc.importance,
        &mut doc.population,
    ]
}

/// A text field's tag, 2 bits per field in `DocTextTags`; its value in
/// `DocTexts` is a string for `ONE`, a list for `MANY`.
pub(super) const ABSENT: u16 = 0;
pub(super) const NULL: u16 = 1;
pub(super) const ONE: u16 = 2;
pub(super) const MANY: u16 = 3;

/// `DocFlags`: per number, bit 2i if it is present, 2i + 1 if it is an
/// integer; then 2 bits for the banId at `BAN_ID`.
pub(super) const BAN_ID: u16 = 12;
pub(super) const BAN_ID_UUID: u16 = 1;
/// Written apart, in `DocBanIdTexts`.
pub(super) const BAN_ID_TEXT: u16 = 2;

/// `HnFlags`: bit i if coordinate i is an integer, then these.
pub(super) const HN_BAN_ID_NULL: u8 = 1 << 4;
/// The id is written apart, in `HnIdExceptions`.
pub(super) const HN_ID_EXCEPTION: u8 = 1 << 5;
/// The coordinates are written apart, in `HnCoordinateExceptions`.
pub(super) const HN_COORDINATES_EXCEPTION: u8 = 1 << 6;
/// The banId is written apart, in `HnBanIdTexts`.
pub(super) const HN_BAN_ID_TEXT: u8 = 1 << 7;

/// House-number coordinates as fixed point: x and y in centimetres, lon and
/// lat in millionths of a degree. Exact for every coordinate of the
/// 2026-10-02 NDJSON; a house number with one that is not is written apart.
pub(super) const SCALES: [f64; 4] = [1e2, 1e2, 1e6, 1e6];

/// A coordinate as fixed point, if that reads back as the same double.
pub(super) fn fixed(value: f64, scale: f64) -> Option<i32> {
    let units = (value * scale).round();
    (units.abs() < 2f64.powi(31) && units / scale == value).then_some(units as i32)
}

/// A number's value, if it reads back as written: an integer must survive
/// its trip through a double.
pub(super) fn value(number: Number) -> f64 {
    if let Number::Integer(integer) = number {
        assert_eq!(
            integer as f64 as i64, integer,
            "an integer a double cannot hold"
        );
    }
    number.value()
}

/// A house number's id as the BAN derives it from its street's and its
/// number: `<street id, lowercase>_<number, 5 digits>[_<suffix>]`. 99.9% of
/// the 2026-10-02 NDJSON's are; the others are written apart.
pub(super) fn derived_id(street: &str, number: &str) -> Option<String> {
    let digits = number.len()
        - number
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .len();
    let value: u64 = number[..digits].parse().ok()?;
    let street = street.to_lowercase();
    let suffix = number[digits..].trim().to_lowercase().replace(' ', "_");
    Some(match suffix.is_empty() {
        true => format!("{street}_{value:05}"),
        false => format!("{street}_{value:05}_{suffix}"),
    })
}

/// A canonical UUID's 16 bytes: lowercase hexadecimal, grouped 8-4-4-4-12.
pub(super) fn uuid_bytes(text: &str) -> Option<[u8; 16]> {
    let digits: String = text.chars().filter(|&c| c != '-').collect();
    let value = u128::from_str_radix(&digits, 16).ok()?;
    let bytes = value.to_be_bytes();
    (uuid_text(&bytes) == text).then_some(bytes)
}

pub(super) fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex = format!("{:032x}", u128::from_be_bytes(*bytes));
    let groups = [
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..],
    ];
    groups.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fields_by_their_place() {
        let mut doc = Document {
            name: Text::One("name".into()),
            postcode: Text::One("postcode".into()),
            city: Text::One("city".into()),
            importance: Some(Number::Float(0.5)),
            ..Document::default()
        };
        let one = |value: &str| Text::One(value.into());
        assert_eq!(*texts(&doc)[NAME], one("name"));
        assert_eq!(*texts(&doc)[POSTCODE], one("postcode"));
        assert_eq!(*texts(&doc)[CITY], one("city"));
        assert_eq!(numbers(&doc)[IMPORTANCE], Some(Number::Float(0.5)));
        assert_eq!(*texts_mut(&mut doc)[NAME], one("name"));
        assert_eq!(*texts_mut(&mut doc)[POSTCODE], one("postcode"));
        assert_eq!(*texts_mut(&mut doc)[CITY], one("city"));
        assert_eq!(*numbers_mut(&mut doc)[IMPORTANCE], Some(Number::Float(0.5)));
    }

    #[test]
    fn writes_coordinates_as_fixed_point_when_exact() {
        assert_eq!(fixed(5.423167, 1e6), Some(5_423_167));
        assert_eq!(fixed(887482.78, 1e2), Some(88_748_278));
        assert_eq!(fixed(5.4231675, 1e6), None);
        assert_eq!(fixed(3e7, 1e2), None);
        assert_eq!(f64::from(fixed(46.006992, 1e6).unwrap()) / 1e6, 46.006992);
    }

    #[test]
    fn derives_a_house_number_s_id_as_the_ban_does() {
        let id = |street, number| derived_id(street, number).unwrap();
        assert_eq!(id("01002_0110", "16"), "01002_0110_00016");
        assert_eq!(id("01001_0370", "38bis"), "01001_0370_00038_bis");
        assert_eq!(id("01001_A105", "2"), "01001_a105_00002");
        assert_eq!(id("01001_0370", "1 B"), "01001_0370_00001_b");
        assert_eq!(derived_id("01001_0370", "bis"), None);
    }

    #[test]
    fn keeps_a_canonical_uuid_in_16_bytes() {
        let uuid = "f6f6b2c3-1d5e-4b9a-8f7e-0123456789ab";
        assert_eq!(uuid_text(&uuid_bytes(uuid).unwrap()), uuid);
        assert_eq!(uuid_bytes("F6F6B2C3-1D5E-4B9A-8F7E-0123456789AB"), None);
        assert_eq!(uuid_bytes("not-a-uuid"), None);
    }
}
