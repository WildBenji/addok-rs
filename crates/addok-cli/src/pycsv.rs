//! Python's csv module as addok-csv uses it, ported from CPython 3.12
//! (Lib/csv.py, Modules/_csv.c), the reference's Python: `Sniffer().sniff`,
//! `reader` fed `str.splitlines(keepends=True)`, `DictReader`'s rows, and
//! `writer` quoting as needed or always. Only the dialects addok-csv makes:
//! quotes doubled, no escape character, not strict.

use std::sync::LazyLock;

use fancy_regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialect {
    pub delimiter: char,
    pub quotechar: char,
    pub skipinitialspace: bool,
    pub lineterminator: &'static str,
    /// `QUOTE_ALL`, else `QUOTE_MINIMAL`.
    pub quote_all: bool,
}

impl Dialect {
    /// `csv.unix_dialect`.
    pub fn unix() -> Dialect {
        Dialect {
            delimiter: ',',
            quotechar: '"',
            skipinitialspace: false,
            lineterminator: "\n",
            quote_all: true,
        }
    }
}

/// `Sniffer().sniff(sample)`, or `None` where it raises "Could not
/// determine delimiter".
pub fn sniff(sample: &str) -> Option<Dialect> {
    let (quotechar, mut delimiter, mut skipinitialspace) = guess_quote_and_delimiter(sample);
    if delimiter.is_none() {
        (delimiter, skipinitialspace) = guess_delimiter(sample);
    }
    Some(Dialect {
        delimiter: delimiter?,
        quotechar: quotechar.unwrap_or('"'),
        skipinitialspace,
        lineterminator: "\r\n",
        quote_all: false,
    })
}

/// `Sniffer._guess_quote_and_delimiter`'s patterns, tried in turn: a quoted
/// field between delimiters, at the start of a line, at its end, alone.
static QUOTED: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        r#"(?P<delim>[^\w\n"'])(?P<space> ?)(?P<quote>["']).*?(?P=quote)(?P=delim)"#,
        r#"(?:^|\n)(?P<quote>["']).*?(?P=quote)(?P<delim>[^\w\n"'])(?P<space> ?)"#,
        r#"(?P<delim>[^\w\n"'])(?P<space> ?)(?P<quote>["']).*?(?P=quote)(?:$|\n)"#,
        r#"(?:^|\n)(?P<quote>["']).*?(?P=quote)(?:$|\n)"#,
    ]
    .map(|pattern| {
        // Python's re has no backtracking limit.
        let pattern = format!("(?sm){pattern}");
        RegexBuilder::new(&pattern)
            .backtrack_limit(usize::MAX)
            .build()
            .unwrap()
    })
});

/// `Sniffer._guess_quote_and_delimiter`, but its double-quote guess, which
/// addok-csv overrides: the quote character, the delimiter and whether
/// spaces follow it, from the quoted fields.
fn guess_quote_and_delimiter(data: &str) -> (Option<char>, Option<char>, bool) {
    let mut found = Vec::new();
    for regex in QUOTED.iter() {
        found = regex.captures_iter(data).map(Result::unwrap).collect();
        if !found.is_empty() {
            break;
        }
    }
    if found.is_empty() {
        return (None, None, false);
    }
    let first_char = |text: &str| text.chars().next();
    // Python's dicts, in insertion order: `max` keeps the first of a tie.
    let mut quotes: Vec<(char, usize)> = Vec::new();
    let mut delims: Vec<(char, usize)> = Vec::new();
    let mut spaces = 0;
    let count = |counts: &mut Vec<(char, usize)>, key: char| match counts
        .iter_mut()
        .find(|(known, _)| *known == key)
    {
        Some((_, n)) => *n += 1,
        None => counts.push((key, 1)),
    };
    for captures in &found {
        let quote = captures.name("quote").and_then(|m| first_char(m.as_str()));
        if let Some(quote) = quote {
            count(&mut quotes, quote);
        }
        let Some(delim) = captures.name("delim") else {
            continue;
        };
        if let Some(delim) = first_char(delim.as_str()) {
            count(&mut delims, delim);
        }
        if captures.name("space").is_some_and(|m| !m.as_str().is_empty()) {
            spaces += 1;
        }
    }
    let most = |counts: &[(char, usize)]| {
        let max = counts.iter().map(|&(_, n)| n).max()?;
        counts.iter().find(|&&(_, n)| n == max).copied()
    };
    let quotechar = most(&quotes).map(|(quote, _)| quote);
    match most(&delims) {
        Some(('\n', _)) => (quotechar, None, false),
        Some((delim, n)) => (quotechar, Some(delim), n == spaces),
        None => (quotechar, None, false),
    }
}

