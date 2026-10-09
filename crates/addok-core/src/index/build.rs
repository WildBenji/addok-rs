//! Building the index: documents in, in any order, sections out, in
//! tie-break order. Documents are compacted as they come, so that the BAN never sits
//! in memory as parsed: strings interned, house numbers in columns. Posting
//! lists, pairs and filters gather by input position, then are renumbered
//! once every document is in and the order known.

use std::collections::HashMap;
use std::io::{self, Write};

use super::encoding::{self, SCALES};
use super::format::{Kind, Writer, hash_table};
use super::{TokenId, doc_filters, document_tokens, edge_ngrams, housenumber_token, importance};
use crate::document::{Document, HouseNumber, Number, Text};
use crate::geohash::{self, Cell};
use crate::postings::DocId;

#[derive(Default)]
pub(super) struct Builder {
    strings: Interner,
    lists: Lists,
    documents: Documents,
    housenumbers: HouseNumbers,
    numbers: Numbers,
    vocabulary: Vocabulary,
    /// The documents each filter holds, by input position.
    filters: HashMap<String, Vec<DocId>>,
    /// The cells documents lie in, by input position, each pair once.
    geohashes: Vec<(Cell, DocId)>,
}

impl Builder {
    pub(super) fn add(&mut self, mut doc: Document) {
        let input = DocId::try_from(self.documents.ids.len()).expect("fewer than 2^32 documents");
        let housenumbers = doc.housenumbers.take().unwrap_or_default();
        // addok's `HousenumbersIndexer` runs first, on the house numbers
        // kept, and is not undone when the next one refuses a document
        // unnamed.
        let (housenumbers, mut cells) = self.add_housenumbers(&doc.id, housenumbers);
        if let Some(tokens) = document_tokens(&doc) {
            self.vocabulary.add(input, tokens);
            for filter in doc_filters(&doc, housenumbers.1 > 0) {
                self.filters.entry(filter).or_default().push(input);
            }
            cells.extend(doc.lat.zip(doc.lon).and_then(|(lat, lon)| cell(lat, lon)));
        }
        cells.sort_unstable();
        cells.dedup();
        self.geohashes.extend(cells.into_iter().map(|cell| (cell, input)));
        self.add_document(input, doc, housenumbers);
    }

    /// A street's house numbers, sorted by token and, as in addok's dict,
    /// one per token: the street's last. Returns where they start in the
    /// columns and how many there are, and the cells they lie in.
    fn add_housenumbers(&mut self, street: &str, housenumbers: Vec<HouseNumber>) -> ((u32, u32), Vec<Cell>) {
        let start = self.housenumbers.numbers.len();
        let mut numbered: Vec<(u32, HouseNumber)> = housenumbers
            .into_iter()
            .map(|housenumber| (self.numbers.number(&housenumber.number), housenumber))
            .collect();
        let tokens = &self.numbers.tokens;
        numbered.reverse(); // So that sorting keeps the last of a token first.
        numbered.sort_by(|a, b| tokens[a.0 as usize].cmp(&tokens[b.0 as usize]));
        numbered.dedup_by(|later, kept| tokens[later.0 as usize] == tokens[kept.0 as usize]);
        let cells = numbered.iter().filter_map(|(_, number)| cell(number.lat, number.lon)).collect();
        for (number, housenumber) in numbered {
            self.housenumbers
                .push(&mut self.strings, street, number, housenumber);
        }
        let count = self.housenumbers.numbers.len() - start;
        let range = (u32::try_from(start).expect("fewer than 2^32 house numbers"), count as u32);
        (range, cells)
    }

