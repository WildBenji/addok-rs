//! Every house number of an index, one row each, with the values batch
//! answers for it: the same columns, built by the same code, from the
//! document search and reverse answer it with. For addresses that need no
//! geocoding, but must read as a geocoded one would.

use std::io::Write;
use std::sync::Arc;

use addok_core::document::Document;
use addok_core::index::Index;
use addok_core::postings::DocId;
use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;

use crate::batch::{Error, RESULTS, parquet_properties, result_columns, split_columns};
use crate::geocoded::{SPLIT_COLUMNS, answered};

/// The columns batch writes that an export keeps, in order: those a house
/// number fills.
pub const COLUMNS: [&str; 15] = [
    "result_id",
    "result_housenumber",
    "result_num",
    "result_num_complement",
    "result_num_complement_short",
    "result_name",
    "result_postcode",
    "result_citycode",
    "result_city",
    "result_oldcitycode",
    "result_oldcity",
    "result_context",
    "result_district",
    "latitude",
    "longitude",
];

/// The column after them, which batch does not write: the house number's
/// `banId`, the BAN's identifier.
pub const BAN_ID: &str = "result_ban_id";

/// How many rows are built and written at a time.
const ROWS: usize = 100_000;

/// Writes every house number the index answers, a Parquet file of
/// `COLUMNS` and `BAN_ID`, sorted by `result_id`, then by the index's order
/// where ids repeat. Of numbers sharing a token on a street, the one search
/// answers: the index keeps only it. Returns how many rows it wrote.
pub fn export<B: AsRef<[u8]>, W: Write + Send>(index: &Index<B>, out: W) -> Result<usize, Error> {
    let error = |e: parquet::errors::ParquetError| Error(e.to_string());
    let mut numbers: Vec<(Box<str>, DocId, u32)> = Vec::new();
    for doc in 0..index.len() as DocId {
        for (at, _, _) in index.housenumber_positions(doc) {
            let at = u32::try_from(at).expect("fewer than 2^32 house numbers");
            numbers.push((index.housenumber_at(doc, at as usize).id.into(), doc, at));
        }
    }
    numbers.sort_unstable();
    let schema = schema();
    let mut writer = ArrowWriter::try_new(out, schema.clone(), Some(parquet_properties())).map_err(error)?;
    // The last street read: numbers sorted by id mostly follow their street.
    let mut street: (DocId, Document) = (DocId::MAX, Document::default());
    for chunk in numbers.chunks(ROWS) {
        let mut rows = Vec::with_capacity(chunk.len());
        let mut ban_ids = Vec::with_capacity(chunk.len());
        for &(_, doc, at) in chunk {
            if street.0 != doc {
                street = (doc, index.document(doc));
            }
            let number = index.housenumber_at(doc, at as usize);
            // The document a house number answers with, as search and
            // reverse make it.
            let mut document = street.1.clone();
            number.answer_own(&mut document);
            rows.push(Some(answered(&document, Some(&number))));
            ban_ids.push(number.ban_id);
        }
        let batch_columns = RESULTS.iter().map(|(name, _)| *name).zip(result_columns(&rows));
        let split = SPLIT_COLUMNS.into_iter().zip(split_columns(&rows));
        let mut named: Vec<(&str, ArrayRef)> = batch_columns.chain(split).collect();
        let mut columns: Vec<ArrayRef> = COLUMNS
            .iter()
            .map(|&name| {
                let found = named.iter().position(|(own, _)| *own == name).expect("a column batch writes");
                named.swap_remove(found).1
            })
            .collect();
        columns.push(Arc::new(StringArray::from(ban_ids)));
        let batch = RecordBatch::try_new(schema.clone(), columns).map_err(|e| Error(e.to_string()))?;
        writer.write(&batch).map_err(error)?;
    }
    writer.close().map_err(error)?;
    Ok(numbers.len())
}

/// `COLUMNS` typed as batch types them, then `BAN_ID`.
fn schema() -> Arc<Schema> {
    let kind = |name: &str| match RESULTS.iter().find(|(own, _)| *own == name) {
        Some((_, kind)) => kind.clone(),
        None => DataType::Utf8,
    };
    let fields = COLUMNS.iter().chain([&BAN_ID]).map(|&name| Field::new(name, kind(name), true));
    Arc::new(Schema::new(fields.collect::<Vec<_>>()))
}
