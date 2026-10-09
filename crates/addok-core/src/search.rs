//! Search: what addok answers to a query, ported from addok 1.3.2's `Search`
//! (addok/core.py) with the preprocessors, collectors and result processors
//! the BAN configures, and addok-france's labels. Fuzzy on, autocomplete off
//! as addok-csv calls it or on as addok's `/search` does by default,
//! filters, and a position to search around. Ported to answer as addok
//! does, quirks included, but for ties: where addok's order follows
//! Python's hash seed or Redis's, results rank by score, then by document
//! number.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use crate::document::{Document, HouseNumber, Number, TextRef};
use crate::geohash;
use crate::index::{Index, TokenId};
use crate::postings::{self, DocId, PostingList};
use crate::text::{self, QueryTooLong};

/// addok's settings, as the BAN leaves them.
const BUCKET_MIN: usize = 10;
const BUCKET_MAX: usize = 100;
const COMMON_THRESHOLD: usize = 10_000;
const INTERSECT_LIMIT: usize = 100_000;
const MATCH_THRESHOLD: f64 = 0.9;
const MIN_SCORE: f64 = 0.1;
const IMPORTANCE_WEIGHT: f64 = 0.1;
/// `Search.MAX_MEANINGFUL`.
const MAX_MEANINGFUL: usize = 10;
/// How many of a common token's documents the `manual_scan` script looks
/// at: its `ZREVRANGE 0 500`.
const MANUAL_SCAN: usize = 501;
/// addok's `MAX_FILTER_VALUES`: the values of one filter it considers.
const MAX_FILTER_VALUES: usize = 10;
/// addok's `GEO_DISTANCE_WEIGHT`.
pub(crate) const GEO_DISTANCE_WEIGHT: f64 = 0.1;

/// addok's `FUZZY_KEY_MAP`: the keys around each letter of an AZERTY keyboard.
const FUZZY_KEY_MAP: [(char, &str); 26] = [
    ('a', "ezqop"),
    ('z', "aqse"),
    ('e', "azsdryu"),
    ('r', "edft"),
    ('t', "rfgy"),
    ('y', "teghu"),
    ('u', "yehji"),
    ('i', "ujko"),
    ('o', "iaklp"),
    ('p', "oalm"),
    ('q', "azsw"),
    ('s', "qzedxw"),
    ('d', "serfcx"),
    ('f', "drtgvc"),
    ('g', "ftyhbv"),
    ('h', "gyujnb"),
    ('j', "huikn"),
    ('k', "jil"),
    ('l', "kom"),
    ('m', "lpu"),
    ('w', "qsx"),
    ('x', "wsdc"),
    ('c', "xdfvio"),
    ('v', "cfgb"),
    ('b', "vghn"),
    ('n', "bhj"),
];

/// A search result, as addok's `Result` hands it to addok-csv.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub doc: DocId,
    /// As the NDJSON writes it, its house numbers apart.
    pub document: Document,
    /// The house number the query matched, if any: the result is then that
    /// house number, with its id and coordinates.
    pub housenumber: Option<HouseNumber>,
    /// The labels addok compared with the query, the result's label first.
    pub labels: Vec<String>,
    /// The document's importance, weighted: the first score addok sums.
    pub importance: f64,
    /// The best bigram similarity of a label with the query: the second.
    pub str_distance: f64,
    pub score: f64,
    /// Its distance to the position searched around, in metres: addok's
    /// `result.distance`; none without one.
    pub distance: Option<f64>,
}

impl Found {
    /// What addok-csv writes as `result_label`.
    pub fn label(&self) -> &str {
        &self.labels[0]
    }

    /// addok's `result.id`: the house number's when one matched.
    pub fn id(&self) -> &str {
        self.housenumber
            .as_ref()
            .map_or(&self.document.id, |number| &number.id)
    }

    /// addok's `result.type`.
    pub fn kind(&self) -> &str {
        match self.housenumber {
            Some(_) => "housenumber",
            None => &self.document.kind,
        }
    }
}

/// The filters a search keeps only the documents of, by the BAN's `FILTERS`:
/// for each, the values a document may hold, any of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// `type`: `housenumber`, `street`, `locality` or `municipality`.
    pub kind: Vec<String>,
    /// The commune's INSEE code.
    pub citycode: Vec<String>,
    pub postcode: Vec<String>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.kind.is_empty() && self.citycode.is_empty() && self.postcode.is_empty()
    }

    /// addok's `_build_filters`, which makes them one key: each filter's
    /// values trimmed, empty ones dropped, the first 10 distinct kept, their
    /// documents united; then the filters' documents intersected. `None`
    /// without filters. Never copied when it can be helped: a lone value's
    /// documents are the index's own list, several values' union the index
    /// keeps once made. `type=street` alone holds over a million.
    pub(crate) fn documents<'a, B: AsRef<[u8]>>(&self, index: &'a Index<B>) -> Option<FilterDocs<'a>> {
        let mut sets: Vec<FilterDocs<'a>> = Vec::new();
        for (name, values) in [("type", &self.kind), ("citycode", &self.citycode), ("postcode", &self.postcode)] {
            let mut distinct: Vec<&str> = Vec::new();
            for value in values.iter().map(|value| value.trim()) {
                if !value.is_empty() && !distinct.contains(&value) {
                    distinct.push(value);
                }
            }
            distinct.truncate(MAX_FILTER_VALUES);
            if distinct.is_empty() {
                continue;
            }
            sets.push(match distinct[..] {
                [value] => FilterDocs::Index(index.filter(name, value)),
                _ => FilterDocs::Shared(index.filter_union(name, &distinct)),
            });
        }
        match sets.len() {
            0 => None,
            1 => sets.pop(),
            _ => {
                let sets: Vec<&[DocId]> = sets.iter().map(|set| &**set).collect();
                Some(FilterDocs::Shared(postings::intersect_sets(&sets).into()))
            }
        }
    }

    /// addok's `_setup_housenumber_checks`, on the `type` values as given:
    /// whether to match the query's house number, and whether a result must
    /// have one.
    pub(crate) fn housenumber_checks(&self) -> (bool, bool) {
        if self.kind.is_empty() {
            return (true, false);
        }
        let only = self.kind.iter().all(|kind| kind == "housenumber");
        (self.kind.iter().any(|kind| kind == "housenumber"), only)
    }
}

/// The documents the filters hold: the index's own list, or a set made of
/// several, shared.
pub(crate) enum FilterDocs<'a> {
    Index(&'a [DocId]),
    Shared(Arc<[DocId]>),
}

impl Deref for FilterDocs<'_> {
    type Target = [DocId];

    fn deref(&self) -> &[DocId] {
        match self {
            FilterDocs::Index(docs) => docs,
            FilterDocs::Shared(docs) => docs,
        }
    }
}

/// The filters' key among a search's keys, as addok's `f|…` keys sit among
/// its `w|…` ones: no token can be it, holding a `|`.
const FILTER_KEY: &str = "f|";

/// The geohash key among a search's keys, as addok's `gx|…` key sits among
/// them: the documents of the nine cells around the position.
const GEOHASH_KEY: &str = "gx|";

/// Whether a key is a word's, not the filters' or the geohash's: addok's
/// `w|` keys.
fn is_word(key: &str) -> bool {
    key != FILTER_KEY && key != GEOHASH_KEY
}

/// A position to search around: addok's `lat` and `lon`, both given.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Center {
    pub lat: f64,
    pub lon: f64,
}

/// What addok's `search` takes beside the query: the results wanted,
/// autocomplete, the filters and a position.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    pub limit: usize,
    pub autocomplete: bool,
    pub filters: Filters,
    pub center: Option<Center>,
}

/// A document's scores, as a `Found` holds them.
#[derive(Debug, Clone, Copy)]
struct Scored {
    importance: f64,
    str_distance: f64,
    score: f64,
    distance: Option<f64>,
}

/// addok-csv's `search(q, autocomplete=False, limit=limit)`: the results,
/// best first, at most `limit` of them and none under addok's minimum score.
pub fn search<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    limit: usize,
) -> Result<Vec<Found>, QueryTooLong> {
    search_filtered(index, query, limit, &Filters::default())
}

/// `search`, keeping only the documents the filters hold: addok's
/// `search(q, autocomplete=False, limit=limit, **filters)`.
pub fn search_filtered<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    limit: usize,
    filters: &Filters,
) -> Result<Vec<Found>, QueryTooLong> {
    let options = Options { limit, filters: filters.clone(), ..Options::default() };
    search_with(index, query, &options)
}

/// `search_filtered`, the query's last word taken as the start of one:
/// addok's `search(q, autocomplete=True, limit=limit, **filters)`, its
/// `/search` by default.
pub fn search_autocomplete<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    limit: usize,
    filters: &Filters,
) -> Result<Vec<Found>, QueryTooLong> {
    let options = Options { limit, autocomplete: true, filters: filters.clone(), center: None };
    search_with(index, query, &options)
}

/// addok's `search(q, limit=…, autocomplete=…, lat=…, lon=…, **filters)`.
pub fn search_with<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    options: &Options,
) -> Result<Vec<Found>, QueryTooLong> {
    let mut helper = Helper::new(index, query, options)?;
    helper.collect();
    Ok(helper.render())
}

