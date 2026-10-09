//! The index: what addok-rs searches, built once per BAN release from the
//! national NDJSON: its documents, and what addok's indexers write to
//! Redis: tokens and their posting lists, pairs, edge ngrams, filters and
//! geohash cells. `write` builds it into a file of sections (see `format`),
//! which `Index` reads in place, memory-mapped.

mod build;
mod encoding;
mod format;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::io::{self, Write};

use bytemuck::Pod;

pub use format::{AlignedBytes, OpenError};
use format::{Kind, Sections, Strings};

use crate::document::{Document, HouseNumber, Number, TextRef};
use crate::postings::{DocId, PostingList};
use crate::text;

/// The tokens addok indexes a document under, in addok's order, each with
/// the document's score in its posting list (`FieldsIndexer`). None when the
/// document has no name, which addok refuses to index.
pub fn document_tokens(doc: &Document) -> Option<Vec<(String, f64)>> {
    if doc.name.values().is_empty() {
        return None;
    }
    let importance = importance(doc) * IMPORTANCE_WEIGHT;
    let mut tokens: Vec<(String, f64)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (values, boost) in fields(doc) {
        let boost = boost + importance;
        for value in values {
            let words = text::index_tokens(value);
            if words.is_empty() {
                continue;
            }
            // addok's `extract_tokens`, in its order of operations.
            let score = DEFAULT_BOOST / words.len() as f64 * boost;
            for word in words {
                match seen.get(&word) {
                    Some(&i) if tokens[i].1 < score => tokens[i].1 = score,
                    Some(_) => {}
                    // addok compares a new token's score with 0.
                    None if 0.0 < score => {
                        seen.insert(word.clone(), tokens.len());
                        tokens.push((word, score));
                    }
                    None => {}
                }
            }
        }
    }
    Some(tokens)
}

/// The token a house number is matched by (addok's `prepare_housenumbers`):
/// its tokens joined, "1bis" becoming "1b" and "13" "trez". Search joins a
/// query's house-number tokens the same way.
pub fn housenumber_token(number: &str) -> String {
    text::index_tokens(number).concat()
}

/// addok's `DEFAULT_BOOST`.
const DEFAULT_BOOST: f64 = 1.0;
/// addok's `IMPORTANCE_WEIGHT`.
const IMPORTANCE_WEIGHT: f64 = 0.1;

/// addok's `float(doc.get("importance", 0.0))`.
fn importance(doc: &Document) -> f64 {
    doc.importance.map_or(0.0, Number::value)
}

/// The fields addok indexes, in its order, with their boosts: addok's
/// `FIELDS` and the BAN's `EXTRA_FIELDS`, less
/// `street`, which no BAN document has, and `housenumbers`, which addok
/// indexes apart.
fn fields(doc: &Document) -> [(&[String], f64); 8] {
    let postcode = if doc.kind == "municipality" { 1.2 } else { 1.0 };
    [
        (doc.name.values(), 4.0),
        (doc.postcode.values(), postcode),
        (doc.city.values(), DEFAULT_BOOST),
        (doc.context.values(), DEFAULT_BOOST),
        (doc.citycode.values(), DEFAULT_BOOST),
        (doc.oldcitycode.values(), DEFAULT_BOOST),
        (doc.oldcity.values(), DEFAULT_BOOST),
        (doc.district.values(), DEFAULT_BOOST),
    ]
}

/// A token's number in the index. Tokens are numbered in byte order.
pub type TokenId = u32;

/// Indexes the documents and writes the index to `out`, in one pass. Returns
/// each section's size in bytes, in the file's order.
pub fn write(
    documents: impl IntoIterator<Item = Document>,
    out: impl Write,
) -> io::Result<Vec<(&'static str, u64)>> {
    let mut builder = build::Builder::default();
    for document in documents {
        builder.add(document);
    }
    builder.write(out)
}

