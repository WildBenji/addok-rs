//! Geocoding a whole table, Parquet or CSV in, Parquet or CSV out:
//! addok-rs's own format, beside addok-csv's `/search/csv`. Each row keeps
//! its columns, Parquet types included, and gains its best result in typed
//! columns, null where `/search/csv` writes nothing. The values and the
//! minimum score are `/search/csv`'s, `result_street` aside, which no BAN
//! document fills.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use addok_core::document::Number;
use addok_core::index::Index;
use arrow::array::{
    Array, ArrayRef, Float64Builder, RecordBatch, RecordBatchReader, StringArray, StringBuilder,
};
use arrow::compute::{cast, concat_batches};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::geocoded::{FilterColumns, Geocoded, SPLIT_COLUMNS, Warning, geocode, split_housenumber};
use crate::pycsv::{self, Dialect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Parquet,
    /// A header line, then one line per row, fields quoted as needed.
    Csv { delimiter: char },
}

/// The CSV delimiter unless one is given.
pub const DELIMITER: char = ';';

impl Format {
    /// A format by its name: `parquet` or `csv`.
    pub fn named(name: &str, delimiter: char) -> Option<Format> {
        match name {
            "parquet" => Some(Format::Parquet),
            "csv" => Some(Format::Csv { delimiter }),
            _ => None,
        }
    }

    /// A file's format by its extension.
    pub fn of_file(path: &str, delimiter: char) -> Option<Format> {
        let extension = path.rsplit_once('.')?.1.to_lowercase();
        match extension.as_str() {
            "parquet" | "pq" => Some(Format::Parquet),
            "csv" | "txt" => Some(Format::Csv { delimiter }),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Parquet => "parquet",
            Format::Csv { .. } => "csv",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Parquet => "application/vnd.apache.parquet",
            Format::Csv { .. } => "text/csv; charset=utf-8",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The columns a row's query joins, in order: all of them if none.
    pub columns: Vec<String>,
    /// The rounded score a best result must exceed.
    pub min_score: f64,
    /// Threads to geocode on, each taking 1,000 consecutive rows at a time.
    pub threads: usize,
    /// Whether to search again without a wrong postcode.
    pub postcode_fallback: bool,
    /// The result columns asked for besides the default ones: those of
    /// `SPLIT_COLUMNS` named here are added, other names ignored.
    pub result_columns: Vec<String>,
    /// The columns whose values filter each row's search.
    pub filters: FilterColumns,
}

/// Why a table cannot be geocoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn error(error: impl std::fmt::Display) -> Error {
    Error(error.to_string())
}

/// The result columns, in order.
const RESULTS: [(&str, DataType); 16] = [
    ("latitude", DataType::Float64),
    ("longitude", DataType::Float64),
    ("result_label", DataType::Utf8),
    ("result_score", DataType::Float64),
    // 0 without a next result, as /search/csv writes it.
    ("result_score_next", DataType::Float64),
    ("result_type", DataType::Utf8),
    ("result_id", DataType::Utf8),
    ("result_housenumber", DataType::Utf8),
    ("result_name", DataType::Utf8),
    ("result_postcode", DataType::Utf8),
    ("result_city", DataType::Utf8),
    ("result_context", DataType::Utf8),
    ("result_citycode", DataType::Utf8),
    ("result_oldcitycode", DataType::Utf8),
    ("result_oldcity", DataType::Utf8),
    ("result_district", DataType::Utf8),
];

/// A table from its file's bytes.
pub fn read(bytes: Vec<u8>, format: Format) -> Result<RecordBatch, Error> {
    match format {
        Format::Parquet => {
            let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes))
                .and_then(|builder| builder.build())
                .map_err(error)?;
            let schema = reader.schema();
            let batches = reader.collect::<Result<Vec<_>, _>>().map_err(error)?;
            concat_batches(&schema, &batches).map_err(error)
        }
        Format::Csv { delimiter } => {
            let text = String::from_utf8(bytes).map_err(|_| Error("the CSV is not UTF-8".into()))?;
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            let mut records = pycsv::records(text, &dialect(delimiter)).into_iter();
            let header = records.next().unwrap_or_default();
            let rows: Vec<Vec<String>> = records.filter(|row| !row.is_empty()).collect();
            let columns = (0..header.len()).map(|i| {
                let values = rows.iter().map(|row| row.get(i).map(String::as_str));
                Arc::new(values.collect::<StringArray>()) as ArrayRef
            });
            let fields = header.iter().map(|name| Field::new(name, DataType::Utf8, true));
            let schema = Arc::new(Schema::new(fields.collect::<Vec<_>>()));
            RecordBatch::try_new(schema, columns.collect()).map_err(error)
        }
    }
}