/// What a search did, for explaining an answer that differs from
/// addok's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    /// Its steps, worded as addok's `Search(verbose=True)` logs them, so
    /// that the two compare line by line.
    pub steps: Vec<String>,
    /// The documents its cuts kept or dropped among equal scores, ascending:
    /// where addok-rs's tie-break and addok's part.
    pub tied: Vec<DocId>,
}

/// The orders search sets where addok's follow its build or its
/// hash seed: which of the documents tied at a cut it keeps
/// (Redis keeps those its internal ids, numbered as the build goes, rank
/// first), in which order it groups tokens into relations (addok, in
/// a Python set's), and in which order it tries the words of equal score
/// that autocomplete finds (addok, in a Lua sort's, unstable, of a Redis
/// set's). `search` keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    /// Keep the highest document numbers of a tie group, not the lowest.
    pub ties_reversed: bool,
    /// The order to take tokens in when grouping them into relations, by
    /// their words: the one addok's set took, say. Those it leaves out come
    /// after, by bytes; all of them when it is empty.
    pub tokens: Vec<String>,
    /// The order to try autocomplete's words of equal score in, by their
    /// words: the one addok's Lua sort returned, say. Those it leaves out
    /// come after, by bytes; all of them when it is empty.
    pub candidates: Vec<String>,
}

/// `search_with` in the order given, and its trace.
pub fn search_traced<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    options: &Options,
    order: &Order,
) -> Result<(Vec<Found>, Trace), QueryTooLong> {
    let filters = &options.filters;
    let mut helper = Helper::new(index, query, options)?;
    helper.order = order.clone();
    helper.trace = Some(Tracing::default());
    helper.debug(|h| format!("Taken tokens: {}", tokens(&h.meaningful)));
    helper.debug(|h| format!("Common tokens: {}", tokens(&h.common)));
    helper.debug(|h| format!("Housenumbers token: {}", h.housenumber));
    helper.debug(|h| format!("Not found tokens: {}", tokens(&h.not_found)));
    helper.debug(|_| format!("Filters: {filters:?}"));
    helper.collect();
    let tracing = helper.trace.take().unwrap_or_default();
    let mut tied: Vec<DocId> = tracing.tied.into_iter().collect();
    tied.sort_unstable();
    Ok((
        helper.render(),
        Trace {
            steps: tracing.steps,
            tied,
        },
    ))
}

/// A collector, by its name in addok.
type Collector<'i, B> = (&'static str, fn(&mut Helper<'i, B>) -> bool);

/// A trace being made.
#[derive(Default)]
struct Tracing {
    steps: Vec<String>,
    tied: HashSet<DocId>,
}

/// Tokens as addok logs them.
fn tokens(tokens: &[Token]) -> String {
    let tokens: Vec<String> = tokens
        .iter()
        .map(|token| format!("<Token {}>", token.value))
        .collect();
    format!("[{}]", tokens.join(", "))
}

/// Keys as addok logs them.
fn keys(keys: &[String]) -> String {
    let key = |key: &String| match is_word(key) {
        true => format!("'w|{key}'"),
        false => format!("'{key}'"),
    };
    let keys: Vec<String> = keys.iter().map(key).collect();
    format!("[{}]", keys.join(", "))
}

/// A query's token, with what search knows of it.
#[derive(Debug, Clone)]
struct Token {
    value: String,
    housenumber: bool,
    position: Vec<usize>,
    is_last: bool,
    /// Its posting list's length, 0 when the index does not hold it.
    frequency: usize,
    /// Whether the index holds it: addok's `db_key`.
    found: bool,
}

impl Token {
    fn is_common(&self) -> bool {
        self.frequency > COMMON_THRESHOLD
    }

    /// Python's `str.isdigit()`, on a token folded to ASCII.
    fn is_digit(&self) -> bool {
        !self.value.is_empty() && self.value.bytes().all(|byte| byte.is_ascii_digit())
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }
}

fn holds(tokens: &[Token], value: &str) -> bool {
    tokens.iter().any(|token| token.value == value)
}

/// A query's search, addok's `Search` helper: its tokens, its bucket of
/// candidate documents, and the results made of them so far.
struct Helper<'i, B> {
    index: &'i Index<B>,
    wanted: usize,
    /// The query folded, which search tokenizes and labels are compared with.
    query: String,
    /// Its bigrams, counted.
    bigrams: QueryBigrams,
    /// Every token but the house number's, longest first.
    tokens: Vec<Token>,
    /// The house-number tokens joined, by position: what a street's house
    /// numbers are matched against.
    housenumber: String,
    meaningful: Vec<Token>,
    common: Vec<Token>,
    not_found: Vec<Token>,
    last_token: Option<Token>,
    /// The meaningful tokens' words, addok's `helper.keys`.
    keys: Vec<String>,
    matched_keys: HashSet<String>,
    should_match_threshold: usize,
    bucket: HashSet<DocId>,
    /// The bucket's documents scored so far.
    results: HashMap<DocId, Scored>,
    /// The documents the filters hold, `FILTER_KEY` among the keys.
    filter: Option<Rc<FilterDocs<'i>>>,
    /// Whether to match the query's house number, and whether a result must
    /// have one: the `type` filter's say.
    check_housenumber: bool,
    only_housenumber: bool,
    /// Whether the query's last word is taken as the start of one.
    autocomplete: bool,
    /// The position to search around.
    center: Option<Center>,
    /// The documents of the cells around it, `GEOHASH_KEY` among the keys,
    /// once computed: none when there is no position, or no document near.
    geohash: Option<Option<Rc<[DocId]>>>,
    order: Order,
    trace: Option<Tracing>,
}

