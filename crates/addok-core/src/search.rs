//! Search: what addok answers to a query, ported from addok 1.3.2's `Search`
//! (addok/core.py) with the preprocessors, collectors and result processors
//! the BAN configures, and addok-france's labels. Only addok-csv's call is
//! ported: autocomplete off, fuzzy on, no filter, no position. Ported to
//! answer as addok does, quirks included, but for ties: where addok's order
//! follows Python's hash seed or Redis's, results rank by score, then by
//! document number.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::document::{Document, HouseNumber, Number, TextRef};
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

/// A document's scores, as a `Found` holds them.
#[derive(Debug, Clone, Copy)]
struct Scored {
    importance: f64,
    str_distance: f64,
    score: f64,
}

/// addok-csv's `search(q, autocomplete=False, limit=limit)`: the results,
/// best first, at most `limit` of them and none under addok's minimum score.
pub fn search<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    limit: usize,
) -> Result<Vec<Found>, QueryTooLong> {
    let mut helper = Helper::new(index, query, limit)?;
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
/// first), and in which order it groups tokens into relations (addok, in
/// a Python set's). `search` keeps the default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    /// Keep the highest document numbers of a tie group, not the lowest.
    pub ties_reversed: bool,
    /// The order to take tokens in when grouping them into relations, by
    /// their words: the one addok's set took, say. Those it leaves out come
    /// after, by bytes; all of them when it is empty.
    pub tokens: Vec<String>,
}

/// `search` in the order given, and its trace.
pub fn search_traced<B: AsRef<[u8]>>(
    index: &Index<B>,
    query: &str,
    limit: usize,
    order: &Order,
) -> Result<(Vec<Found>, Trace), QueryTooLong> {
    let mut helper = Helper::new(index, query, limit)?;
    helper.order = order.clone();
    helper.trace = Some(Tracing::default());
    helper.debug(|h| format!("Taken tokens: {}", tokens(&h.meaningful)));
    helper.debug(|h| format!("Common tokens: {}", tokens(&h.common)));
    helper.debug(|h| format!("Housenumbers token: {}", h.housenumber));
    helper.debug(|h| format!("Not found tokens: {}", tokens(&h.not_found)));
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
    let keys: Vec<String> = keys.iter().map(|key| format!("'w|{key}'")).collect();
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
    order: Order,
    trace: Option<Tracing>,
}

