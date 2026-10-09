//! addok-rs's command line.
//!
//! ```sh
//! addok-cli build adresses-addok-france.ndjson.gz ban.addok
//! addok-cli serve ban.addok --host 0.0.0.0 --port 7878
//! addok-cli batch ban.addok addresses.parquet geocoded.parquet --columns ad3,city,zip_code
//! addok-cli reverse ban.addok positions.parquet addresses.parquet --nearest on
//! addok-cli --version
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Instant;

use addok_cli::batch::{self, Format, Options, ReverseOptions};
use addok_cli::geocoded::{FilterColumns, MIN_SCORE, PositionColumns, nearest_radius};
use addok_cli::search_csv::flag;
use addok_core::document::Document;
use addok_core::index::{Index, OpenError, write};
use flate2::read::MultiGzDecoder;
use memmap2::Mmap;

const USAGE: &str = "\
usage:
  addok-cli build <ndjson[.gz]> <index>    index the BAN's NDJSON into a file
  addok-cli serve <index> [--host HOST] [--port PORT] [--cores N]
                                           serve /search, /reverse, their /csv, /batch and
                                           /reverse/batch (default 127.0.0.1:7878)
  addok-cli batch <index> <input> <output> [options]
                                           geocode a Parquet or CSV file into another
    --columns A,B,C           the columns a row's query joins (default: all)
    --input-format FORMAT     parquet or csv (default: the input's extension)
    --output-format FORMAT    parquet or csv (default: the output's extension)
    --input-delimiter C       the input CSV's delimiter (default: ;)
    --output-delimiter C      the output CSV's delimiter (default: ;)
    --min-score X             the rounded score a result must exceed (default: 0.5)
    --postcode-fallback on    search again without a wrong postcode (default: off)
    --result-columns A,B      add the house number split: result_num, result_num_complement,
                              result_num_complement_short (default: none)
    --filters F=COL,F=COL     keep the results each row's value in COL allows, for each
                              filter F: type, citycode or postcode (default: none)
    --lat COL --lon COL       search each row around its position in these columns
                              (default: none)
    --cores N                 (see below)
  addok-cli reverse <index> <input> <output> [options]
                                           the address nearest each row's position, a Parquet or
                                           CSV file into another
    --lat COL --lon COL       the position's columns (default: latitude or lat, then
                              longitude, lon, lng or long)
    --nearest on              the nearest address within the radius, where addok looks no
                              further than about 150 m around (default: off)
    --radius M                the nearest's radius in metres, up to 10000 (default: 5000)
    --filters, --input-format, --output-format, --input-delimiter, --output-delimiter,
    --cores N                 as for batch
  addok-cli --version                      print addok-cli's version, as /health gives it

  --cores N: how many cores to use, from 1 to the machine's (default: all of them)";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match args[..] {
        ["build", ndjson, index] => build(ndjson, index),
        ["serve", index, ref options @ ..] => serve(index, options),
        ["batch", index, input, output, ref options @ ..] => batch_file(index, input, output, options),
        ["reverse", index, input, output, ref options @ ..] => reverse_file(index, input, output, options),
        ["--version"] => {
            println!("addok-cli {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Indexes the NDJSON, gzipped or not, into a file.
fn build(ndjson: &str, index: &str) -> Result<(), String> {
    let start = Instant::now();
    let file = File::open(ndjson).map_err(|e| format!("{ndjson}: {e}"))?;
    let reader: Box<dyn Read> = match ndjson.ends_with(".gz") {
        true => Box::new(MultiGzDecoder::new(file)),
        false => Box::new(file),
    };
    let mut failed = None;
    let documents = BufReader::new(reader).lines().enumerate().map_while(|(i, line)| {
        let document = line
            .map_err(|e| e.to_string())
            .and_then(|line| Document::from_ndjson(&line).map_err(|e| e.to_string()));
        document
            .map_err(|e| failed = Some(format!("{ndjson}, line {}: {e}", i + 1)))
            .ok()
    });
    // Written apart, then renamed over the index: a server mapping the old
    // file keeps it, and a failed build leaves it whole.
    let partial = format!("{index}.partial");
    let out = File::create(&partial).map_err(|e| format!("{partial}: {e}"))?;
    let written = write(documents, BufWriter::new(out)).map_err(|e| format!("{partial}: {e}"));
    let sizes = match (written, failed) {
        (Ok(sizes), None) => sizes,
        (Err(failed), _) | (_, Some(failed)) => {
            let _ = std::fs::remove_file(&partial);
            return Err(failed);
        }
    };
    std::fs::rename(&partial, index).map_err(|e| format!("{index}: {e}"))?;
    let bytes: u64 = sizes.iter().map(|(_, size)| size).sum();
    println!(
        "{index}: {:.2} GB in {} sections, written in {:.0?}",
        bytes as f64 / 1e9,
        sizes.len(),
        start.elapsed()
    );
    Ok(())
}

/// Memory-maps the index and serves it.
fn serve(index: &str, options: &[&str]) -> Result<(), String> {
    let (mut host, mut port) = ("127.0.0.1", "7878");
    let mut cores = available_cores();
    for option in options.chunks(2) {
        match *option {
            ["--host", value] => host = value,
            ["--port", value] => port = value,
            ["--cores", value] => cores = parse_cores(value)?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let address: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("{host}:{port}: {e}"))?;
    let index = open(index)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime
        .block_on(addok_cli::http::serve(index, address, cores))
        .map_err(|e| format!("{address}: {e}"))
}

/// Geocodes a Parquet or CSV file into another, on every core.
fn batch_file(index: &str, input: &str, output: &str, options: &[&str]) -> Result<(), String> {
    let mut columns = Vec::new();
    let mut result_columns = Vec::new();
    let (mut input_format, mut output_format) = (None, None);
    let (mut input_delimiter, mut output_delimiter) = (batch::DELIMITER, batch::DELIMITER);
    let mut min_score = MIN_SCORE;
    let mut postcode_fallback = false;
    let mut filters = FilterColumns::default();
    let (mut lat, mut lon) = (None, None);
    let mut cores = available_cores();
    for option in options.chunks(2) {
        match *option {
            ["--columns", value] => columns = value.split(',').map(str::to_owned).collect(),
            ["--result-columns", value] => result_columns = value.split(',').map(str::to_owned).collect(),
            ["--input-format", value] => input_format = Some(value),
            ["--output-format", value] => output_format = Some(value),
            ["--input-delimiter", value] => input_delimiter = one(value)?,
            ["--output-delimiter", value] => output_delimiter = one(value)?,
            ["--min-score", value] => min_score = value.parse().map_err(|_| USAGE.to_owned())?,
            ["--postcode-fallback", value] => postcode_fallback = flag(value).ok_or(USAGE.to_owned())?,
            ["--filters", value] => filters = filter_columns(value)?,
            ["--lat", value] => lat = Some(value),
            ["--lon", value] => lon = Some(value),
            ["--cores", value] => cores = parse_cores(value)?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let input_format = format(input_format, input, input_delimiter)?;
    let output_format = format(output_format, output, output_delimiter)?;
    let index = open(index)?;
    let start = Instant::now();
    let bytes = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
    let table = batch::read(bytes, input_format).map_err(|e| format!("{input}: {e}"))?;
    let options = Options {
        columns,
        min_score,
        threads: cores,
        postcode_fallback,
        result_columns,
        filters,
        position: PositionColumns::named(lat, lon).map_err(|_| "--lat and --lon go together".to_owned())?,
    };
    let (geocoded, warnings) = batch::geocode_table(&index, &table, &options).map_err(|e| format!("{input}: {e}"))?;
    for warning in &warnings {
        eprintln!("{input}: warning: {warning}");
    }
    let bytes = batch::write(&geocoded, output_format).map_err(|e| format!("{output}: {e}"))?;
    std::fs::write(output, bytes).map_err(|e| format!("{output}: {e}"))?;
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{output}: {} rows geocoded in {elapsed:.1} s, {:.0} rows/s",
        table.num_rows(),
        table.num_rows() as f64 / elapsed
    );
    Ok(())
}

/// The address nearest each row's position, a file into another.
fn reverse_file(index: &str, input: &str, output: &str, options: &[&str]) -> Result<(), String> {
    let (mut input_format, mut output_format) = (None, None);
    let (mut input_delimiter, mut output_delimiter) = (batch::DELIMITER, batch::DELIMITER);
    let mut filters = FilterColumns::default();
    let (mut lat, mut lon, mut nearest, mut radius) = (None, None, None, None);
    let mut cores = available_cores();
    for option in options.chunks(2) {
        match *option {
            ["--input-format", value] => input_format = Some(value),
            ["--output-format", value] => output_format = Some(value),
            ["--input-delimiter", value] => input_delimiter = one(value)?,
            ["--output-delimiter", value] => output_delimiter = one(value)?,
            ["--filters", value] => filters = filter_columns(value)?,
            ["--lat", value] => lat = Some(value),
            ["--lon", value] => lon = Some(value),
            ["--nearest", value] => nearest = Some(value),
            ["--radius", value] => radius = Some(value),
            ["--cores", value] => cores = parse_cores(value)?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let nearest = nearest_radius(|name| match name {
        "nearest" => nearest,
        _ => radius,
    })
    .map_err(|e| e.replace("\"nearest\"", "--nearest").replace("\"radius\"", "--radius"))?;
    let input_format = format(input_format, input, input_delimiter)?;
    let output_format = format(output_format, output, output_delimiter)?;
    let index = open(index)?;
    let start = Instant::now();
    let bytes = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
    let table = batch::read(bytes, input_format).map_err(|e| format!("{input}: {e}"))?;
    let options = ReverseOptions {
        position: PositionColumns::named(lat, lon).map_err(|_| "--lat and --lon go together".to_owned())?,
        threads: cores,
        filters,
        nearest,
    };
    let found = batch::reverse_table(&index, &table, &options).map_err(|e| format!("{input}: {e}"))?;
    let bytes = batch::write(&found, output_format).map_err(|e| format!("{output}: {e}"))?;
    std::fs::write(output, bytes).map_err(|e| format!("{output}: {e}"))?;
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "{output}: {} rows in {elapsed:.1} s, {:.0} rows/s",
        table.num_rows(),
        table.num_rows() as f64 / elapsed
    );
    Ok(())
}

/// A file's format: the one named, or its extension's.
fn format(name: Option<&str>, path: &str, delimiter: char) -> Result<Format, String> {
    match name {
        Some(name) => Format::named(name, delimiter).ok_or(format!("unknown format {name:?}")),
        None => Format::of_file(path, delimiter).ok_or(format!("{path}: name its format with --input-format or --output-format")),
    }
}

/// A CSV delimiter: one character.
fn one(value: &str) -> Result<char, String> {
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(format!("a delimiter is one character, not {value:?}")),
    }
}

/// `--filters type=COL,postcode=COL`: the columns of each filter, a filter
/// named once per column.
fn filter_columns(value: &str) -> Result<FilterColumns, String> {
    let mut filters = FilterColumns::default();
    for pair in value.split(',') {
        let column = |prefix: &str| pair.strip_prefix(prefix).filter(|column| !column.is_empty()).map(str::to_owned);
        match (column("type="), column("citycode="), column("postcode=")) {
            (Some(column), _, _) => filters.kind.push(column),
            (_, Some(column), _) => filters.citycode.push(column),
            (_, _, Some(column)) => filters.postcode.push(column),
            _ => return Err(format!("--filters takes type=, citycode= or postcode= and a column, not {pair:?}")),
        }
    }
    Ok(filters)
}

/// The cores this process may use: the machine's, or fewer where a
/// container limits it.
fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get())
}

/// `--cores`: a whole number from 1 to the available cores.
fn parse_cores(value: &str) -> Result<usize, String> {
    let available = available_cores();
    match value.parse() {
        Ok(cores) if (1..=available).contains(&cores) => Ok(cores),
        Ok(cores) if cores > available => Err(format!(
            "--cores {cores}: this machine has {available} cores available"
        )),
        _ => Err(format!(
            "--cores takes a whole number from 1 to {available}, not {value:?}"
        )),
    }
}

fn open(path: &str) -> Result<Index<Mmap>, String> {
    let start = Instant::now();
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    // SAFETY: the index is read only, and a new release is a new file:
    // nothing writes the file while it is mapped.
    let bytes = unsafe { Mmap::map(&file) }.map_err(|e| format!("{path}: {e}"))?;
    let index = Index::open(bytes).map_err(|e| match e {
        OpenError::MissingSection(_) => {
            format!("{path}: {e}; rebuild it with `addok-cli build`")
        }
        _ => format!("{path}: {e}"),
    })?;
    println!("{path}: {} documents, opened in {:.1?}", index.len(), start.elapsed());
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_from_one_core_to_those_available() {
        let available = available_cores();
        assert_eq!(parse_cores("1"), Ok(1));
        assert_eq!(parse_cores(&available.to_string()), Ok(available));
        let over = (available + 1).to_string();
        assert_eq!(
            parse_cores(&over),
            Err(format!("--cores {over}: this machine has {available} cores available"))
        );
        for value in ["0", "-1", "2.5", "abc", ""] {
            assert!(parse_cores(value).is_err(), "{value:?}");
        }
    }
}
