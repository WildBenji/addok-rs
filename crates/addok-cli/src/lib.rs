//! addok-rs's command line, as a library for its tests: Python's csv
//! module, addok-csv's /search/csv and /reverse/csv on top of it, batch
//! geocoding and reverse geocoding of Parquet or CSV tables, addok's JSON
//! /search and /reverse, and the server.

pub mod batch;
pub mod geocoded;
pub mod http;
pub mod pycsv;
pub mod search_csv;
pub mod search_json;