impl<'i, B: AsRef<[u8]>> Helper<'i, B> {
    /// The query folded and tokenized, its tokens looked up and sorted
    /// out: addok's `SEARCH_PREPROCESSORS`.
    fn new(
        index: &'i Index<B>,
        query: &str,
        options: &Options,
    ) -> Result<Self, QueryTooLong> {
        let (wanted, filters) = (options.limit, &options.filters);
        let query = text::fold(query);
        let processed = text::query_tokens(&query)?;
        let last = processed.len().checked_sub(1);
        let mut tokens: Vec<Token> = (0..)
            .zip(processed)
            .map(|(i, token)| {
                let list = index.postings(&token.value);
                Token {
                    is_last: Some(i) == last,
                    frequency: list.map_or(0, |list| list.len()),
                    found: list.is_some(),
                    value: token.value,
                    housenumber: token.housenumber,
                    position: token.position,
                }
            })
            .collect();
        let last_token = tokens.last().cloned();
        tokens.sort_by_key(|token| Reverse(token.len()));
        let mut numbers: Vec<&Token> = tokens.iter().filter(|token| token.housenumber).collect();
        numbers.sort_by(|a, b| a.position.cmp(&b.position));
        let housenumber: String = numbers.iter().map(|token| token.value.as_str()).collect();
        let tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|token| !token.housenumber)
            .collect();
        let (mut meaningful, mut common, mut not_found) = (Vec::new(), Vec::new(), Vec::new());
        for token in &tokens {
            if token.is_common() {
                common.push(token.clone());
            } else if token.found {
                meaningful.push(token.clone());
            } else {
                not_found.push(token.clone());
            }
        }
        common.sort_by_key(|token| token.frequency);
        meaningful.sort_by_key(|token| token.frequency);
        if meaningful.len() > MAX_MEANINGFUL {
            common.extend(meaningful.split_off(MAX_MEANINGFUL));
        }
        let should_match_threshold = (2.0 / 3.0 * tokens.len() as f64).ceil() as usize;
        let (check_housenumber, only_housenumber) = filters.housenumber_checks();
        Ok(Helper {
            index,
            wanted,
            bigrams: QueryBigrams::new(&query),
            query,
            tokens,
            housenumber,
            meaningful,
            common,
            not_found,
            last_token,
            keys: Vec::new(),
            matched_keys: HashSet::new(),
            should_match_threshold,
            bucket: HashSet::new(),
            results: HashMap::new(),
            filter: filters.documents(index).map(Rc::new),
            check_housenumber,
            only_housenumber,
            autocomplete: options.autocomplete,
            center: options.center,
            geohash: None,
            order: Order::default(),
            trace: None,
        })
    }

    /// A step of the trace, when tracing.
    fn debug(&mut self, step: impl FnOnce(&Self) -> String) {
        if self.trace.is_none() {
            return;
        }
        let step = step(self);
        if let Some(trace) = &mut self.trace {
            trace.steps.push(step);
        }
    }

    /// addok's `RESULTS_COLLECTORS`, in order, until one says it is done.
    fn collect(&mut self) {
        let collectors: [Collector<'i, B>; 13] = [
            (
                "ONLY_COMMONS_BUT_GEOHASH_TRY_AUTOCOMPLETE_COLLECTOR",
                Self::only_commons_but_geohash_try_autocomplete,
            ),
            (
                "NO_TOKENS_BUT_HOUSENUMBERS_AND_GEOHASH",
                Self::no_tokens_but_housenumbers_and_geohash,
            ),
            ("NO_AVAILABLE_TOKENS_ABORT", Self::no_available_tokens_abort),
            ("ONLY_COMMONS", Self::only_commons_collector),
            (
                "NO_MEANINGFUL_BUT_COMMON_TRY_AUTOCOMPLETE_COLLECTOR",
                Self::no_meaningful_but_common_try_autocomplete,
            ),
            (
                "ONLY_COMMONS_TRY_AUTOCOMPLETE_COLLECTOR",
                Self::only_commons_try_autocomplete,
            ),
            ("BUCKET_WITH_MEANINGFUL", Self::bucket_with_meaningful),
            ("REDUCE_WITH_OTHER_COMMONS", Self::reduce_with_other_commons),
            (
                "ENSURE_GEOHASH_RESULTS_ARE_INCLUDED_IF_CENTER_IS_GIVEN",
                Self::ensure_geohash_results_are_included_if_center_is_given,
            ),
            (
                "AUTOCOMPLETE_MEANINGFUL_COLLECTOR",
                Self::autocomplete_meaningful,
            ),
            ("FUZZY_COLLECTOR", Self::fuzzy),
            (
                "EXTEND_RESULTS_EXTRAPOLING_RELATIONS",
                Self::extend_results_extrapoling_relations,
            ),
            (
                "EXTEND_RESULTS_REDUCING_TOKENS",
                Self::extend_results_reducing_tokens,
            ),
        ];
        for (name, collector) in collectors {
            self.debug(|_| format!("** {name} **"));
            if collector(self) {
                break;
            }
        }
    }

    /// addok's `render`: every result, by score, then by document number;
    /// the first `wanted`, but those under the minimum score.
    fn render(mut self) -> Vec<Found> {
        self.convert();
        let mut results: Vec<(DocId, Scored)> = self.results.drain().collect();
        results.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));
        results.truncate(self.wanted);
        results.retain(|(_, scored)| scored.score >= MIN_SCORE);
        let result = |&(doc, scored)| self.result(doc, scored);
        results.iter().map(result).collect()
    }

    /// addok's `geohash_key`: whether documents lie in the cells around the
    /// position, computed once. Not with a coordinate of 0, which Python
    /// takes for false, nor a latitude python-geohash refuses.
    fn geohash_key(&mut self) -> bool {
        if self.geohash.is_none() {
            let cell = self
                .center
                .filter(|center| center.lat != 0.0 && center.lon != 0.0)
                .and_then(|center| geohash::encode(center.lat, center.lon));
            self.geohash = Some(cell.and_then(|cell| {
                let lists: Vec<&[DocId]> = geohash::expand(cell).into_iter().map(|cell| self.index.geohash(cell)).collect();
                let docs = postings::union_sets(&lists);
                self.debug(|_| match docs.is_empty() {
                    true => "Empty geohash key".to_owned(),
                    false => format!("Computed geohash key gx|{}", geohash::text(cell)),
                });
                (!docs.is_empty()).then(|| docs.into())
            }));
        }
        matches!(self.geohash, Some(Some(_)))
    }

    fn bucket_full(&self) -> bool {
        (self.wanted..BUCKET_MAX).contains(&self.bucket.len())
    }

    fn bucket_overflow(&self) -> bool {
        self.bucket.len() >= BUCKET_MAX
    }

    fn bucket_dry(&self) -> bool {
        self.bucket.len() < self.wanted
    }

    fn bucket_empty(&self) -> bool {
        self.bucket.is_empty()
    }

    fn only_commons(&self) -> bool {
        !self.tokens.is_empty() && self.tokens.len() == self.common.len()
    }

    fn pass_should_match_threshold(&self) -> bool {
        self.matched_keys.len() >= self.should_match_threshold
    }

    /// How many results match the query closely.
    fn cream(&self) -> usize {
        let close = self
            .results
            .values()
            .filter(|result| result.str_distance >= MATCH_THRESHOLD);
        close.count()
    }

    /// Whether a small bucket holds a close match, after making results of it.
    fn has_cream(&mut self) -> bool {
        if self.bucket_empty() || self.bucket_overflow() || self.bucket.len() > BUCKET_MIN {
            return false;
        }
        self.debug(|_| "Checking cream.".to_owned());
        self.convert();
        self.cream() > 0
    }

    /// The best `limit` documents holding every key, by summed score; 100 if
    /// `limit` is not positive (addok's `intersect`). Keys given, the
    /// filters' key joins them, in the caller's list too: addok extends the
    /// list it is handed, and the collectors that hand it `helper.keys` keep
    /// the filter key there, which then reaches fuzzy's pair lookups.
    fn intersect(&mut self, keys: &mut Vec<String>, limit: i64) -> Vec<DocId> {
        let index = self.index;
        if keys.is_empty() {
            return Vec::new();
        }
        let filter = self.filter.clone();
        if filter.is_some() {
            keys.push(FILTER_KEY.to_owned());
        }
        let geohash = self.geohash.clone().flatten();
        // A set alone gives all its members, as `SMEMBERS` does.
        if let ([key], Some(geohash)) = (&keys[..], &geohash)
            && key == GEOHASH_KEY
        {
            return geohash.to_vec();
        }
        let limit = match usize::try_from(limit) {
            Ok(limit) if limit > 0 => limit,
            _ => self.wanted.max(BUCKET_MAX),
        };
        let mut words: Vec<&str> = keys.iter().map(String::as_str).collect();
        words.sort_unstable();
        words.dedup();
        let mut lists = Vec::with_capacity(words.len());
        for word in words {
            if word == FILTER_KEY
                && let Some(filter) = &filter
            {
                lists.push(PostingList::set(filter));
                continue;
            }
            if word == GEOHASH_KEY
                && let Some(geohash) = &geohash
            {
                lists.push(PostingList::set(geohash));
                continue;
            }
            match index.postings(word) {
                Some(list) => lists.push(list),
                None => return Vec::new(),
            }
        }
        let found = self.best(&lists, limit);
        found.into_iter().map(|(doc, _)| doc).collect()
    }

    /// `postings::intersect`, noting the documents tied at its cut when
    /// tracing, and keeping the other end of them in reversed order.
    fn best(&mut self, lists: &[PostingList], limit: usize) -> Vec<(DocId, f64)> {
        let Some(trace) = &mut self.trace else {
            return postings::intersect(lists, limit);
        };
        let (mut best, mut at_cut) = postings::intersect_with_ties(lists, limit);
        if self.order.ties_reversed && !at_cut.is_empty() {
            // A tie at the cut leaves `best` full, its last score the cut's.
            let cut = best[best.len() - 1].1;
            let kept = best.iter().filter(|&&(_, score)| score == cut).count();
            best.retain(|&(_, score)| score != cut);
            at_cut.sort_unstable_by_key(|&doc| Reverse(doc));
            best.extend(at_cut.iter().take(kept).map(|&doc| (doc, cut)));
        }
        trace.tied.extend(at_cut);
        best
    }

    fn add_to_bucket(&mut self, keys: &mut Vec<String>) {
        self.add_to_bucket_up_to(keys, None);
    }

    /// `add_to_bucket`, to `limit` documents when given, as many as the
    /// bucket holds otherwise.
    fn add_to_bucket_up_to(&mut self, keys: &mut Vec<String>, limit: Option<i64>) {
        self.debug(|_| format!("Adding to bucket with keys {}", self::keys(keys)));
        let words = keys.iter().filter(|key| is_word(key));
        self.matched_keys.extend(words.cloned());
        let limit = limit.unwrap_or(BUCKET_MAX as i64 - self.bucket.len() as i64);
        let found = self.intersect(keys, limit);
        self.bucket.extend(found);
        self.debug(|h| format!("{} ids in bucket so far", h.bucket.len()));
    }

    fn new_bucket(&mut self, keys: &mut Vec<String>, limit: i64) {
        self.debug(|_| {
            format!(
                "New bucket with keys {} and limit {limit}",
                self::keys(keys)
            )
        });
        let words = keys.iter().filter(|key| is_word(key));
        self.matched_keys = words.cloned().collect();
        self.bucket = self.intersect(keys, limit).into_iter().collect();
        self.debug(|h| format!("{} ids in bucket so far", h.bucket.len()));
    }

    /// Results made of the documents in the bucket that have none yet:
    /// addok's `convert`. Results stay when the bucket is replaced.
    fn convert(&mut self) {
        let new: Vec<DocId> = self
            .bucket
            .iter()
            .copied()
            .filter(|doc| !self.results.contains_key(doc))
            .collect();
        for doc in new {
            if let Some(scored) = self.score(doc) {
                self.results.insert(doc, scored);
            }
        }
    }

    /// A document's scores, through addok's `SEARCH_RESULT_PROCESSORS`: the
    /// house number matched, labels made, then scores for importance and
    /// for the best label. Read in place: only the results search returns
    /// are made whole, by `result`. None for a document without the house
    /// number when the `type` filter asks for house numbers only.
    fn score(&mut self, doc: DocId) -> Option<Scored> {
        let index = self.index;
        let number = match self.housenumber.is_empty() || !self.check_housenumber {
            true => None,
            false => index.housenumber_number(doc, &self.housenumber),
        };
        if self.only_housenumber && number.is_none() {
            return None;
        }
        let kind = match number {
            Some(_) => "housenumber",
            None => index.kind(doc),
        };
        let (name, postcode, city) = (index.name(doc), index.postcode(doc), index.city(doc));
        let pieces = labels(&name, &postcode, &city, number, kind);
        let importance = index.importance(doc).map_or(0.0, Number::value) * IMPORTANCE_WEIGHT;
        let mut folded_pieces: Vec<(&str, String)> = Vec::new();
        let mut folded = String::new();
        let mut str_distance = 0.0;
        // addok's `score_by_autocomplete_distance`: the best of the labels
        // the query is, begins or is in; the bigrams, scaled, if none.
        let mut matched = false;
        if self.autocomplete {
            for label in &pieces {
                fold_label(label, &mut folded_pieces, &mut folded);
                let score = if folded == self.query {
                    1.0
                } else if folded.starts_with(self.query.as_str()) {
                    0.9
                } else if folded.contains(self.query.as_str()) {
                    0.7
                } else {
                    continue;
                };
                matched = true;
                if score >= str_distance {
                    str_distance = score;
                }
            }
        }
        if !matched {
            // addok's `score_by_ngram_distance`, or the autocomplete's own.
            let scale = if self.autocomplete { 0.9 } else { 1.0 };
            for label in &pieces {
                fold_label(label, &mut folded_pieces, &mut folded);
                let score = compare_ngrams(&folded, &self.query, &mut self.bigrams) * scale;
                if score >= str_distance {
                    str_distance = score;
                }
                if score >= MATCH_THRESHOLD {
                    break;
                }
            }
        }
        // addok's `score_by_geo_distance`: the distance to the position,
        // the house number's when one matched.
        let geo = self.center.map(|center| {
            let position = match number {
                Some(_) => index.housenumber_position(doc, &self.housenumber),
                None => index.position(doc),
            };
            let (lat, lon) = position.expect("a BAN document has a position");
            let km = haversine_distance((lat.value(), lon.value()), (center.lat, center.lon));
            (km * 1000.0, km_to_score(km) * GEO_DISTANCE_WEIGHT)
        });
        // addok sums its scores and their ceilings in this order, with
        // Python's `sum`.
        let score = match geo {
            None => python_sum(&[importance, str_distance]) / python_sum(&[IMPORTANCE_WEIGHT, 1.0]),
            Some((_, geo)) => {
                python_sum(&[importance, str_distance, geo])
                    / python_sum(&[IMPORTANCE_WEIGHT, 1.0, GEO_DISTANCE_WEIGHT])
            }
        };
        Some(Scored {
            importance,
            str_distance,
            score,
            distance: geo.map(|(distance, _)| distance),
        })
    }

    /// A document `score` scored, made a result.
    fn result(&self, doc: DocId, scored: Scored) -> Found {
        let document = self.index.document(doc);
        let housenumber = match self.housenumber.is_empty() || !self.check_housenumber {
            true => None,
            false => self.index.housenumber(doc, &self.housenumber),
        };
        let kind = match housenumber {
            Some(_) => "housenumber",
            None => document.kind.as_str(),
        };
        let pieces = labels(
            &document.name.borrowed(),
            &document.postcode.borrowed(),
            &document.city.borrowed(),
            housenumber.as_ref().map(|number| number.number.as_str()),
            kind,
        );
        let labels = pieces.iter().map(|label| label.join(" ")).collect();
        Found {
            doc,
            document,
            housenumber,
            labels,
            importance: scored.importance,
            str_distance: scored.str_distance,
            score: scored.score,
            distance: scored.distance,
        }
    }

    /// addok's `only_commons_but_geohash_try_autocomplete_collector`, which
    /// autocompletes whether autocomplete is on or not.
    fn only_commons_but_geohash_try_autocomplete(&mut self) -> bool {
        if self.geohash_key() && self.only_commons() {
            let tokens = self.tokens.clone();
            self.autocomplete(&tokens, false, true);
        }
        false
    }

    /// addok's `no_tokens_but_housenumbers_and_geohash`: a house number
    /// alone looks around the position.
    fn no_tokens_but_housenumbers_and_geohash(&mut self) -> bool {
        if self.tokens.is_empty() && !self.housenumber.is_empty() && self.geohash_key() {
            self.new_bucket(&mut vec![GEOHASH_KEY.to_owned()], BUCKET_MIN as i64);
        }
        false
    }

    /// addok's `ensure_geohash_results_are_included_if_center_is_given`.
    fn ensure_geohash_results_are_included_if_center_is_given(&mut self) -> bool {
        if self.bucket_overflow() && self.geohash_key() {
            self.debug(|_| "Bucket overflow and center, force nearby look up".to_owned());
            let mut keys = self.keys.clone();
            keys.push(GEOHASH_KEY.to_owned());
            self.add_to_bucket_up_to(&mut keys, Some(self.wanted.max(BUCKET_MIN) as i64));
        }
        false
    }

    fn no_available_tokens_abort(&mut self) -> bool {
        self.tokens.is_empty()
    }

    /// addok's `only_commons` collector. Its single key, once intersected,
    /// holds the filter key too, and so counts as more than one.
    fn only_commons_collector(&mut self) -> bool {
        if !self.only_commons() {
            return false;
        }
        let mut keys: Vec<String> = self
            .tokens
            .iter()
            .map(|token| token.value.clone())
            .collect();
        let geohash = self.geohash_key();
        if geohash {
            keys.push(GEOHASH_KEY.to_owned());
            self.debug(|_| "Adding geohash".to_owned());
        }
        if keys.len() == 1 || geohash {
            self.add_to_bucket(&mut keys);
        }
        if self.bucket_dry() && keys.len() > 1 {
            self.tokens.sort_by_key(|token| token.frequency);
            let mut keys: Vec<String> = self
                .tokens
                .iter()
                .map(|token| token.value.clone())
                .collect();
            let first = self.tokens[0].frequency;
            let filter = self.filter.as_ref().map(|filter| filter.len());
            if first < INTERSECT_LIMIT {
                self.debug(|_| "Under INTERSECT_LIMIT, force intersect.".to_owned());
                self.add_to_bucket(&mut keys);
            } else if let Some(filter) = filter {
                let mut all_keys = keys.clone();
                all_keys.push(FILTER_KEY.to_owned());
                if filter < first {
                    self.debug(|_| {
                        format!("Filter ({filter}) more selective than token ({first}), use intersect")
                    });
                    self.add_to_bucket(&mut all_keys);
                } else {
                    self.debug(|_| {
                        format!("Token ({first}) and filter ({filter}) both large, manual scan")
                    });
                    let found = self.manual_scan(&all_keys);
                    self.bucket.extend(found);
                    self.debug(|h| format!("{} results after scan", h.bucket.len()));
                }
            } else {
                self.debug(|h| {
                    format!(
                        "INTERSECT_LIMIT hit, manual scan on '{}'",
                        h.tokens[0].value
                    )
                });
                let found = self.manual_scan(&keys);
                self.bucket.extend(found);
                self.debug(|h| format!("{} results after scan", h.bucket.len()));
            }
        }
        false
    }

    /// addok's `manual_scan` script: of the first key's 501 best documents,
    /// those every other key holds, up to `wanted`. The filter key's
    /// documents are a set, as Redis's are.
    fn manual_scan(&mut self, keys: &[String]) -> Vec<DocId> {
        let index = self.index;
        let filter = self.filter.clone();
        let holds = |key: &String, doc: DocId| match (key == FILTER_KEY, &filter) {
            (true, Some(filter)) => filter.binary_search(&doc).is_ok(),
            _ => index.postings(key).is_some_and(|list| list.contains(doc)),
        };
        let Some(first) = keys.first().and_then(|key| index.postings(key)) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for (doc, _) in self.best(&[first], MANUAL_SCAN) {
            if keys[1..].iter().all(|key| holds(key, doc)) {
                candidates.push(doc);
            }
            if candidates.len() == self.wanted {
                break;
            }
        }
        candidates
    }

    fn no_meaningful_but_common_try_autocomplete(&mut self) -> bool {
        if !self.meaningful.is_empty() || self.common.is_empty() {
            return false;
        }
        self.debug(|_| "Only commons, trying autocomplete".to_owned());
        let common = self.common.clone();
        self.autocomplete(&common, false, false);
        self.meaningful = self.common[..1].to_vec();
        if !self.pass_should_match_threshold() {
            return false;
        }
        self.bucket_full() || self.bucket_overflow() || self.has_cream()
    }

    fn only_commons_try_autocomplete(&mut self) -> bool {
        if !self.only_commons() {
            return false;
        }
        let tokens = self.tokens.clone();
        self.autocomplete(&tokens, true, false);
        if !self.bucket_empty() {
            self.debug(|_| "Only common terms. Return.".to_owned());
        }
        !self.bucket_empty()
    }

    /// addok's `autocomplete_meaningful_collector`: the meaningful tokens
    /// with each word the last one begins, unless the bucket overflows.
    fn autocomplete_meaningful(&mut self) -> bool {
        if self.bucket_overflow() {
            return false;
        }
        if !self.autocomplete {
            self.debug(|_| "Autocomplete not active. Abort.".to_owned());
            return false;
        }
        let meaningful = self.meaningful.clone();
        if self.geohash_key() {
            self.autocomplete(&meaningful, false, true);
        }
        self.autocomplete(&meaningful, false, false);
        false
    }

    /// addok's `autocomplete`: the tokens the last one begins that share a
    /// document with every other, each tried with the others.
    /// With `use_geohash`, each word is tried with the geohash key too.
    fn autocomplete(&mut self, tokens: &[Token], skip_commons: bool, use_geohash: bool) {
        let index = self.index;
        let Some(last) = self.last_token.clone() else {
            return;
        };
        self.debug(|_| format!("Autocompleting {}", last.value));
        let keys: Vec<String> = tokens
            .iter()
            .filter(|token| !token.is_last)
            .map(|token| token.value.clone())
            .collect();
        let mut sets: Vec<&[TokenId]> = keys.iter().map(|key| index.pairs(key)).collect();
        sets.push(index.edge_ngram(&last.value));
        let candidates = postings::intersect_sets(&sets);
        if candidates.is_empty() {
            self.debug(|_| "No candidates. Aborting.".to_owned());
            return;
        }
        let list = |token: TokenId| index.postings(index.word(token));
        // addok's Lua scripts order them, by best score for one token, by
        // frequency otherwise, both descending; their sort is unstable, ours
        // keeps byte order among equals, or the order given.
        let mut ordered: Vec<(TokenId, f64)> = candidates
            .into_iter()
            .map(|token| {
                let list = list(token);
                let order = match tokens.len() {
                    1 => list.map_or(0.0, |list| list.max_score()),
                    _ => list.map_or(0, |list| list.len()) as f64,
                };
                (token, order)
            })
            .collect();
        let given = |token: TokenId| {
            let word = index.word(token);
            let at = self.order.candidates.iter().position(|candidate| candidate == word);
            at.unwrap_or(usize::MAX)
        };
        ordered.sort_by(|a, b| b.1.total_cmp(&a.1).then(given(a.0).cmp(&given(b.0))));
        for (token, _) in ordered {
            let word = index.word(token);
            if skip_commons && list(token).map_or(0, |list| list.len()) > COMMON_THRESHOLD {
                self.debug(|_| format!("Skip common token to autocomplete w|{word}"));
                continue;
            }
            if !self.bucket_overflow() || holds(&self.not_found, &last.value) {
                self.debug(|_| format!("Trying to extend bucket. Autocomplete w|{word}"));
                let mut extended = keys.clone();
                extended.push(word.to_owned());
                if use_geohash && self.geohash_key() {
                    extended.push(GEOHASH_KEY.to_owned());
                }
                self.add_to_bucket(&mut extended);
            }
        }
    }

    fn bucket_with_meaningful(&mut self) -> bool {
        if self.meaningful.is_empty() {
            return false;
        }
        if self.meaningful.len() == 1 && !self.common.is_empty() && self.filter.is_none() {
            // One more token, so as not to search with too few.
            let other = self
                .common
                .iter()
                .find(|token| !holds(&self.meaningful, &token.value));
            if let Some(other) = other.cloned() {
                self.meaningful.push(other);
            }
        }
        self.keys = self
            .meaningful
            .iter()
            .map(|token| token.value.clone())
            .collect();
        // Handed `helper.keys` itself, which keeps the filter key.
        let mut keys = std::mem::take(&mut self.keys);
        if self.bucket_empty() {
            self.new_bucket(&mut keys, BUCKET_MIN as i64);
            if self.bucket.len() == BUCKET_MIN {
                self.new_bucket(&mut keys, 0);
            }
            self.keys = keys;
            // Autocomplete computes before cream is checked.
            if !self.autocomplete && self.has_cream() && self.cream() < BUCKET_MIN {
                self.debug(|_| "Cream found. Returning.".to_owned());
                return true;
            }
        } else {
            self.add_to_bucket(&mut keys);
            self.keys = keys;
        }
        false
    }

    fn reduce_with_other_commons(&mut self) -> bool {
        if self.only_commons() {
            return false;
        }
        for token in self.common.clone() {
            if !holds(&self.meaningful, &token.value) && self.bucket_overflow() {
                self.debug(|_| format!("Now considering also common token {}", token.value));
                self.meaningful.push(token);
                self.keys = self
                    .meaningful
                    .iter()
                    .map(|token| token.value.clone())
                    .collect();
                let mut keys = std::mem::take(&mut self.keys);
                self.new_bucket(&mut keys, 0);
                self.keys = keys;
            }
        }
        false
    }

    /// addok's `fuzzy_collector`.
    fn fuzzy(&mut self) -> bool {
        if self.has_cream() {
            return false;
        }
        if !self.not_found.is_empty() {
            self.try_fuzzy(Group::NotFound, true);
        }
        if self.bucket_dry() && !self.has_cream() {
            self.try_fuzzy(Group::Meaningful, true);
        }
        if self.bucket_dry() && !self.has_cream() && !self.common.is_empty() {
            self.try_fuzzy(Group::Meaningful, false);
        }
        false
    }

    /// addok's `try_fuzzy`: each token in turn, longest first, replaced by
    /// its neighbours one keystroke away that share a document with the
    /// other keys.
    fn try_fuzzy(&mut self, group: Group, include_common: bool) {
        let index = self.index;
        let dry = self.bucket_dry();
        let tokens = match group {
            Group::NotFound => &mut self.not_found,
            Group::Meaningful => &mut self.meaningful,
        };
        if !dry || tokens.is_empty() {
            return;
        }
        let tried = self::tokens(tokens);
        tokens.sort_by_key(|token| Reverse(token.len()));
        let tokens = tokens.clone();
        self.debug(|_| format!("Fuzzy on. Trying with {tried}."));
        let mut allkeys = self.keys.clone();
        if include_common {
            let unused = self
                .common
                .iter()
                .filter(|token| !self.keys.contains(&token.value));
            allkeys.extend(unused.map(|token| token.value.clone()));
        }
        for try_one in &tokens {
            if self.bucket_full() {
                break;
            }
            let mut keys = allkeys.clone();
            if try_one.found
                && let Some(i) = keys.iter().position(|key| *key == try_one.value)
            {
                keys.remove(i);
            }
            if try_one.is_digit() {
                continue;
            }
            self.debug(|_| {
                format!(
                    "Going fuzzy with {} and {}",
                    try_one.value,
                    self::keys(&keys)
                )
            });
            // The neighbours the index knows, in the order they were made:
            // swaps first.
            let mut known: Vec<TokenId> = Vec::new();
            fuzzy_neighbors(&try_one.value, |neighbor| known.extend(index.token(neighbor)));
            if !keys.is_empty() {
                let mut sorted = known.clone();
                sorted.sort_unstable();
                let mut sets: Vec<&[TokenId]> = keys.iter().map(|key| index.pairs(key)).collect();
                sets.push(&sorted);
                let shared = postings::intersect_sets(&sets);
                known.retain(|token| shared.binary_search(token).is_ok());
            }
            let words: Vec<String> = known
                .iter()
                .map(|&token| index.word(token).to_owned())
                .collect();
            if !words.is_empty() {
                self.debug(|_| format!("Found fuzzy candidates {words:?}"));
            }
            for word in words {
                if self.bucket_dry() {
                    let mut extended = keys.clone();
                    extended.push(word);
                    self.add_to_bucket(&mut extended);
                }
            }
        }
    }

    fn extend_results_extrapoling_relations(&mut self) -> bool {
        if !self.bucket_dry() {
            return false;
        }
        // addok's `set(meaningful + common)`: in byte order here, in hash
        // order there. The order decides the groups where a token could
        // join several.
        let mut tokens: Vec<Token> = self
            .meaningful
            .iter()
            .chain(&self.common)
            .cloned()
            .collect();
        tokens.sort_by(|a, b| a.value.cmp(&b.value));
        tokens.dedup_by(|a, b| a.value == b.value);
        let words = &self.order.tokens;
        let rank = |token: &Token| words.iter().position(|word| *word == token.value);
        tokens.sort_by_key(|token| rank(token).unwrap_or(words.len()));
        let mut overflow = false;
        for mut relation in self.relations(&tokens) {
            self.add_to_bucket(&mut relation);
            if self.bucket_overflow() {
                overflow = true;
                break;
            }
        }
        if !overflow {
            self.debug(|_| "No relation extrapolated.".to_owned());
        }
        false
    }

    /// addok's `_extract_manytomany_relations`, sorted by average frequency:
    /// the groups of tokens that each share a document with all the others,
    /// common tokens left out, but those another group holds.
    fn relations(&self, tokens: &[Token]) -> Vec<Vec<String>> {
        let index = self.index;
        let paired = |a: &Token, b: &Token| {
            let other = index.token(&b.value);
            other.is_some_and(|other| index.pairs(&a.value).binary_search(&other).is_ok())
        };
        let others: Vec<Vec<usize>> = (0..tokens.len())
            .map(|i| {
                (0..tokens.len())
                    .filter(|&j| j != i && paired(&tokens[i], &tokens[j]))
                    .collect()
            })
            .collect();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (origin, related) in others.iter().enumerate() {
            let mut group = vec![origin];
            for &token in related {
                if group.iter().all(|&member| others[member].contains(&token)) {
                    group.push(token);
                }
            }
            group.retain(|&member| !tokens[member].is_common());
            group.sort_unstable();
            group.dedup();
            if group.len() > 1 {
                groups.push(group);
            }
        }
        groups.sort_by_key(|group| Reverse(group.len()));
        let mut unique: Vec<Vec<usize>> = Vec::new();
        for group in groups {
            if !unique
                .iter()
                .any(|kept| group.iter().all(|member| kept.contains(member)))
            {
                unique.push(group);
            }
        }
        let average = |group: &Vec<usize>| {
            let total: usize = group.iter().map(|&member| tokens[member].frequency).sum();
            total as f64 / group.len() as f64
        };
        unique.sort_by(|a, b| average(a).total_cmp(&average(b)));
        let words = |group: Vec<usize>| {
            group
                .into_iter()
                .map(|member| tokens[member].value.clone())
                .collect()
        };
        unique.into_iter().map(words).collect()
    }

    fn extend_results_reducing_tokens(&mut self) -> bool {
        if self.bucket_full() || self.has_cream() {
            return true;
        }
        if !self.bucket_dry() {
            return false;
        }
        if self.bucket_empty()
            || self.meaningful.len() as i64 - 1 > self.should_match_threshold as i64
        {
            self.debug(|_| "Bucket dry. Trying to remove 1 meaningful token.".to_owned());
            // Numbers first, then by frequency, both descending.
            let order = |token: &Token| (if token.is_digit() { 2 } else { 1 }, token.frequency);
            self.meaningful.sort_by_key(|token| Reverse(order(token)));
            let meaningful = self.meaningful.clone();
            for token in &meaningful {
                let mut keys = without(&self.keys, &[&token.value]);
                self.add_to_bucket(&mut keys);
                if self.bucket_overflow() {
                    break;
                }
            }
            if self.bucket_empty() && meaningful.len() > 3 {
                self.debug(|_| "Bucket still empty, remove 2 meaningful tokens.".to_owned());
                'pairs: for token in &meaningful {
                    for other in &meaningful {
                        if token.value != other.value {
                            let mut keys = without(&self.keys, &[&token.value, &other.value]);
                            self.add_to_bucket(&mut keys);
                            if self.bucket_overflow() {
                                break 'pairs;
                            }
                        }
                    }
                }
            }
        }
        false
    }
}

