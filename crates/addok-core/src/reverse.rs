//! Reverse geocoding: the addresses nearest a position, ported from addok
//! 1.3.2's `Reverse` (addok/core.py) with the `REVERSE_RESULT_PROCESSORS`
//! the BAN leaves as addok sets them: each document as the closest of its
//! house numbers and itself, labelled by addok's own `make_labels` (not
//! addok-france's), scored by its distance alone. Ported to answer as addok
//! does, but for ties: where addok's order follows its sets, results rank
//! by score, then by document number. `reverse_nearest`, asked for, is the
//! one departure: the nearest addresses wherever they lie within a radius,
//! where addok looks no further than the cells around the position.

use std::collections::HashSet;

use crate::document::{Document, Number, Text};
use crate::geohash::{self, Cell};
use crate::index::Index;
use crate::postings::{self, DocId};
use crate::search::{Center, Filters, Found, GEO_DISTANCE_WEIGHT, haversine_distance, km_to_score, python_sum};

/// addok's `reverse(lat, lon, limit, **filters)`: the `limit` documents
/// nearest the position, best first, among those the filters hold in the
/// cell of the position and its eight neighbours, or, if none, in the ring
/// of sixteen around them. None for a latitude python-geohash refuses,
/// where addok fails.
pub fn reverse<B: AsRef<[u8]>>(index: &Index<B>, center: Center, limit: usize, filters: &Filters) -> Vec<Found> {
    let Some(cell) = geohash::encode(center.lat, center.lon) else {
        return Vec::new();
    };
    let filter = filters.documents(index);
    let held = |cells: &[Cell]| {
        let lists: Vec<&[DocId]> = cells.iter().map(|&cell| index.geohash(cell)).collect();
        let docs = postings::union_sets(&lists);
        match &filter {
            None => docs,
            Some(filter) => postings::intersect_sets(&[&docs, filter]),
        }
    };
    let fetched = geohash::expand(cell);
    let mut docs = held(&fetched);
    if docs.is_empty() {
        // addok's `expand` of the cells fetched: their neighbours not
        // fetched yet.
        let ring: Vec<Cell> = fetched
            .iter()
            .flat_map(|&cell| geohash::expand(cell))
            .filter(|cell| !fetched.contains(cell))
            .collect();
        docs = held(&ring);
    }
    let checks = filters.housenumber_checks();
    let mut candidates: Vec<Candidate> = docs.into_iter().filter_map(|doc| closest(index, doc, center, checks)).collect();
    ranked(&mut candidates);
    candidates.truncate(limit);
    candidates.into_iter().map(|candidate| candidate.found(index)).collect()
}

/// A document as reverse finds it, before it is made a result: its house
/// number closest to the position if it answers as one, its distance in
/// kilometres, and the score of that distance.
struct Candidate {
    doc: DocId,
    at: Option<usize>,
    km: f64,
    score: f64,
}

impl Candidate {
    /// The candidate made a result, labelled.
    fn found<B: AsRef<[u8]>>(self, index: &Index<B>) -> Found {
        let document = index.document(self.doc);
        let housenumber = self.at.map(|at| index.housenumber_at(self.doc, at));
        let labels = labels(&document, housenumber.as_ref().map(|number| number.number.as_str()));
        Found {
            doc: self.doc,
            document,
            housenumber,
            labels,
            importance: 0.0,
            str_distance: 0.0,
            score: self.score,
            distance: Some(self.km * 1000.0),
        }
    }
}

/// Best first: score descending, then document number.
fn ranked(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
}

/// The `limit` documents nearest the position within `radius_m` metres,
/// best first, each as `reverse` makes it: rings of cells explored outward
/// until no document left unexplored can come closer than those found.
/// The same answer as `reverse` where its cells hold the nearest; others
/// where they do not, or hold nothing.
pub fn reverse_nearest<B: AsRef<[u8]>>(
    index: &Index<B>,
    center: Center,
    limit: usize,
    filters: &Filters,
    radius_m: f64,
) -> Vec<Found> {
    let Some(cell) = geohash::encode(center.lat, center.lon).filter(|_| limit > 0) else {
        return Vec::new();
    };
    let filter = filters.documents(index);
    let checks = filters.housenumber_checks();
    // The narrowest side of a cell, north or east, a degree further from the
    // equator than the position for room: what each ring adds, at least,
    // to the distance of what lies beyond it.
    let cell_m = cell_side_m(center.lat);
    let mut considered: HashSet<DocId> = HashSet::new();
    let mut found: Vec<Candidate> = Vec::new();
    for k in 0i64.. {
        let ring = geohash::ring(cell, k);
        for &cell in &ring {
            for &doc in index.geohash(cell) {
                let held = filter.as_ref().is_none_or(|filter| filter.binary_search(&doc).is_ok());
                if held && considered.insert(doc) {
                    found.extend(closest(index, doc, center, checks));
                }
            }
        }
        ranked(&mut found);
        found.retain(|found| found.km * 1000.0 <= radius_m);
        // What lies beyond ring `k` is at least `k` cells away.
        let bound = k as f64 * cell_m;
        let enough = found.len() >= limit && found[limit - 1].km * 1000.0 <= bound;
        if enough || bound > radius_m || ring.is_empty() {
            break;
        }
    }
    found.truncate(limit);
    found.into_iter().map(|candidate| candidate.found(index)).collect()
}

