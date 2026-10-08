//! addok-france 1.2.0, ported (`addok_france/utils.py`): cleaning a query
//! down to its address, gluing ordinals to their number ("12 bis" becomes
//! "12b"), and flagging the house number. MIT, see LICENSE-addok.

use std::sync::LazyLock;

use fancy_regex::Regex;

use super::Token;

/// Street types, addok-france's `TYPES`.
const TYPES: &[&str] = &[
    "aer(odrome)?",
    "all([ée]es?)?",
    "anc(ien(ne)?)?",
    "av(enue)?",
    "b(oulevar|l?v?)?d",
    "b(ou)?cle",
    "ber(ges?)?",
    "bois",
    "c(en)?tre",
    "c(ou)?rs?",
    "c[ôo]te",
    "carr?(efour)?",
    "ch(am)?p",
    "chauss[ée]e",
    "che?(m(in)?)?",
    "cit[ée]",
    "clos",
    "desserte",
    "devi(ation)?",
    "dig(ue)?",
    "dom(aine)?",
    "embr(anchement)?",
    "espl?(anade)?",
    "eta?ng",
    "éta?ng",
    "f(aubour|b|bour)?g",
    "giratoire",
    "gr(ande?)?",
    "gr(e|es|s)?",
    "ham(eau)?",
    "imp(asse)?",
    "j[ée]?t[ée]e",
    "jard(in)?",
    "l[ôo]t(issement)?",
    "mont",
    "mont[ée]e",
    "p(arvis|rvs?|vr)?",
    "p(asserel|l)?le",
    "p(lace)?tte",
    "p(or)?te",
    "parc",
    "pas(se)?",
    "pass(age)?",
    "pl(ace)?",
    "porte",
    "pr[ée]",
    "pro?m(enade)?",
    "q(uart(ier)?|ua|rt(ier)?)",
    "qu?(ai)?",
    "r(ou)?te",
    "r(uel|l)?le",
    "r(ue)?",
    "r[ée]s(idence)?",
    "rd?pt",
    "rocade",
    "rond[- ]point",
    "sent(e|ier)",
    "squ?(are)?",
    "t(erra)?sse",
    "taillis",
    "trav(erse)?",
    "tunn?(el)?",
    "v(il|l)?la",
    "via",
    "viad(uc)?",
    "voie",
    "[îi]lot",
];

/// addok-france's `ORDINAL_REGEX`.
const ORDINAL: &str = "bis|ter|quater|quinquies|sexies|[a-z]";

/// addok-france's `CLEAN_PATTERNS`, verbatim but for the replacement syntax
/// (`${1}` for `\1`) and `\d{0,2}` for Python's `\d{,2}`.
const CLEAN: [(&str, &str); 9] = [
    (r"([\d]{5})", " ${1} "),
    (r"(^| )(b\.?p\.?|cs|tsa|cidex) *(n(o|°|) *|)[\d]+ *", "${1}"),
    (r"([\d]{2})[\d]{3}(.*)c(e|é)dex ?[\d]*", "${1}${2}"),
    (r"c(e|é)dex ?[\d]*", ""),
    (r"\d{0,2}(e|[eè]me) ([eé]tage)", ""),
    (r" {2,}", " "),
    (r"[ -]s/[ -]", " sur "),
    (r"[ -]s/s[ -]", " sous "),
    (r"^lieux?[ -]?dits?\b(?=.)", ""),
];

/// Every pattern is addok-france's, compiled with its `re.IGNORECASE`.
fn regex(pattern: &str) -> Regex {
    Regex::new(&format!("(?i){pattern}")).unwrap()
}

/// `re.match`: whether the pattern matches at the start of `text`.
fn starts(pattern: &Regex, text: &str) -> bool {
    pattern.is_match(text).unwrap()
}

/// `EXTRACT_ADDRESS_PATTERN`.
static EXTRACT_ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    let types = TYPES.join("|");
    regex(&format!(
        r"(\b\d{{1,4}}( *({ORDINAL}))?,? +({types}) .*(\d{{5}})?).*"
    ))
});
/// `ORDINAL_PATTERN`, anchored for `re.match`.
static ORDINAL_START: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^\b({ORDINAL})\b")));
/// `ORDINAL_PATTERN`, as a whole word.
static ORDINAL_WORD: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^({ORDINAL})$")));
/// `TYPES_PATTERN`, anchored for `re.match`.
static TYPE_START: LazyLock<Regex> =
    LazyLock::new(|| regex(&format!(r"^\b({})\b", TYPES.join("|"))));
/// `TYPES_PATTERN`, as a whole word.
static TYPE_WORD: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^({})$", TYPES.join("|"))));
/// `FOLD_PATTERN`.
static FOLD: LazyLock<Regex> = LazyLock::new(|| regex(&format!(r"^(\d{{1,4}})({ORDINAL})$")));
/// `NUMBER_PATTERN`, anchored for `re.match`.
static NUMBER_START: LazyLock<Regex> = LazyLock::new(|| regex(r"^\b\d{1,4}[a-z]?\b"));
static CLEAN_COMPILED: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    CLEAN
        .iter()
        .map(|&(pattern, replacement)| (regex(pattern), replacement))
        .collect()
});
static LEADING_ZEROS: LazyLock<Regex> = LazyLock::new(|| regex(r"\b0+(\d{1,3})\b"));