/// Which of the helper's token lists `try_fuzzy` works on.
#[derive(Clone, Copy)]
enum Group {
    NotFound,
    Meaningful,
}

/// The keys, each of `removed` taken out once, as Python's `list.remove`.
fn without(keys: &[String], removed: &[&str]) -> Vec<String> {
    let mut keys = keys.to_vec();
    for &word in removed {
        if let Some(i) = keys.iter().position(|key| key == word) {
            keys.remove(i);
        }
    }
    keys
}

/// addok's `make_fuzzy`: the words one keystroke away, in addok's order:
/// two neighbouring letters swapped, a letter replaced by a neighbouring key
/// (AZERTY), any letter inserted, then, past 3 letters, a letter removed;
/// each once. Only equal letters repeat a word: swapped, they give the word
/// back; inserted or removed next to each other, the same word twice. A
/// replacement is always new, no key being among its own neighbours. Each
/// is given in turn to `visit`, in one buffer.
fn fuzzy_neighbors(word: &str, mut visit: impl FnMut(&str)) {
    let letters: Vec<char> = word.chars().collect();
    let mut neighbor = String::with_capacity(word.len() + 4);
    // The letters before `i`, `middle`, then those from `after` on.
    let mut splice = |i: usize, middle: &[char], after: usize| {
        neighbor.clear();
        neighbor.extend(&letters[..i]);
        neighbor.extend(middle);
        neighbor.extend(&letters[after..]);
        visit(&neighbor);
    };
    let mut itself = false;
    for i in 0..letters.len().saturating_sub(1) {
        if letters[i] == letters[i + 1] {
            if itself {
                continue;
            }
            itself = true;
        }
        splice(i, &[letters[i + 1], letters[i]], i + 2);
    }
    for (i, &original) in letters.iter().enumerate() {
        let keys = FUZZY_KEY_MAP.iter().find(|&&(key, _)| key == original);
        // Compared with the letter it last put in, as addok does.
        let mut previous = original;
        for letter in keys.map_or("", |&(_, keys)| keys).chars() {
            if letter != previous {
                previous = letter;
                splice(i, &[letter], i + 1);
            }
        }
    }
    for letter in 'a'..='z' {
        for i in 0..=letters.len() {
            if i == 0 || letters[i - 1] != letter {
                splice(i, &[letter], i);
            }
        }
    }
    if letters.len() > 3 {
        for i in 0..letters.len() {
            if i == 0 || letters[i - 1] != letters[i] {
                splice(i, &[], i + 1);
            }
        }
    }
}