/// A row's text in a column, empty if null.
fn cell(column: &StringArray, row: usize) -> &str {
    match column.is_null(row) {
        true => "",
        false => column.value(row),
    }
}

/// The table, each row with its best result, and what geocoding left undone:
/// a row whose query is too long gets no result, and is reported.
pub fn geocode_table<B: AsRef<[u8]> + Sync>(
    index: &Index<B>,
    table: &RecordBatch,
    options: &Options,
) -> Result<(RecordBatch, Vec<Warning>), Error> {
    let schema = table.schema();
    let names: Vec<&str> = match options.columns.is_empty() {
        true => schema.fields().iter().map(|field| field.name().as_str()).collect(),
        false => options.columns.iter().map(String::as_str).collect(),
    };
    let text = |name: &str| -> Result<StringArray, Error> {
        let column = table
            .column_by_name(name)
            .ok_or_else(|| Error(format!("no column \"{name}\"")))?;
        let text = cast(column, &DataType::Utf8).map_err(error)?;
        Ok(text.as_any().downcast_ref::<StringArray>().unwrap().clone())
    };
    let columns = names.into_iter().map(text).collect::<Result<Vec<_>, _>>()?;
    let mut filter_columns = HashMap::new();
    for name in options.filters.columns() {
        filter_columns.insert(name.as_str(), text(name)?);
    }
    let query = |row: usize| {
        let values = columns.iter().map(|column| cell(column, row));
        values.collect::<Vec<_>>().join(" ")
    };
    let filters = |row: usize| options.filters.filters(|name| cell(&filter_columns[name], row));

    let blocks: Vec<std::ops::Range<usize>> = (0..table.num_rows())
        .step_by(1_000)
        .map(|start| start..(start + 1_000).min(table.num_rows()))
        .collect();
    let next = AtomicUsize::new(0);
    let mut too_long = Vec::new();
    let mut geocoded: Vec<(usize, Vec<Option<Geocoded>>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..options.threads.max(1))
            .map(|_| {
                scope.spawn(|| {
                    let (mut done, mut long) = (Vec::new(), Vec::new());
                    while let Some(rows) = blocks.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let block = rows.clone().map(|row| {
                            geocode(index, &query(row), &filters(row), options.min_score, options.postcode_fallback)
                                .unwrap_or_else(|_| {
                                    long.push(row + 1);
                                    None
                                })
                        });
                        done.push((rows.start, block.collect()));
                    }
                    (done, long)
                })
            })
            .collect();
        let mut geocoded = Vec::new();
        for worker in workers {
            let (done, long) = worker.join().unwrap();
            geocoded.extend(done);
            too_long.extend(long);
        }
        geocoded
    });
    geocoded.sort_unstable_by_key(|&(start, _)| start);
    let rows: Vec<Option<Geocoded>> = geocoded.into_iter().flat_map(|(_, block)| block).collect();
    too_long.sort_unstable();
    let warnings = match too_long.is_empty() {
        true => Vec::new(),
        false => vec![Warning::QueryTooLong { rows: too_long }],
    };

    let mut results: Vec<(&str, DataType, ArrayRef)> =
        RESULTS.iter().zip(result_columns(&rows)).map(|((name, kind), array)| (*name, kind.clone(), array)).collect();
    let parts: Vec<[Option<String>; 3]> = rows
        .iter()
        .map(|row| row.as_ref().and_then(|row| row.housenumber.as_deref()).map(split_housenumber).unwrap_or_default())
        .collect();
    for (i, name) in SPLIT_COLUMNS.into_iter().enumerate() {
        if options.result_columns.iter().any(|named| named == name) {
            let column: StringArray = parts.iter().map(|part| part[i].as_deref()).collect();
            results.push((name, DataType::Utf8, Arc::new(column)));
        }
    }
    let mut fields = Vec::new();
    let mut arrays = Vec::new();
    for (field, array) in schema.fields().iter().zip(table.columns()) {
        // A column named as a result gives way to it.
        if !results.iter().any(|(name, _, _)| name == field.name()) {
            fields.push(field.as_ref().clone());
            arrays.push(array.clone());
        }
    }
    for (name, kind, array) in results {
        fields.push(Field::new(name, kind, true));
        arrays.push(array);
    }
    let table = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).map_err(error)?;
    Ok((table, warnings))
}