/// An index, read in place from its bytes: a memory-mapped file, or the
/// `AlignedBytes` it was written into. Its documents are numbered in
/// tie-break order (see `DocId`).
pub struct Index<B> {
    bytes: B,
    sections: Sections,
    /// The unions of a filter's values, kept once made, by filter and
    /// values: addok keeps those of over 100,000 documents in Redis, and
    /// `type=street locality` unites over a million.
    unions: Mutex<HashMap<String, Arc<[DocId]>>>,
}

/// How many unions an index keeps; past that, it starts over.
const UNIONS_KEPT: usize = 64;

impl<B: AsRef<[u8]>> Index<B> {
    /// Checks that the bytes hold an index this build reads: its trailer, its
    /// sections' bounds, sizes and alignment. Their content is trusted: a
    /// corrupt index makes a read panic, never read out of bounds.
    pub fn open(bytes: B) -> Result<Index<B>, OpenError> {
        let sections = Sections::read(bytes.as_ref())?;
        Ok(Index {
            bytes,
            sections,
            unions: Mutex::default(),
        })
    }

    fn array<T: Pod>(&self, kind: Kind) -> &[T] {
        self.sections.array(self.bytes.as_ref(), kind)
    }

    fn strings(&self, blob: Kind, ends: Kind) -> Strings<'_> {
        Strings::new(self.array(blob), self.array(ends))
    }

    /// The `i`th of lists written one after another, `ends` from 0.
    fn list<T: Pod>(&self, ends: Kind, items: Kind, i: usize) -> &[T] {
        let ends: &[u32] = self.array(ends);
        &self.array(items)[ends[i] as usize..ends[i + 1] as usize]
    }

    /// A value written apart, from a table of (position, string).
    fn written_apart(&self, table: Kind, position: u32) -> String {
        let table: &[[u32; 2]] = self.array(table);
        let found = table.binary_search_by_key(&position, |entry| entry[0]);
        let string = table[found.expect("an entry, as the flag says")][1];
        self.strings(Kind::Strings, Kind::StringOffsets)
            .get(string as usize)
            .to_owned()
    }

    /// How many documents it holds.
    pub fn len(&self) -> usize {
        self.array::<u32>(Kind::DocIds).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A document as the NDJSON writes it, but its house numbers, which
    /// `housenumber` finds by token.
    pub fn document(&self, doc: DocId) -> Document {
        let d = doc as usize;
        let strings = self.strings(Kind::Strings, Kind::StringOffsets);
        let string = |number: u32| strings.get(number as usize).to_owned();
        let flags = self.array::<u16>(Kind::DocFlags)[d];
        let ban_id = match (flags >> encoding::BAN_ID) & 3 {
            0 => None,
            encoding::BAN_ID_UUID => Some(encoding::uuid_text(&self.array(Kind::DocBanIds)[d])),
            _ => Some(self.written_apart(Kind::DocBanIdTexts, doc)),
        };
        let mut document = Document {
            id: string(self.array::<u32>(Kind::DocIds)[d]),
            ban_id,
            kind: self.kind(doc).to_owned(),
            ..Document::default()
        };
        for (field, text) in encoding::texts_mut(&mut document).into_iter().enumerate() {
            *text = self.text(doc, field).to_owned();
        }
        for (i, number) in encoding::numbers_mut(&mut document).into_iter().enumerate() {
            *number = self.number(doc, i);
        }
        document
    }

    /// A document's `type`.
    pub fn kind(&self, doc: DocId) -> &str {
        let kind = self.array::<u32>(Kind::DocKinds)[doc as usize];
        self.strings(Kind::Strings, Kind::StringOffsets)
            .get(kind as usize)
    }

    /// A document's `name`, borrowed.
    pub fn name(&self, doc: DocId) -> TextRef<'_> {
        self.text(doc, encoding::NAME)
    }

    /// A document's `postcode`, borrowed.
    pub fn postcode(&self, doc: DocId) -> TextRef<'_> {
        self.text(doc, encoding::POSTCODE)
    }

    /// A document's `city`, borrowed.
    pub fn city(&self, doc: DocId) -> TextRef<'_> {
        self.text(doc, encoding::CITY)
    }

    /// A document's `importance`.
    pub fn importance(&self, doc: DocId) -> Option<Number> {
        self.number(doc, encoding::IMPORTANCE)
    }

    /// A document's position, `lat` and `lon`.
    pub fn position(&self, doc: DocId) -> Option<(Number, Number)> {
        self.number(doc, encoding::LAT).zip(self.number(doc, encoding::LON))
    }

    /// The position of a street's house number by its token, `lat` and
    /// `lon`: `housenumber`'s, without the rest.
    pub fn housenumber_position(&self, doc: DocId, token: &str) -> Option<(Number, Number)> {
        let (position, _) = self.find_housenumber(doc, token)?;
        let coordinates = self.housenumber_coordinates(position);
        Some((coordinates[3], coordinates[2]))
    }

    /// A house number's x, y, lon and lat, as the NDJSON writes them.
    fn housenumber_coordinates(&self, position: usize) -> [Number; 4] {
        let flags = self.array::<u8>(Kind::HnFlags)[position];
        let values: [f64; 4] = if flags & encoding::HN_COORDINATES_EXCEPTION != 0 {
            let indexes: &[u32] = self.array(Kind::HnCoordinateExceptionIndexes);
            let found = indexes
                .binary_search(&(position as u32))
                .expect("an entry, as the flag says");
            self.array::<[f64; 4]>(Kind::HnCoordinateExceptions)[found]
        } else {
            let units = self.array::<[i32; 4]>(Kind::HnCoordinates)[position];
            std::array::from_fn(|i| f64::from(units[i]) / encoding::SCALES[i])
        };
        std::array::from_fn(|i| match flags & (1 << i) {
            0 => Number::Float(values[i]),
            _ => Number::Integer(values[i] as i64),
        })
    }

    /// A document's text field, by its place in `encoding::texts_mut`.
    fn text(&self, doc: DocId, field: usize) -> TextRef<'_> {
        let strings = self.strings(Kind::Strings, Kind::StringOffsets);
        let value = self.array::<[u32; 8]>(Kind::DocTexts)[doc as usize][field];
        match (self.array::<u16>(Kind::DocTextTags)[doc as usize] >> (2 * field)) & 3 {
            encoding::ABSENT => TextRef::Absent,
            encoding::NULL => TextRef::Null,
            encoding::ONE => TextRef::One(strings.get(value as usize)),
            _ => {
                let items = self.list::<u32>(Kind::ListOffsets, Kind::ListItems, value as usize);
                TextRef::Many(items.iter().map(|&item| strings.get(item as usize)).collect())
            }
        }
    }

    /// A document's number, by its place in `encoding::numbers_mut`.
    fn number(&self, doc: DocId, i: usize) -> Option<Number> {
        let flags = self.array::<u16>(Kind::DocFlags)[doc as usize];
        let number = self.array::<[f64; 6]>(Kind::DocNumbers)[doc as usize][i];
        let present = (flags >> (2 * i)) & 1 != 0;
        let integer = (flags >> (2 * i + 1)) & 1 != 0;
        present.then_some(match integer {
            true => Number::Integer(number as i64),
            false => Number::Float(number),
        })
    }

    /// `housenumber`'s number alone, as the NDJSON writes it.
    pub fn housenumber_number(&self, doc: DocId, token: &str) -> Option<&str> {
        self.find_housenumber(doc, token).map(|(_, number)| number)
    }

    /// A street's house numbers, those addok keeps (one per token), by
    /// token: each one's place among the index's, its `lat` and `lon`.
    pub fn housenumber_positions(&self, doc: DocId) -> impl Iterator<Item = (usize, Number, Number)> + '_ {
        let ends: &[u32] = self.array(Kind::DocHouseNumbers);
        let (start, end) = (ends[doc as usize] as usize, ends[doc as usize + 1] as usize);
        (start..end).map(|at| {
            let [_, _, lon, lat] = self.housenumber_coordinates(at);
            (at, lat, lon)
        })
    }

    /// A street's house number by its place among the index's, as
    /// `housenumber_positions` gives it.
    pub fn housenumber_at(&self, doc: DocId, at: usize) -> HouseNumber {
        let number = self
            .strings(Kind::NumberWritten, Kind::NumberWrittenOffsets)
            .get(self.array::<u32>(Kind::HnNumbers)[at] as usize)
            .to_owned();
        self.housenumber_built(doc, at, number)
    }

    /// Where `housenumber` finds a street's house number, and its number.
    fn find_housenumber(&self, doc: DocId, token: &str) -> Option<(usize, &str)> {
        let ends: &[u32] = self.array(Kind::DocHouseNumbers);
        let start = ends[doc as usize] as usize;
        let numbers = &self.array::<u32>(Kind::HnNumbers)[start..ends[doc as usize + 1] as usize];
        let tokens = self.strings(Kind::NumberTokens, Kind::NumberTokenOffsets);
        let found =
            numbers.binary_search_by(|&number| tokens.bytes(number as usize).cmp(token.as_bytes()));
        let found = found.ok()?;
        let written = self.strings(Kind::NumberWritten, Kind::NumberWrittenOffsets);
        Some((start + found, written.get(numbers[found] as usize)))
    }

    /// A document's `id`.
    pub fn id(&self, doc: DocId) -> &str {
        let id = self.array::<u32>(Kind::DocIds)[doc as usize];
        self.strings(Kind::Strings, Kind::StringOffsets)
            .get(id as usize)
    }

    /// A street's house number by its token, as the NDJSON writes it: what
    /// addok's `match_housenumber` reads. Of numbers sharing a token, the
    /// street's last, as in addok.
    pub fn housenumber(&self, doc: DocId, token: &str) -> Option<HouseNumber> {
        let (position, number) = self.find_housenumber(doc, token)?;
        Some(self.housenumber_built(doc, position, number.to_owned()))
    }

    /// The house number at `position` among the index's, its number given.
    fn housenumber_built(&self, doc: DocId, position: usize, number: String) -> HouseNumber {
        let at = position as u32;
        let flags = self.array::<u8>(Kind::HnFlags)[position];
        let id = match flags & encoding::HN_ID_EXCEPTION {
            0 => encoding::derived_id(self.id(doc), &number).expect("derived, as the flag says"),
            _ => self.written_apart(Kind::HnIdExceptions, at),
        };
        let ban_id = if flags & encoding::HN_BAN_ID_NULL != 0 {
            None
        } else if flags & encoding::HN_BAN_ID_TEXT != 0 {
            Some(self.written_apart(Kind::HnBanIdTexts, at))
        } else {
            Some(encoding::uuid_text(&self.array(Kind::HnBanIds)[position]))
        };
        let [x, y, lon, lat] = self.housenumber_coordinates(position);
        HouseNumber {
            number,
            id,
            ban_id,
            x,
            y,
            lon,
            lat,
        }
    }

    /// A word's number, if a document holds it.
    pub fn token(&self, word: &str) -> Option<TokenId> {
        let words = self.strings(Kind::Words, Kind::WordOffsets);
        let found = words.find_hashed(self.array(Kind::WordTable), word);
        found.map(|i| i as TokenId)
    }

    /// The word a token stands for.
    pub fn word(&self, token: TokenId) -> &str {
        self.strings(Kind::Words, Kind::WordOffsets)
            .get(token as usize)
    }

    /// A word's posting list, addok's `w|<word>`.
    pub fn postings(&self, word: &str) -> Option<PostingList<'_>> {
        let token = self.token(word)? as usize;
        let ids = self.list(Kind::PostingOffsets, Kind::PostingIds, token);
        let scores = self.list(Kind::PostingOffsets, Kind::PostingScores, token);
        Some(PostingList::new(ids, scores))
    }

    /// The tokens a word shares a document with, ascending, addok's
    /// `p|<word>`.
    pub fn pairs(&self, word: &str) -> &[TokenId] {
        match self.token(word) {
            Some(token) => self.list(Kind::PairOffsets, Kind::Pairs, token as usize),
            None => &[],
        }
    }

    /// The tokens an edge ngram begins, ascending, addok's `n|<ngram>`.
    pub fn edge_ngram(&self, ngram: &str) -> &[TokenId] {
        match self
            .strings(Kind::NgramKeys, Kind::NgramKeyOffsets)
            .find(ngram)
        {
            Some(i) => self.list(Kind::NgramOffsets, Kind::Ngrams, i),
            None => &[],
        }
    }

    /// The documents filed under a cell, ascending, addok's `g|<geohash>`:
    /// those lying in it, and the streets with a house number there.
    pub fn geohash(&self, cell: crate::geohash::Cell) -> &[DocId] {
        let cells: &[u64] = self.array(Kind::GeohashCells);
        match cells.binary_search(&cell) {
            Ok(i) => self.list(Kind::GeohashOffsets, Kind::Geohashes, i),
            Err(_) => &[],
        }
    }

    /// How many cells documents lie in.
    pub fn geohash_cells(&self) -> usize {
        self.array::<u64>(Kind::GeohashCells).len()
    }

    /// The documents a filter holds, ascending, addok's `f|<name>|<value>`.
    pub fn filter(&self, name: &str, value: &str) -> &[DocId] {
        let keys = self.strings(Kind::FilterKeys, Kind::FilterKeyOffsets);
        match keys.find(&format!("{name}|{value}")) {
            Some(i) => self.list(Kind::FilterOffsets, Kind::Filters, i),
            None => &[],
        }
    }
}

