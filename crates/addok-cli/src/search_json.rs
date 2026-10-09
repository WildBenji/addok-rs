//! addok's `GET /search`: a query in, its results out as addok's GeoJSON
//! `FeatureCollection`. Parameters are read as falcon reads them: the last
//! of a repeated one wins, a blank one is kept, a filter's values split on
//! spaces; autocomplete is on unless turned off, as in addok. `lat`/`lon`
//! are refused: geohashes are not ported yet. The answer equals addok's as
//! JSON, not as text: the properties follow another order.

use addok_core::document::Number;
use addok_core::index::Index;
use addok_core::search::{Filters, Found, search_autocomplete, search_filtered};
use addok_core::text::{QUERY_MAX_LENGTH, fold};
use serde_json::{Map, Value, json};

use crate::search_csv::param_flag;

/// The BAN's `ATTRIBUTION` and `LICENCE`.
const ATTRIBUTION: &str = "BAN";
const LICENCE: &str = "ETALAB-2.0";

/// The BAN's `FILTERS`, in its order.
const FILTERS: [&str; 3] = ["type", "citycode", "postcode"];

/// The parameters addok reads a position from, which geohashes need.
const POSITION: [&str; 6] = ["lat", "latitude", "lon", "lng", "long", "longitude"];

/// A request refused, as falcon writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub status: u16,
    pub title: String,
    pub description: Option<String>,
}

impl Error {
    fn bad(title: &str, description: Option<String>) -> Error {
        Error { status: 400, title: title.to_owned(), description }
    }

    fn invalid(name: &str, why: &str) -> Error {
        Error::bad("Invalid parameter", Some(format!("The \"{name}\" parameter is invalid. {why}")))
    }

    pub fn body(&self) -> Value {
        match &self.description {
            Some(description) => json!({ "title": self.title, "description": description }),
            None => json!({ "title": self.title }),
        }
    }
}

/// A `/search` request, its parameters read.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'p> {
    pub query: &'p str,
    pub limit: usize,
    /// Whether the query's last word is taken as the start of one.
    pub autocomplete: bool,
    /// What search keeps.
    pub filters: Filters,
    /// The filters as the request names them, which the answer repeats.
    named: Vec<(&'static str, Vec<String>)>,
}

/// A `/search` request of these parameters, in order, read as addok reads
/// them, or refused as it refuses them; or as addok-rs refuses what it does
/// not port yet.
pub fn read(params: &[(String, String)]) -> Result<Request<'_>, Error> {
    let last = |name: &str| params.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let query = match last("q") {
        Some(query) if !query.is_empty() => query,
        _ => return Err(Error::bad("Missing parameter", Some("The \"q\" parameter is required.".into()))),
    };
    let limit = match last("limit").map(python_int) {
        None | Some(Some(0)) => 5,
        Some(Some(limit @ 1..=100)) => limit as usize,
        Some(Some(_)) => return Err(Error::invalid("limit", "out of range (1..100)")),
        Some(None) => return Err(Error::invalid("limit", "The value must be an integer.")),
    };
    let autocomplete = match last("autocomplete").map(param_flag) {
        None => true,
        Some(Some(autocomplete)) => autocomplete,
        Some(None) => {
            return Err(Error::invalid("autocomplete", "The value of the parameter must be \"true\" or \"false\"."));
        }
    };
    if let Some(name) = POSITION.iter().find(|name| last(name).is_some()) {
        return Err(Error::bad(&format!("Unsupported parameter \"{name}\""), None));
    }
    let named = filters(params);
    let values = |name: &str| {
        let values = named.iter().find(|(filter, _)| *filter == name).map(|(_, values)| values.clone());
        values.unwrap_or_default()
    };
    // A `type` given but split to nothing still turns house-number matching
    // off, as a blank one does: hand search a blank one.
    let kind = match values("type") {
        kind if kind.is_empty() && named.iter().any(|(filter, _)| *filter == "type") => vec![String::new()],
        kind => kind,
    };
    let filters = Filters {
        kind,
        citycode: values("citycode"),
        postcode: values("postcode"),
    };
    Ok(Request { query, limit, autocomplete, filters, named })
}