/// Whether a word is one of addok-france's street types: "rue", "av",
/// "chemin", "allees"…
pub(super) fn is_street_type(word: &str) -> bool {
    starts(&TYPE_WORD, word)
}

/// `extract_address`: the address within a query that holds more, from its
/// number to the end ("22 rue des Fleurs 59350 Lille Cedex 23" in "XYZ Ets
/// bâtiment B 22 rue des Fleurs 59350 Lille Cedex 23"), or the whole query.
pub(super) fn extract_address(query: &str) -> &str {
    EXTRACT_ADDRESS
        .find(query)
        .unwrap()
        .map_or(query, |address| address.as_str())
}

/// `clean_query`: strips what postal addresses carry besides the address
/// (BP, CS, CEDEX, floors, "lieu-dit"), and spells out "s/" and "s/s".
pub(super) fn clean_query(query: &str) -> String {
    let mut query = query.to_owned();
    for (pattern, replacement) in CLEAN_COMPILED.iter() {
        query = pattern.replace_all(&query, *replacement).into_owned();
    }
    strip(&query).to_owned()
}

/// Python's `str.strip()`: Unicode whitespace, and the separators U+001C to
/// U+001F, which Python counts as whitespace and Rust does not.
fn strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// `remove_leading_zeros`: "0003" becomes "3"; a postcode keeps its zero.
pub(super) fn remove_leading_zeros(query: &str) -> String {
    LEADING_ZEROS.replace_all(query, "${1}").into_owned()
}

/// Python's `str.isdigit()` on a normalized token, ASCII by then.
fn is_number(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())
}

/// `glue_ordinal`: a number of up to 4 digits, then an ordinal that ends the
/// string or precedes a street type, become one token: "12 bis" becomes
/// "12bis", folded next. Like addok-france, a short number followed by
/// another is dropped: "12 14 rue" keeps only "14".
pub(super) fn glue_ordinal(tokens: Vec<Token>) -> Vec<Token> {
    let mut glued = Vec::with_capacity(tokens.len());
    let mut previous: Option<Token> = None;
    let mut tokens = tokens.into_iter().peekable();
    while let Some(mut token) = tokens.next() {
        // addok's `neighborhood` gives no next token after the last, and an
        // empty one counts as none.
        let next = tokens.peek().filter(|next| !next.value.is_empty());
        if next.is_some() && is_number(&token.value) && token.value.chars().count() < 5 {
            previous = Some(token);
            continue;
        }
        if let Some(number) = previous.take() {
            if starts(&ORDINAL_START, &token.value)
                && next.is_none_or(|next| starts(&TYPE_START, &next.value))
            {
                let value = format!("{} {}", number.value, token.value).replace(' ', "");
                token = Token { value, ..number };
            } else {
                glued.push(number);
            }
        }
        glued.push(token);
    }
    glued
}

/// `fold_ordinal`: "3bis" becomes "3b", "10ter" "10t".
pub(super) fn fold_ordinal(mut token: Token) -> Token {
    // addok raises on an empty token here (`s[0]`); no BAN string or address
    // query makes one.
    if token.value.starts_with(|c: char| c.is_ascii_digit())
        && !is_number(&token.value)
        && let Some(folded) = fold(&token.value)
    {
        token.value = folded;
    }
    token
}

fn fold(token: &str) -> Option<String> {
    let parts = FOLD.captures(token).unwrap()?;
    Some(format!("{}{}", &parts[1], short(&parts[2])))
}

/// An ordinal as `fold_ordinal` writes it: "bis" becomes "b", "ter" "t"…;
/// a single letter stays.
fn short(ordinal: &str) -> &str {
    match ordinal.to_lowercase().as_str() {
        "bis" => "b",
        "ter" => "t",
        "quater" => "q",
        "quinquies" => "c",
        "sexies" => "s",
        _ => ordinal,
    }
}

/// A house number's suffix folded as `fold_ordinal` folds it, lowercased:
/// "Bis" becomes "b", "C" "c". None unless the whole suffix is one of
/// addok-france's ordinals.
pub(super) fn short_ordinal(suffix: &str) -> Option<String> {
    starts(&ORDINAL_WORD, suffix).then(|| short(suffix).to_lowercase())
}

/// `flag_housenumber`: the first token that reads as a house number ("12",
/// "12b") and is its string's first word or precedes a street type.
pub(super) fn flag_housenumber(mut tokens: Vec<Token>) -> Vec<Token> {
    let found = (0..tokens.len()).find(|&i| {
        let next = tokens.get(i + 1).filter(|next| !next.value.is_empty());
        (tokens[i].is_first() || next.is_some_and(|next| starts(&TYPE_START, &next.value)))
            && starts(&NUMBER_START, &tokens[i].value)
    });
    if let Some(i) = found {
        tokens[i].housenumber = true;
    }
    tokens
}
