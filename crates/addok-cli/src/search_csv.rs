//! addok-csv 1.1.0's `/search/csv` (its `CSVSearch`), on addok-rs's search:
//! a CSV file in, the same file out with the best result of each row. The
//! response is addok-csv's, byte for byte, but where search answers
//! otherwise and for three bugs of addok-csv's, fixed rather than
//! reproduced: a dialect its Sniffer guesses wrong, an
//! empty `result_street` column, and a request failed
//! whole for one query over addok's length limit, whose row is answered
//! empty and reported instead (`Warning`). Filters name columns, whose value
//! filters each row, as addok-csv means them; addok-csv 1.1.0 fails on them
//! instead. The `lat`/`lon` columns are refused: geohashes are not ported
//! yet. Two departures from addok are asked for
//! per request: the postcode fallback, and the house number split into
//! columns a client names in `result_columns`.

use std::collections::HashMap;
use std::fmt;

use addok_core::document::Number;
use addok_core::index::Index;

use crate::geocoded::{FilterColumns, Geocoded, MIN_SCORE, SPLIT_COLUMNS, Warning, geocode, split_housenumber};
use crate::pycsv::{self, Dialect};

/// A `/search/csv` request: its multipart form.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The first `data` part: the file.
    pub data: Vec<u8>,
    pub filename: String,
    /// The other parts' text, by name, in order.
    pub params: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub body: Vec<u8>,
    pub content_type: String,
    pub content_disposition: String,
    /// What the answer leaves undone: rows too long to search.
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// HTTP 400.
    BadRequest(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::BadRequest(title) => f.write_str(title),
        }
    }
}

impl std::error::Error for Error {}

/// addok-csv's `CSV_HEADERS`, then its `CSV_EXTRA_FIELDS`: addok's `FIELDS`
/// but house numbers, then the BAN's `EXTRA_FIELDS`;
/// but `result_street`, which addok-csv writes empty, no BAN document having
/// a street.
const RESULT_HEADERS: [&str; 16] = [
    "latitude",
    "longitude",
    "result_label",
    "result_score",
    "result_score_next",
    "result_type",
    "result_id",
    "result_housenumber",
    "result_name",
    "result_postcode",
    "result_city",
    "result_context",
    "result_citycode",
    "result_oldcitycode",
    "result_oldcity",
    "result_district",
];

