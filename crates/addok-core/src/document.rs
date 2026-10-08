//! A BAN document as the national NDJSON writes it, every field
//! kept as written so that results print as addok prints them: a number as
//! an integer or a float, a text field as absent, null, one string or a list.
//! The schema is the BAN's alone: a field it does not know is an
//! error, not a silent loss.

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// A BAN document: a municipality, a street or a locality.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// The BAN's `cle_interop`.
    pub id: String,
    #[serde(rename = "banId")]
    pub ban_id: Option<String>,
    /// The NDJSON's `type`: municipality, street or locality.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: Text,
    #[serde(default)]
    pub postcode: Text,
    #[serde(default)]
    pub citycode: Text,
    #[serde(default)]
    pub city: Text,
    #[serde(default)]
    pub context: Text,
    #[serde(default)]
    pub oldcitycode: Text,
    #[serde(default)]
    pub oldcity: Text,
    #[serde(default)]
    pub district: Text,
    pub x: Option<Number>,
    pub y: Option<Number>,
    pub lon: Option<Number>,
    pub lat: Option<Number>,
    pub importance: Option<Number>,
    pub population: Option<Number>,
    /// A street's house numbers, in the NDJSON's order; none for a
    /// municipality or a locality.
    #[serde(default, deserialize_with = "housenumbers")]
    pub housenumbers: Option<Vec<HouseNumber>>,
}

impl Document {
    /// One line of the BAN's NDJSON.
    pub fn from_ndjson(line: &str) -> Result<Document, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// A house number, under its street.
#[derive(Debug, Clone, PartialEq)]
pub struct HouseNumber {
    /// The number as written ("1", "1 bis"), the key of its entry in the
    /// NDJSON: addok's `raw`.
    pub number: String,
    pub id: String,
    pub ban_id: Option<String>,
    pub x: Number,
    pub y: Number,
    pub lon: Number,
    pub lat: Number,
}

/// A house number's entry in the NDJSON, its number being the key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HouseNumberEntry {
    id: String,
    #[serde(rename = "banId")]
    ban_id: Option<String>,
    x: Number,
    y: Number,
    lon: Number,
    lat: Number,
}

/// A number as the NDJSON writes it: addok prints `887247` and `887247.0`
/// apart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    pub fn value(self) -> f64 {
        match self {
            Number::Integer(value) => value as f64,
            Number::Float(value) => value,
        }
    }
}

/// A text field as the NDJSON writes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Text {
    #[default]
    Absent,
    Null,
    One(String),
    Many(Vec<String>),
}

/// A text field, borrowed.
#[derive(Debug, Clone, PartialEq)]
pub enum TextRef<'a> {
    Absent,
    Null,
    One(&'a str),
    Many(Vec<&'a str>),
}

impl TextRef<'_> {
    pub fn to_owned(&self) -> Text {
        match self {
            TextRef::Absent => Text::Absent,
            TextRef::Null => Text::Null,
            TextRef::One(value) => Text::One((*value).to_owned()),
            TextRef::Many(values) => Text::Many(values.iter().map(|&value| value.to_owned()).collect()),
        }
    }
}

impl Text {
    pub fn borrowed(&self) -> TextRef<'_> {
        match self {
            Text::Absent => TextRef::Absent,
            Text::Null => TextRef::Null,
            Text::One(value) => TextRef::One(value),
            Text::Many(values) => TextRef::Many(values.iter().map(String::as_str).collect()),
        }
    }

    /// The values addok reads from the field: none if it is absent, null, an
    /// empty string or an empty list; each item of a list.
    pub fn values(&self) -> &[String] {
        match self {
            Text::One(value) if !value.is_empty() => std::slice::from_ref(value),
            Text::Many(values) => values,
            _ => &[],
        }
    }
}

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Number, D::Error> {
        struct NumberVisitor;
        impl Visitor<'_> for NumberVisitor {
            type Value = Number;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a number")
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Number, E> {
                Ok(Number::Integer(value))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Number, E> {
                i64::try_from(value).map(Number::Integer).map_err(E::custom)
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Number, E> {
                Ok(Number::Float(value))
            }
        }
        deserializer.deserialize_any(NumberVisitor)
    }
}

impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Text, D::Error> {
        struct TextVisitor;
        impl<'de> Visitor<'de> for TextVisitor {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("null, a string or a list of strings")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Text, E> {
                Ok(Text::Null)
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Text, E> {
                Ok(Text::One(value.to_owned()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Text, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = items.next_element()? {
                    values.push(value);
                }
                Ok(Text::Many(values))
            }
        }
        deserializer.deserialize_any(TextVisitor)
    }
}

/// The `housenumbers` object, its entries kept in order.
fn housenumbers<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<HouseNumber>>, D::Error> {
    struct HouseNumbersVisitor;
    impl<'de> Visitor<'de> for HouseNumbersVisitor {
        type Value = Option<Vec<HouseNumber>>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an object of house numbers")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
            let mut numbers = Vec::new();
            while let Some((number, entry)) = entries.next_entry::<String, HouseNumberEntry>()? {
                numbers.push(HouseNumber {
                    number,
                    id: entry.id,
                    ban_id: entry.ban_id,
                    x: entry.x,
                    y: entry.y,
                    lon: entry.lon,
                    lat: entry.lat,
                });
            }
            Ok(Some(numbers))
        }
    }
    deserializer.deserialize_map(HouseNumbersVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_municipality_as_written() {
        let line = r#"{"id":"01002","banId":null,"type":"municipality","name":"L'Abergement-de-Varey","postcode":["01640"],"citycode":"01002","x":887247.06,"y":6548295.66,"lon":5.420189,"lat":46.008573,"population":270,"city":"L'Abergement-de-Varey","context":"01, Ain, Auvergne-Rhône-Alpes","importance":0.22554}"#;
        let doc = Document::from_ndjson(line).unwrap();
        assert_eq!(doc.ban_id, None);
        assert_eq!(doc.postcode, Text::Many(vec!["01640".into()]));
        assert_eq!(doc.citycode, Text::One("01002".into()));
        assert_eq!(doc.oldcity, Text::Absent);
        assert_eq!(doc.x, Some(Number::Float(887247.06)));
        assert_eq!(doc.population, Some(Number::Integer(270)));
        assert_eq!(doc.housenumbers, None);
    }

    #[test]
    fn keeps_house_numbers_in_order_and_integers_as_integers() {
        let line = r#"{"id":"01002_0110","banId":null,"name":"Montee de la Foret","postcode":"01640","citycode":["01002"],"oldcitycode":null,"lon":5.421924,"lat":46.007342,"x":887385.45,"y":6548163.14,"city":["L'Abergement-de-Varey"],"oldcity":null,"context":"01, Ain, Auvergne-Rhône-Alpes","type":"street","importance":0.3672,"housenumbers":{"22":{"id":"01002_0110_00022","banId":"f6f6b2c3-1d5e-4b9a-8f7e-0123456789ab","x":887475,"y":6548124.92,"lon":5.423076,"lat":46.006973},"16":{"id":"01002_0110_00016","banId":null,"x":887482.78,"y":6548127.24,"lon":5.423167,"lat":46.006992}}}"#;
        let doc = Document::from_ndjson(line).unwrap();
        assert_eq!(doc.oldcitycode, Text::Null);
        let numbers = doc.housenumbers.unwrap();
        assert_eq!(
            [numbers[0].number.as_str(), numbers[1].number.as_str()],
            ["22", "16"]
        );
        assert_eq!(numbers[0].x, Number::Integer(887475));
        assert_eq!(
            numbers[0].ban_id.as_deref(),
            Some("f6f6b2c3-1d5e-4b9a-8f7e-0123456789ab")
        );
    }

    #[test]
    fn refuses_a_field_it_does_not_know() {
        let line = r#"{"id":"1","banId":null,"type":"street","name":"x","lon":1,"lat":2,"importance":0.1,"new":1}"#;
        assert!(Document::from_ndjson(line).is_err());
    }

    #[test]
    fn reads_values_as_addok() {
        assert!(Text::Absent.values().is_empty());
        assert!(Text::Null.values().is_empty());
        assert!(Text::One(String::new()).values().is_empty());
        assert_eq!(Text::One("a".into()).values(), ["a"]);
        assert_eq!(
            Text::Many(vec!["a".into(), String::new()]).values(),
            ["a", ""]
        );
    }
}