/// `fuzzy_neighbors`, collected.
#[cfg(test)]
fn make_fuzzy(word: &str) -> Vec<String> {
    let mut neighbors = Vec::new();
    fuzzy_neighbors(word, |neighbor| neighbors.push(neighbor.to_owned()));
    neighbors
}

/// A folded query's bigrams, counted, for comparing labels with it.
struct QueryBigrams {
    /// How many times the query holds each bigram, by `bigram`. A folded
    /// query is at most 200 characters long, else search refuses it.
    counts: Vec<u8>,
    /// `counts`, less those the label being compared took.
    left: Vec<u8>,
    len: usize,
}

impl QueryBigrams {
    fn new(folded: &str) -> Self {
        let mut counts = vec![0; 1 << 14];
        for pair in folded.as_bytes().windows(2) {
            counts[bigram(pair)] += 1;
        }
        QueryBigrams {
            left: counts.clone(),
            counts,
            len: folded.len().saturating_sub(1),
        }
    }
}

/// Python 3.12's `sum` of floats, from 0: compensated (Neumaier), so that
/// three terms may round otherwise than added in turn: `0.1 + 1.0 + 0.1`
/// gives 1.2000000000000002, their `sum` 1.2.
pub(crate) fn python_sum(terms: &[f64]) -> f64 {
    let (mut sum, mut compensation) = (0.0f64, 0.0f64);
    for &term in terms {
        let total = sum + term;
        compensation += match sum.abs() >= term.abs() {
            true => (sum - total) + term,
            false => (term - total) + sum,
        };
        sum = total;
    }
    if compensation != 0.0 && compensation.is_finite() {
        sum += compensation;
    }
    sum
}

