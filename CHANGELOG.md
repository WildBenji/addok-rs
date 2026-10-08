# Changelog

## v0.7.0 — 2026-10-08

The first public release.

### Features

- **`addok-cli build`** builds the index from the BAN's national Addok export (`adresses-addok-france.ndjson.gz`): one 1.86 GB file, in about 85 s on one core. The new index is written beside the old one, then renamed over it.
- **`addok-cli batch`** geocodes a whole Parquet or CSV file on every core, without a server: about 45,000 addresses/s on an 8-core M1 Pro, reading and writing included.
- **`addok-cli serve`** serves the index over HTTP, on port 7878 as addok does:
  - **`POST /search/csv`:** addok-csv 1.1.0's endpoint, byte for byte, with three of its defects fixed: a CSV delimiter guessed wrong, an always-empty `result_street` column, and a whole file refused for one row over 200 characters.
  - **`POST /batch`:** Parquet or CSV in, Parquet or CSV out, input columns keeping their type and results typed.
  - **`GET /health`:** the index loaded, the cores in use and the version, answered at once even under load.
  - **`X-Addok-Warning`:** a header naming the rows left without a result for a reason other than an address not found.
- **Answers as addok 1.3.2 gives them,** scores included, with addok-fr 1.1.0 and addok-france 1.2.0: identical on 99.86% of 88,811 real address queries, every other answer tied with addok's or explained by the order addok leaves to chance.
- **Deterministic ties:** score, then importance, then BAN id.
- **Two improvements, off by default:**
  - **`postcode_fallback`:** a wrong postcode no longer loses an address in a commune the query names.
  - **`result_columns`:** the house number split into `result_num`, `result_num_complement` and `result_num_complement_short`.
- **Docker image** `ghcr.io/wildbenji/addok-rs`, for linux/amd64 and linux/arm64, the index mounted from outside.

### Not yet supported

- addok's filters (`type`, `citycode`, `postcode`), refused with a 400 rather than ignored.
- Search around a point (`lat`, `lon`), reverse geocoding, the JSON `/search` endpoint and autocomplete.
