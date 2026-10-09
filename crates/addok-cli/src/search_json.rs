//! addok's `GET /search` and `GET /reverse`: a query or a position in, the
//! results out as addok's GeoJSON `FeatureCollection`. Parameters are read
//! as falcon reads them: the last of a repeated one wins, a blank one is
//! kept, a filter's values split on spaces; autocomplete is on unless
//! turned off, and `lat` and `lon` give a position, as in addok. The answer
//! equals addok's as JSON, not as text: the properties follow another
//! order.

use addok_core::document::Number;
use addok_core::index::Index;
use addok_core::reverse::{reverse, reverse_nearest};
use addok_core::search::{Center, Filters, Found, Options, search_with};
use addok_core::text::{QUERY_MAX_LENGTH, fold};
use serde_json::{Map, Value, json};

use crate::geocoded::{nearest_radius, python_float};
use crate::search_csv::param_flag;

/// The BAN's `ATTRIBUTION` and `LICENCE`.
const ATTRIBUTION: &str = "BAN";
const LICENCE: &str = "ETALAB-2.0";

/// The BAN's `FILTERS`, in its order.
const FILTERS: [&str; 3] = ["type", "citycode", "postcode"];

/// The parameters addok reads a position from, by axis, in its order.
const LAT: [&str; 2] = ["lat", "latitude"];
const LON: [&str; 4] = ["lon", "lng", "long", "longitude"];

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
    /// The position to search around: both coordinates, given.
    pub center: Option<Center>,
    /// The position the answer repeats: addok's, both coordinates other
    /// than 0, longitude first.
    shown_center: Option<(f64, f64)>,
    /// The filters as the request names them, which the answer repeats.
    named: Vec<(&'static str, Vec<String>)>,
}

impl Request<'_> {
    /// What search takes of the request.
    pub fn options(&self) -> Options {
        Options {
            limit: self.limit,
            autocomplete: self.autocomplete,
            filters: self.filters.clone(),
            center: self.center,
        }
    }
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
    let (lat, lon) = coordinates(params)?;
    let center = lat.zip(lon).map(|(lat, lon)| Center { lat, lon });
    let shown_center = lon.zip(lat).filter(|&(lon, lat)| lon != 0.0 && lat != 0.0);
    let named = filters(params);
    let filters = search_filters(&named);
    Ok(Request { query, limit, autocomplete, filters, center, shown_center, named })
}

/// addok's `parse_lon_lat`: each axis's first name given, read as Python
/// reads a float, then checked in range; a coordinate of 0 is not.
fn coordinates(params: &[(String, String)]) -> Result<(Option<f64>, Option<f64>), Error> {
    let last = |name: &str| params.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let coordinate = |names: &[&str]| -> Result<Option<f64>, Error> {
        match names.iter().find_map(|&name| last(name).map(|value| (name, value))) {
            None => Ok(None),
            Some((name, value)) => python_float(value).map(Some).ok_or_else(|| Error::invalid(name, "invalid value")),
        }
    };
    let (lat, lon) = (coordinate(&LAT)?, coordinate(&LON)?);
    if lon.is_some_and(|lon| lon != 0.0 && (lon > 180.0 || lon < -180.0)) {
        return Err(Error::invalid("lon", "out of range"));
    }
    if lat.is_some_and(|lat| lat != 0.0 && (lat > 90.0 || lat < -90.0)) {
        return Err(Error::invalid("lat", "out of range"));
    }
    Ok((lat, lon))
}

/// What search keeps of the filters a request names.
fn search_filters(named: &[(&str, Vec<String>)]) -> Filters {
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
    Filters {
        kind,
        citycode: values("citycode"),
        postcode: values("postcode"),
    }
}

