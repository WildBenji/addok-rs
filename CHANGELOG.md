# Changelog

## v0.10.0 — 2026-10-09

addok's autocomplete, at parity with addok 1.3.2: `/search` now answers as addok's does by default.

### Features

- **Autocomplete on `/search`, on by default as in addok:** the query's last word is taken as the start of one, as someone typing sends it (`8 rue de la paix par`). `autocomplete=0` turns it off, for a complete address and the scores `/search/csv` gives; a blank value turns it on, as in addok. A client written for addok no longer needs `autocomplete=0`.
  - **Search:** `addok_core::search::search_autocomplete`, addok's `search(q, autocomplete=True)`, beside `search_filtered`. Labels are scored by the query they hold: 1 for a label that is the query, 0.9 for one that begins with it, 0.7 for one that contains it.
  - **Measured:** on 90,108 requests over real addresses, autocomplete off, on, and on addresses cut short as typed, under limits 1 to 100 and several filters, every result both answers hold at the same rank is identical. The 2,134 requests where search itself diverges somewhere are all explained by the orders addok leaves to chance, among them the order addok tries autocomplete's words of equal frequency in.
  - **Autocomplete off is unchanged:** the same answers as before on `/search/csv`, `/batch` and `addok-cli batch`, at the same throughput.

### Changed

- **`addok_core::search::search_traced`** takes the autocomplete switch.
- **`addok_core::search::Order`** gains `candidates`, the order to try autocomplete's words of equal score in.
- **`/search` no longer refuses requests without `autocomplete=0`;** `lat` and `lon` still are.

## v0.9.0 — 2026-10-09

addok's JSON `/search`, at parity with addok 1.3.2.

### Features

- **`GET /search`:** addok's JSON endpoint. One query in, its results out as addok's GeoJSON `FeatureCollection`, equal to addok's answer as JSON (its keys in another order). `limit` from 1 to 100, 5 by default; the filters `type`, `citycode` and `postcode` take values, several separated by spaces or with the parameter repeated.
  - **Parameters are read as addok's HTTP layer reads them,** quirks included, and refused with its errors: 400 for a missing `q` or a `limit` out of range, 413 for a query over 200 characters.
  - **Autocomplete must be turned off.** addok autocompletes by default, and autocomplete is not ported yet: a request without `autocomplete=0` is refused with a 400 that says so, rather than answered otherwise than addok would. `lat` and `lon`, and their aliases, are refused too.
  - **A refused request is answered at once,** without waiting for a core busy geocoding.
  - **Measured:** on 90,102 requests over real addresses, under limits 1, 5, 10, 50 and 100 and several filters, every result both answers hold at the same rank is identical, property by property. The 2,675 requests where search itself diverges, at any rank up to the 100th, are all explained by the orders addok leaves to chance.
- **`addok_core::document::Text::first`:** a field's value as addok reads it, a list's first.

### Fixed

- **`/search/csv` reads a blank `with_bom` as true,** as addok-csv does, instead of refusing it.

### Changed

- **The Docker image workflow** uses the Node 24 releases of its actions.

## v0.8.0 — 2026-10-08

addok's filters, at parity with addok 1.3.2, on every interface.

### Features

- **addok's filters:** `type`, `citycode` and `postcode`, several values of one filter meaning any of them, several filters all of them.
  - **Search:** `addok_core::search::search_filtered`, and `Filters`. Under filters, search answers as addok 1.3.2 does, quirks included: a filter scores 1 in intersections, as a Redis set does, and once set it keeps fuzzy matching from suggesting words.
  - **`type` decides the house number:** `housenumber` alone keeps only results with the query's number; other types alone leave the number aside.
  - **`/search/csv`, `/batch` and `addok-cli batch`:** a filter names a column, whose value filters each row (`-F postcode=zip_code`, `--filters postcode=zip_code`), as addok-csv means it. addok-csv 1.1.0 fails on filters with addok 1.3.2. An empty cell filters nothing; a column the file lacks is refused.
  - **The postcode fallback keeps the filters** in its second search.
  - **Measured:** 99.87% of 540,226 filtered searches on real addresses answer as addok's, score included; every other one ties or is explained by the orders addok leaves to chance. Throughput stays within 10% of unfiltered search.

### Changed

- **`addok_core::search::search_traced`** takes the filters.
- **`/search/csv` no longer refuses `type`, `citycode` and `postcode`;** `lat` and `lon` still are.

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
