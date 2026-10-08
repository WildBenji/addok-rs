//! The text chain: what addok makes of a string before indexing or searching
//! it, ported from addok 1.3.2 (`addok/helpers/text.py`) with the processors
//! of addok-fr and addok-france, chained as the BAN configures them. Ported
//! to answer as addok does, quirks included: it gives addok's tokens for
//! every string the BAN indexes and for real address queries.

mod fr;
mod france;

/// A query's token, with what search reads from it.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub value: String,
    /// Whether it is the query's house number, which search matches against
    /// a street's numbers instead of looking it up in the index.
    pub housenumber: bool,
    /// Its word's rank in the query, then its rank among the words of the
    /// synonym it was expanded into. House-number tokens join in this order.
    pub position: Vec<usize>,
}

impl Token {
    /// Whether it comes from its string's first word (addok's `is_first`).
    fn is_first(&self) -> bool {
        self.position[0] == 0
    }
}

/// The longest query addok accepts, in characters (`QUERY_MAX_LENGTH`).
pub const QUERY_MAX_LENGTH: usize = 200;

/// A query longer than [`QUERY_MAX_LENGTH`], which addok refuses.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryTooLong;

impl std::fmt::Display for QueryTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "query longer than {QUERY_MAX_LENGTH} characters")
    }
}

impl std::error::Error for QueryTooLong {}

/// The tokens addok indexes a document's field value under (`preprocess`).
pub fn index_tokens(value: &str) -> Vec<String> {
    process(value)
        .into_iter()
        .map(|token| token.value)
        .collect()
}

/// The tokens addok searches a query with (`preprocess_query`): the query
/// stripped of what is not an address, then processed as an indexed value.
pub fn query_tokens(query: &str) -> Result<Vec<Token>, QueryTooLong> {
    if query.chars().count() > QUERY_MAX_LENGTH {
        return Err(QueryTooLong);
    }
    let address = france::clean_query(france::extract_address(query));
    Ok(process(&france::remove_leading_zeros(&address)))
}

/// addok's `PROCESSORS`, in the BAN's order.
fn process(text: &str) -> Vec<Token> {
    let tokens = tokenize(text).into_iter().map(normalize).collect();
    let tokens = france::glue_ordinal(tokens)
        .into_iter()
        .map(france::fold_ordinal)
        .collect();
    let tokens = synonymize(france::flag_housenumber(tokens));
    tokens
        .into_iter()
        .map(|token| Token {
            value: fr::phonemicize(&token.value),
            ..token
        })
        .collect()
}

/// `tokenize`: the runs of word characters, each numbered by its rank.
fn tokenize(text: &str) -> Vec<Token> {
    let words = text
        .split(|c: char| !is_word(c))
        .filter(|word| !word.is_empty());
    let tokens = words.enumerate().map(|(rank, word)| Token {
        value: word.to_owned(),
        housenumber: false,
        position: vec![rank],
    });
    tokens.collect()
}

/// Python's `\w`: letters, numbers and `_`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Unidecode's transliteration where deunicode's differs, for the characters
/// the BAN or real addresses hold, and only those:
/// Unidecode is GPLv2+.
const UNIDECODE: [(char, &str); 1] = [('½', " 1/2")];

/// Lowercased, then transliterated to ASCII, dropping what has no
/// transliteration, as Unidecode does.
fn transliterate(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let mut text = text.to_lowercase();
    for (character, ascii) in UNIDECODE {
        if text.contains(character) {
            text = text.replace(character, ascii);
        }
    }
    deunicode::deunicode_with_tofu(&text, "")
}

/// `normalize`: a token transliterated.
fn normalize(token: Token) -> Token {
    Token {
        value: transliterate(&token.value),
        ..token
    }
}

/// Whether a word is one of addok-france's street types: "rue", "av",
/// "chemin", "allees"…, matched as addok-france matches them, case aside.
pub fn is_street_type(word: &str) -> bool {
    france::is_street_type(word)
}

/// A house number's suffix as addok-france folds it, lowercased: "bis"
/// becomes "b", "ter" "t", "quater" "q", "quinquies" "c", "sexies" "s", a
/// single letter stays ("C" becomes "c"). None for any other suffix.
pub fn short_ordinal(suffix: &str) -> Option<String> {
    france::short_ordinal(suffix)
}

/// addok's `ascii`, which search applies to the whole query before it
/// tokenizes it, and to labels before comparing them with the query:
/// transliterated, then every character but ASCII letters, digits and `_`
/// made a space, spaces collapsed and trimmed. "n°5" becomes "ndeg5", and
/// "s/ mer" "s mer".
pub fn fold(text: &str) -> String {
    let text = transliterate(text);
    let mut folded = String::with_capacity(text.len());
    let words = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
    for word in words.filter(|word| !word.is_empty()) {
        if !folded.is_empty() {
            folded.push(' ');
        }
        folded.push_str(word);
    }
    folded
}