/// A `/reverse` request, its parameters read.
#[derive(Debug, Clone, PartialEq)]
pub struct ReverseRequest {
    pub center: Center,
    /// addok's `int(limit) or 1`, unchecked: a negative one leaves out as
    /// many results from the end, as Python's slices do.
    pub limit: i64,
    pub filters: Filters,
    /// Not addok's: the radius to search the nearest addresses within,
    /// asked for with `nearest`.
    pub nearest: Option<f64>,
    named: Vec<(&'static str, Vec<String>)>,
}

/// A `/reverse` request of these parameters, read as addok reads them, or
/// refused as it refuses them.
pub fn read_reverse(params: &[(String, String)]) -> Result<ReverseRequest, Error> {
    let last = |name: &str| params.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    let (lat, lon) = coordinates(params)?;
    let missing = |name: &str| Error::bad("Missing parameter", Some(format!("The \"{name}\" parameter is required.")));
    let lon = lon.ok_or_else(|| missing("lon"))?;
    let lat = lat.ok_or_else(|| missing("lat"))?;
    let limit = match last("limit").map(python_int) {
        None | Some(Some(0)) => 1,
        Some(Some(limit)) => limit,
        Some(None) => return Err(Error::invalid("limit", "The value must be an integer.")),
    };
    let nearest = nearest_radius(last).map_err(|title| Error::bad(&title, None))?;
    let named = filters(params);
    let filters = search_filters(&named);
    Ok(ReverseRequest { center: Center { lat, lon }, limit, filters, nearest, named })
}

/// What addok answers to a `/reverse` request of these parameters.
pub fn reverse_json<B: AsRef<[u8]>>(index: &Index<B>, params: &[(String, String)]) -> Result<Value, Error> {
    let request = read_reverse(params)?;
    let reverse = |limit: usize| match request.nearest {
        None => reverse(index, request.center, limit, &request.filters),
        Some(radius) => reverse_nearest(index, request.center, limit, &request.filters, radius),
    };
    let found = match usize::try_from(request.limit) {
        Ok(limit) => reverse(limit),
        Err(_) => {
            let mut found = reverse(usize::MAX);
            found.truncate(found.len().saturating_sub(request.limit.unsigned_abs() as usize));
            found
        }
    };
    let mut collection = collection(&found);
    if !request.named.is_empty() {
        let named = request.named.into_iter().map(|(name, values)| (name.to_owned(), values.into()));
        collection.insert("filters".into(), Value::Object(named.collect()));
    }
    collection.insert("limit".into(), request.limit.into());
    Ok(Value::Object(collection))
}

/// addok's `render`: its results, then its attribution and licence.
fn collection(found: &[Found]) -> Map<String, Value> {
    let mut collection = Map::new();
    collection.insert("type".into(), "FeatureCollection".into());
    collection.insert("version".into(), "draft".into());
    collection.insert("features".into(), found.iter().map(feature).collect());
    collection.insert("attribution".into(), ATTRIBUTION.into());
    collection.insert("licence".into(), LICENCE.into());
    collection
}

/// What addok answers to a `/search` request of these parameters, in order.
pub fn search_json<B: AsRef<[u8]>>(index: &Index<B>, params: &[(String, String)]) -> Result<Value, Error> {
    let request = read(params)?;
    let found = search_with(index, request.query, &request.options()).map_err(|_| Error {
        status: 413,
        title: format!("Query too long, {} chars, limit is {QUERY_MAX_LENGTH}", fold(request.query).chars().count()),
        description: None,
    })?;
    let mut collection = collection(&found);
    collection.insert("query".into(), request.query.into());
    if !request.named.is_empty() {
        let named = request.named.into_iter().map(|(name, values)| (name.to_owned(), values.into()));
        collection.insert("filters".into(), Value::Object(named.collect()));
    }
    if let Some((lon, lat)) = request.shown_center {
        collection.insert("center".into(), json!([lon, lat]));
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
    if let Some(distance) = found.distance {
        properties.insert("distance".into(), (distance as i64).into());
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
    fn reads_a_position_as_addok() {
        let read_center = |pairs: &[(&str, &str)]| {
            let given = params(pairs);
            let request = read(&given).unwrap();
            (request.center.map(|center| (center.lat, center.lon)), request.shown_center)
        };
        assert_eq!(read_center(&[("q", "paris"), ("lat", "48.8"), ("lon", "2.3")]), (Some((48.8, 2.3)), Some((2.3, 48.8))));
        assert_eq!(read_center(&[("q", "paris"), ("latitude", " 4_8.5 "), ("lng", "2")]), (Some((48.5, 2.0)), Some((2.0, 48.5))));
        // One coordinate alone is no position; a 0 is one, not shown.
        assert_eq!(read_center(&[("q", "paris"), ("lat", "48.8")]), (None, None));
        assert_eq!(read_center(&[("q", "paris"), ("lat", "0"), ("lon", "2.3")]), (Some((0.0, 2.3)), None));
        let refused = |pairs: &[(&str, &str)]| search_json_error(pairs);
        assert_eq!(refused(&[("q", "paris"), ("lng", "abc")]), "The \"lng\" parameter is invalid. invalid value");
        assert_eq!(refused(&[("q", "paris"), ("lat", "91"), ("lon", "2")]), "The \"lat\" parameter is invalid. out of range");
        assert_eq!(refused(&[("q", "paris"), ("lat", "91"), ("lon", "181")]), "The \"lon\" parameter is invalid. out of range");
    }

    fn search_json_error(pairs: &[(&str, &str)]) -> String {
        let given = params(pairs);
        read(&given).unwrap_err().description.unwrap()
    }

    #[test]
    fn reads_a_reverse_request_as_addok() {
        let read = |pairs: &[(&str, &str)]| {
            let given = params(pairs);
            read_reverse(&given).map(|request| (request.center.lat, request.center.lon, request.limit))
        };
        assert_eq!(read(&[("lat", "48.8"), ("lon", "2.3")]), Ok((48.8, 2.3, 1)));
        assert_eq!(read(&[("lat", "48.8"), ("lng", "2.3"), ("limit", "0")]), Ok((48.8, 2.3, 1)));
        // Unchecked, as in addok: a negative limit, a 0 coordinate.
        assert_eq!(read(&[("lat", "0"), ("lon", "0"), ("limit", "-2")]), Ok((0.0, 0.0, -2)));
        let refused = |pairs: &[(&str, &str)]| read(pairs).unwrap_err().description.unwrap();
        // The longitude is missed first, as addok checks it first.
        assert_eq!(refused(&[]), "The \"lon\" parameter is required.");
        assert_eq!(refused(&[("lon", "2.3")]), "The \"lat\" parameter is required.");
        assert_eq!(refused(&[("lat", "abc")]), "The \"lat\" parameter is invalid. invalid value");
        assert_eq!(refused(&[("lat", "48"), ("lon", "2"), ("limit", "x")]), "The \"limit\" parameter is invalid. The value must be an integer.");
        // Not addok's: the nearest addresses within a radius, asked for.
        let given = params(&[("lat", "48"), ("lon", "2"), ("nearest", "1"), ("radius", "800")]);
        assert_eq!(read_reverse(&given).unwrap().nearest, Some(800.0));
        assert_eq!(read_reverse(&params(&[("lat", "48"), ("lon", "2")])).unwrap().nearest, None);
    }

    #[test]
    fn refuses_as_addok_does() {
        let mut bytes = AlignedBytes::default();
        write(std::iter::empty(), &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let refused = |pairs: &[(&str, &str)]| search_json(&index, &params(pairs)).unwrap_err().title;
        assert_eq!(refused(&[("q", "paris"), ("autocomplete", "maybe")]), "Invalid parameter");
        assert_eq!(refused(&[("autocomplete", "0")]), "Missing parameter");
        let answer = search_json(&index, &params(&[("q", "paris"), ("autocomplete", "off"), ("limit", "0")])).unwrap();
        assert_eq!(answer["limit"], 5);
    }
}