    fn add_document(&mut self, input: DocId, doc: Document, housenumbers: (u32, u32)) {
        let strings = &mut self.strings;
        let documents = &mut self.documents;
        documents.ids.push(strings.intern(&doc.id));
        documents.kinds.push(strings.intern(&doc.kind));
        let mut flags = 0;
        let ban_id = match doc
            .ban_id
            .as_deref()
            .map(|text| (text, encoding::uuid_bytes(text)))
        {
            None => [0; 16],
            Some((_, Some(bytes))) => {
                flags |= encoding::BAN_ID_UUID << encoding::BAN_ID;
                bytes
            }
            Some((text, None)) => {
                flags |= encoding::BAN_ID_TEXT << encoding::BAN_ID;
                documents.ban_id_texts.push([input, strings.intern(text)]);
                [0; 16]
            }
        };
        documents.ban_ids.push(ban_id);
        let (mut tags, mut values) = (0, [0; 8]);
        for (field, text) in encoding::texts(&doc).into_iter().enumerate() {
            let (tag, value) = match text {
                Text::Absent => (encoding::ABSENT, 0),
                Text::Null => (encoding::NULL, 0),
                Text::One(value) => (encoding::ONE, strings.intern(value)),
                Text::Many(values) => (encoding::MANY, self.lists.add(strings, values)),
            };
            tags |= tag << (2 * field);
            values[field] = value;
        }
        documents.text_tags.push(tags);
        documents.texts.push(values);
        let mut numbers = [0.0; 6];
        for (i, number) in encoding::numbers(&doc).into_iter().enumerate() {
            if let Some(number) = number {
                numbers[i] = encoding::value(number);
                flags |= 1 << (2 * i);
                if let Number::Integer(_) = number {
                    flags |= 1 << (2 * i + 1);
                }
            }
        }
        documents.numbers.push(numbers);
        documents.flags.push(flags);
        documents.housenumbers.push(housenumbers);
        documents.importance.push(importance(&doc));
    }

    /// Writes the index, its documents numbered in tie-break
    /// order: importance descending, then `id` ascending. Returns each
    /// section's size.
    pub(super) fn write(self, out: impl Write) -> io::Result<Vec<(&'static str, u64)>> {
        let Builder {
            strings,
            lists,
            documents,
            housenumbers,
            numbers,
            vocabulary,
            filters,
            geohashes,
        } = self;
        let count = documents.ids.len();
        let mut order: Vec<DocId> = (0..count as DocId).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (a as usize, b as usize);
            let (id_a, id_b) = (strings.get(documents.ids[a]), strings.get(documents.ids[b]));
            documents.importance[b]
                .total_cmp(&documents.importance[a])
                .then_with(|| id_a.cmp(id_b))
        });
        let mut numbered = vec![0; count];
        for (number, &input) in (0..).zip(&order) {
            numbered[input as usize] = number;
        }
        let mut writer = Writer::new(out);
        write_tokens(&mut writer, vocabulary, &numbered)?;
        write_filters(&mut writer, filters, &numbered)?;
        write_geohashes(&mut writer, geohashes, &numbered)?;
        writer.section(Kind::Strings, &strings.blob)?;
        writer.section(Kind::StringOffsets, &strings.ends)?;
        drop(strings);
        writer.section(Kind::ListOffsets, &lists.ends)?;
        writer.section(Kind::ListItems, &lists.items)?;
        drop(lists);
        let starts =
            write_housenumbers(&mut writer, housenumbers, &documents.housenumbers, &order)?;
        write_documents(&mut writer, documents, &order, &numbered, starts)?;
        let written = numbers.written.iter().map(String::as_str);
        writer.strings(Kind::NumberWritten, Kind::NumberWrittenOffsets, written)?;
        let tokens = numbers.tokens.iter().map(String::as_str);
        writer.strings(Kind::NumberTokens, Kind::NumberTokenOffsets, tokens)?;
        writer.finish()
    }
}

/// From 0, where each of the lists ends, written one after another.
fn ends<T>(lists: &[Vec<T>]) -> Vec<u32> {
    let mut ends = vec![0u32];
    for list in lists {
        let end = ends.last().unwrap() + u32::try_from(list.len()).unwrap();
        ends.push(end);
    }
    ends
}

