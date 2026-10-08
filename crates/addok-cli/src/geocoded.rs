//! A row's best result as addok-csv keeps it, typed: what `/search/csv`
//! writes as text, and batch writes as columns.

use addok_core::document::{Document, Number, Text};
use addok_core::index::Index;
use addok_core::search::{Found, search};
use addok_core::text::{QUERY_MAX_LENGTH, QueryTooLong, fold, is_street_type, short_ordinal};

/// addok-csv's `CSV_MIN_SCORE`.
pub const MIN_SCORE: f64 = 0.5;

/// The rounded score at which a house number is trusted, the postcode
/// fallback's bar, measured on 648,328 real addresses on 2026-10-06.
pub const CONFIDENT_SCORE: f64 = 0.62;

/// What a request's geocoding left undone, reported beside its answer rather
/// than failing it: one bad row should not cost the others their results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// Rows, numbered from 1, whose query is longer than addok's
    /// `QUERY_MAX_LENGTH`. They get no result. addok-csv refuses the whole
    /// request instead (HTTP 413): a single row of junk then cost a client's
    /// 999 other rows their answers.
    QueryTooLong { rows: Vec<usize> },
}

/// How many row numbers a warning's header lists; its count says how many
/// there are.
const LISTED_ROWS: usize = 100;

impl Warning {
    /// Its `X-Addok-Warning` header: a name, then parameters.
    pub fn header(&self) -> String {
        match self {
            Warning::QueryTooLong { rows } => {
                let listed: Vec<String> = rows.iter().take(LISTED_ROWS).map(usize::to_string).collect();
                format!(
                    "query_too_long; limit={QUERY_MAX_LENGTH}; count={}; rows={}",
                    rows.len(),
                    listed.join(",")
                )
            }
        }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Warning::QueryTooLong { rows } => {
                let listed: Vec<String> = rows.iter().take(LISTED_ROWS).map(usize::to_string).collect();
                let more = if rows.len() > LISTED_ROWS { ", …" } else { "" };
                let (count, row) = match rows.len() {
                    1 => ("1 row".to_owned(), "row"),
                    n => (format!("{n} rows"), "rows"),
                };
                write!(
                    f,
                    "{count} longer than {QUERY_MAX_LENGTH} characters left without a result: {row} {}{more}",
                    listed.join(", ")
                )
            }
        }
    }
}

/// What addok-csv writes of a row's best result, typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Geocoded {
    /// The house number's when one matched, else the document's.
    pub lat: Option<Number>,
    pub lon: Option<Number>,
    pub label: String,
    /// Rounded to two decimals.
    pub score: f64,
    /// The next result's score, rounded; none without one.
    pub score_next: Option<f64>,
    pub kind: String,
    pub id: String,
    pub housenumber: Option<String>,
    /// The document's fields, their first value; none when null or absent.
    /// addok-csv's `CSV_EXTRA_FIELDS` but `street`, which no BAN document
    /// has.
    pub name: Option<String>,
    pub postcode: Option<String>,
    pub city: Option<String>,
    pub context: Option<String>,
    pub citycode: Option<String>,
    pub oldcitycode: Option<String>,
    pub oldcity: Option<String>,
    pub district: Option<String>,
}

/// The result columns a client gets only by naming them in
/// `result_columns`: the house number split into its number, its
/// suffix as the BAN writes it, and that suffix short.
pub const SPLIT_COLUMNS: [&str; 3] = ["result_num", "result_num_complement", "result_num_complement_short"];

/// `SPLIT_COLUMNS`' values for a house number as the BAN writes it: "12bis"
/// gives 12, bis and b, "7C" 7, C and c. The last two are none without a
/// suffix.
pub fn split_housenumber(housenumber: &str) -> [Option<String>; 3] {
    let digits = housenumber.find(|c: char| !c.is_ascii_digit()).unwrap_or(housenumber.len());
    let (num, complement) = housenumber.split_at(digits);
    let complement = complement.trim();
    // A suffix that is not one of addok-france's ordinals ("bis a", "appt
    // 1") stays as written, so the column is never empty for a number that
    // has a suffix.
    let short = short_ordinal(complement).unwrap_or_else(|| complement.to_owned());
    let some = |text: &str| (!text.is_empty()).then(|| text.to_owned());
    [some(num), some(complement), some(&short)]
}