/// addok's `haversine_distance`: the great-circle distance between two
/// positions, latitude then longitude, in kilometres, on a sphere of radius
/// 6,367 km.
pub(crate) fn haversine_distance((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let (lon1, lat1, lon2, lat2) = (lon1.to_radians(), lat1.to_radians(), lon2.to_radians(), lat2.to_radians());
    let (dlon, dlat) = (lon2 - lon1, lat2 - lat1);
    let a = squared((dlat / 2.0).sin()) + lat1.cos() * lat2.cos() * squared((dlon / 2.0).sin());
    let c = 2.0 * a.sqrt().asin();
    6367.0 * c
}

/// addok's `km_to_score`: 1 at the position, falling to 0 past 100 km.
pub(crate) fn km_to_score(km: f64) -> f64 {
    if km > 100.0 { 0.0 } else { (-squared(km / 50.0)).exp() }
}

/// Python's `x ** 2`: the C library's `pow`, which need not round as
/// `x * x` does, and which the compiler would otherwise turn into it.
fn squared(x: f64) -> f64 {
    x.powf(std::hint::black_box(2.0))
}

/// A bigram of folded text, two ASCII bytes, as 14 bits.
fn bigram(pair: &[u8]) -> usize {
    debug_assert!(pair.is_ascii(), "folded strings are ASCII");
    usize::from(pair[0]) << 7 | usize::from(pair[1])
}

/// A label folded into `folded`: its pieces, each folded once and kept in
/// `pieces`, joined. Folding turns every run of spaces and symbols into one
/// space, so the pieces fold as the whole would.
fn fold_label<'p>(label: &[&'p str], pieces: &mut Vec<(&'p str, String)>, folded: &mut String) {
    folded.clear();
    for &piece in label {
        let i = match pieces.iter().position(|&(raw, _)| raw == piece) {
            Some(i) => i,
            None => {
                pieces.push((piece, text::fold(piece)));
                pieces.len() - 1
            }
        };
        let piece = &pieces[i].1;
        if !piece.is_empty() {
            if !folded.is_empty() {
                folded.push(' ');
            }
            folded.push_str(piece);
        }
    }
}

/// addok's `compare_ngrams`: python-ngram's `NGram.compare` with N=2 and no
/// padding, shared bigrams over all bigrams, counted with repetitions; but 1
/// or 0 between single characters.
fn compare_ngrams(label: &str, query: &str, query_bigrams: &mut QueryBigrams) -> f64 {
    if label.len() == 1 && query.len() == 1 {
        return if label == query { 1.0 } else { 0.0 };
    }
    let pairs = label.as_bytes().windows(2);
    let mut same = 0;
    for pair in pairs.clone() {
        let left = &mut query_bigrams.left[bigram(pair)];
        if *left > 0 {
            *left -= 1;
            same += 1;
        }
    }
    for pair in pairs {
        let bigram = bigram(pair);
        query_bigrams.left[bigram] = query_bigrams.counts[bigram];
    }
    if same == 0 {
        return 0.0;
    }
    let all = label.len().saturating_sub(1) + query_bigrams.len - same;
    same as f64 / all as f64
}