/// The vocabulary in byte order, then each token's posting list, pairs and
/// edge ngrams.
fn write_tokens<W: Write>(
    writer: &mut Writer<W>,
    vocabulary: Vocabulary,
    numbered: &[DocId],
) -> io::Result<()> {
    let (words, mut postings, pairs) = vocabulary.in_byte_order();
    writer.strings(
        Kind::Words,
        Kind::WordOffsets,
        words.iter().map(String::as_str),
    )?;
    let table = hash_table(words.iter().map(String::as_str));
    writer.section(Kind::WordTable, &table)?;
    for list in &mut postings {
        for entry in list.iter_mut() {
            entry.0 = numbered[entry.0 as usize];
        }
        list.sort_unstable_by_key(|&(doc, _)| doc);
        debug_assert!(list.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }
    writer.section(Kind::PostingOffsets, &ends(&postings))?;
    writer.begin(Kind::PostingIds);
    for list in &postings {
        writer.extend(&list.iter().map(|&(doc, _)| doc).collect::<Vec<_>>())?;
    }
    writer.end()?;
    writer.begin(Kind::PostingScores);
    for list in &postings {
        writer.extend(&list.iter().map(|&(_, score)| score).collect::<Vec<_>>())?;
    }
    writer.end()?;
    drop(postings);
    writer.section(Kind::PairOffsets, &ends(&pairs))?;
    writer.begin(Kind::Pairs);
    for paired in &pairs {
        writer.extend(paired)?;
    }
    writer.end()?;
    drop(pairs);
    let mut ngrams: HashMap<&str, Vec<TokenId>> = HashMap::new();
    for (token, word) in (0..).zip(&words) {
        for ngram in edge_ngrams(word) {
            ngrams.entry(ngram).or_default().push(token);
        }
    }
    let mut ngrams: Vec<(&str, Vec<TokenId>)> = ngrams.into_iter().collect();
    ngrams.sort_unstable_by(|a, b| a.0.cmp(b.0));
    writer.strings(
        Kind::NgramKeys,
        Kind::NgramKeyOffsets,
        ngrams.iter().map(|(ngram, _)| *ngram),
    )?;
    let lists: Vec<Vec<TokenId>> = ngrams.into_iter().map(|(_, tokens)| tokens).collect();
    writer.section(Kind::NgramOffsets, &ends(&lists))?;
    writer.begin(Kind::Ngrams);
    for tokens in &lists {
        writer.extend(tokens)?;
    }
    writer.end()
}

/// The filters, by `name|value` in byte order, each its documents.
fn write_filters<W: Write>(
    writer: &mut Writer<W>,
    filters: HashMap<String, Vec<DocId>>,
    numbered: &[DocId],
) -> io::Result<()> {
    let mut filters: Vec<(String, Vec<DocId>)> = filters.into_iter().collect();
    filters.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (_, documents) in &mut filters {
        for doc in documents.iter_mut() {
            *doc = numbered[*doc as usize];
        }
        documents.sort_unstable();
        documents.dedup();
    }
    writer.strings(
        Kind::FilterKeys,
        Kind::FilterKeyOffsets,
        filters.iter().map(|(key, _)| key.as_str()),
    )?;
    let lists: Vec<Vec<DocId>> = filters
        .into_iter()
        .map(|(_, documents)| documents)
        .collect();
    writer.section(Kind::FilterOffsets, &ends(&lists))?;
    writer.begin(Kind::Filters);
    for documents in &lists {
        writer.extend(documents)?;
    }
    writer.end()
}

/// The cell of a position, as addok's `index_geohash` files it.
fn cell(lat: Number, lon: Number) -> Option<Cell> {
    geohash::encode(lat.value(), lon.value())
}

/// The cells, ascending, each its documents: addok's `g|<geohash>` sets.
fn write_geohashes<W: Write>(
    writer: &mut Writer<W>,
    mut geohashes: Vec<(Cell, DocId)>,
    numbered: &[DocId],
) -> io::Result<()> {
    for (_, doc) in &mut geohashes {
        *doc = numbered[*doc as usize];
    }
    geohashes.sort_unstable();
    let mut cells: Vec<Cell> = geohashes.iter().map(|&(cell, _)| cell).collect();
    cells.dedup();
    let mut ends = vec![0u32];
    for window in geohashes.chunk_by(|a, b| a.0 == b.0) {
        ends.push(ends.last().unwrap() + u32::try_from(window.len()).expect("fewer than 2^32 cell entries"));
    }
    writer.section(Kind::GeohashCells, &cells)?;
    writer.section(Kind::GeohashOffsets, &ends)?;
    let documents: Vec<DocId> = geohashes.into_iter().map(|(_, doc)| doc).collect();
    writer.section(Kind::Geohashes, &documents)
}

/// The house numbers, street after street in the documents' order. Returns,
/// from 0, where each document's end in the written columns.
fn write_housenumbers<W: Write>(
    writer: &mut Writer<W>,
    housenumbers: HouseNumbers,
    ranges: &[(u32, u32)],
    order: &[DocId],
) -> io::Result<Vec<u32>> {
    let HouseNumbers {
        numbers,
        ban_ids,
        coordinates,
        flags,
        ids,
        coordinate_exceptions,
        ban_id_texts,
    } = housenumbers;
    let mut ends = vec![0u32];
    for &input in order {
        let end = ends.last().unwrap() + ranges[input as usize].1;
        ends.push(end);
    }
    fn column<T: bytemuck::Pod, W: Write>(
        writer: &mut Writer<W>,
        kind: Kind,
        values: Vec<T>,
        ranges: &[(u32, u32)],
        order: &[DocId],
    ) -> io::Result<()> {
        writer.begin(kind);
        for &input in order {
            let (start, count) = ranges[input as usize];
            writer.extend(&values[start as usize..(start + count) as usize])?;
        }
        writer.end()
    }
    column(writer, Kind::HnNumbers, numbers, ranges, order)?;
    column(writer, Kind::HnBanIds, ban_ids, ranges, order)?;
    column(writer, Kind::HnCoordinates, coordinates, ranges, order)?;
    column(writer, Kind::HnFlags, flags, ranges, order)?;
    // An exception's position among the written house numbers: its
    // document's start there, plus its rank within the document.
    let mut written_start = vec![0u32; ranges.len()];
    for (number, &input) in order.iter().enumerate() {
        written_start[input as usize] = ends[number];
    }
    let written = |position: u32| {
        let input = ranges.partition_point(|&(start, _)| start <= position) - 1;
        written_start[input] + position - ranges[input].0
    };
    let mut ids: Vec<[u32; 2]> = ids
        .into_iter()
        .map(|(position, id)| [written(position), id])
        .collect();
    ids.sort_unstable();
    writer.section(Kind::HnIdExceptions, &ids)?;
    let mut exceptions: Vec<(u32, [f64; 4])> = coordinate_exceptions
        .into_iter()
        .map(|(position, values)| (written(position), values))
        .collect();
    exceptions.sort_unstable_by_key(|&(position, _)| position);
    let indexes: Vec<u32> = exceptions.iter().map(|&(position, _)| position).collect();
    writer.section(Kind::HnCoordinateExceptionIndexes, &indexes)?;
    let values: Vec<[f64; 4]> = exceptions.iter().map(|&(_, values)| values).collect();
    writer.section(Kind::HnCoordinateExceptions, &values)?;
    let mut texts: Vec<[u32; 2]> = ban_id_texts
        .into_iter()
        .map(|(position, text)| [written(position), text])
        .collect();
    texts.sort_unstable();
    writer.section(Kind::HnBanIdTexts, &texts)?;
    Ok(ends)
}

/// The documents' columns, in their order, then where each one's house
/// numbers end.
fn write_documents<W: Write>(
    writer: &mut Writer<W>,
    documents: Documents,
    order: &[DocId],
    numbered: &[DocId],
    housenumber_ends: Vec<u32>,
) -> io::Result<()> {
    fn column<T: Copy + bytemuck::Pod>(values: &[T], order: &[DocId]) -> Vec<T> {
        order.iter().map(|&input| values[input as usize]).collect()
    }
    writer.section(Kind::DocIds, &column(&documents.ids, order))?;
    writer.section(Kind::DocKinds, &column(&documents.kinds, order))?;
    writer.section(Kind::DocBanIds, &column(&documents.ban_ids, order))?;
    writer.section(Kind::DocTextTags, &column(&documents.text_tags, order))?;
    writer.section(Kind::DocTexts, &column(&documents.texts, order))?;
    writer.section(Kind::DocNumbers, &column(&documents.numbers, order))?;
    writer.section(Kind::DocFlags, &column(&documents.flags, order))?;
    let mut texts: Vec<[u32; 2]> = documents
        .ban_id_texts
        .iter()
        .map(|&[input, text]| [numbered[input as usize], text])
        .collect();
    texts.sort_unstable();
    writer.section(Kind::DocBanIdTexts, &texts)?;
    writer.section(Kind::DocHouseNumbers, &housenumber_ends)
}

/// Strings interned: each distinct one written once, numbered as met.
struct Interner {
    blob: Vec<u8>,
    /// From 0, where each string ends in `blob`.
    ends: Vec<u32>,
    numbers: HashMap<Box<str>, u32>,
}

impl Default for Interner {
    fn default() -> Self {
        Interner {
            blob: Vec::new(),
            ends: vec![0],
            numbers: HashMap::new(),
        }
    }
}

impl Interner {
    fn intern(&mut self, string: &str) -> u32 {
        if let Some(&number) = self.numbers.get(string) {
            return number;
        }
        let number = u32::try_from(self.ends.len() - 1).expect("fewer than 2^32 strings");
        self.blob.extend_from_slice(string.as_bytes());
        self.ends
            .push(u32::try_from(self.blob.len()).expect("strings under 4 GiB"));
        self.numbers.insert(string.into(), number);
        number
    }

    fn get(&self, number: u32) -> &str {
        let range = self.ends[number as usize] as usize..self.ends[number as usize + 1] as usize;
        std::str::from_utf8(&self.blob[range]).unwrap()
    }
}

/// Lists of strings: a field's values when it holds a list.
struct Lists {
    items: Vec<u32>,
    /// From 0, where each list ends in `items`.
    ends: Vec<u32>,
}

impl Default for Lists {
    fn default() -> Self {
        Lists {
            items: Vec::new(),
            ends: vec![0],
        }
    }
}

impl Lists {
    fn add(&mut self, strings: &mut Interner, values: &[String]) -> u32 {
        let number = u32::try_from(self.ends.len() - 1).expect("fewer than 2^32 lists");
        self.items
            .extend(values.iter().map(|value| strings.intern(value)));
        self.ends
            .push(u32::try_from(self.items.len()).expect("fewer than 2^32 list items"));
        number
    }
}

/// Documents as they come, compacted into columns.
#[derive(Default)]
struct Documents {
    ids: Vec<u32>,
    kinds: Vec<u32>,
    ban_ids: Vec<[u8; 16]>,
    text_tags: Vec<u16>,
    texts: Vec<[u32; 8]>,
    numbers: Vec<[f64; 6]>,
    flags: Vec<u16>,
    /// A banId that is no canonical UUID: the document, its text.
    ban_id_texts: Vec<[u32; 2]>,
    /// Where each document's house numbers start in `HouseNumbers`, and how
    /// many it has.
    housenumbers: Vec<(u32, u32)>,
    importance: Vec<f64>,
}

/// House numbers as they come, compacted into columns, each street's sorted
/// by token.
#[derive(Default)]
struct HouseNumbers {
    numbers: Vec<u32>,
    ban_ids: Vec<[u8; 16]>,
    coordinates: Vec<[i32; 4]>,
    flags: Vec<u8>,
    /// Written apart, by position in the columns: ids that are not derived
    /// from their street's, coordinates fixed point cannot hold, banIds that
    /// are no canonical UUIDs.
    ids: Vec<(u32, u32)>,
    coordinate_exceptions: Vec<(u32, [f64; 4])>,
    ban_id_texts: Vec<(u32, u32)>,
}

impl HouseNumbers {
    fn push(
        &mut self,
        strings: &mut Interner,
        street: &str,
        number: u32,
        housenumber: HouseNumber,
    ) {
        let position = u32::try_from(self.numbers.len()).expect("fewer than 2^32 house numbers");
        let mut flags = 0;
        let derived = encoding::derived_id(street, &housenumber.number);
        if derived.as_deref() != Some(housenumber.id.as_str()) {
            flags |= encoding::HN_ID_EXCEPTION;
            self.ids.push((position, strings.intern(&housenumber.id)));
        }
        let ban_id = match housenumber
            .ban_id
            .as_deref()
            .map(|text| (text, encoding::uuid_bytes(text)))
        {
            None => {
                flags |= encoding::HN_BAN_ID_NULL;
                [0; 16]
            }
            Some((_, Some(bytes))) => bytes,
            Some((text, None)) => {
                flags |= encoding::HN_BAN_ID_TEXT;
                self.ban_id_texts.push((position, strings.intern(text)));
                [0; 16]
            }
        };
        let written = [
            housenumber.x,
            housenumber.y,
            housenumber.lon,
            housenumber.lat,
        ];
        let values = written.map(encoding::value);
        let mut coordinates = [0; 4];
        for (i, &coordinate) in written.iter().enumerate() {
            if let Number::Integer(_) = coordinate {
                flags |= 1 << i;
            }
        }
        match std::array::from_fn::<_, 4, _>(|i| encoding::fixed(values[i], SCALES[i])) {
            fixed if fixed.iter().all(Option::is_some) => coordinates = fixed.map(Option::unwrap),
            _ => {
                flags |= encoding::HN_COORDINATES_EXCEPTION;
                self.coordinate_exceptions.push((position, values));
            }
        }
        self.numbers.push(number);
        self.ban_ids.push(ban_id);
        self.coordinates.push(coordinates);
        self.flags.push(flags);
    }
}

/// The distinct house numbers as written, each with its token: 62,197 for
/// the 26 M house numbers of the 2026-10-02 NDJSON.
#[derive(Default)]
struct Numbers {
    written: Vec<String>,
    tokens: Vec<String>,
    numbers: HashMap<String, u32>,
}

impl Numbers {
    fn number(&mut self, written: &str) -> u32 {
        if let Some(&number) = self.numbers.get(written) {
            return number;
        }
        let number = u32::try_from(self.written.len()).expect("fewer than 2^32 house numbers");
        self.tokens.push(housenumber_token(written));
        self.written.push(written.to_owned());
        self.numbers.insert(written.to_owned(), number);
        number
    }
}

/// Tokens being numbered, with their posting lists and pairs.
#[derive(Default)]
struct Vocabulary {
    numbers: HashMap<String, usize>,
    words: Vec<String>,
    /// By input position: the documents are numbered once all are in.
    postings: Vec<Vec<(DocId, f64)>>,
    pairs: Vec<Vec<usize>>,
    /// How long each token's pairs were when last deduplicated.
    deduplicated: Vec<usize>,
}

impl Vocabulary {
    /// A document's tokens: into their posting lists, and paired with each
    /// other (addok's `PairsIndexer`).
    fn add(&mut self, doc: DocId, tokens: Vec<(String, f64)>) {
        let mut met = Vec::with_capacity(tokens.len());
        for (word, score) in tokens {
            let token = match self.numbers.get(&word) {
                Some(&token) => token,
                None => {
                    let token = self.words.len();
                    self.numbers.insert(word.clone(), token);
                    self.words.push(word);
                    self.postings.push(Vec::new());
                    self.pairs.push(Vec::new());
                    self.deduplicated.push(0);
                    token
                }
            };
            self.postings[token].push((doc, score));
            met.push(token);
        }
        for &token in &met {
            let pairs = &mut self.pairs[token];
            pairs.extend(met.iter().filter(|&&other| other != token));
            // Common tokens meet the same pairs over and over: deduplicate
            // as they grow, which bounds memory and costs little.
            if pairs.len() > 2 * self.deduplicated[token] + 64 {
                pairs.sort_unstable();
                pairs.dedup();
                self.deduplicated[token] = pairs.len();
            }
        }
    }

    /// The tokens renumbered in byte order: their words, posting lists and
    /// pairs, by number.
    #[allow(clippy::type_complexity)]
    fn in_byte_order(mut self) -> (Vec<String>, Vec<Vec<(DocId, f64)>>, Vec<Vec<TokenId>>) {
        let mut order: Vec<usize> = (0..self.words.len()).collect();
        order.sort_unstable_by(|&a, &b| self.words[a].cmp(&self.words[b]));
        let mut renumbered = vec![0; order.len()];
        for (token, &met) in (0..).zip(&order) {
            renumbered[met] = token;
        }
        let mut words = Vec::with_capacity(order.len());
        let mut postings = Vec::with_capacity(order.len());
        let mut pairs = Vec::with_capacity(order.len());
        for &met in &order {
            let mut paired: Vec<TokenId> = self.pairs[met]
                .iter()
                .map(|&other| renumbered[other])
                .collect();
            paired.sort_unstable();
            paired.dedup();
            self.pairs[met] = Vec::new(); // Freed as we go.
            words.push(std::mem::take(&mut self.words[met]));
            postings.push(std::mem::take(&mut self.postings[met]));
            pairs.push(paired);
        }
        (words, postings, pairs)
    }
}