/// `Sniffer._guess_delimiter`: the 7-bit character whose count per line is
/// the most consistent, ten lines at a time; and whether spaces follow it.
fn guess_delimiter(data: &str) -> (Option<char>, bool) {
    const ASCII: usize = 127;
    let lines: Vec<&str> = data.split('\n').filter(|line| !line.is_empty()).collect();
    let counts: Vec<[usize; ASCII]> = lines
        .iter()
        .map(|line| {
            let mut counts = [0; ASCII];
            for byte in line.bytes().filter(|&byte| usize::from(byte) < ASCII) {
                counts[usize::from(byte)] += 1;
            }
            counts
        })
        .collect();
    let chunk = lines.len().min(10);
    // Per character, how many lines held it how many times, in the order
    // the counts came: Python's dict of dicts.
    let mut frequencies: Vec<Vec<(usize, i64)>> = vec![Vec::new(); ASCII];
    let mut modes: Vec<Option<(usize, i64)>> = vec![None; ASCII];
    let mut delims: Vec<(char, (usize, i64))> = Vec::new();
    let (mut start, mut end, mut iteration) = (0, chunk, 0);
    while start < lines.len() {
        iteration += 1;
        for line in &counts[start..end.min(lines.len())] {
            for (c, frequency) in frequencies.iter_mut().enumerate() {
                match frequency.iter_mut().find(|(count, _)| *count == line[c]) {
                    Some((_, lines)) => *lines += 1,
                    None => frequency.push((line[c], 1)),
                }
            }
        }
        for (c, frequency) in frequencies.iter().enumerate() {
            if frequency.len() == 1 && frequency[0].0 == 0 {
                continue;
            }
            let mode = frequency
                .iter()
                .fold(frequency[0], |best, &item| if item.1 > best.1 { item } else { best });
            let others: i64 = frequency.iter().map(|item| item.1).sum::<i64>() - mode.1;
            modes[c] = Some((mode.0, mode.1 - others));
        }
        let total = (chunk * iteration).min(lines.len()) as f64;
        let mut consistency = 1.0;
        while delims.is_empty() && consistency >= 0.9 {
            for (c, mode) in modes.iter().enumerate() {
                if let &Some((frequency, lines)) = mode
                    && frequency > 0
                    && lines > 0
                    && lines as f64 / total >= consistency
                {
                    delims.push((char::from(c as u8), (frequency, lines)));
                }
            }
            consistency -= 0.01;
        }
        if let [(delim, _)] = delims[..] {
            return (Some(delim), follows_spaces(lines[0], delim));
        }
        start = end;
        end += chunk;
    }
    if delims.is_empty() {
        return (None, false);
    }
    for preferred in [',', '\t', ';', ' ', ':'] {
        if delims.iter().any(|&(delim, _)| delim == preferred) {
            return (Some(preferred), follows_spaces(lines[0], preferred));
        }
    }
    let &(delim, _) = delims
        .iter()
        .max_by_key(|&&(delim, mode)| (mode, delim))
        .unwrap();
    (Some(delim), follows_spaces(lines[0], delim))
}

/// Whether a space follows every `delim` of the line, as Python counts:
/// `line.count(delim) == line.count(delim + " ")`.
fn follows_spaces(line: &str, delim: char) -> bool {
    line.matches(delim).count() == line.matches(&format!("{delim} ")).count()
}

/// The records of `reader(text.splitlines(keepends=True), dialect)`.
pub fn records(text: &str, dialect: &Dialect) -> Vec<Vec<String>> {
    let mut parser = Parser {
        dialect,
        state: State::StartRecord,
        field: String::new(),
        fields: Vec::new(),
        records: Vec::new(),
    };
    for line in splitlines(text) {
        for c in line.chars() {
            parser.process(Some(c));
        }
        parser.process(None);
        if parser.state == State::StartRecord {
            let record = std::mem::take(&mut parser.fields);
            parser.records.push(record);
        }
    }
    // The end of the text inside a field: Python saves what it read.
    if !parser.field.is_empty() || parser.state == State::InQuotedField {
        parser.save_field();
        parser.records.push(parser.fields);
    }
    parser.records
}