/// What addok answers to a `/search` request of these parameters, in order.
pub fn search_json<B: AsRef<[u8]>>(index: &Index<B>, params: &[(String, String)]) -> Result<Value, Error> {
    let request = read(params)?;
    let search = if request.autocomplete { search_autocomplete } else { search_filtered };
    let found = search(index, request.query, request.limit, &request.filters).map_err(|_| Error {
        status: 413,
        title: format!("Query too long, {} chars, limit is {QUERY_MAX_LENGTH}", fold(request.query).chars().count()),
        description: None,
    })?;
    let mut collection = Map::new();
    collection.insert("type".into(), "FeatureCollection".into());
    collection.insert("version".into(), "draft".into());
    collection.insert("features".into(), found.iter().map(feature).collect());
    collection.insert("attribution".into(), ATTRIBUTION.into());
    collection.insert("licence".into(), LICENCE.into());
    collection.insert("query".into(), request.query.into());
    if !request.named.is_empty() {
        let named = request.named.into_iter().map(|(name, values)| (name.to_owned(), values.into()));
        collection.insert("filters".into(), Value::Object(named.collect()));
    }
    collection.insert("limit".into(), request.limit.into());
    Ok(Value::Object(collection))
}

/// Python's `int()` of a string: blanks around, a sign, digits that single
/// underscores may group. Beyond an `i64`, the furthest one of its sign.
fn python_int(value: &str) -> Option<i64> {
    let value = value.trim();
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    let grouped = digits.split('_').all(|group| !group.is_empty() && group.bytes().all(|b| b.is_ascii_digit()));
    if !grouped {
        return None;
    }
    let furthest = if value.starts_with('-') { i64::MIN } else { i64::MAX };
    Some(value.replace('_', "").parse().unwrap_or(furthest))
}

/// addok's `match_filters`: the values of each filter the request names, in
/// the BAN's order, a value holding a space split on spaces and its blanks
/// dropped, any other kept trimmed.
fn filters(params: &[(String, String)]) -> Vec<(&'static str, Vec<String>)> {
    let mut named = Vec::new();
    for name in FILTERS {
        let given: Vec<&str> = params.iter().filter(|(key, _)| key == name).map(|(_, value)| value.as_str()).collect();
        if given.is_empty() {
            continue;
        }
        let mut values = Vec::new();
        for value in given {
            match value.contains(' ') {
                true => values.extend(value.split(' ').map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)),
                false => values.push(value.trim().to_owned()),
            }
        }
        named.push((name, values));
    }
    named
}

/// addok's `geojson` formatter: a result as a GeoJSON `Feature`, its
/// document's fields as properties but those Python reads as false, a
/// matched house number's own over its street's.
fn feature(found: &Found) -> Value {
    let doc = &found.document;
    let number = found.housenumber.as_ref();
    let mut properties = Map::new();
    properties.insert("label".into(), found.label().into());
    properties.insert("score".into(), found.score.into());
    let mut text = |name: &str, value: Option<&str>| {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            properties.insert(name.into(), value.into());
        }
    };
    text("housenumber", number.map(|number| number.number.as_str()));
    text("id", Some(found.id()));
    text("banId", number.map_or(doc.ban_id.as_deref(), |number| number.ban_id.as_deref()));
    text("type", Some(found.kind()));
    for (name, value) in [
        ("name", &doc.name),
        ("postcode", &doc.postcode),
        ("citycode", &doc.citycode),
        ("oldcitycode", &doc.oldcitycode),
        ("city", &doc.city),
        ("oldcity", &doc.oldcity),
        ("district", &doc.district),
        ("context", &doc.context),
    ] {
        text(name, value.first());
    }
    let (x, y) = match number {
        Some(number) => (Some(number.x), Some(number.y)),
        None => (doc.x, doc.y),
    };
    for (name, value) in [("x", x), ("y", y), ("importance", doc.importance), ("population", doc.population)] {
        if let Some(value) = value.filter(|value| value.value() != 0.0) {
            properties.insert(name.into(), number_value(value));
        }
    }
    // The document's own type names its name; the house number then
    // prefixes it.
    if !doc.kind.is_empty() && !properties.contains_key(&doc.kind) {
        let name = properties.get("name").cloned().unwrap_or(Value::Null);
        properties.insert(doc.kind.clone(), name);
    }
    if let Some(number) = number {
        let name = doc.name.first().filter(|name| !name.is_empty()).unwrap_or("None");
        properties.insert("name".into(), format!("{} {name}", number.number).into());
    }
    let (lon, lat) = match number {
        Some(number) => (number.lon, number.lat),
        None => (
            doc.lon.expect("a BAN document has a position"),
            doc.lat.expect("a BAN document has a position"),
        ),
    };
    json!({
        "type": "Feature",
        "geometry": { "type": "Point", "coordinates": [lon.value(), lat.value()] },
        "properties": properties,
    })
}

