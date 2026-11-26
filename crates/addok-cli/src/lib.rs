//! addok-rs's command line, as a library for its tests: Python's csv
//! module, addok-csv's /search/csv and /reverse/csv on top of it, batch
//! geocoding and reverse geocoding of Parquet or CSV tables, addok's JSON
//! /search and /reverse, the server, the BAN's per-address CSV read for the
//! index, and the export of every house number as batch answers it.

pub mod batch;
pub mod export;
pub mod geocoded;
pub mod http;
pub mod numbers;
pub mod pycsv;
pub mod search_csv;
pub mod search_json;