/// `str.splitlines(keepends=True)`.
fn splitlines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut chars = rest.char_indices();
        let end = loop {
            match chars.next() {
                None => break rest.len(),
                Some((i, '\r')) if rest[i + 1..].starts_with('\n') => break i + 2,
                Some((i, c)) if is_line_boundary(c) => break i + c.len_utf8(),
                Some(_) => {}
            }
        };
        let (line, after) = rest.split_at(end);
        rest = after;
        Some(line)
    })
}

/// The line boundaries of `str.splitlines`.
fn is_line_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}'
            | '\u{2029}'
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    StartRecord,
    StartField,
    InField,
    InQuotedField,
    QuoteInQuotedField,
    EatCrnl,
}

/// `_csv.c`'s reader: its states, and what each does with a character or
/// with the end of a line (`None`).
struct Parser<'d> {
    dialect: &'d Dialect,
    state: State,
    field: String,
    fields: Vec<String>,
    records: Vec<Vec<String>>,
}

impl Parser<'_> {
    fn save_field(&mut self) {
        let field = std::mem::take(&mut self.field);
        self.fields.push(field);
    }

    /// After a field ends with its line, or with a line break.
    fn end_field(&mut self, c: Option<char>) {
        self.save_field();
        self.state = match c {
            None => State::StartRecord,
            Some(_) => State::EatCrnl,
        };
    }

    fn process(&mut self, c: Option<char>) {
        let Dialect {
            delimiter,
            quotechar,
            skipinitialspace,
            ..
        } = *self.dialect;
        let line_end = matches!(c, None | Some('\n' | '\r'));
        match self.state {
            State::StartRecord => match c {
                None => {}
                Some('\n' | '\r') => self.state = State::EatCrnl,
                Some(_) => {
                    self.state = State::StartField;
                    self.process(c);
                }
            },
            State::StartField => match c {
                _ if line_end => self.end_field(c),
                Some(c) if c == quotechar => self.state = State::InQuotedField,
                Some(' ') if skipinitialspace => {}
                Some(c) if c == delimiter => self.save_field(),
                Some(c) => {
                    self.field.push(c);
                    self.state = State::InField;
                }
                None => unreachable!(),
            },
            State::InField => match c {
                _ if line_end => self.end_field(c),
                Some(c) if c == delimiter => {
                    self.save_field();
                    self.state = State::StartField;
                }
                Some(c) => self.field.push(c),
                None => unreachable!(),
            },
            State::InQuotedField => match c {
                None => {}
                Some(c) if c == quotechar => self.state = State::QuoteInQuotedField,
                Some(c) => self.field.push(c),
            },
            State::QuoteInQuotedField => match c {
                Some(c) if c == quotechar => {
                    self.field.push(c);
                    self.state = State::InQuotedField;
                }
                Some(c) if c == delimiter => {
                    self.save_field();
                    self.state = State::StartField;
                }
                _ if line_end => self.end_field(c),
                Some(c) => {
                    self.field.push(c);
                    self.state = State::InField;
                }
                None => unreachable!(),
            },
            State::EatCrnl => match c {
                Some('\n' | '\r') => {}
                None => self.state = State::StartRecord,
                // Python raises "new-line character seen in unquoted field",
                // which a line split by splitlines never brings.
                Some(c) => {
                    self.state = State::StartField;
                    self.process(Some(c));
                }
            },
        }
    }
}

/// A `DictReader` row's value for a column: `None` where the row is too
/// short, as `restval`; the last of columns of the same name.
pub fn value<'r>(fieldnames: &[String], row: &'r [String], key: &str) -> Option<&'r str> {
    if fieldnames[row.len().min(fieldnames.len())..].iter().any(|name| name == key) {
        return None;
    }
    let mut names = fieldnames.iter().zip(row);
    names.rfind(|(name, _)| *name == key).map(|(_, value)| value.as_str())
}