/// The result columns of these rows, in `RESULTS`'s order.
fn result_columns(rows: &[Option<Geocoded>]) -> Vec<ArrayRef> {
    let float = |value: fn(&Geocoded) -> Option<f64>| -> ArrayRef {
        let mut column = Float64Builder::with_capacity(rows.len());
        for row in rows {
            column.append_option(row.as_ref().and_then(value));
        }
        Arc::new(column.finish())
    };
    let text = |value: fn(&Geocoded) -> Option<&str>| -> ArrayRef {
        let mut column = StringBuilder::new();
        for row in rows {
            column.append_option(row.as_ref().and_then(value));
        }
        Arc::new(column.finish())
    };
    vec![
        float(|row| row.lat.map(Number::value)),
        float(|row| row.lon.map(Number::value)),
        text(|row| Some(&row.label)),
        float(|row| Some(row.score)),
        float(|row| Some(row.score_next.unwrap_or(0.0))),
        text(|row| Some(&row.kind)),
        text(|row| Some(&row.id)),
        text(|row| row.housenumber.as_deref()),
        text(|row| row.name.as_deref()),
        text(|row| row.postcode.as_deref()),
        text(|row| row.city.as_deref()),
        text(|row| row.context.as_deref()),
        text(|row| row.citycode.as_deref()),
        text(|row| row.oldcitycode.as_deref()),
        text(|row| row.oldcity.as_deref()),
        text(|row| row.district.as_deref()),
    ]
}

/// The table's file.
pub fn write(table: &RecordBatch, format: Format) -> Result<Vec<u8>, Error> {
    match format {
        Format::Parquet => {
            let properties = WriterProperties::builder()
                .set_compression(Compression::ZSTD(ZstdLevel::default()))
                .build();
            let mut writer =
                ArrowWriter::try_new(Vec::new(), table.schema(), Some(properties)).map_err(error)?;
            writer.write(table).map_err(error)?;
            writer.into_inner().map_err(error)
        }
        Format::Csv { delimiter } => {
            let dialect = dialect(delimiter);
            let mut out = String::new();
            let schema = table.schema();
            let names = schema.fields().iter().map(|field| field.name().as_str());
            pycsv::write_row(&mut out, names, &dialect);
            let options = FormatOptions::default().with_null("");
            let formatters = table
                .columns()
                .iter()
                .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
                .collect::<Result<Vec<_>, _>>()
                .map_err(error)?;
            for row in 0..table.num_rows() {
                let cells: Vec<String> = formatters
                    .iter()
                    .map(|formatter| formatter.value(row).to_string())
                    .collect();
                pycsv::write_row(&mut out, cells.iter().map(String::as_str), &dialect);
            }
            Ok(out.into_bytes())
        }
    }
}