/// addok-france's `make_labels`: for each name and city, the variants the
/// query is compared with, the most complete first; the first is the
/// result's label. Each is given as its pieces, which it joins with spaces.
fn labels<'d>(
    name: &TextRef<'d>,
    postcode: &TextRef<'d>,
    city: &TextRef<'d>,
    housenumber: Option<&'d str>,
    kind: &str,
) -> Vec<Vec<&'d str>> {
    // addok's `_rawattr`: a list as is (empty: one missing value), else
    // the value alone; and `result.postcode`, the first value.
    let values = |text: &TextRef<'d>| -> Vec<Option<&'d str>> {
        match text {
            TextRef::Many(values) if values.is_empty() => vec![None],
            TextRef::Many(values) => values.iter().map(|&value| Some(value)).collect(),
            TextRef::One(value) => vec![Some(*value)],
            TextRef::Null | TextRef::Absent => vec![None],
        }
    };
    let names: Vec<&str> = values(name)
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    let cities = values(city);
    let postcode = values(postcode)
        .swap_remove(0)
        .filter(|postcode| !postcode.is_empty());
    let add = |labels: &mut Vec<Vec<&'d str>>, label: Vec<&'d str>| {
        if let Some(number) = housenumber {
            labels.insert(0, [&[number], &label[..]].concat());
            labels.insert(1, label);
        } else {
            labels.insert(0, label);
        }
    };
    let mut all = Vec::new();
    for &name in &names {
        for city in &cities {
            let mut labels = Vec::new();
            if let Some(postcode) = postcode
                && kind == "municipality"
            {
                add(&mut labels, vec![name, postcode]);
                add(&mut labels, vec![postcode, name]);
            }
            add(&mut labels, vec![name]);
            if let Some(city) = city.filter(|city| !city.is_empty() && *city != name) {
                add(&mut labels, vec![name, city]);
                if let Some(postcode) = postcode {
                    add(&mut labels, vec![name, postcode]);
                    add(&mut labels, vec![name, postcode, city]);
                }
            }
            all.extend(labels);
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Text;

    // Expected values are the reference's own answers.

    #[test]
    fn makes_fuzzy_neighbours_in_addok_s_order() {
        let paix = make_fuzzy("paix");
        assert_eq!(paix.len(), 150);
        let first = [
            "apix", "piax", "paxi", "oaix", "aaix", "laix", "maix", "peix", "pzix", "pqix", "poix",
            "ppix",
        ];
        assert_eq!(paix[..12], first);
        assert_eq!(paix[144..], ["paizx", "paixz", "aix", "pix", "pax", "pai"]);
        let ru = make_fuzzy("ru");
        assert_eq!(ru.len(), 86);
        assert_eq!(
            ru[..10],
            ["ur", "eu", "du", "fu", "tu", "ry", "re", "rh", "rj", "ri"]
        );
    }

    #[test]
    fn makes_each_fuzzy_neighbour_once() {
        // What make_fuzzy counts on to skip repeats as it goes.
        for (key, keys) in FUZZY_KEY_MAP {
            assert!(!keys.contains(key), "{key}");
            assert_eq!(keys.chars().collect::<HashSet<_>>().len(), keys.len(), "{key}");
        }
        for word in ["allee", "aaaa", "pp", "été", "nn°"] {
            let neighbors = make_fuzzy(word);
            let unique: HashSet<&String> = neighbors.iter().collect();
            assert_eq!(unique.len(), neighbors.len(), "{word}");
        }
    }

    #[test]
    fn compares_bigrams_as_python_ngram() {
        let compare =
            |label: &str, query: &str| compare_ngrams(label, query, &mut QueryBigrams::new(query));
        assert_eq!(
            compare("rue de la gare 45000 orleans", "rue de la gare"),
            0.48148148148148145
        );
        assert_eq!(compare("a", "a"), 1.0);
        assert_eq!(compare("a", "b"), 0.0);
        assert_eq!(compare("aa", "aaa"), 0.5);
        assert_eq!(compare("paris", "pariss"), 0.8);
        assert_eq!(compare("", "abc"), 0.0);
        assert_eq!(compare("ab", "x"), 0.0);
    }

    fn text(value: &str) -> Text {
        Text::One(value.to_owned())
    }

    fn many(values: &[&str]) -> Text {
        Text::Many(values.iter().map(|&value| value.to_owned()).collect())
    }

    #[test]
    fn makes_labels_as_addok_france() {
        let labels = |document: &Document, housenumber, kind| -> Vec<String> {
            let (name, postcode) = (document.name.borrowed(), document.postcode.borrowed());
            let pieces = labels(&name, &postcode, &document.city.borrowed(), housenumber, kind);
            pieces.iter().map(|label| label.join(" ")).collect()
        };
        let street = Document {
            name: text("Montee de la Foret"),
            postcode: text("01640"),
            city: many(&["L'Abergement-de-Varey"]),
            ..Document::default()
        };
        assert_eq!(
            labels(&street, Some("16"), "housenumber"),
            [
                "16 Montee de la Foret 01640 L'Abergement-de-Varey",
                "Montee de la Foret 01640 L'Abergement-de-Varey",
                "16 Montee de la Foret 01640",
                "Montee de la Foret 01640",
                "16 Montee de la Foret L'Abergement-de-Varey",
                "Montee de la Foret L'Abergement-de-Varey",
                "16 Montee de la Foret",
                "Montee de la Foret",
            ]
        );
        let paris = Document {
            name: text("Paris"),
            postcode: many(&["75001", "75002"]),
            city: text("Paris"),
            ..Document::default()
        };
        assert_eq!(
            labels(&paris, None, "municipality"),
            ["Paris", "75001 Paris", "Paris 75001"]
        );
        let locality = Document {
            name: many(&["A", "B"]),
            postcode: Text::Null,
            city: many(&["X", "Y"]),
            ..Document::default()
        };
        assert_eq!(
            labels(&locality, None, "locality"),
            ["A X", "A", "A Y", "A", "B X", "B", "B Y", "B"]
        );
    }

    /// Two streets of one name in two communes, the first with a house
    /// number 4.
    fn two_communes() -> Index<crate::index::AlignedBytes> {
        let street = |id: &str, citycode: &str, postcode: &str, city: &str, numbers: &str| {
            Document::from_ndjson(&format!(
                r#"{{"id":"{id}","banId":null,"type":"street","name":"Rue Lima","postcode":"{postcode}","citycode":"{citycode}","city":"{city}","context":"","x":0,"y":0,"lon":2.3,"lat":48.8,"importance":0.5,"housenumbers":{{{numbers}}}}}"#
            ))
            .unwrap()
        };
        let four = r#""4":{"id":"75101_0001_00004","banId":null,"x":0,"y":0,"lon":2.3,"lat":48.8}"#;
        let streets = [
            street("75101_0001", "75101", "75001", "Paris", four),
            street("69381_0001", "69381", "69001", "Lyon", ""),
        ];
        let mut bytes = crate::index::AlignedBytes::default();
        crate::index::write(streets, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    /// Rue Lima in Paris, with a number 4, and in Lyon, each where it is.
    fn paris_and_lyon() -> Index<crate::index::AlignedBytes> {
        let street = |id: &str, postcode: &str, city: &str, lat: f64, lon: f64, numbers: &str| {
            Document::from_ndjson(&format!(
                r#"{{"id":"{id}","banId":null,"type":"street","name":"Rue Lima","postcode":"{postcode}","citycode":"{}","city":"{city}","context":"","x":0,"y":0,"lon":{lon},"lat":{lat},"importance":0.5,"housenumbers":{{{numbers}}}}}"#,
                &id[..5]
            ))
            .unwrap()
        };
        let four = r#""4":{"id":"75101_0001_00004","banId":null,"x":0,"y":0,"lon":2.3401,"lat":48.8601}"#;
        let streets = [
            street("75101_0001", "75001", "Paris", 48.86, 2.34, four),
            street("69381_0001", "69001", "Lyon", 45.76, 4.83, ""),
        ];
        let mut bytes = crate::index::AlignedBytes::default();
        crate::index::write(streets, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    #[test]
    fn scores_the_distance_to_the_position_searched_around() {
        let index = paris_and_lyon();
        let near = |lat: f64, lon: f64| Options { limit: 3, center: Some(Center { lat, lon }), ..Options::default() };
        let found = search_with(&index, "rue lima", &near(45.7601, 4.8301)).unwrap();
        assert_eq!(ids(&found), ["69381_0001", "75101_0001"]);
        // addok's score: importance, label, then distance, over their
        // ceilings; nothing for the distance past 100 km.
        let lyon = &found[0];
        let km = haversine_distance((45.76, 4.83), (45.7601, 4.8301));
        let geo = km_to_score(km) * GEO_DISTANCE_WEIGHT;
        assert_eq!(lyon.score, python_sum(&[lyon.importance, lyon.str_distance, geo]) / 1.2);
        assert_eq!(lyon.distance, Some(km * 1000.0));
        let paris = &found[1];
        assert!(paris.distance.unwrap() > 390_000.0);
        assert_eq!(paris.score, python_sum(&[paris.importance, paris.str_distance, 0.0]) / 1.2);
        // Without a position, no distance, and the score addok-csv gives.
        let anywhere = search_filtered(&index, "rue lima", 3, &Filters::default()).unwrap();
        assert!(anywhere.iter().all(|found| found.distance.is_none()));
    }

    #[test]
    fn squares_as_python() {
        // CPython 3.12.3's `sin(dlon / 2) ** 2`, and its distance, on macOS,
        // where `pow` does not round as a product does here.
        let x = (((-1.27112f64).to_radians() - (-1.380786f64).to_radians()) / 2.0).sin();
        let distance = haversine_distance((45.990167, -1.380786), (45.994359, -1.27112)) * 1000.0;
        if cfg!(target_os = "macos") {
            assert_ne!(squared(x), x * x);
            assert_eq!(distance, 8479.54453403133);
        }
    }

    #[test]
    fn sums_as_python_3_12() {
        // CPython 3.12.3's own answers.
        assert_eq!(python_sum(&[0.1, 1.0, 0.1]), 1.2);
        assert_eq!(0.1 + 1.0 + 0.1, 1.2000000000000002);
        assert_eq!(python_sum(&[0.1, 1.0]), 1.1);
        assert_eq!(python_sum(&[]), 0.0);
    }

    #[test]
    fn finds_a_lone_house_number_around_the_position() {
        let index = paris_and_lyon();
        let near = |lat: f64, lon: f64| Options { limit: 3, center: Some(Center { lat, lon }), ..Options::default() };
        let found = search_with(&index, "4", &near(48.8601, 2.3401)).unwrap();
        assert_eq!(ids(&found), ["75101_0001_00004"]);
        assert_eq!(found[0].distance, Some(0.0));
        assert!(search_with(&index, "4", &near(43.3, 5.4)).unwrap().is_empty());
        // Python takes a coordinate of 0 for no position: no cell, so
        // nothing for a house number alone.
        assert!(search_with(&index, "4", &near(48.8601, 0.0)).unwrap().is_empty());
    }

    #[test]
    fn tries_autocomplete_s_words_of_equal_score_in_the_order_given() {
        let street = |id: &str, city: &str| {
            Document::from_ndjson(&format!(
                r#"{{"id":"{id}","banId":null,"type":"street","name":"Rue Lima","postcode":"82000","citycode":"{}","city":"{city}","context":"","x":0,"y":0,"lon":1.3,"lat":44.0,"importance":0.5}}"#,
                &id[..5]
            ))
            .unwrap()
        };
        let mut bytes = crate::index::AlignedBytes::default();
        crate::index::write([street("82001_0001", "Montjoie"), street("82002_0001", "Montjoli")], &mut bytes).unwrap();
        let index = Index::open(bytes).unwrap();
        let tried = |candidates: &[&str]| {
            let order = Order { candidates: candidates.iter().map(|&word| word.to_owned()).collect(), ..Order::default() };
            let options = Options { limit: 3, autocomplete: true, ..Options::default() };
            let (_, trace) = search_traced(&index, "rue lima mon", &options, &order).unwrap();
            let steps = trace.steps.iter().filter(|step| step.starts_with("Trying to extend bucket"));
            steps.map(|step| step.rsplit('|').next().unwrap().to_owned()).collect::<Vec<_>>()
        };
        // Byte order by default; addok's Lua sort may have taken the other.
        assert_eq!(tried(&[]), ["monjoi", "monjoli"]);
        assert_eq!(tried(&["monjoli", "monjoi"]), ["monjoli", "monjoi"]);
    }

    #[test]
    fn scores_labels_by_the_query_they_hold_when_autocompleting() {
        let index = two_communes();
        let distances = |query: &str, autocomplete: bool| {
            let search = if autocomplete { search_autocomplete } else { search_filtered };
            let found = search(&index, query, 3, &Filters::default()).unwrap();
            found.iter().map(|found| (found.label().to_owned(), found.str_distance)).collect::<Vec<_>>()
        };
        // A label the query begins: 0.9; the query itself: 1; a label it is
        // in: 0.7. None of them: the bigrams' score, times 0.9.
        let lyo = distances("rue lima lyo", true);
        assert_eq!(lyo[0], ("Rue Lima 69001 Lyon".to_owned(), 0.9));
        let paris = distances("rue lima lyo", false)[1].1;
        assert_eq!(lyo[1], ("Rue Lima 75001 Paris".to_owned(), paris * 0.9));
        assert_eq!(distances("rue lima lyon", true)[0].1, 1.0);
        assert_eq!(distances("lima lyon", true)[0], ("Rue Lima 69001 Lyon".to_owned(), 0.7));
    }

    fn ids(found: &[Found]) -> Vec<&str> {
        found.iter().map(Found::id).collect()
    }

    fn filters(kind: &[&str], citycode: &[&str], postcode: &[&str]) -> Filters {
        let owned = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
        Filters {
            kind: owned(kind),
            citycode: owned(citycode),
            postcode: owned(postcode),
        }
    }

    #[test]
    fn keeps_only_the_documents_a_filter_holds() {
        let index = two_communes();
        let search = |filters: &Filters| search_filtered(&index, "rue lima", 3, filters).unwrap();
        assert_eq!(ids(&search(&Filters::default())).len(), 2);
        assert_eq!(ids(&search(&filters(&[], &[], &["69001"]))), ["69381_0001"]);
        assert_eq!(ids(&search(&filters(&[], &["75101"], &[]))), ["75101_0001"]);
        // Several values of a filter: any of them.
        assert_eq!(ids(&search(&filters(&[], &[], &["69001", "75001"]))).len(), 2);
        // Several filters: all of them.
        assert!(search(&filters(&[], &["75101"], &["69001"])).is_empty());
        // A value no document holds matches nothing.
        assert!(search(&filters(&[], &[], &["13001"])).is_empty());
        // Values trimmed, empty ones dropped, as addok does.
        assert_eq!(ids(&search(&filters(&[], &[], &[" 69001 ", ""]))), ["69381_0001"]);
    }

    #[test]
    fn matches_house_numbers_as_the_type_filter_says() {
        let index = two_communes();
        let search = |query: &str, filters: &Filters| search_filtered(&index, query, 3, filters).unwrap();
        let paris = filters(&[], &["75101"], &[]);
        assert_eq!(search("4 rue lima", &paris)[0].kind(), "housenumber");
        // A street filter leaves the house number aside.
        let street = filters(&["street"], &["75101"], &[]);
        assert_eq!(search("4 rue lima", &street)[0].kind(), "street");
        // House numbers only: a street without the number is no result.
        let housenumber = filters(&["housenumber"], &[], &[]);
        assert_eq!(ids(&search("4 rue lima", &housenumber)), ["75101_0001_00004"]);
        assert!(search("9 rue lima", &housenumber).is_empty());
        // House numbers among other types: matched, but not required.
        let either = filters(&["housenumber", "street"], &[], &[]);
        assert_eq!(search("9 rue lima", &either).len(), 2);
    }

    // The orders below are addok-rs's own, on small indexes of its own.

    /// Streets of these names, numbered as listed: ids of three digits
    /// order alike as bytes and as numbers.
    fn index(names: &[&str]) -> Index<crate::index::AlignedBytes> {
        let streets = (100..).zip(names).map(|(id, &name)| Document {
            id: id.to_string(),
            kind: "street".into(),
            name: text(name),
            ..Document::default()
        });
        let mut bytes = crate::index::AlignedBytes::default();
        crate::index::write(streets, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    #[test]
    fn keeps_either_end_of_a_tie_at_a_cut() {
        // A hundred and two equal streets, of which the bucket keeps a
        // hundred: the order picks the two it drops.
        let index = index(&["Lima"; 102]);
        let ids = |order: &Order| {
            let (found, trace) = search_traced(&index, "lima", &Options { limit: 3, ..Options::default() }, order).unwrap();
            assert_eq!(trace.tied.len(), 102);
            let ids: Vec<String> = found.iter().map(|found| found.id().to_owned()).collect();
            ids
        };
        assert_eq!(ids(&Order::default()), ["100", "101", "102"]);
        let reversed = Order {
            ties_reversed: true,
            ..Order::default()
        };
        assert_eq!(ids(&reversed), ["102", "103", "104"]);
    }

    #[test]
    fn groups_relations_in_the_order_given() {
        // Each word shares a street with the next, the last with the first,
        // so a token joins the group of whichever neighbour comes first.
        let index = index(&["Lima Oslo", "Oslo Rome", "Rome Kiev", "Kiev Lima"]);
        let groups = |order: &Order| {
            let (_, trace) = search_traced(&index, "lima oslo rome kiev", &Options { limit: 3, ..Options::default() }, order).unwrap();
            let steps = trace.steps.into_iter();
            let keys = steps.filter_map(|step| {
                let keys = step.strip_prefix("Adding to bucket with keys ");
                keys.map(str::to_owned)
            });
            keys.collect::<Vec<String>>()
        };
        // By bytes: kiev, lima, oslo, rom.
        assert_eq!(
            groups(&Order::default()),
            [
                "['w|kiev', 'w|lima']",
                "['w|lima', 'w|oslo']",
                "['w|kiev', 'w|rom']"
            ]
        );
        let given = Order {
            tokens: ["oslo", "rom", "kiev", "lima"].map(str::to_owned).to_vec(),
            ..Order::default()
        };
        assert_eq!(
            groups(&given),
            [
                "['w|oslo', 'w|rom']",
                "['w|rom', 'w|kiev']",
                "['w|oslo', 'w|lima']"
            ]
        );
    }
}