impl<B: AsRef<[u8]>> Index<B> {
    /// The documents any of a filter's values holds, ascending: addok's
    /// `SUNIONSTORE` of `f|<name>|<value>`, made once per set of values.
    pub fn filter_union(&self, name: &str, values: &[&str]) -> Arc<[DocId]> {
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let key = format!("{name}|{}", sorted.join("|"));
        let mut unions = self.unions.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(union) = unions.get(&key) {
            return union.clone();
        }
        let lists: Vec<&[DocId]> = sorted.iter().map(|value| self.filter(name, value)).collect();
        let union: Arc<[DocId]> = crate::postings::union_sets(&lists).into();
        if unions.len() >= UNIONS_KEPT {
            unions.clear();
        }
        unions.insert(key, union.clone());
        union
    }
}

/// The filters a document joins, as `name|value`: addok's `FiltersIndexer`
/// over the BAN's `FILTERS` (type, citycode, postcode), plus `type` =
/// `housenumber` for a street with house numbers.
fn doc_filters(doc: &Document, housenumbers: bool) -> Vec<String> {
    let mut filters = Vec::new();
    if !doc.kind.is_empty() {
        filters.push(format!("type|{}", doc.kind));
    }
    let citycodes = doc.citycode.values().iter();
    filters.extend(citycodes.map(|value| format!("citycode|{value}")));
    let postcodes = doc.postcode.values().iter();
    filters.extend(postcodes.map(|value| format!("postcode|{value}")));
    if housenumbers {
        filters.push("type|housenumber".to_owned());
    }
    filters
}