impl<'i, B: AsRef<[u8]>> Helper<'i, B> {
    /// The query folded and tokenized, its tokens looked up and sorted
    /// out: addok's `SEARCH_PREPROCESSORS`.
    fn new(index: &'i Index<B>, query: &str, wanted: usize) -> Result<Self, QueryTooLong> {
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

    /// addok's `RESULTS_COLLECTORS`, in order, until one says it is done;
    /// less those that need a position or autocomplete, which addok-csv's
    /// call never runs.
    fn collect(&mut self) {
        let collectors: [Collector<'i, B>; 9] = [
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
    /// `limit` is not positive (addok's `intersect`).
    fn intersect(&mut self, keys: &[String], limit: i64) -> Vec<DocId> {
        let index = self.index;
        let limit = match usize::try_from(limit) {
            Ok(limit) if limit > 0 => limit,
            _ => self.wanted.max(BUCKET_MAX),
        };
        let mut words: Vec<&str> = keys.iter().map(String::as_str).collect();
        words.sort_unstable();
        words.dedup();
        let mut lists = Vec::with_capacity(words.len());
        for word in words {
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

    fn add_to_bucket(&mut self, keys: &[String]) {
        self.debug(|_| format!("Adding to bucket with keys {}", self::keys(keys)));
        self.matched_keys.extend(keys.iter().cloned());
        let limit = BUCKET_MAX as i64 - self.bucket.len() as i64;
        let found = self.intersect(keys, limit);
        self.bucket.extend(found);
        self.debug(|h| format!("{} ids in bucket so far", h.bucket.len()));
    }

    fn new_bucket(&mut self, keys: &[String], limit: i64) {
        self.debug(|_| {
            format!(
                "New bucket with keys {} and limit {limit}",
                self::keys(keys)
            )
        });
        self.matched_keys = keys.iter().cloned().collect();
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
            let scored = self.score(doc);
            self.results.insert(doc, scored);
        }
    }

    /// A document's scores, through addok's `SEARCH_RESULT_PROCESSORS`: the
    /// house number matched, labels made, then scores for importance and
    /// for the best label. Read in place: only the results search returns
    /// are made whole, by `result`.
    fn score(&mut self, doc: DocId) -> Scored {
        let index = self.index;
        let number = match self.housenumber.is_empty() {
            true => None,
            false => index.housenumber_number(doc, &self.housenumber),
        };
        let kind = match number {
            Some(_) => "housenumber",
            None => index.kind(doc),
        };
        let (name, postcode, city) = (index.name(doc), index.postcode(doc), index.city(doc));
        let pieces = labels(&name, &postcode, &city, number, kind);
        let importance = index.importance(doc).map_or(0.0, Number::value) * IMPORTANCE_WEIGHT;
        let mut str_distance = 0.0;
        // A label folds as its pieces, each folded once, joined: folding
        // turns every run of spaces and symbols into one space.
        let mut folded_pieces: Vec<(&str, String)> = Vec::new();
        let mut folded = String::new();
        for label in &pieces {
            folded.clear();
            for &piece in label {
                let i = match folded_pieces.iter().position(|&(raw, _)| raw == piece) {
                    Some(i) => i,
                    None => {
                        folded_pieces.push((piece, text::fold(piece)));
                        folded_pieces.len() - 1
                    }
                };
                let piece = &folded_pieces[i].1;
                if !piece.is_empty() {
                    if !folded.is_empty() {
                        folded.push(' ');
                    }
                    folded.push_str(piece);
                }
            }
            let score = compare_ngrams(&folded, &self.query, &mut self.bigrams);
            if score >= str_distance {
                str_distance = score;
            }
            if score >= MATCH_THRESHOLD {
                break;
            }
        }
        // addok sums its scores and their ceilings in this order.
        let score = (0.0 + importance + str_distance) / (0.0 + IMPORTANCE_WEIGHT + 1.0);
        Scored {
            importance,
            str_distance,
            score,
        }
    }

    /// A document `score` scored, made a result.
    fn result(&self, doc: DocId, scored: Scored) -> Found {
        let document = self.index.document(doc);
        let housenumber = match self.housenumber.is_empty() {
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
        }
    }

    fn no_available_tokens_abort(&mut self) -> bool {
        self.tokens.is_empty()
    }

    /// addok's `only_commons` collector.
    fn only_commons_collector(&mut self) -> bool {
        if !self.only_commons() {
            return false;
        }
        let keys: Vec<String> = self
            .tokens
            .iter()
            .map(|token| token.value.clone())
            .collect();
        if keys.len() == 1 {
            self.add_to_bucket(&keys);
        }
        if self.bucket_dry() && keys.len() > 1 {
            self.tokens.sort_by_key(|token| token.frequency);
            let keys: Vec<String> = self
                .tokens
                .iter()
                .map(|token| token.value.clone())
                .collect();
            if self.tokens[0].frequency < INTERSECT_LIMIT {
                self.debug(|_| "Under INTERSECT_LIMIT, force intersect.".to_owned());
                self.add_to_bucket(&keys);
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
    /// those every other key holds, up to `wanted`.
    fn manual_scan(&mut self, keys: &[String]) -> Vec<DocId> {
        let index = self.index;
        let lists: Vec<_> = keys.iter().map(|key| index.postings(key)).collect();
        let Some(Some(first)) = lists.first().copied() else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for (doc, _) in self.best(&[first], MANUAL_SCAN) {
            if lists[1..]
                .iter()
                .all(|list| list.is_some_and(|list| list.contains(doc)))
            {
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
        self.autocomplete(&common, false);
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
        self.autocomplete(&tokens, true);
        if !self.bucket_empty() {
            self.debug(|_| "Only common terms. Return.".to_owned());
        }
        !self.bucket_empty()
    }

    /// addok's `autocomplete`: the tokens the last one begins that share a
    /// document with every other, each tried with the others.
    fn autocomplete(&mut self, tokens: &[Token], skip_commons: bool) {
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
        // keeps byte order among equals.
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
        ordered.sort_by(|a, b| b.1.total_cmp(&a.1));
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
                self.add_to_bucket(&extended);
            }
        }
    }

    fn bucket_with_meaningful(&mut self) -> bool {
        if self.meaningful.is_empty() {
            return false;
        }
        if self.meaningful.len() == 1 && !self.common.is_empty() {
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
        let keys = self.keys.clone();
        if self.bucket_empty() {
            self.new_bucket(&keys, BUCKET_MIN as i64);
            if self.bucket.len() == BUCKET_MIN {
                self.new_bucket(&keys, 0);
            }
            if self.has_cream() && self.cream() < BUCKET_MIN {
                self.debug(|_| "Cream found. Returning.".to_owned());
                return true;
            }
        } else {
            self.add_to_bucket(&keys);
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
                let keys = self.keys.clone();
                self.new_bucket(&keys, 0);
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
                    self.add_to_bucket(&extended);
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
        for relation in self.relations(&tokens) {
            self.add_to_bucket(&relation);
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
                let keys = without(&self.keys, &[&token.value]);
                self.add_to_bucket(&keys);
                if self.bucket_overflow() {
                    break;
                }
            }
            if self.bucket_empty() && meaningful.len() > 3 {
                self.debug(|_| "Bucket still empty, remove 2 meaningful tokens.".to_owned());
                'pairs: for token in &meaningful {
                    for other in &meaningful {
                        if token.value != other.value {
                            let keys = without(&self.keys, &[&token.value, &other.value]);
                            self.add_to_bucket(&keys);
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

/// A bigram of folded text, two ASCII bytes, as 14 bits.
fn bigram(pair: &[u8]) -> usize {
    debug_assert!(pair.is_ascii(), "folded strings are ASCII");
    usize::from(pair[0]) << 7 | usize::from(pair[1])
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
            let (found, trace) = search_traced(&index, "lima", 3, order).unwrap();
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
            let (_, trace) = search_traced(&index, "lima oslo rome kiev", 3, order).unwrap();
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