fn dialect(delimiter: char) -> Dialect {
    Dialect {
        delimiter,
        quotechar: '"',
        skipinitialspace: false,
        lineterminator: "\n",
        quote_all: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use addok_core::document::Document;
    use addok_core::index::{AlignedBytes, write as write_index};
    use arrow::array::{Float64Array, Int64Array};

    /// An index of a municipality and one of its streets, as the BAN
    /// writes them.
    fn index() -> Index<AlignedBytes> {
        let lines = [
            r#"{"id":"01002","banId":null,"type":"municipality","name":"L'Abergement-de-Varey","postcode":["01640"],"citycode":"01002","x":887247.06,"y":6548295.66,"lon":5.420189,"lat":46.008573,"population":270,"city":"L'Abergement-de-Varey","context":"01, Ain, Auvergne-Rhône-Alpes","importance":0.22554}"#,
            r#"{"id":"01002_0110","banId":null,"name":"Montee de la Foret","postcode":"01640","citycode":["01002"],"oldcitycode":null,"lon":5.421924,"lat":46.007342,"x":887385.45,"y":6548163.14,"city":["L'Abergement-de-Varey"],"oldcity":null,"context":"01, Ain, Auvergne-Rhône-Alpes","type":"street","importance":0.3672,"housenumbers":{"12bis":{"id":"01002_0110_00012_bis","banId":null,"x":887385.45,"y":6548163.14,"lon":5.421924,"lat":46.007342}}}"#,
        ];
        let documents = lines.map(|line| Document::from_ndjson(line).unwrap());
        let mut bytes = AlignedBytes::default();
        write_index(documents, &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    fn options(columns: &[&str]) -> Options {
        Options {
            columns: columns.iter().map(|&column| column.to_owned()).collect(),
            min_score: 0.5,
            threads: 2,
            postcode_fallback: false,
            result_columns: Vec::new(),
            filters: FilterColumns::default(),
        }
    }

    #[test]
    fn geocodes_a_csv_into_a_csv() {
        let csv = "id;street;city\n1;MONTEE DE LA FORET;L ABERGEMENT DE VAREY\n2;zzz;qqq\n";
        let table = read(csv.as_bytes().to_vec(), Format::Csv { delimiter: ';' }).unwrap();
        let (geocoded, warnings) = geocode_table(&index(), &table, &options(&["street", "city"])).unwrap();
        assert!(warnings.is_empty());
        let out = String::from_utf8(write(&geocoded, Format::Csv { delimiter: ',' }).unwrap()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "id,street,city,latitude,longitude,result_label,result_score,result_score_next,\
             result_type,result_id,result_housenumber,result_name,result_postcode,result_city,\
             result_context,result_citycode,result_oldcitycode,result_oldcity,result_district"
        );
        assert!(
            lines[1].starts_with(
                "1,MONTEE DE LA FORET,L ABERGEMENT DE VAREY,46.007342,5.421924,\
                 Montee de la Foret 01640 L'Abergement-de-Varey,"
            ),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with(",street,01002_0110,,Montee de la Foret,01640,L'Abergement-de-Varey,\"01, Ain, Auvergne-Rhône-Alpes\",01002,,,"));
        // No result: every result column empty.
        assert_eq!(lines[2], format!("2,zzz,qqq{}", ",".repeat(16)));
    }

    #[test]
    fn keeps_parquet_types_and_types_its_results() {
        let ids = Arc::new(Int64Array::from(vec![7, 8])) as ArrayRef;
        let streets = Arc::new(StringArray::from(vec![Some("montee de la foret 01640"), None])) as ArrayRef;
        // A column named as a result gives way to it.
        let stale = Arc::new(StringArray::from(vec!["old", "old"])) as ArrayRef;
        let schema = Schema::new(vec![
            Field::new("row_id", DataType::Int64, false),
            Field::new("street", DataType::Utf8, true),
            Field::new("result_label", DataType::Utf8, false),
        ]);
        let table = RecordBatch::try_new(Arc::new(schema), vec![ids, streets, stale]).unwrap();
        let bytes = write(&table, Format::Parquet).unwrap();
        let table = read(bytes, Format::Parquet).unwrap();
        let (geocoded, _) = geocode_table(&index(), &table, &options(&["street"])).unwrap();
        let geocoded = read(write(&geocoded, Format::Parquet).unwrap(), Format::Parquet).unwrap();
        let schema = geocoded.schema();
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(schema.fields().len(), 2 + RESULTS.len());
        let label = geocoded.column_by_name("result_label").unwrap();
        let label = label.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(label.value(0), "Montee de la Foret 01640 L'Abergement-de-Varey");
        assert!(label.is_null(1));
        let score = geocoded.column_by_name("result_score").unwrap();
        let score = score.as_any().downcast_ref::<Float64Array>().unwrap();
        assert!(score.value(0) > 0.5 && score.is_null(1));
    }

    #[test]
    fn adds_the_house_number_split_only_when_named() {
        let csv = "id;street\n1;12 BIS MONTEE DE LA FORET\n2;MONTEE DE LA FORET\n3;zzz\n";
        let table = read(csv.as_bytes().to_vec(), Format::Csv { delimiter: ';' }).unwrap();
        let (plain, _) = geocode_table(&index(), &table, &options(&["street"])).unwrap();
        assert_eq!(plain.num_columns(), 2 + RESULTS.len());
        let mut asked = options(&["street"]);
        asked.result_columns = ["result_num_complement_short", "result_label", "result_num"].map(str::to_owned).to_vec();
        let (split, _) = geocode_table(&index(), &table, &asked).unwrap();
        let schema = split.schema();
        let names: Vec<&str> = schema.fields().iter().skip(2 + RESULTS.len()).map(|field| field.name().as_str()).collect();
        assert_eq!(names, ["result_num", "result_num_complement_short"]);
        let text = |name: &str| {
            let column = split.column_by_name(name).unwrap().as_any().downcast_ref::<StringArray>().unwrap().clone();
            (0..3).map(|row| (!column.is_null(row)).then(|| column.value(row).to_owned())).collect::<Vec<_>>()
        };
        assert_eq!(text("result_housenumber"), [Some("12bis".to_owned()), None, None]);
        assert_eq!(text("result_num"), [Some("12".to_owned()), None, None]);
        assert_eq!(text("result_num_complement_short"), [Some("b".to_owned()), None, None]);
    }

    #[test]
    fn refuses_a_missing_column() {
        let table = read(b"a;b\n1;2\n".to_vec(), Format::Csv { delimiter: ';' }).unwrap();
        let refused = geocode_table(&index(), &table, &options(&["c"]));
        assert_eq!(refused, Err(Error("no column \"c\"".into())));
        let mut filtered = options(&["a"]);
        filtered.filters.postcode = vec!["zip".to_owned()];
        let refused = geocode_table(&index(), &table, &filtered);
        assert_eq!(refused, Err(Error("no column \"zip\"".into())));
    }

    #[test]
    fn filters_each_row_by_its_own_value() {
        let csv = "id;street;zip;kind\n1;MONTEE DE LA FORET;01640;\n2;MONTEE DE LA FORET;69001;\n3;MONTEE DE LA FORET;;municipality\n";
        let table = read(csv.as_bytes().to_vec(), Format::Csv { delimiter: ';' }).unwrap();
        let mut filtered = options(&["street"]);
        filtered.filters = FilterColumns {
            kind: vec!["kind".to_owned()],
            citycode: Vec::new(),
            postcode: vec!["zip".to_owned()],
        };
        let (geocoded, _) = geocode_table(&index(), &table, &filtered).unwrap();
        let types = geocoded.column_by_name("result_type").unwrap();
        let types = types.as_any().downcast_ref::<StringArray>().unwrap();
        // Its postcode: found. Another one: nothing. An empty cell filters
        // nothing, and the third row's type keeps only municipalities,
        // which the street's name does not find.
        assert_eq!(types.value(0), "street");
        assert!(types.is_null(1));
        assert!(types.is_null(2));
    }

    #[test]
    fn names_formats() {
        assert_eq!(Format::of_file("addresses.PARQUET", ';'), Some(Format::Parquet));
        assert_eq!(Format::of_file("out.csv", '|'), Some(Format::Csv { delimiter: '|' }));
        assert_eq!(Format::of_file("addresses", ';'), None);
        assert_eq!(Format::named("csv", ';'), Some(Format::Csv { delimiter: ';' }));
    }

    #[test]
    fn answers_a_row_too_long_to_search_empty_and_reports_it() {
        let junk = "VOLUPTATEM ".repeat(25);
        let csv = format!("id;street\n1;MONTEE DE LA FORET\n2;{junk}\n3;MONTEE DE LA FORET\n");
        let table = read(csv.into_bytes(), Format::Csv { delimiter: ';' }).unwrap();
        let (geocoded, warnings) = geocode_table(&index(), &table, &options(&["street"])).unwrap();
        assert_eq!(warnings, [Warning::QueryTooLong { rows: vec![2] }]);
        let labels = geocoded.column_by_name("result_label").unwrap();
        assert_eq!((labels.is_null(0), labels.is_null(1), labels.is_null(2)), (false, true, false));
    }
}
