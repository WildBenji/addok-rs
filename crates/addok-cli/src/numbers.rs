//! The BAN's per-address CSV (`adresses-france.csv.gz`) read into the
//! table `build --numbers` gives the index: each house number's own
//! former commune, by its id. Read a line at a
//! time: the national file is 4 GB unzipped.

use std::io::BufRead;

use addok_core::index::NumberTable;

/// The columns read, by their names in the header.
const COLUMNS: [&str; 3] = ["id", "code_insee_ancienne_commune", "nom_ancienne_commune"];

/// The table of the CSV's rows, `;`-separated, a header first. An id
/// written more than once keeps its last row.
pub fn read(lines: impl BufRead) -> Result<NumberTable, String> {
    let mut lines = lines.lines();
    let header = match lines.next() {
        Some(line) => line.map_err(|e| e.to_string())?,
        None => return Err("empty, without a header".to_owned()),
    };
    let header = fields(header.trim_start_matches('\u{feff}')).ok_or("header: a quote left open")?;
    let mut at = [0; 3];
    for (column, at) in COLUMNS.iter().zip(&mut at) {
        *at = header
            .iter()
            .position(|name| name == column)
            .ok_or_else(|| format!("header: no {column} column"))?;
    }
    let mut table = NumberTable::default();
    for (i, line) in lines.enumerate() {
        let line = line.map_err(|e| format!("line {}: {e}", i + 2))?;
        if line.is_empty() {
            continue;
        }
        let row = fields(&line).ok_or_else(|| format!("line {}: a quote left open", i + 2))?;
        let [id, code, name] = at.map(|at| row.get(at).map(String::as_str));
        let (Some(id), Some(code), Some(name)) = (id, code, name) else {
            return Err(format!("line {}: {} fields, {} in the header", i + 2, row.len(), header.len()));
        };
        table.insert(id, code, name);
    }
    Ok(table)
}

/// A line's fields: `;`-separated, a field in quotes holding `;` or
/// doubled quotes. None for a quote left open.
fn fields(line: &str) -> Option<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars().peekable();
    let mut starts = true;
    while let Some(c) = chars.next() {
        let field = fields.last_mut().expect("one field at least");
        match c {
            '"' if starts => loop {
                match chars.next()? {
                    '"' if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    '"' => break,
                    c => field.push(c),
                }
            },
            ';' => fields.push(String::new()),
            c => field.push(c),
        }
        starts = c == ';';
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_fields_as_the_ban_quotes_them() {
        let split = |line| fields(line).unwrap();
        assert_eq!(split("a;b;;c"), ["a", "b", "", "c"]);
        assert_eq!(split("a;"), ["a", ""]);
        assert_eq!(split(";"), ["", ""]);
        assert_eq!(split(r#""La Sa;lle";x"#), ["La Sa;lle", "x"]);
        assert_eq!(split(r#"a;"""sous le château""";b"#), ["a", r#""sous le château""#, "b"]);
        assert_eq!(fields(r#"a;"open"#), None);
    }

    #[test]
    fn reads_each_number_s_former_commune() {
        let csv = "\
id;numero;code_postal;code_insee_ancienne_commune;nom_ancienne_commune;nom_ld
01002_0110_00022;22;01641;01997;Vieux;
01002_0110_00030;30;;01998;Arandas;\"Le Pont;du Village\"
01002_0110_00022;22;01642;;;
";
        let table = read(csv.as_bytes()).unwrap();
        assert_eq!(table.len(), 2);
        assert_eq!(table.get("01002_0110_00022"), Some([None, None]));
        assert_eq!(table.get("01002_0110_00030"), Some([Some("01998"), Some("Arandas")]));
    }

    #[test]
    fn refuses_a_csv_without_its_columns_or_with_a_short_row() {
        assert_eq!(read("".as_bytes()).err().unwrap(), "empty, without a header");
        assert_eq!(read("id;code_postal\n".as_bytes()).err().unwrap(), "header: no code_insee_ancienne_commune column");
        let short = "id;code_postal;code_insee_ancienne_commune;nom_ancienne_commune\n1;2\n";
        assert_eq!(read(short.as_bytes()).err().unwrap(), "line 2: 2 fields, 4 in the header");
    }
}