/// addok-csv's `process_row`: the query's best result, unless its rounded
/// score is not above `min_score`. With `postcode_fallback`, asked for, the
/// one departure from it.
pub fn geocode<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    min_score: f64,
    postcode_fallback: bool,
) -> Result<Option<Geocoded>, QueryTooLong> {
    let mut found = search(index, query, 3)?;
    if postcode_fallback && let Some(rescued) = self::postcode_fallback(index, query, &found)? {
        found = rescued;
    }
    let best = found.first().filter(|best| round(best.score) > min_score);
    Ok(best.map(|best| geocoded(best, found.get(1))))
}

/// The postcode fallback: when `found` holds no confident house
/// number, the query carries a postcode and dropping it can change the
/// answer, the results of the query without it, if their best is a
/// confident house number in a commune the query names. A city's postcode
/// with the wrong district (Perpignan 66100 for a street of 66000)
/// otherwise outranks the right street.
pub fn postcode_fallback<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    found: &[Found],
) -> Result<Option<Vec<Found>>, QueryTooLong> {
    if found.first().is_some_and(confident) || !worth_retrying(index, found.first(), query) {
        return Ok(None);
    }
    let Some(without) = without_postcode(query) else {
        return Ok(None);
    };
    let retried = search(index, &without, 3)?;
    let Some(best) = retried.first().filter(|best| confident(best) && named_in(&best.document, query)) else {
        return Ok(None);
    };
    if namesake_left_behind(index, &best.document, query)? {
        return Ok(None);
    }
    Ok(Some(retried))
}