/// A switch's value by falcon's true and false strings; none if it is
/// neither. A blank value is none here, where falcon reads it as true: see
/// `param_flag`.
pub fn flag(value: &str) -> Option<bool> {
    match value {
        "true" | "True" | "t" | "yes" | "y" | "1" | "on" => Some(true),
        "false" | "False" | "f" | "no" | "n" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// A switch's value as falcon's `get_param_as_bool` reads addok-csv's
/// `with_bom` and addok's `autocomplete`: `flag`, a blank value true.
pub fn param_flag(value: &str) -> Option<bool> {
    match value.is_empty() {
        true => Some(true),
        false => flag(value),
    }
}

/// What addok-csv answers to the request.
pub fn search_csv<B: AsRef<[u8]>>(index: &Index<B>, request: &Request) -> Result<Response, Error> {
    let bad = |title: String| Err(Error::BadRequest(title));
    let param = |name: &str| request.params.get(name).and_then(|values| values.last());
    for name in ["lat", "lon"] {
        if request.params.contains_key(name) {
            return bad(format!("Unsupported parameter \"{name}\""));
        }
    }
    let encoding = param("encoding").map_or("utf-8-sig", String::as_str);
    let with_signature = match encoding.to_lowercase().replace('-', "_").as_str() {
        "utf_8" | "utf8" | "u8" | "utf" => false,
        "utf_8_sig" | "utf8_sig" => true,
        _ => return bad(format!("Unable to decode with encoding \"{encoding}\"")),
    };
    let Ok(text) = std::str::from_utf8(&request.data) else {
        return bad(format!("Unable to decode with encoding \"{encoding}\""));
    };
    let text = match with_signature {
        true => text.strip_prefix('\u{feff}').unwrap_or(text),
        false => text,
    };
    // addok-csv's line breaks, as per RFC 4180.
    let content = text.replace('\r', "").replace('\n', "\r\n");
    if content.is_empty() {
        return bad("Empty file".into());
    }
    let dialect = dialect(&content, param("delimiter"), param("quote"))?;
    let min_score = match param("min_score") {
        None => MIN_SCORE,
        Some(value) => match value.trim().parse() {
            Ok(score) => score,
            Err(_) => return bad(format!("Invalid parameter \"min_score\": {value}")),
        },
    };
    let with_bom = match param("with_bom").map(|value| param_flag(value).ok_or(value)) {
        None => false,
        Some(Ok(on)) => on,
        Some(Err(value)) => return bad(format!("Invalid parameter \"with_bom\": {value}")),
    };
    // Not addok-csv's: the postcode fallback, off unless asked for.
    let postcode_fallback = match param("postcode_fallback").map(|value| flag(value).ok_or(value)) {
        None => false,
        Some(Ok(on)) => on,
        Some(Err(value)) => return bad(format!("Invalid parameter \"postcode_fallback\": {value}")),
    };

    let requested = request.params.get("columns").filter(|columns| !columns.is_empty());
    let quote = param("quote").and_then(|quote| quote.chars().next());
    let (dialect, records) = read(&content, dialect, requested, quote);
    let mut records = records.into_iter();
    let fieldnames = records.next().unwrap_or_default();
    let columns = match requested {
        Some(columns) => columns.clone(),
        None => fieldnames.clone(),
    };
    let filter_columns = FilterColumns::named(|name| request.params.get(name).map(Vec::as_slice));
    if let Some(missing) = columns.iter().chain(filter_columns.columns()).find(|column| !fieldnames.contains(column)) {
        let fieldnames: Vec<String> = fieldnames.iter().map(|name| python_str(name)).collect();
        let fieldnames = fieldnames.join(", ");
        return bad(format!("Cannot found column '{missing}' in columns [{fieldnames}]"));
    }
    // Not addok-csv's, which ignores `result_columns`: the house number
    // split, for a client that names its columns there.
    let named = request.params.get("result_columns");
    let split: Vec<&str> = SPLIT_COLUMNS
        .into_iter()
        .filter(|column| named.is_some_and(|names| names.iter().any(|name| name == column)))
        .collect();
    let mut headers = fieldnames.clone();
    let new = RESULT_HEADERS.iter().chain(&split).filter(|&&header| !fieldnames.iter().any(|name| name == header));
    headers.extend(new.map(|&header| header.to_owned()));

    let mut out = String::new();
    if encoding.starts_with("utf-8") && with_bom {
        out.push('\u{feff}');
    }
    pycsv::write_row(&mut out, headers.iter().map(String::as_str), &dialect);
    // DictReader skips empty records.
    let mut too_long = Vec::new();
    for (i, row) in records.filter(|row| !row.is_empty()).enumerate() {
        let value = |column: &str| pycsv::value(&fieldnames, &row, column).unwrap_or("");
        let query = columns.iter().map(|column| value(column)).collect::<Vec<_>>().join(" ");
        // addok-csv fails the whole request on a query over addok's limit
        // (HTTP 413); the row is answered empty instead, and reported.
        let filters = filter_columns.filters(value);
        let geocoded = geocode(index, &query, &filters, min_score, postcode_fallback).unwrap_or_else(|_| {
            too_long.push(i + 1);
            None
        });
        let parts = geocoded.as_ref().map(|found| found.housenumber.as_deref().map(split_housenumber).unwrap_or_default());
        let result = geocoded.map(cells);
        let cells = headers.iter().map(|header| {
            let computed = RESULT_HEADERS.iter().position(|known| known == header);
            let part = SPLIT_COLUMNS.iter().position(|known| known == header).filter(|_| split.contains(&header.as_str()));
            match (&result, computed, &parts, part) {
                (Some(result), Some(i), _, _) => result[i].as_str(),
                (_, _, Some(parts), Some(i)) => parts[i].as_deref().unwrap_or(""),
                _ => value(header),
            }
        });
        pycsv::write_row(&mut out, cells, &dialect);
    }

    let mut body = Vec::with_capacity(out.len() + 3);
    if with_signature {
        body.extend_from_slice("\u{feff}".as_bytes());
    }
    body.extend_from_slice(out.as_bytes());
    let warnings = match too_long.is_empty() {
        true => Vec::new(),
        false => vec![Warning::QueryTooLong { rows: too_long }],
    };
    Ok(Response {
        body,
        content_type: format!("text/csv; charset={encoding}"),
        content_disposition: format!(
            "attachment; filename=\"{}.geocoded.csv\"",
            stem(&request.filename)
        ),
        warnings,
    })
}

/// addok-csv's `compute_dialect`: Python's sniffed dialect, or the Unix
/// one, quotes doubled, with the request's delimiter and quote; a one-column
/// file gets a delimiter the file does not hold.
fn dialect(content: &str, delimiter: Option<&String>, quote: Option<&String>) -> Result<Dialect, Error> {
    let mut dialect = pycsv::sniff(content).unwrap_or_else(Dialect::unix);
    let one = |name: &str, value: &str| {
        let mut chars = value.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Ok(c),
            _ => Err(Error::BadRequest(format!("\"{name}\" must be a 1-character string"))),
        }
    };
    if let Some(delimiter) = delimiter.filter(|value| !value.is_empty()) {
        dialect.delimiter = one("delimiter", delimiter)?;
    }
    if let Some(quote) = quote.filter(|value| !value.is_empty()) {
        dialect.quotechar = one("quotechar", quote)?;
    }
    if dialect.delimiter.is_alphanumeric() || dialect.delimiter == '\r' {
        let Some(free) = ";,\t|~^°".chars().find(|&c| !content.contains(c)) else {
            let message = "Unable to detect delimiter, please add one with \"delimiter\" parameter.";
            return Err(Error::BadRequest(message.into()));
        };
        dialect.delimiter = free;
    }
    Ok(dialect)
}

/// The records of the file, and the dialect read: addok-csv's, unless its
/// header lacks a requested column, or holds a single column when none is
/// requested though a common delimiter appears in it. addok-csv's Sniffer
/// then guessed wrong (a space for the delimiter and `'` for the quote, on
/// a ` 'EST ' ` in an address) and addok-csv fails or misreads the file:
/// it is read again with each common delimiter and `"` (or the request's
/// quote), the first whose header fits kept.
fn read(
    content: &str,
    dialect: Dialect,
    requested: Option<&Vec<String>>,
    quote: Option<char>,
) -> (Dialect, Vec<Vec<String>>) {
    const DELIMITERS: [char; 4] = [',', ';', '\t', '|'];
    let line = content.split("\r\n").next().unwrap_or_default();
    let fits = |records: &[Vec<String>]| {
        let header = records.first().map_or(&[][..], Vec::as_slice);
        match requested {
            Some(columns) => columns.iter().all(|column| header.contains(column)),
            None => header.len() > 1 || !line.contains(DELIMITERS),
        }
    };
    let records = pycsv::records(content, &dialect);
    if fits(&records) {
        return (dialect, records);
    }
    let fixed = DELIMITERS.into_iter().find_map(|delimiter| {
        let dialect = Dialect {
            delimiter,
            quotechar: quote.unwrap_or('"'),
            skipinitialspace: false,
            lineterminator: "\r\n",
            quote_all: false,
        };
        let records = pycsv::records(content, &dialect);
        fits(&records).then_some((dialect, records))
    });
    fixed.unwrap_or((dialect, records))
}

/// The values a row takes for `RESULT_HEADERS`, as addok-csv writes them.
fn cells(geocoded: Geocoded) -> [String; 16] {
    let number = |number: Option<Number>| number.as_ref().map_or(String::new(), python_number);
    [
        number(geocoded.lat),
        number(geocoded.lon),
        geocoded.label,
        python_float(geocoded.score),
        geocoded.score_next.map_or("0".into(), python_float),
        geocoded.kind,
        geocoded.id,
        geocoded.housenumber.unwrap_or_default(),
        geocoded.name.unwrap_or_default(),
        geocoded.postcode.unwrap_or_default(),
        geocoded.city.unwrap_or_default(),
        geocoded.context.unwrap_or_default(),
        geocoded.citycode.unwrap_or_default(),
        geocoded.oldcitycode.unwrap_or_default(),
        geocoded.oldcity.unwrap_or_default(),
        geocoded.district.unwrap_or_default(),
    ]
}

/// Python's `str` of a number as the NDJSON wrote it.
fn python_number(number: &Number) -> String {
    match *number {
        Number::Integer(integer) => integer.to_string(),
        Number::Float(float) => python_float(float),
    }
}

/// Python's `repr` of a float: its shortest digits that read back the same,
/// fixed between 1e-4 and 1e16, else in exponent notation.
fn python_float(x: f64) -> String {
    if !x.is_finite() {
        return match x {
            x if x.is_nan() => "nan".into(),
            x if x > 0.0 => "inf".into(),
            _ => "-inf".into(),
        };
    }
    let shortest = format!("{x:e}");
    let (mantissa, exponent) = shortest.split_once('e').unwrap();
    let exponent: i32 = exponent.parse().unwrap();
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(mantissa) => ("-", mantissa),
        None => ("", mantissa),
    };
    let digits = mantissa.replace('.', "");
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat(-point as usize))
        } else if point as usize >= digits.len() {
            format!("{sign}{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{sign}{whole}.{fraction}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let point = if rest.is_empty() { String::new() } else { format!(".{rest}") };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{first}{point}e{exponent_sign}{:02}", exponent.abs())
    }
}