/// `synonymize`: each token replaced by its synonym's words if it has one,
/// split on whitespace either way; a word's rank extends its position.
fn synonymize(tokens: Vec<Token>) -> Vec<Token> {
    let mut words = Vec::with_capacity(tokens.len());
    for token in tokens {
        for (rank, word) in fr::synonym(&token.value).split_whitespace().enumerate() {
            let position = [&token.position[..], &[rank]].concat();
            words.push(Token {
                value: word.to_owned(),
                housenumber: token.housenumber,
                position,
            });
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected tokens are addok's own answers.

    fn query(q: &str) -> Vec<(String, bool, Vec<usize>)> {
        let tokens = query_tokens(q).unwrap();
        tokens
            .into_iter()
            .map(|t| (t.value, t.housenumber, t.position))
            .collect()
    }

    fn tokens(expected: &[(&str, bool, &[usize])]) -> Vec<(String, bool, Vec<usize>)> {
        expected
            .iter()
            .map(|&(value, housenumber, position)| {
                (value.to_owned(), housenumber, position.to_vec())
            })
            .collect()
    }

    #[test]
    fn splits_lowercases_and_transliterates() {
        assert_eq!(
            index_tokens("Montee de la Foret"),
            ["monte", "de", "la", "foret"]
        );
        assert_eq!(
            index_tokens("L'Abergement-de-Varey"),
            ["l", "aberjemen", "de", "varei"]
        );
        assert_eq!(index_tokens("Œuvre Ærø Straße"), ["euvr", "ero", "stras"]);
        assert_eq!(index_tokens("½ chemin"), ["1/2", "chemin"]);
    }

    #[test]
    fn folds_as_addok_s_ascii() {
        let fold = |text| super::fold(text);
        assert_eq!(
            fold("n°5 route de st jean s/ mer 06230"),
            "ndeg5 route de st jean s mer 06230"
        );
        assert_eq!(
            fold("Rue du Général  de Gaulle "),
            "rue du general de gaulle"
        );
        assert_eq!(fold("L'Abergement-de-Varey"), "l abergement de varey");
        assert_eq!(fold("½ chemin"), "1 2 chemin");
        assert_eq!(fold("ŒUVRE – Ærø"), "oeuvre aero");
        assert_eq!(fold("  !! "), "");
    }

    #[test]
    fn transliterates_as_unidecode_where_deunicode_differs() {
        // The BAN writes "œ" as "½" in a few names; Unidecode's " 1/2" then
        // splits the word.
        assert_eq!(index_tokens("Bonn½uvre"), ["bon", "1/2uvr"]);
    }

    #[test]
    fn applies_the_french_phonetic_rules() {
        assert_eq!(
            index_tokens("Rue du Général de Gaulle"),
            ["ru", "du", "jeneral", "de", "gaul"]
        );
        assert_eq!(
            index_tokens("Vingt seigneur Georges Pforzheim boeufs chateaux"),
            ["vin", "senieur", "jorj", "pforzaim", "beu", "chateau"]
        );
        assert_eq!(
            index_tokens("Champvallon Montbon impossible"),
            ["chanvalon", "monbon", "inposibl"]
        );
    }

    #[test]
    fn expands_synonyms() {
        assert_eq!(
            index_tokens("Allée des 3 Fontaines"),
            ["ale", "de", "troi", "fontain"]
        );
        assert_eq!(index_tokens("St Michel"), ["sain", "michel"]);
        assert_eq!(index_tokens("ª ²"), ["a", "deu"]);
        assert_eq!(index_tokens("75002"), ["75002"]);
    }

    #[test]
    fn glues_ordinals_to_their_number() {
        assert_eq!(index_tokens("10 ter avenue"), ["10t", "avenu"]);
        assert_eq!(index_tokens("4 bis"), ["4b"]);
        assert_eq!(index_tokens("1 a 3 rue"), ["un", "a", "troi", "ru"]);
        assert_eq!(index_tokens("12345 b rue"), ["12345", "b", "ru"]);
        assert_eq!(
            query("6bis place carnot"),
            tokens(&[
                ("6b", true, &[0, 0]),
                ("plas", false, &[1, 0]),
                ("karno", false, &[2, 0])
            ])
        );
    }

    #[test]
    fn drops_a_number_followed_by_another() {
        // addok-france's glue_ordinal forgets the first of two numbers.
        assert_eq!(index_tokens("12 14 rue"), ["katorz", "ru"]);
    }

    #[test]
    fn flags_the_house_number() {
        assert_eq!(
            query("8 bis rue de la paix 75002 paris"),
            tokens(&[
                ("8b", true, &[0, 0]),
                ("ru", false, &[2, 0]),
                ("de", false, &[3, 0]),
                ("la", false, &[4, 0]),
                ("paix", false, &[5, 0]),
                ("75002", false, &[6, 0]),
                ("pari", false, &[7, 0]),
            ])
        );
        assert_eq!(
            query("7 b chemin des vignes"),
            tokens(&[
                ("7b", true, &[0, 0]),
                ("chemin", false, &[2, 0]),
                ("de", false, &[3, 0]),
                ("vign", false, &[4, 0]),
            ])
        );
        assert_eq!(
            query("rue haute 12"),
            tokens(&[
                ("ru", false, &[0, 0]),
                ("aut", false, &[1, 0]),
                ("douz", false, &[2, 0])
            ])
        );
    }

    #[test]
    fn folds_a_suffix_as_addok_france() {
        let short = |suffix| short_ordinal(suffix);
        assert_eq!(short("bis").as_deref(), Some("b"));
        assert_eq!(short("Bis").as_deref(), Some("b"));
        assert_eq!(short("TER").as_deref(), Some("t"));
        assert_eq!(short("quater").as_deref(), Some("q"));
        assert_eq!(short("quinquies").as_deref(), Some("c"));
        assert_eq!(short("sexies").as_deref(), Some("s"));
        assert_eq!(short("C").as_deref(), Some("c"));
        assert_eq!(short("a").as_deref(), Some("a"));
        for suffix in ["bis a", "appt 1", "qua", "P1", "a1", "", "route"] {
            assert_eq!(short(suffix), None, "{suffix}");
        }
    }

    #[test]
    fn knows_addok_france_s_street_types() {
        for word in ["rue", "RUE", "av", "avenue", "allees", "chemin", "imp", "route", "bois", "cite"] {
            assert!(is_street_type(word), "{word}");
        }
        for word in ["paris", "de", "saint", "rues", ""] {
            assert!(!is_street_type(word), "{word}");
        }
    }

    #[test]
    fn keeps_the_address_out_of_a_longer_query() {
        assert_eq!(
            query("Ets Dupont batiment B 22 rue des Fleurs 59350 Lille Cedex 23"),
            tokens(&[
                ("22", true, &[0, 0]),
                ("ru", false, &[1, 0]),
                ("de", false, &[2, 0]),
                ("fleur", false, &[3, 0]),
                ("59", false, &[4, 0]),
                ("lil", false, &[5, 0]),
            ])
        );
        assert_eq!(
            query("residence les pins 13 allee b 33000 bordeaux"),
            tokens(&[
                ("trez", true, &[0, 0]),
                ("ale", false, &[1, 0]),
                ("b", false, &[2, 0]),
                ("33000", false, &[3, 0]),
                ("bordeau", false, &[4, 0]),
            ])
        );
    }

    #[test]
    fn cleans_postal_noise() {
        assert_eq!(
            query("BP 123 75001 Paris Cedex 01"),
            tokens(&[("75", true, &[0, 0]), ("pari", false, &[1, 0])])
        );
        assert_eq!(
            query("CS 70001 33000 Bordeaux"),
            tokens(&[("33000", false, &[0, 0]), ("bordeau", false, &[1, 0])])
        );
        assert_eq!(
            query("3ème étage 5 rue de la gare 69001 lyon"),
            tokens(&[
                ("sink", true, &[0, 0]),
                ("ru", false, &[1, 0]),
                ("de", false, &[2, 0]),
                ("la", false, &[3, 0]),
                ("gar", false, &[4, 0]),
                ("69001", false, &[5, 0]),
                ("lion", false, &[6, 0]),
            ])
        );
        assert_eq!(
            query("lieu dit les granges 12345 village"),
            tokens(&[
                ("le", false, &[0, 0]),
                ("granj", false, &[1, 0]),
                ("12345", false, &[2, 0]),
                ("vilaj", false, &[3, 0]),
            ])
        );
        assert_eq!(
            query("route de st jean s/ mer 06230"),
            tokens(&[
                ("rout", false, &[0, 0]),
                ("de", false, &[1, 0]),
                ("sain", false, &[2, 0]),
                ("jan", false, &[3, 0]),
                ("sur", false, &[4, 0]),
                ("mer", false, &[5, 0]),
                ("06230", false, &[6, 0]),
            ])
        );
    }

    #[test]
    fn removes_leading_zeros() {
        assert_eq!(
            query("0003 avenue foch 75016 paris"),
            tokens(&[
                ("troi", true, &[0, 0]),
                ("avenu", false, &[1, 0]),
                ("fok", false, &[2, 0]),
                ("75016", false, &[3, 0]),
                ("pari", false, &[4, 0]),
            ])
        );
    }

    #[test]
    fn refuses_a_query_over_200_characters() {
        assert_eq!(query_tokens(&"x".repeat(201)), Err(QueryTooLong));
        assert!(query_tokens(&"x".repeat(200)).is_ok());
    }
}
