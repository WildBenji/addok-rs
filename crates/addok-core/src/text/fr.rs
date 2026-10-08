//! addok-fr 1.1.0, ported: its French phonetics (`addok_fr/utils.py`) and the
//! synonyms it ships (`resources/synonyms.txt`, copied verbatim). MIT, see
//! LICENSE-addok.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

use fancy_regex::Regex;

/// addok-fr's `RULES`, in its order and verbatim but for the replacement
/// syntax (`${1}` for `\1`) and one rule, with its comments.
const RULES: [(&str, &str); 38] = [
    (r"(?<=a)(mp|nd|nt)s?$", "n"),            // champ(s) > cham
    (r"([aeiouy])mp(?=[^aeiouyr])", "${1}n"), // champvallon -> chanvalon
    (r"ngt(?=[aeiouy])", "nt"),               // vingtieme > vintieme
    (r"ngt", "n"),                            // vingt > vin
    (r"((?<=[^g])g|^g)(?=[eyi])", "j"),
    (r"(?<=g)u(?=[aeio])", ""),
    (r"(?<=ei)gn([aeiouy])", "ni${1}"), // seigneur -> senieur
    (r"je([aeiouy])", "j${1}"),         // georges -> jorj
    (r"c(?=[^hieyw])", "k"),
    (r"anc$", "an"),                       // blanc -> blan
    (r"((?<=[^s])ch|(?<=[^0-9])c)$", "k"), // final "c", "ch", but not "sch" and not 10c.
    (r"(?<=[aeiouy])s(?=[aeiouy])", "z"),
    (r"((?<=[^0-9])q|^q)u?", "k"),
    (r"cc(?=[ie])", "s"), // Others will hit the c => k and deduplicate
    (r"ck", "k"),
    (r"ph", "f"),
    (r"th$", "te"), // This t sounds.
    (r"(?<=[^sc0-9])h", ""),
    // addok-fr's `^h(?=.)+`: fancy-regex refuses to repeat a lookahead, and
    // repeating it changes nothing (0 differences over 501,836 BAN and address
    // words in Python).
    (r"^h(?=.)", ""),
    (r"sc", "s"),
    (r"sh", "ch"),
    (r"((?<=[^0-9])w|^w)", "v"),
    (r"c(?=[eiy])", "s"),
    (r"((?<=[^0-9])y|^y)", "i"), // also handle y at beginning
    (r"esn", "en"),
    (r"eim( |$)", "aim"),    // pforzheim -> pforzaim
    (r"(ae|ei)(?=\w)", "e"), // improved ae/ei handling
    (r"oeufs( |$)", "eu"),   // special case for oeufs
    (r"oeu?(?=\w)", "eu"),   // oe/oeu -> eu
    (r"(?<=[^0-9])s$", ""),
    (r"(?<=u)l?x$", ""), // eaux, eux, aux, aulx
    (r"(?<=u)lt$", "t"),
    (r"(?<=[a-z])[dg]$", ""),
    (r"(?<=[^es0-9])t$", ""),
    (r"(?<=[aeiou])(m)(?=[pbgft])", "n"), // impossible -> inpossible
    (r"(?<=[a-z]{2})(e$)", ""), // Remove "e" at last position only if it follows two letters?
    (r"(?<=[aeiouy])n[dt](?=[^aeiouyr])", "n"), // montbon -> monbon
    (r"([a-z])\1+", "${1}"),    // Remove duplicate letters
];

static COMPILED: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(|| {
    RULES
        .iter()
        .map(|&(rule, replacement)| (Regex::new(rule).unwrap(), replacement))
        .collect()
});

/// Words whose phonemes each thread keeps, addok-fr's
/// `PHONEMICIZE_CACHE_SIZE`: the rules cost ~10 µs a word, and addresses
/// reuse few words.
const CACHE_SIZE: usize = 500_000;

thread_local! {
    /// Per thread, so that parallel geocoding never waits on a lock. Emptied
    /// when full, where addok's evicts the least recently used word.
    static CACHE: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// addok-fr's "very lite French phonemicization": drops the letters that do
/// not change how a word sounds, so spellings that sound alike share a token.
pub(super) fn phonemicize(word: &str) -> String {
    CACHE.with_borrow_mut(|cache| {
        if let Some(phonemes) = cache.get(word) {
            return phonemes.clone();
        }
        if cache.len() >= CACHE_SIZE {
            cache.clear();
        }
        let phonemes = apply_rules(word);
        cache.insert(word.to_owned(), phonemes.clone());
        phonemes
    })
}

fn apply_rules(word: &str) -> String {
    let mut word = word.to_owned();
    for (rule, replacement) in COMPILED.iter() {
        word = rule.replace_all(&word, *replacement).into_owned();
    }
    word
}

/// addok's `load_synonyms` over addok-fr's file: every comma-separated form
/// left of `=>` stands for the words right of it.
static SYNONYMS: LazyLock<HashMap<&str, &str>> = LazyLock::new(|| {
    let mut synonyms = HashMap::new();
    for line in include_str!("synonyms.txt")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let (forms, wanted) = line.split_once("=>").expect("a synonym line holds =>");
        for form in forms
            .split(',')
            .map(str::trim)
            .filter(|form| !form.is_empty())
        {
            synonyms.insert(form, wanted.trim());
        }
    }
    synonyms
});

/// The words a token stands for: its synonym's, or its own.
pub(super) fn synonym(word: &str) -> &str {
    SYNONYMS.get(word).copied().unwrap_or(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_compiles() {
        assert_eq!(COMPILED.len(), RULES.len());
    }

    #[test]
    fn a_cached_word_keeps_its_phonemes() {
        assert_eq!(phonemicize("seigneur"), "senieur");
        assert_eq!(phonemicize("seigneur"), "senieur");
    }
}