/// Python's `repr` of a string.
fn python_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut repr = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => repr.push_str("\\\\"),
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            c if c == quote => {
                repr.push('\\');
                repr.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => repr.push_str(&format!("\\x{:02x}", c as u32)),
            c => repr.push(c),
        }
    }
    repr.push(quote);
    repr
}

/// Python's `os.path.splitext(filename)[0]`.
fn stem(filename: &str) -> &str {
    let base = filename.rfind('/').map_or(0, |slash| slash + 1);
    match filename.rfind('.') {
        Some(dot) if dot > base && filename[base..dot].chars().any(|c| c != '.') => &filename[..dot],
        _ => filename,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are CPython 3.12's.

    #[test]
    fn writes_floats_as_python() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.87, "0.87"),
            (45.123456, "45.123456"),
            (0.0001, "0.0001"),
            (0.00001234, "1.234e-05"),
            (1e16, "1e+16"),
            (123456789012345.6, "123456789012345.6"),
            (-2.5e-7, "-2.5e-07"),
        ];
        for (x, python) in cases {
            assert_eq!(python_float(x), python, "{x}");
        }
        assert_eq!(python_float(crate::geocoded::round(1.005)), "1.0");
        assert_eq!(python_float(crate::geocoded::round(0.125)), "0.12");
    }

    #[test]
    fn reads_what_addok_csv_misreads() {
        // CPython 3.12's Sniffer takes a space for the delimiter and `'`
        // for the quote here.
        let content = "ad3,city,zip_code\r\n1 RUE D 'ARC,PARIS,75001\r\n2 RUE L 'EST ' B,LYON,69001\r\n";
        let sniffed = pycsv::sniff(content).unwrap();
        assert_eq!((sniffed.delimiter, sniffed.quotechar), (' ', '\''));
        let columns: Vec<String> = ["ad3", "city", "zip_code"].map(String::from).to_vec();
        for requested in [Some(&columns), None] {
            let (dialect, records) = read(content, sniffed.clone(), requested, None);
            assert_eq!((dialect.delimiter, dialect.quotechar), (',', '"'));
            assert_eq!(records[2], ["2 RUE L 'EST ' B", "LYON", "69001"]);
        }
        // What addok-csv reads right stays as it reads it.
        let plain = "a;b\r\n1;2\r\n";
        let dialect = pycsv::sniff(plain).unwrap();
        assert_eq!(read(plain, dialect.clone(), None, None).0, dialect);
        // A one-column file stays one column.
        let single = Dialect { delimiter: ';', ..dialect };
        assert_eq!(read("name\r\nx\r\n", single.clone(), None, None).0, single);
    }

    #[test]
    fn writes_strings_as_python() {
        assert_eq!(python_str("ad3,city"), "'ad3,city'");
        assert_eq!(python_str("l'eglise"), "\"l'eglise\"");
        assert_eq!(python_str("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(python_str("a\tb\\"), "'a\\tb\\\\'");
    }

    #[test]
    fn splits_extensions_as_python() {
        assert_eq!(stem("chunk.csv"), "chunk");
        assert_eq!(stem("a.b.csv"), "a.b");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("dir.d/file"), "dir.d/file");
        assert_eq!(stem(""), "");
    }

    #[test]
    fn reads_a_blank_with_bom_as_true_as_addok_csv() {
        use addok_core::index::{AlignedBytes, write};
        let mut bytes = AlignedBytes::default();
        write(std::iter::empty(), &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let answer = |with_bom: Option<&str>| {
            let mut request = Request {
                data: b"q\nparis\n".to_vec(),
                filename: "a.csv".into(),
                ..Request::default()
            };
            request.params.insert("encoding".into(), vec!["utf-8".into()]);
            if let Some(value) = with_bom {
                request.params.insert("with_bom".into(), vec![value.into()]);
            }
            search_csv(&index, &request).map(|response| response.body.starts_with("\u{feff}".as_bytes()))
        };
        // addok-csv 1.1.0 writes a byte-order mark for a blank `with_bom`, as
        // falcon reads a blank flag; none without one.
        assert_eq!(answer(Some("")), Ok(true));
        assert_eq!(answer(Some("true")), Ok(true));
        assert_eq!(answer(Some("off")), Ok(false));
        assert_eq!(answer(None), Ok(false));
        assert!(answer(Some("maybe")).is_err());
    }

    #[test]
    fn splits_the_house_number_only_into_the_columns_named() {
        use addok_core::document::Document;
        use addok_core::index::{AlignedBytes, write};
        let number = |number: &str| format!(r#""{number}":{{"id":"66136_0001_{number}","banId":null,"x":0,"y":0,"lon":2.89,"lat":42.69}}"#);
        let street = format!(
            r#"{{"id":"66136_0001","banId":null,"name":"Rue Clément Marot","postcode":"66000","citycode":["66136"],"oldcitycode":null,"lon":2.89,"lat":42.69,"x":0,"y":0,"city":["Perpignan"],"oldcity":null,"context":"66, Pyrénées-Orientales, Occitanie","type":"street","importance":0.5,"housenumbers":{{{},{}}}}}"#,
            number("4"),
            number("12bis")
        );
        let mut bytes = AlignedBytes::default();
        write([Document::from_ndjson(&street).unwrap()], &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let csv = "q\n12 BIS RUE CLEMENT MAROT PERPIGNAN\n4 RUE CLEMENT MAROT PERPIGNAN\nzzz\n";
        let answer = |names: &[&str]| {
            let mut request = Request {
                data: csv.as_bytes().to_vec(),
                filename: "addresses.csv".into(),
                ..Request::default()
            };
            let names = names.iter().map(|&name| name.to_owned()).collect();
            request.params.insert("result_columns".into(), names);
            let body = search_csv(&index, &request).unwrap().body;
            let lines: Vec<String> = String::from_utf8(body).unwrap().lines().map(str::to_owned).collect();
            lines
        };
        // Not named: addok-csv's columns alone, as before.
        let before = answer(&["result_label", "result_housenumber"]);
        assert!(before[0].ends_with(";result_district"), "{}", before[0]);
        assert!(before[1].contains(";12bis;"), "{}", before[1]);
        // Named: after addok-csv's columns, in their own order.
        let split = answer(&["result_num_complement_short", "result_label", "result_num"]);
        assert!(split[0].ends_with(";result_district;result_num;result_num_complement_short"), "{}", split[0]);
        assert_eq!(split[1], format!("{};12;b", before[1]));
        assert_eq!(split[2], format!("{};4;", before[2]));
        assert_eq!(split[3], format!("{};;", before[3]));
    }

    #[test]
    fn filters_each_row_by_the_columns_named() {
        use addok_core::document::Document;
        use addok_core::index::{AlignedBytes, write};
        let street = |id: &str, postcode: &str, city: &str| {
            format!(
                r#"{{"id":"{id}","banId":null,"name":"Rue Clément Marot","postcode":"{postcode}","citycode":["{}"],"oldcitycode":null,"lon":2.89,"lat":42.69,"x":0,"y":0,"city":["{city}"],"oldcity":null,"context":"","type":"street","importance":0.5}}"#,
                &id[..5]
            )
        };
        let streets = [street("66136_0001", "66000", "Perpignan"), street("69381_0001", "69001", "Lyon")];
        let mut bytes = AlignedBytes::default();
        write(streets.map(|line| Document::from_ndjson(&line).unwrap()), &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let csv = "q;cp\nRUE CLEMENT MAROT;69001\nRUE CLEMENT MAROT;13001\nRUE CLEMENT MAROT;\n";
        let request = |params: &[(&str, &str)]| {
            let mut request = Request {
                data: csv.as_bytes().to_vec(),
                filename: "addresses.csv".into(),
                ..Request::default()
            };
            request.params.insert("columns".into(), vec!["q".into()]);
            for &(name, value) in params {
                request.params.entry(name.into()).or_default().push(value.into());
            }
            request
        };
        let body = search_csv(&index, &request(&[("postcode", "cp")])).unwrap().body;
        let lines: Vec<String> = String::from_utf8(body).unwrap().lines().map(str::to_owned).collect();
        assert!(lines[1].contains("69381_0001"), "{}", lines[1]);
        // A postcode no street holds: no result. An empty cell: no filter.
        assert!(!lines[2].contains("_0001"), "{}", lines[2]);
        assert!(lines[3].contains("_0001"), "{}", lines[3]);
        // A column the file lacks is refused, as a missing query column is.
        let refused = search_csv(&index, &request(&[("postcode", "zip")]));
        assert!(matches!(refused, Err(Error::BadRequest(title)) if title.starts_with("Cannot found column 'zip'")));
        // Positions are not ported yet.
        let refused = search_csv(&index, &request(&[("lat", "lat")]));
        assert!(matches!(refused, Err(Error::BadRequest(title)) if title == "Unsupported parameter \"lat\""));
    }
}