/// `writer(output, dialect).writerow(fields)`.
pub fn write_row<'f>(out: &mut String, fields: impl IntoIterator<Item = &'f str>, dialect: &Dialect) {
    let mut count = 0;
    let mut written = 0;
    for field in fields {
        if count > 0 {
            out.push(dialect.delimiter);
        }
        count += 1;
        written += field.len();
        let special = |c: char| {
            c == dialect.delimiter || c == dialect.quotechar || c == '\n' || c == '\r'
        };
        if dialect.quote_all || field.contains(special) {
            out.push(dialect.quotechar);
            for c in field.chars() {
                if c == dialect.quotechar {
                    out.push(c);
                }
                out.push(c);
            }
            out.push(dialect.quotechar);
        } else {
            out.push_str(field);
        }
    }
    // A record of one empty field is written quoted, not as a blank line.
    if count == 1 && written == 0 && !dialect.quote_all {
        out.push(dialect.quotechar);
        out.push(dialect.quotechar);
    }
    out.push_str(dialect.lineterminator);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal(delimiter: char) -> Dialect {
        Dialect {
            delimiter,
            quotechar: '"',
            skipinitialspace: false,
            lineterminator: "\r\n",
            quote_all: false,
        }
    }

    // Expected values are CPython 3.12's.

    #[test]
    fn sniffs_as_python() {
        let quoted = "ad3,city,zip_code\r\n\"1 RUE A, BAT B\",PARIS,75001\r\n2 RUE C,LYON,69001\r\n";
        assert_eq!(sniff(quoted), Some(minimal(',')));
        let plain = "a;b;c\r\n1;2;3\r\n4;5;6\r\n";
        assert_eq!(sniff(plain), Some(minimal(';')));
        let spaced = "a, b, c\r\n1, 2, 3\r\n";
        let dialect = sniff(spaced).unwrap();
        assert_eq!((dialect.delimiter, dialect.skipinitialspace), (',', true));
        // Every line holds its `\r` once, and `,` is preferred to it.
        assert_eq!(sniff("a,b\r\nc,d\r\n").unwrap().delimiter, ',');
        // `\r` alone: a one-column file, which addok-csv fixes after.
        assert_eq!(sniff("abc\r\ndef\r\n").unwrap().delimiter, '\r');
        // Every character is as consistent: the greatest wins.
        assert_eq!(sniff("abc").unwrap().delimiter, 'c');
        assert_eq!(sniff(""), None);
    }

    #[test]
    fn reads_as_python() {
        let dialect = minimal(',');
        let text = "a,b,c\r\n\"x, \"\"y\"\"\",,z\r\n\r\n\"multi\r\nline\",2\r\nend";
        assert_eq!(
            records(text, &dialect),
            [
                vec!["a", "b", "c"],
                vec!["x, \"y\"", "", "z"],
                vec![],
                vec!["multi\r\nline", "2"],
                vec!["end"],
            ]
        );
        // Text after a closing quote joins the field; an unclosed quote
        // runs to the end.
        assert_eq!(records("\"a\"b,c\r\n\"d", &dialect), [vec!["ab", "c"], vec!["d"]]);
        // splitlines' other boundaries end a record too, kept in the field.
        assert_eq!(records("a\u{2028}b\r\n", &dialect), [vec!["a\u{2028}"], vec!["b"]]);
    }

    #[test]
    fn reads_rows_as_dictreader() {
        let names: Vec<String> = ["a", "b", "a", "c"].map(String::from).to_vec();
        let row: Vec<String> = ["1", "2", "3"].map(String::from).to_vec();
        assert_eq!(value(&names, &row, "a"), Some("3"));
        assert_eq!(value(&names, &row, "b"), Some("2"));
        assert_eq!(value(&names, &row, "c"), None);
        let short: Vec<String> = vec!["1".into()];
        // "a" again past the row's end: restval wins.
        assert_eq!(value(&names, &short, "a"), None);
    }

    #[test]
    fn writes_as_python() {
        let mut out = String::new();
        write_row(&mut out, ["a", "b,c", "d\"e", "f\ng", ""], &minimal(','));
        write_row(&mut out, [""], &minimal(','));
        write_row(&mut out, ["a", ""], &Dialect::unix());
        assert_eq!(out, "a,\"b,c\",\"d\"\"e\",\"f\ng\",\r\n\"\"\r\n\"a\",\"\"\n");
    }
}