/// addok's `MIN_EDGE_NGRAMS`.
const MIN_EDGE_NGRAMS: usize = 3;
/// addok's `MAX_EDGE_NGRAMS`.
const MAX_EDGE_NGRAMS: usize = 20;

/// addok's `compute_edge_ngrams`: a token's first 3 to 20 characters, short
/// of the whole token.
fn edge_ngrams(token: &str) -> impl Iterator<Item = &str> {
    // Where the token's first 1, 2, … characters end.
    let ends: Vec<usize> = token
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .collect();
    let longest = ends.len().saturating_sub(1).min(MAX_EDGE_NGRAMS);
    (MIN_EDGE_NGRAMS..=longest).map(move |length| &token[..ends[length - 1]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Text;
    use crate::postings::intersect;

    /// The index of these documents, written then opened.
    fn build(documents: Vec<Document>) -> Index<AlignedBytes> {
        let mut bytes = AlignedBytes::default();
        write(documents, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    // Expected scores are addok's own.

    fn one(value: &str) -> Text {
        Text::One(value.to_owned())
    }

    fn many(values: &[&str]) -> Text {
        Text::Many(values.iter().map(|&value| value.to_owned()).collect())
    }

    fn float(value: f64) -> Option<Number> {
        Some(Number::Float(value))
    }

    fn housenumber(number: &str, id: &str) -> HouseNumber {
        HouseNumber {
            number: number.into(),
            id: id.into(),
            ban_id: None,
            x: Number::Float(887482.78),
            y: Number::Float(6548127.24),
            lon: Number::Float(5.423167),
            lat: Number::Float(46.006992),
        }
    }

    fn tokens(expected: &[(&str, f64)]) -> Vec<(String, f64)> {
        expected
            .iter()
            .map(|&(token, score)| (token.to_owned(), score))
            .collect()
    }

    fn municipality() -> Document {
        Document {
            id: "01002".into(),
            kind: "municipality".into(),
            importance: float(0.22554),
            name: one("L'Abergement-de-Varey"),
            postcode: many(&["01640"]),
            city: one("L'Abergement-de-Varey"),
            context: one("01, Ain, Auvergne-Rhône-Alpes"),
            citycode: one("01002"),
            ..Document::default()
        }
    }

    fn street() -> Document {
        Document {
            id: "01002_0110".into(),
            kind: "street".into(),
            importance: float(0.3672),
            name: one("Montee de la Foret"),
            postcode: one("01640"),
            city: many(&["L'Abergement-de-Varey"]),
            context: one("01, Ain, Auvergne-Rhône-Alpes"),
            citycode: many(&["01002"]),
            ..Document::default()
        }
    }

    #[test]
    fn shares_a_value_s_boost_among_its_tokens() {
        // name 4, postcode 1.2 for a municipality, the rest 1; each plus a
        // tenth of the importance, then divided among the value's tokens. A
        // token in several values keeps its best score.
        assert_eq!(
            document_tokens(&municipality()),
            Some(tokens(&[
                ("l", 1.0056385),
                ("aberjemen", 1.0056385),
                ("de", 1.0056385),
                ("varei", 1.0056385),
                ("01640", 1.222554),
                ("01", 0.2045108),
                ("ain", 0.2045108),
                ("auvergn", 0.2045108),
                ("ron", 0.2045108),
                ("alp", 0.2045108),
                ("01002", 1.022554),
            ]))
        );
    }

    #[test]
    fn scores_as_addok_rounds() {
        // A street's postcode boosts 1, and addok divides before multiplying:
        // (1 / 5) * 1.03672 is 0.20734400000000003.
        assert_eq!(
            document_tokens(&street()),
            Some(tokens(&[
                ("monte", 1.00918),
                ("de", 1.00918),
                ("la", 1.00918),
                ("foret", 1.00918),
                ("01640", 1.03672),
                ("l", 0.25918),
                ("aberjemen", 0.25918),
                ("varei", 0.25918),
                ("01", 0.20734400000000003),
                ("ain", 0.20734400000000003),
                ("auvergn", 0.20734400000000003),
                ("ron", 0.20734400000000003),
                ("alp", 0.20734400000000003),
                ("01002", 1.03672),
            ]))
        );
    }

    #[test]
    fn leaves_out_a_document_without_name() {
        let nameless = Document {
            name: one(""),
            ..street()
        };
        assert_eq!(document_tokens(&nameless), None);
    }

    #[test]
    fn numbers_documents_by_importance_then_id() {
        let doc = |id: &str, importance| Document {
            id: id.into(),
            importance: float(importance),
            ..street()
        };
        let index = build(vec![doc("b", 0.5), doc("a", 0.5), doc("c", 0.9)]);
        assert_eq!([index.id(0), index.id(1), index.id(2)], ["c", "a", "b"]);
    }

    #[test]
    fn lists_each_token_s_documents_with_their_scores() {
        let index = build(vec![street(), municipality()]);
        // The street is more important, so it is numbered first.
        let foret = index.postings("foret").unwrap();
        assert_eq!(intersect(&[foret], 10), [(0, 1.00918)]);
        let aberjemen = index.postings("aberjemen").unwrap();
        assert_eq!(intersect(&[aberjemen], 10), [(1, 1.0056385), (0, 0.25918)]);
        assert!(index.postings("absent").is_none());
    }

    fn words<'a>(index: &'a Index<AlignedBytes>, tokens: &[TokenId]) -> Vec<&'a str> {
        tokens.iter().map(|&token| index.word(token)).collect()
    }

    #[test]
    fn pairs_each_token_with_those_it_shares_a_document_with() {
        let index = build(vec![street(), municipality()]);
        let foret = [
            "01",
            "01002",
            "01640",
            "aberjemen",
            "ain",
            "alp",
            "auvergn",
            "de",
            "l",
            "la",
            "monte",
            "ron",
            "varei",
        ];
        assert_eq!(words(&index, index.pairs("foret")), foret);
        let varei = [
            "01",
            "01002",
            "01640",
            "aberjemen",
            "ain",
            "alp",
            "auvergn",
            "de",
            "foret",
            "l",
            "la",
            "monte",
            "ron",
        ];
        assert_eq!(words(&index, index.pairs("varei")), varei);
        assert!(index.pairs("absent").is_empty());
    }

    #[test]
    fn makes_a_house_number_one_token() {
        assert_eq!(housenumber_token("1bis"), "1b");
        assert_eq!(housenumber_token("13"), "trez");
        assert_eq!(housenumber_token("1"), "un");
        assert_eq!(housenumber_token("101"), "101");
        assert_eq!(housenumber_token("0AA"), "0a");
    }

    #[test]
    fn computes_edge_ngrams_as_addok() {
        let ngrams = |token| edge_ngrams(token).collect::<Vec<_>>();
        assert!(ngrams("abc").is_empty());
        assert_eq!(ngrams("abcd"), ["abc"]);
        assert_eq!(ngrams("jeneral"), ["jen", "jene", "jener", "jenera"]);
        assert_eq!(ngrams("75002"), ["750", "7500"]);
        let long = ngrams("saintjeandemaurienelongtoken");
        assert_eq!((long.len(), long[17]), (18, "saintjeandemaurienel"));
    }

    #[test]
    fn indexes_each_token_under_its_edge_ngrams() {
        let index = build(vec![street(), municipality()]);
        assert_eq!(words(&index, index.edge_ngram("abe")), ["aberjemen"]);
        assert_eq!(words(&index, index.edge_ngram("016")), ["01640"]);
        assert!(index.edge_ngram("monte").is_empty());
    }

    #[test]
    fn files_documents_under_their_filters() {
        let numbered = Document {
            housenumbers: Some(vec![housenumber("16", "01002_0110_00016")]),
            ..street()
        };
        let index = build(vec![numbered, municipality()]);
        assert_eq!(index.filter("type", "street"), [0]);
        assert_eq!(index.filter("type", "municipality"), [1]);
        assert_eq!(index.filter("type", "housenumber"), [0]);
        assert_eq!(index.filter("citycode", "01002"), [0, 1]);
        assert_eq!(index.filter("postcode", "01640"), [0, 1]);
        assert!(index.filter("postcode", "75002").is_empty());
    }

    #[test]
    fn files_documents_and_house_numbers_under_their_cells() {
        use crate::geohash::encode;
        let at = |lat: f64, lon: f64| encode(lat, lon).unwrap();
        let placed = |document: Document, lat: f64, lon: f64| Document {
            lat: Some(Number::Float(lat)),
            lon: Some(Number::Float(lon)),
            ..document
        };
        let numbered = Document {
            housenumbers: Some(vec![housenumber("16", "01002_0110_00016")]),
            ..placed(street(), 46.007342, 5.421924)
        };
        let nameless = Document {
            id: "01002_0111".into(),
            name: one(""),
            housenumbers: Some(vec![housenumber("2", "01002_0111_00002")]),
            ..placed(street(), 48.8566, 2.3522)
        };
        // Numbered by importance, then id: the two streets, the municipality.
        let index = build(vec![numbered, placed(municipality(), 46.008573, 5.420189), nameless]);
        // A street lies in its own cell, and in its house numbers'.
        assert!(index.geohash(at(46.007342, 5.421924)).contains(&0));
        assert!(index.geohash(at(46.006992, 5.423167)).contains(&0));
        assert!(index.geohash(at(46.008573, 5.420189)).contains(&2));
        // addok refuses an unnamed document once its house numbers are filed.
        assert!(index.geohash(at(46.006992, 5.423167)).contains(&1));
        assert!(index.geohash(at(48.8566, 2.3522)).is_empty());
    }

    #[test]
    fn files_only_the_house_number_addok_keeps_of_a_token() {
        use crate::geohash::encode;
        let at = |lat: f64, lon: f64| encode(lat, lon).unwrap();
        let elsewhere = HouseNumber {
            lat: Number::Float(45.0),
            lon: Number::Float(4.0),
            ..housenumber("50bis", "01002_0110_00050_bis")
        };
        // `50bis` and `50B` share the token `50b`: the street's last is kept.
        let numbered = Document {
            housenumbers: Some(vec![elsewhere, housenumber("50B", "01002_0110_00050_b")]),
            ..street()
        };
        let index = build(vec![numbered]);
        assert!(index.geohash(at(45.0, 4.0)).is_empty());
        assert_eq!(index.geohash(at(46.006992, 5.423167)), [0]);
    }

    #[test]
    fn indexes_a_document_without_name_under_nothing() {
        let nameless = Document {
            name: one(""),
            ..municipality()
        };
        let index = build(vec![street(), nameless]);
        assert_eq!(index.id(1), "01002");
        let varei = intersect(&[index.postings("varei").unwrap()], 10);
        assert_eq!(varei, [(0, 0.25918)]);
        assert!(index.filter("type", "municipality").is_empty());
        assert_eq!(index.filter("postcode", "01640"), [0]);
    }

    #[test]
    fn keeps_documents_as_written() {
        let index = build(vec![municipality()]);
        assert_eq!(index.document(0), municipality());
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn finds_a_house_number_by_its_token_as_written() {
        let mut exact = housenumber("1 bis", "01002_0110_00001_bis");
        exact.x = Number::Integer(887475);
        exact.ban_id = Some("f6f6b2c3-1d5e-4b9a-8f7e-0123456789ab".into());
        let mut odd = housenumber("13", "01002_0110_00013");
        odd.ban_id = Some("not-a-uuid".into());
        let street = Document {
            housenumbers: Some(vec![exact.clone(), odd.clone()]),
            ..street()
        };
        let index = build(vec![street]);
        assert_eq!(index.housenumber(0, "1b"), Some(exact));
        assert_eq!(index.housenumber(0, "trez"), Some(odd));
        assert_eq!(index.housenumber(0, "un"), None);
        assert_eq!(index.document(0).housenumbers, None);
    }

    #[test]
    fn keeps_a_street_s_last_house_number_of_a_token() {
        // As addok's dict does: "1bis" then "1 bis" both make "1b".
        let first = housenumber("1bis", "01002_0110_00001_bis");
        let last = housenumber("1 bis", "01002_0110_00001_b");
        let street = Document {
            housenumbers: Some(vec![first, last.clone()]),
            ..street()
        };
        let index = build(vec![street]);
        assert_eq!(index.housenumber(0, "1b"), Some(last));
    }

    #[test]
    fn writes_apart_what_its_columns_cannot_hold() {
        // An id the BAN's rule does not give, a longitude fixed point cannot
        // hold, a banId that is no canonical UUID: all read back as written.
        let mut odd = housenumber("16", "01002_0110_00016_x");
        odd.lon = Number::Float(5.4231675);
        let street = Document {
            ban_id: Some("not-a-uuid".into()),
            housenumbers: Some(vec![odd.clone()]),
            ..street()
        };
        let uuid = Document {
            ban_id: Some("f6f6b2c3-1d5e-4b9a-8f7e-0123456789ab".into()),
            ..municipality()
        };
        let index = build(vec![street.clone(), uuid.clone()]);
        assert_eq!(index.housenumber(0, &housenumber_token("16")), Some(odd));
        let without_numbers = Document {
            housenumbers: None,
            ..street
        };
        assert_eq!(index.document(0), without_numbers);
        assert_eq!(index.document(1), uuid);
    }
}