/// A cell's narrowest side in metres around a latitude: its height, or its
/// width a degree nearer the pole, where meridians close in.
fn cell_side_m(lat: f64) -> f64 {
    const METRES_PER_DEGREE: f64 = 111_000.0;
    let height = 180.0 / (1u64 << 17) as f64 * METRES_PER_DEGREE;
    let poleward = (lat.abs() + 1.0).min(90.0).to_radians().cos();
    let width = 360.0 / (1u64 << 18) as f64 * METRES_PER_DEGREE * poleward;
    height.min(width)
}

/// A document as addok's `load_closer` makes it: its house number closest
/// to the position, if closer than the document itself, when the `type`
/// filter lets house numbers be matched; none when it asks for house
/// numbers only and none is. Then scored by its distance.
fn closest<B: AsRef<[u8]>>(
    index: &Index<B>,
    doc: DocId,
    center: Center,
    (check_housenumber, only_housenumber): (bool, bool),
) -> Option<Candidate> {
    let km = |(lat, lon): (Number, Number)| haversine_distance((lat.value(), lon.value()), (center.lat, center.lon));
    let own = index.position(doc);
    // addok sorts the house numbers, then the document, by distance, the
    // first of equals kept.
    let mut best: Option<(Option<usize>, f64)> = None;
    if check_housenumber {
        for (at, lat, lon) in index.housenumber_positions(doc) {
            let distance = km((lat, lon));
            if best.is_none_or(|(_, best)| distance < best) {
                best = Some((Some(at), distance));
            }
        }
        if !only_housenumber && let Some(own) = own {
            let distance = km(own);
            if best.is_none_or(|(_, best)| distance < best) {
                best = Some((None, distance));
            }
        }
    }
    let (at, distance) = match best {
        Some(best) => best,
        None => (None, km(own?)),
    };
    if only_housenumber && at.is_none() {
        return None;
    }
    let geo = km_to_score(distance) * GEO_DISTANCE_WEIGHT;
    let score = python_sum(&[geo]) / python_sum(&[GEO_DISTANCE_WEIGHT]);
    Some(Candidate { doc, at, km: distance, score })
}