fn number_value(number: Number) -> Value {
    match number {
        Number::Integer(value) => value.into(),
        Number::Float(value) => value.into(),
    }
}

#[cfg(test)]
mod tests {
    use addok_core::index::{AlignedBytes, write};

    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|&(key, value)| (key.to_owned(), value.to_owned())).collect()
    }

    #[test]
    fn reads_limits_as_python() {
        let cases = [
            ("5", Some(5)),
            (" 2 ", Some(2)),
            ("+2", Some(2)),
            ("-1", Some(-1)),
            ("1_0", Some(10)),
            ("99999999999999999999", Some(i64::MAX)),
            ("", None),
            ("2.0", None),
            ("1__0", None),
            ("_1", None),
            ("abc", None),
        ];
        for (value, python) in cases {
            assert_eq!(python_int(value), python, "{value:?}");
        }
    }

    #[test]
    fn reads_filters_as_falcon_hands_them_to_addok() {
        let given = params(&[
            ("postcode", "75001 75002"),
            ("type", ""),
            ("TYPE", "street"),
            ("postcode", " "),
            ("postcode", " 75003 "),
            ("type", "street,locality"),
        ]);
        let expected = vec![
            ("type", vec!["".to_owned(), "street,locality".to_owned()]),
            ("postcode", ["75001", "75002", "75003"].map(String::from).to_vec()),
        ];
        assert_eq!(filters(&given), expected);
    }

    #[test]
    fn autocompletes_unless_told_not_to_as_addok() {
        let autocomplete = |pairs: &[(&str, &str)]| read(&params(pairs)).unwrap().autocomplete;
        assert!(autocomplete(&[("q", "paris")]));
        assert!(autocomplete(&[("q", "paris"), ("autocomplete", "")]));
        assert!(autocomplete(&[("q", "paris"), ("autocomplete", "1")]));
        assert!(!autocomplete(&[("q", "paris"), ("autocomplete", "0")]));
        assert!(!autocomplete(&[("q", "paris"), ("autocomplete", "1"), ("autocomplete", "off")]));
    }

    #[test]
    fn refuses_what_is_not_ported() {
        let mut bytes = AlignedBytes::default();
        write(std::iter::empty(), &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let refused = |pairs: &[(&str, &str)]| search_json(&index, &params(pairs)).unwrap_err().title;
        assert_eq!(refused(&[("q", "paris"), ("autocomplete", "maybe")]), "Invalid parameter");
        assert_eq!(refused(&[("q", "paris"), ("lng", "2.3")]), "Unsupported parameter \"lng\"");
        assert_eq!(refused(&[("autocomplete", "0")]), "Missing parameter");
        let answer = search_json(&index, &params(&[("q", "paris"), ("autocomplete", "off"), ("limit", "0")])).unwrap();
        assert_eq!(answer["limit"], 5);
    }
}
