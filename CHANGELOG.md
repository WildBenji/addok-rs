# Changelog

## v0.12.0 — 2026-10-09

addok's reverse geocoding, at parity with addok 1.3.2, on every interface. With it every addok feature used for geocoding is ported: search, autocomplete, filters, search around a point and reverse. The index is unchanged: no rebuild.

### Features

- **Reverse geocoding:** the address nearest a position, as addok finds it.
  - **`GET /reverse`** is addok's endpoint, in the GeoJSON of `/search`: `lat`, `lon` (with `latitude`, `lng`, `long`, `longitude`), `limit`, the filters `type`, `citycode` and `postcode`. Each result has its `distance` in metres; a street answers as whichever of its house numbers is closest to the position, or as its own point if that is closer.
  - **`POST /reverse/csv`** is addok-csv's: each row's position is read from its `latitude` or `lat` and `longitude`, `lon`, `lng` or `long` columns, and the 15 result columns follow. A row whose position is not made of numbers stays without a result. Filters name columns here as elsewhere; addok-csv 1.1.0 fails on them. A latitude outside [-90, 90) is refused with a 400 naming its row, where addok-csv fails the whole request.
  - **`POST /reverse/batch` and `addok-cli reverse`** bring it to whole files, Parquet or CSV in and out, as `/batch` does for search: the position in addok-csv's columns or in the ones named (`lat`, `lon`; `--lat`, `--lon`), the results typed, `--filters`, `--cores`.
  - **Measured:** on 43,094 `/reverse` requests and 20 files of 1,000 rows over positions of the BAN, offset by 0 to 2 km, and random ones, every answer is identical to addok's but for addresses at exactly the same distance, which addok's sets order as they please.
- **The nearest address, asked for:** `nearest=1` (`--nearest on`) on `/reverse`, `/reverse/csv`, `/reverse/batch` and `addok-cli reverse`. addok looks no further than about 150 m around the position: past that it answers nothing, and an address just outside its cells loses to a farther one inside. `nearest` finds the nearest address within a `radius` in metres (5,000 by default, 10,000 at most). Where addok's cells hold the nearest address it gives addok's answer; elsewhere the nearest. At 1 km from an address addok answers 78% of positions and not with the nearest in 4% of them; `nearest` answers all, and was checked against a scan of all 28.5 million positions of the BAN. Near an address it answers as fast as addok's search; where none lies within the radius it explores it whole, 0.5 ms a position for 5 km.
- **`addok_core::reverse`:** `reverse` and `reverse_nearest`; `addok_core::geohash::ring`.

### Changed

- **The index size in the guides** is corrected to 1.94 GB, as of v0.11.0.

## v0.11.0 — 2026-10-09

addok's search around a point, at parity with addok 1.3.2, on every interface. **The index must be rebuilt** (`addok-cli build`): it gains the geohash sections search around a point reads.

### Features

- **Search around a point** (`lat`, `lon`), as addok does: addresses near the point rank higher, with a distance score up to 0.1 within 100 km, and the addresses right around it (about 150 m) are searched too, so that a house number alone (`12`) finds the nearest ones.
  - **`/search`** reads `lat` and `lon` as addok does, `latitude`, `lng`, `long` and `longitude` included, and answers each result's `distance` in metres and the `center`.
  - **`/search/csv`, `/batch` and `addok-cli batch`** (`--lat COL --lon COL`): two columns give each row its own position, as in addok-csv; a row with an empty cell is searched without one. One column without the other, a column the file lacks, or a value that is not a number or not a latitude is refused with a 400 naming the row, where addok-csv ignores the first two and fails the whole request on the third.
  - **Scores change scale with a position:** the distance's ceiling joins the sum the score is divided by. A confidence threshold set without a position does not hold with one.
  - **Search:** `addok_core::search::search_with` and `Options` (limit, autocomplete, filters, `Center`); `addok_core::geohash`, python-geohash's cells.
  - **Measured:** every geohash cell of addok's index holds as many documents in addok-rs's. On 90,128 requests over real addresses, half around a position near or far, every result both answers hold at the same rank is identical, distance included; the 2,131 requests where search itself diverges are all explained by the orders addok leaves to chance.
- **The index file** grows from 1.86 to 1.94 GB, built in about 90 s as before.

### Changed

- **An index built by an earlier addok-rs is refused,** with the message that says to rebuild it.
- **`addok_core::search::search_traced`** takes `Options`.
- **`/search`, `/search/csv` and `/batch` no longer refuse `lat` and `lon`.**

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