/// addok's own `make_labels`: the document's names, the first followed by
/// its postcode and city when the city is not the name, and preceded by
/// the house number; a list's first value each.
fn labels(document: &Document, housenumber: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = match &document.name {
        Text::One(name) => vec![name.clone()],
        Text::Many(names) if !names.is_empty() => names.clone(),
        _ => vec![String::new()],
    };
    let mut label = names[0].clone();
    let city = document.city.first().unwrap_or_default();
    if !city.is_empty() && city != label {
        let postcode = document.postcode.first().unwrap_or_default();
        if !postcode.is_empty() {
            label = format!("{label} {postcode}");
        }
        label = format!("{label} {city}");
    }
    if let Some(number) = housenumber.filter(|number| !number.is_empty()) {
        label = format!("{number} {label}");
    }
    names[0] = label;
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{AlignedBytes, write};

    /// A municipality and one of its streets, with house numbers 2 and 4,
    /// and a street of another commune 200 km away.
    fn index() -> Index<AlignedBytes> {
        let lines = [
            r#"{"id":"01002","banId":null,"type":"municipality","name":"L'Abergement-de-Varey","postcode":["01640"],"citycode":"01002","x":0,"y":0,"lon":5.420189,"lat":46.008573,"population":270,"city":"L'Abergement-de-Varey","context":"01, Ain","importance":0.22554}"#,
            r#"{"id":"01002_0110","banId":null,"name":"Montee de la Foret","postcode":"01640","citycode":["01002"],"oldcitycode":null,"lon":5.421924,"lat":46.007342,"x":0,"y":0,"city":["L'Abergement-de-Varey"],"oldcity":null,"context":"01, Ain","type":"street","importance":0.3672,"housenumbers":{"2":{"id":"01002_0110_00002","banId":null,"x":0,"y":0,"lon":5.4232,"lat":46.0068},"4":{"id":"01002_0110_00004","banId":null,"x":0,"y":0,"lon":5.4226,"lat":46.0071}}}"#,
            r#"{"id":"69381_0001","banId":null,"name":"Rue Lima","postcode":"69001","citycode":"69381","lon":4.83,"lat":45.76,"x":0,"y":0,"city":"Lyon","context":"69, Rhône","type":"street","importance":0.5}"#,
        ];
        let mut bytes = AlignedBytes::default();
        write(lines.map(|line| Document::from_ndjson(line).unwrap()), &mut bytes).unwrap();
        Index::open(bytes).unwrap()
    }

    fn ids(found: &[Found]) -> Vec<&str> {
        found.iter().map(Found::id).collect()
    }

    #[test]
    fn answers_the_closest_house_number_or_document() {
        let index = index();
        let at = |lat: f64, lon: f64| Center { lat, lon };
        // On number 4: the street becomes it.
        let found = reverse(&index, at(46.0071, 5.4226), 5, &Filters::default());
        assert_eq!(ids(&found), ["01002_0110_00004"]);
        assert_eq!(found[0].label(), "4 Montee de la Foret 01640 L'Abergement-de-Varey");
        assert_eq!(found[0].distance, Some(0.0));
        assert_eq!(found[0].score, 1.0);
        // On the municipality: its label its name, its city being the same.
        let found = reverse(&index, at(46.008573, 5.420189), 5, &Filters::default());
        assert_eq!(ids(&found)[0], "01002");
        assert_eq!(found[0].label(), "L'Abergement-de-Varey");
        // Limited, best first.
        assert_eq!(ids(&reverse(&index, at(46.008573, 5.420189), 1, &Filters::default())), ["01002"]);
    }

    #[test]
    fn keeps_to_what_the_type_filter_asks() {
        let index = index();
        let near = Center { lat: 46.0071, lon: 5.4226 };
        let kind = |kinds: &[&str]| Filters { kind: kinds.iter().map(|&kind| kind.to_owned()).collect(), ..Filters::default() };
        // Streets only: the street itself, its numbers left aside.
        assert_eq!(ids(&reverse(&index, near, 5, &kind(&["street"]))), ["01002_0110"]);
        assert_eq!(ids(&reverse(&index, near, 5, &kind(&["housenumber"]))), ["01002_0110_00004"]);
    }

    #[test]
    fn finds_the_nearest_within_the_radius() {
        let index = index();
        let none = Filters::default();
        // 2 km north of number 4, where addok finds nothing: the nearest.
        let far = Center { lat: 46.025, lon: 5.4226 };
        assert!(reverse(&index, far, 1, &none).is_empty());
        let found = reverse_nearest(&index, far, 1, &none, 5000.0);
        assert_eq!(ids(&found), ["01002"]);
        assert!(found[0].distance.unwrap() > 1500.0);
        // Not past the radius.
        assert!(reverse_nearest(&index, far, 1, &none, 1000.0).is_empty());
        // Near an address, addok's answer.
        let near = Center { lat: 46.0071, lon: 5.4226 };
        assert_eq!(reverse_nearest(&index, near, 1, &none, 5000.0), reverse(&index, near, 1, &none));
        // The nearest several, in order, and by the type filter.
        assert_eq!(ids(&reverse_nearest(&index, near, 2, &none, 5000.0)), ["01002_0110_00004", "01002"]);
        let streets = Filters { kind: vec!["street".to_owned()], ..Filters::default() };
        assert_eq!(ids(&reverse_nearest(&index, far, 5, &streets, 5000.0)), ["01002_0110"]);
        // Lyon is 100 km off: past any radius asked here.
        assert_eq!(reverse_nearest(&index, far, 5, &none, 5000.0).len(), 2);
        assert!(reverse_nearest(&index, far, 0, &none, 5000.0).is_empty());
    }

    #[test]
    fn looks_one_ring_further_then_gives_up() {
        let index = index();
        // About 500 m north of number 4: past the nine cells, in the ring.
        let ring = reverse(&index, Center { lat: 46.0116, lon: 5.4226 }, 5, &Filters::default());
        assert!(!ring.is_empty());
        // 2 km away: nothing, as in addok.
        assert!(reverse(&index, Center { lat: 46.025, lon: 5.4226 }, 5, &Filters::default()).is_empty());
        // A latitude python-geohash refuses.
        assert!(reverse(&index, Center { lat: 90.0, lon: 5.0 }, 5, &Filters::default()).is_empty());
    }
}