/// Whether the rescue left a commune of the same name behind, in the
/// département of the query's postcode: `SAINT AIGNAN 82100` is
/// Saint-Aignan of Tarn-et-Garonne, which has its own, not Saint-Aignan
/// 72110. Only checked when the rescue changes département, which is rare,
/// by searching the commune's name with the postcode for a municipality of
/// that name there.
fn namesake_left_behind<B: AsRef<[u8]>>(index: &Index<B>, rescued: &Document, query: &str) -> Result<bool, QueryTooLong> {
    let Some(rescued_departement) = rescued.postcode.values().first().map(|postcode| departement(postcode)) else {
        return Ok(false);
    };
    let postcodes: Vec<&str> = query.split_whitespace().filter(|word| is_postcode(word)).collect();
    if postcodes.iter().any(|postcode| departement(postcode) == rescued_departement) {
        return Ok(false);
    }
    let names: Vec<&String> = [&rescued.city, &rescued.oldcity]
        .into_iter()
        .flat_map(|text| text.values())
        .collect();
    for postcode in postcodes {
        for name in &names {
            let found = search(index, &format!("{name} {postcode}"), 10)?;
            let namesake = found.iter().any(|found| {
                let document = &found.document;
                document.kind == "municipality"
                    && document.name.values().iter().any(|own| words(own) == words(name))
                    && document.postcode.values().iter().any(|own| departement(own) == departement(postcode))
            });
            if namesake {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// A postcode's département: its first two digits, three overseas.
fn departement(postcode: &str) -> &str {
    let digits = if postcode.starts_with("97") { 3 } else { 2 };
    &postcode[..digits.min(postcode.len())]
}

fn is_postcode(word: &str) -> bool {
    word.len() == 5 && word.bytes().all(|b| b.is_ascii_digit())
}

/// Whether dropping the postcode can change the answer. Not when the first
/// one is a street, a locality or a municipality in a commune the query
/// names, and that commune has a single postcode: the postcode then agrees
/// with it, and the house number is missing from the BAN, not elsewhere.
/// That skips over half the retries, and they rescued mostly wrong
/// addresses (`84 RUE DE LILLE, ARRAS` as 84 Rue d'Arras, Lille).
fn worth_retrying<B: AsRef<[u8]>>(index: &Index<B>, first: Option<&Found>, query: &str) -> bool {
    let Some(first) = first else {
        return true;
    };
    let document = &first.document;
    first.housenumber.is_some() || !named_in(document, query) || several_postcodes(index, document)
}

/// Whether the document's commune spans several postcodes, as its
/// municipality lists them; unknown counts as several.
fn several_postcodes<B: AsRef<[u8]>>(index: &Index<B>, document: &Document) -> bool {
    let Some(citycode) = document.citycode.values().first() else {
        return true;
    };
    let municipalities = index.filter("type", "municipality");
    let commune = index.filter("citycode", city_of(citycode));
    match commune.iter().find(|doc| municipalities.binary_search(doc).is_ok()) {
        Some(&doc) => index.document(doc).postcode.values().len() > 1,
        None => true,
    }
}

/// The commune an arrondissement of Paris, Marseille or Lyon belongs to: the
/// BAN files their streets under each arrondissement, a municipality of one
/// postcode, though a street may cross several.
fn city_of(citycode: &str) -> &str {
    match citycode.parse::<u32>() {
        Ok(75101..=75120) => "75056",
        Ok(13201..=13216) => "13055",
        Ok(69381..=69389) => "69123",
        _ => citycode,
    }
}

fn confident(found: &Found) -> bool {
    found.housenumber.is_some() && round(found.score) >= CONFIDENT_SCORE
}

/// The query without its postcodes, five-digit words; none if it has none,
/// or nothing else.
fn without_postcode(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let kept: Vec<&str> = words.iter().copied().filter(|word| !is_postcode(word)).collect();
    (kept.len() < words.len() && !kept.is_empty()).then(|| kept.join(" "))
}

/// Whether the query names the document's commune, current or merged
/// into it, as consecutive words: "ST" and "SAINT" alike. Not as the name
/// of a street: `RUE DE PARIS TOULOUSE` names Toulouse, not Paris. A name
/// right after a street type, articles and prepositions between, counts
/// only where the city goes, at the end or before a postcode
/// (`GRANDE RUE PARIS 75001`).
fn named_in(document: &Document, query: &str) -> bool {
    let query = words(query);
    [&document.city, &document.oldcity]
        .into_iter()
        .flat_map(|text| match text {
            Text::One(value) => vec![value.as_str()],
            Text::Many(values) => values.iter().map(String::as_str).collect(),
            Text::Null | Text::Absent => vec![],
        })
        .map(words)
        .any(|name| {
            !name.is_empty()
                && query.windows(name.len()).enumerate().any(|(at, window)| {
                    let end = at + name.len();
                    window == name
                        && (end == query.len() || is_postcode(&query[end]) || !after_street_type(&query, at))
                })
        })
}

/// Whether the words before `at` end with a street type, then any articles
/// and prepositions: `rue de la`, `av`, `chemin des`.
fn after_street_type(words: &[String], at: usize) -> bool {
    const LINKS: [&str; 9] = ["de", "du", "des", "d", "la", "le", "les", "l", "en"];
    let mut before = at;
    while before > 0 && LINKS.contains(&words[before - 1].as_str()) {
        before -= 1;
    }
    before > 0 && is_street_type(&words[before - 1])
}

fn words(text: &str) -> Vec<String> {
    let folded = fold(text).to_lowercase();
    let word = |word: &str| match word {
        "saint" => "st".to_owned(),
        "sainte" => "ste".to_owned(),
        word => word.to_owned(),
    };
    folded.split(' ').filter(|w| !w.is_empty()).map(word).collect()
}

fn geocoded(best: &Found, next: Option<&Found>) -> Geocoded {
    let document = &best.document;
    let (lat, lon) = match &best.housenumber {
        Some(housenumber) => (Some(housenumber.lat), Some(housenumber.lon)),
        None => (document.lat, document.lon),
    };
    // addok's `Result` attributes: a list's first value.
    let text = |text: &Text| match text {
        Text::One(value) => Some(value.clone()),
        Text::Many(values) => values.first().cloned(),
        Text::Null | Text::Absent => None,
    };
    Geocoded {
        lat,
        lon,
        label: best.label().to_owned(),
        score: round(best.score),
        score_next: next.map(|next| round(next.score)),
        kind: best.kind().to_owned(),
        id: best.id().to_owned(),
        housenumber: best.housenumber.as_ref().map(|housenumber| housenumber.number.clone()),
        name: text(&document.name),
        postcode: text(&document.postcode),
        city: text(&document.city),
        context: text(&document.context),
        citycode: text(&document.citycode),
        oldcitycode: text(&document.oldcitycode),
        oldcity: text(&document.oldcity),
        district: text(&document.district),
    }
}

/// Python's `round(x, 2)`: both round the exact value, ties to even.
pub fn round(x: f64) -> f64 {
    format!("{x:.2}").parse().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use addok_core::index::{AlignedBytes, write};

    /// A street with a number 4, in the commune its id starts with.
    fn street(id: &str, name: &str, postcode: &str, city: &str, oldcity: &str) -> Document {
        let oldcity = if oldcity.is_empty() { "null".to_owned() } else { format!("\"{oldcity}\"") };
        let citycode = &id[..5];
        Document::from_ndjson(&format!(
            r#"{{"id":"{id}","banId":null,"name":"{name}","postcode":"{postcode}","citycode":["{citycode}"],"oldcitycode":null,"lon":2.89,"lat":42.69,"x":0,"y":0,"city":["{city}"],"oldcity":{oldcity},"context":"66, Pyrénées-Orientales, Occitanie","type":"street","importance":0.5,"housenumbers":{{"4":{{"id":"{id}_00004","banId":null,"x":0,"y":0,"lon":2.89,"lat":42.69}}}}}}"#
        ))
        .unwrap()
    }

    fn municipality(citycode: &str, name: &str, postcodes: &[&str]) -> Document {
        let postcodes = postcodes.iter().map(|postcode| format!("\"{postcode}\"")).collect::<Vec<_>>().join(",");
        Document::from_ndjson(&format!(
            r#"{{"id":"{citycode}","banId":null,"type":"municipality","name":"{name}","postcode":[{postcodes}],"citycode":"{citycode}","x":0,"y":0,"lon":2.89,"lat":42.69,"population":1000,"city":"{name}","context":"66, Pyrénées-Orientales, Occitanie","importance":0.5}}"#
        ))
        .unwrap()
    }

    fn index(documents: Vec<Document>) -> Index<AlignedBytes> {
        let mut bytes = AlignedBytes::default();
        write(documents, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    #[test]
    fn drops_only_five_digit_words() {
        assert_eq!(without_postcode("4 RUE CLEMENT MAROT PERPIGNAN 66100").as_deref(), Some("4 RUE CLEMENT MAROT PERPIGNAN"));
        assert_eq!(without_postcode("4 RUE CLEMENT MAROT PERPIGNAN"), None);
        assert_eq!(without_postcode("BP 1234 PERPIGNAN 661000"), None);
        assert_eq!(without_postcode("66100"), None);
    }

    #[test]
    fn names_the_commune_as_whole_words() {
        let uriage = street("38422_0001", "Chemin du Luiset", "38410", "Saint-Martin-d'Uriage", "");
        assert!(named_in(&uriage, "310 ROUTE DU LUISET SAINT MARTIN D''URIAGE 38410"));
        assert!(named_in(&uriage, "310 ROUTE DU LUISET ST MARTIN D URIAGE"));
        assert!(!named_in(&uriage, "310 ROUTE DU LUISET GRENOBLE"));
        let lens = street("62498_0001", "Rue de Lille", "62300", "Lens", "");
        assert!(!named_in(&lens, "4 RUE DE LILLE VALENCIENNES"));
        let merged = street("41167_0001", "Rue Jean Jaurès", "41700", "Le Controis-en-Sologne", "Chémery");
        assert!(named_in(&merged, "9 RUE JEAN JAURES CHEMERY 41700"));
    }

    #[test]
    fn does_not_take_a_street_s_name_for_its_commune() {
        // Measured on real addresses: each was a wrong rescue.
        let paris = street("75119_0001", "Rue de Toulouse", "75019", "Paris", "");
        assert!(!named_in(&paris, "11 RUE DE PARIS TOULOUSE 31100"));
        let tours = street("37261_0001", "Rue de la Source", "37100", "Tours", "");
        assert!(!named_in(&tours, "5 RUE DE TOURS LA SOURCE 45100"));
        let etienne = street("42218_0001", "Rue du Monteil", "42000", "Saint-Étienne", "");
        assert!(!named_in(&etienne, "4 RUE SAINT ETIENNE LE MONTEIL AU VICOMTE 23460"));
        // Where the city goes, a name counts though a street type comes first.
        assert!(named_in(&paris, "12 GRANDE RUE PARIS 75001"));
        assert!(named_in(&paris, "12 GRANDE RUE PARIS"));
        // After the street, not as its name: the commune, rightly.
        let pernay = street("37179_0001", "Rue du Lavoir", "37230", "Pernay", "");
        assert!(named_in(&pernay, "9 BIS RUE DU LAVOIR PERNAY FONDETTES 37230"));
    }

    #[test]
    fn keeps_a_commune_s_namesake_in_the_postcode_s_departement() {
        let index = index(vec![
            municipality("02305", "Fayet", &["02100"]),
            municipality("12101", "Fayet", &["12360"]),
            street("12101_0001", "Rue de la Côte", "12360", "Fayet", ""),
        ]);
        // Fayet of the Aisne exists: 02100 is its postcode, not a mistake for
        // Fayet of Aveyron, so the rescue is dropped.
        assert!(postcode_fallback(&index, "4 RUE DE LA COTE FAYET 02100", &[]).unwrap().is_none());
        // No Fayet in the Alpes-Maritimes: the postcode is the mistake.
        let rescued = postcode_fallback(&index, "4 RUE DE LA COTE FAYET 06700", &[]).unwrap().unwrap();
        assert_eq!(rescued[0].label(), "4 Rue de la Côte 12360 Fayet");
    }

    fn perpignan() -> Index<AlignedBytes> {
        index(vec![
            municipality("66136", "Perpignan", &["66000", "66100"]),
            street("66136_0001", "Rue Clément Marot", "66000", "Perpignan", ""),
            street("66136_0002", "Rue Marcel Aymé", "66100", "Perpignan", ""),
        ])
    }

    #[test]
    fn retries_a_weak_answer_without_its_postcode() {
        // In the BAN, the thousands of 66100 addresses outrank the street of
        // 66000 the query names; two streets cannot, so the first pass is
        // given as found nothing.
        let index = perpignan();
        let rescued = postcode_fallback(&index, "4 RUE CLEMENT MAROT PERPIGNAN 66100", &[]).unwrap().unwrap();
        assert_eq!(rescued[0].label(), "4 Rue Clément Marot 66000 Perpignan");
        // Not in a commune the query names: the first answer stands.
        assert!(postcode_fallback(&index, "4 RUE CLEMENT MAROT CANET 66140", &[]).unwrap().is_none());
    }

    #[test]
    fn splits_a_house_number_as_the_ban_writes_it() {
        let split = |number| split_housenumber(number).map(|value| value.unwrap_or_default());
        assert_eq!(split("12"), ["12", "", ""]);
        assert_eq!(split("12bis"), ["12", "bis", "b"]);
        assert_eq!(split("25ter"), ["25", "ter", "t"]);
        assert_eq!(split("3Bis"), ["3", "Bis", "b"]);
        assert_eq!(split("7C"), ["7", "C", "c"]);
        assert_eq!(split("62a"), ["62", "a", "a"]);
        // Not one of addok-france's ordinals: the short form keeps it.
        assert_eq!(split("11bis a"), ["11", "bis a", "bis a"]);
        assert_eq!(split("2appt 1"), ["2", "appt 1", "appt 1"]);
        assert_eq!(split("2qua"), ["2", "qua", "qua"]);
        assert_eq!(split("0P1"), ["0", "P1", "P1"]);
    }

    #[test]
    fn keeps_a_confident_answer_without_retrying() {
        let index = perpignan();
        let query = "4 RUE CLEMENT MAROT PERPIGNAN 66100";
        let found = search(&index, query, 3).unwrap();
        assert!(confident(&found[0]));
        assert!(postcode_fallback(&index, query, &found).unwrap().is_none());
    }

    #[test]
    fn does_not_retry_a_street_its_single_postcode_agrees_with() {
        // Benfeld has one postcode: the street is right and number 48 is not
        // in the BAN. Without the postcode, the query finds 48 Rue de
        // Benfeld in Strasbourg, which it also names.
        let index = index(vec![
            municipality("67024", "Benfeld", &["67230"]),
            street("67024_0001", "Rue de Strasbourg", "67230", "Benfeld", ""),
        ]);
        let query = "48 RUE DE STRASBOURG BENFELD 67230";
        let found = search(&index, query, 3).unwrap();
        assert!(found[0].housenumber.is_none());
        assert!(!worth_retrying(&index, found.first(), query));
        assert!(worth_retrying(&index, None, query));
    }

    #[test]
    fn retries_a_street_of_a_city_of_several_postcodes() {
        let index = perpignan();
        let query = "8 RUE CLEMENT MAROT PERPIGNAN 66100";
        let found = search(&index, query, 3).unwrap();
        assert!(found[0].housenumber.is_none());
        assert!(worth_retrying(&index, found.first(), query));
    }

    #[test]
    fn counts_an_arrondissement_with_its_city() {
        assert_eq!(city_of("75104"), "75056");
        assert_eq!(city_of("13206"), "13055");
        assert_eq!(city_of("69381"), "69123");
        assert_eq!(city_of("75056"), "75056");
        assert_eq!(city_of("2A004"), "2A004");
        let paris = index(vec![
            municipality("75056", "Paris", &["75001", "75004"]),
            municipality("75104", "Paris 4e Arrondissement", &["75004"]),
        ]);
        let rivoli = Document { citycode: Text::One("75104".into()), ..Document::default() };
        assert!(several_postcodes(&paris, &rivoli));
    }

    #[test]
    fn search_csv_takes_the_fallback_as_a_switch() {
        use crate::search_csv::{Error, Request, search_csv};
        use std::collections::HashMap;
        let index = perpignan();
        let request = |fallback: &str| {
            let mut params = HashMap::new();
            params.insert("columns".to_owned(), vec!["ad3".to_owned(), "city".to_owned(), "zip".to_owned()]);
            params.insert("postcode_fallback".to_owned(), vec![fallback.to_owned()]);
            let data = b"ad3,city,zip\n4 RUE CLEMENT MAROT,PERPIGNAN,66100\n".to_vec();
            Request { data, filename: "a.csv".into(), params }
        };
        for on in ["1", "on", "true", "0", "off"] {
            assert!(search_csv(&index, &request(on)).is_ok(), "{on}");
        }
        match search_csv(&index, &request("maybe")) {
            Err(Error::BadRequest(title)) => assert_eq!(title, "Invalid parameter \"postcode_fallback\": maybe"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn answers_a_row_too_long_to_search_empty_and_reports_it() {
        use crate::search_csv::{Request, search_csv};
        use std::collections::HashMap;
        let junk = "VOLUPTATEM ".repeat(25);
        let mut params = HashMap::new();
        params.insert("columns".to_owned(), vec!["ad3".to_owned(), "city".to_owned()]);
        let data = format!("ad3,city\n4 RUE CLEMENT MAROT,PERPIGNAN\n{junk},PERPIGNAN\n4 RUE MARCEL AYME,PERPIGNAN\n");
        let request = Request { data: data.into_bytes(), filename: "a.csv".into(), params };
        // addok-csv refuses the whole request (HTTP 413); the row is answered
        // empty instead, the others as usual.
        let response = search_csv(&perpignan(), &request).unwrap();
        assert_eq!(response.warnings, [Warning::QueryTooLong { rows: vec![2] }]);
        let body = String::from_utf8(response.body).unwrap();
        let rows: Vec<&str> = body.lines().collect();
        assert!(rows[1].contains("4 Rue Clément Marot 66000 Perpignan"), "{}", rows[1]);
        assert!(rows[2].ends_with(&",".repeat(16)), "{}", rows[2]);
        assert!(rows[3].contains("4 Rue Marcel Aymé 66100 Perpignan"), "{}", rows[3]);
    }

    #[test]
    fn writes_a_warning_as_a_header_and_a_sentence() {
        let warning = Warning::QueryTooLong { rows: vec![2, 7] };
        assert_eq!(warning.header(), "query_too_long; limit=200; count=2; rows=2,7");
        assert_eq!(warning.to_string(), "2 rows longer than 200 characters left without a result: rows 2, 7");
        let one = Warning::QueryTooLong { rows: vec![352] };
        assert_eq!(one.to_string(), "1 row longer than 200 characters left without a result: row 352");
        // The header lists the first hundred rows; its count says how many.
        let many = Warning::QueryTooLong { rows: (1..=250).collect() };
        assert!(many.header().starts_with("query_too_long; limit=200; count=250; rows=1,2,"));
        assert!(many.header().ends_with(",100"));
        assert!(many.to_string().ends_with("99, 100, …"));
    }
}
