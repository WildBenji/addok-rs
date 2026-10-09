//! Geohashes as addok computes them with python-geohash 0.8.5: a position's
//! cell, 7 characters long (addok's `GEOHASH_PRECISION`), about 150 m high
//! and 105 m wide in France, and the cells around one. Search around a
//! point looks for documents in the cell of the point and its eight
//! neighbours; reverse geocoding, in those and the ring around them.

/// addok's `GEOHASH_PRECISION`.
pub const PRECISION: usize = 7;

const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// The bits a cell gives each axis: longitude takes the first bit, and so
/// one more when their sum is odd.
const LON_BITS: u32 = (5 * PRECISION as u32).div_ceil(2);
const LAT_BITS: u32 = 5 * PRECISION as u32 / 2;

/// A cell, its bits interleaved from the longitude's first: the number its
/// characters spell in base 32.
pub type Cell = u64;

/// The cell of a position: python-geohash's `encode(lat, lon, 7)`. None
/// for a latitude outside [-90, 90), which it refuses; a longitude wraps
/// around.
pub fn encode(lat: f64, lon: f64) -> Option<Cell> {
    if !(-90.0..90.0).contains(&lat) {
        return None;
    }
    let lon = (lon + 180.0).rem_euclid(360.0) - 180.0;
    Some(cell(index(lat / 90.0, LAT_BITS), index(lon / 180.0, LON_BITS)))
}

/// The cell and its eight neighbours, those north of the north pole and
/// south of the south one left out: python-geohash's `expand`.
pub fn expand(code: Cell) -> Vec<Cell> {
    let (lat, lon) = decode(code);
    let mut cells = Vec::with_capacity(9);
    for dlat in [-1i64, 0, 1] {
        let lat = lat + dlat;
        if !(0..1 << LAT_BITS).contains(&lat) {
            continue;
        }
        for dlon in [-1i64, 0, 1] {
            // Around the antimeridian, longitudes wrap.
            cells.push(cell(lat, (lon + dlon).rem_euclid(1 << LON_BITS)));
        }
    }
    cells
}

/// The cells `k` cells away from one, in every direction: its ring of
/// `8k` (the cell itself for 0), those beyond a pole left out, longitudes
/// wrapping around.
pub fn ring(code: Cell, k: i64) -> Vec<Cell> {
    let (lat, lon) = decode(code);
    let at = |dlat: i64, dlon: i64| {
        let lat = lat + dlat;
        (0..1 << LAT_BITS).contains(&lat).then(|| cell(lat, (lon + dlon).rem_euclid(1 << LON_BITS)))
    };
    if k == 0 {
        return vec![code];
    }
    let mut cells = Vec::with_capacity(8 * k as usize);
    for d in -k..k {
        cells.extend(at(-k, d));
        cells.extend(at(d, k));
        cells.extend(at(k, -d));
        cells.extend(at(-d, -k));
    }
    cells
}

/// A cell as python-geohash writes it, in base 32.
pub fn text(code: Cell) -> String {
    let total = LAT_BITS + LON_BITS;
    (0..PRECISION as u32)
        .map(|i| BASE32[(code >> (total - 5 * (i + 1)) & 31) as usize] as char)
        .collect()
}

/// The cell python-geohash writes so; none for another length or a
/// character outside its base 32.
pub fn parse(text: &str) -> Option<Cell> {
    if text.len() != PRECISION {
        return None;
    }
    text.bytes().try_fold(0, |code, byte| {
        let value = BASE32.iter().position(|&c| c == byte)?;
        Some(code << 5 | value as Cell)
    })
}

/// A coordinate's index among `bits`-bit slices of [-1, 1), from its
/// fraction of its axis: exact, the fraction scaled by a power of two.
fn index(fraction: f64, bits: u32) -> i64 {
    let half = 1i64 << (bits - 1);
    (fraction * half as f64).floor() as i64 + half
}

/// The cell of two indexes, their bits interleaved from the longitude's.
fn cell(lat: i64, lon: i64) -> Cell {
    let mut code = 0;
    for bit in 0..LAT_BITS + LON_BITS {
        let (axis, from) = match bit % 2 {
            0 => (lon, LON_BITS - 1 - bit / 2),
            _ => (lat, LAT_BITS - 1 - bit / 2),
        };
        code = code << 1 | (axis as u64 >> from & 1);
    }
    code
}

/// A cell's two indexes.
fn decode(code: Cell) -> (i64, i64) {
    let (mut lat, mut lon) = (0, 0);
    for bit in 0..LAT_BITS + LON_BITS {
        let set = (code >> (LAT_BITS + LON_BITS - 1 - bit) & 1) as i64;
        match bit % 2 {
            0 => lon = lon << 1 | set,
            _ => lat = lat << 1 | set,
        }
    }
    (lat, lon)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// python-geohash 0.8.5's C extension's own answers: a position, its
    /// cell, and the cells `expand` gives, sorted. On cell boundaries, in
    /// France and overseas.
    const VECTORS: &[(f64, f64, &str, &str)] = &[
        (48.8566, 2.3522, "u09tvw0", "u09tvmz u09tvqp u09tvqr u09tvtb u09tvtc u09tvw0 u09tvw1 u09tvw2 u09tvw3"),
        (45.758, 4.835, "u05kq1b", "u05kmcx u05kmcz u05kmfp u05kq18 u05kq19 u05kq1b u05kq1c u05kq40 u05kq41"),
        (43.2965, 5.3698, "spey61y", "spey61t spey61v spey61w spey61x spey61y spey61z spey64j spey64n spey64p"),
        (-21.1151, 55.5364, "mhprzu0", "mhprzez mhprzgb mhprzgc mhprzsp mhprzsr mhprzu0 mhprzu1 mhprzu2 mhprzu3"),
        (4.9372, -52.326, "dbfu16c", "dbfu168 dbfu169 dbfu16b dbfu16c dbfu16d dbfu16f dbfu170 dbfu171 dbfu174"),
        (16.2412, -61.533, "dduhru9", "dduhru2 dduhru3 dduhru6 dduhru8 dduhru9 dduhrub dduhruc dduhrud dduhruf"),
        (14.6161, -61.0588, "ddse4sf", "ddse4s9 ddse4sc ddse4sd ddse4se ddse4sf ddse4sg ddse4t1 ddse4t4 ddse4t5"),
        (-12.7806, 45.2279, "mj8pm1m", "mj8pm1h mj8pm1j mj8pm1k mj8pm1m mj8pm1n mj8pm1q mj8pm1s mj8pm1t mj8pm1w"),
        (46.7811, -56.1764, "fb241sj", "fb241eu fb241ev fb241ey fb241sh fb241sj fb241sk fb241sm fb241sn fb241sq"),
        (0.0, 0.0, "s000000", "7zzzzzz ebpbpbp ebpbpbr kpbpbpb kpbpbpc s000000 s000001 s000002 s000003"),
        (-1e-07, -1e-07, "7zzzzzz", "7zzzzzw 7zzzzzx 7zzzzzy 7zzzzzz ebpbpbn ebpbpbp kpbpbp8 kpbpbpb s000000"),
        (0.001373291015625, 0.001373291015625, "s000003", "s000000 s000001 s000002 s000003 s000004 s000006 s000008 s000009 s00000d"),
        (-0.001373291015625, -0.001373291015625, "7zzzzzz", "7zzzzzw 7zzzzzx 7zzzzzy 7zzzzzz ebpbpbn ebpbpbp kpbpbp8 kpbpbpb s000000"),
        (44.001373291015625, 3.001373291015625, "spf4k61", "spf4k3b spf4k3c spf4k3f spf4k60 spf4k61 spf4k62 spf4k63 spf4k64 spf4k66"),
        (0.009613037109375, 0.009613037109375, "s00001z", "s00001w s00001x s00001y s00001z s000038 s00003b s00004n s00004p s000060"),
        (-0.009613037109375, -0.009613037109375, "7zzzzy3", "7zzzzy0 7zzzzy1 7zzzzy2 7zzzzy3 7zzzzy4 7zzzzy6 7zzzzy8 7zzzzy9 7zzzzyd"),
        (44.009613037109375, 3.009613037109375, "spf4k7x", "spf4k7q spf4k7r spf4k7w spf4k7x spf4k7y spf4k7z spf4ke2 spf4ke8 spf4keb"),
        (16.953277587890625, 16.953277587890625, "s7h03y3", "s7h03y0 s7h03y1 s7h03y2 s7h03y3 s7h03y4 s7h03y6 s7h03y8 s7h03y9 s7h03yd"),
        (-16.953277587890625, -16.953277587890625, "7sgzw1z", "7sgzw1w 7sgzw1x 7sgzw1y 7sgzw1z 7sgzw38 7sgzw3b 7sgzw4n 7sgzw4p 7sgzw60"),
        (44.953277587890625, 3.953277587890625, "spfz9zq", "spfz9zj spfz9zm spfz9zn spfz9zp spfz9zq spfz9zr spfz9zt spfz9zw spfz9zx"),
        (41.19873046875, 41.19873046875, "szm63s0", "szm637z szm63eb szm63ec szm63kp szm63kr szm63s0 szm63s1 szm63s2 szm63s3"),
        (-41.19873046875, -41.19873046875, "70dtws0", "70dtw7z 70dtweb 70dtwec 70dtwkp 70dtwkr 70dtws0 70dtws1 70dtws2 70dtws3"),
        (44.19873046875, 3.19873046875, "spf72y1", "spf72vb spf72vc spf72vf spf72y0 spf72y1 spf72y2 spf72y3 spf72y4 spf72y6"),
        (44.47356109536499, -2.9674322259173715, "ezvvh2p", "ezvuury ezvuurz ezvuuxb ezvvh2n ezvvh2p ezvvh2q ezvvh2r ezvvh80 ezvvh82"),
        (47.67915783579057, -4.127942957320367, "gbmp4pd", "gbmp4p3 gbmp4p6 gbmp4p7 gbmp4p9 gbmp4pc gbmp4pd gbmp4pe gbmp4pf gbmp4pg"),
        (46.551643642205555, 0.2121959703062659, "u020uf6", "u020uf1 u020uf3 u020uf4 u020uf5 u020uf6 u020uf7 u020uf9 u020ufd u020ufe"),
        (41.86838946279212, 2.31004885120342, "sp3whtf", "sp3wht9 sp3whtc sp3whtd sp3whte sp3whtf sp3whtg sp3whw1 sp3whw4 sp3whw5"),
        (41.66745745273145, 1.2179561182033112, "sp2v59q", "sp2v59j sp2v59m sp2v59n sp2v59p sp2v59q sp2v59r sp2v59t sp2v59w sp2v59x"),
        (41.984583151031266, -3.857447402510797, "ezmqb5p", "ezmqb4y ezmqb4z ezmqb5n ezmqb5p ezmqb5q ezmqb5r ezmqb6b ezmqb70 ezmqb72"),
        (45.460288053596635, 7.037411445146163, "u0j485u", "u0j485e u0j485g u0j485s u0j485t u0j485u u0j485v u0j48h5 u0j48hh u0j48hj"),
        (42.51325921926652, -1.8960633238161848, "ezw9ug7", "ezw9ug4 ezw9ug5 ezw9ug6 ezw9ug7 ezw9ugd ezw9uge ezw9ugh ezw9ugk ezw9ugs"),
        (47.44884557957477, 8.826092348363684, "u0qmbvf", "u0qmbv9 u0qmbvc u0qmbvd u0qmbve u0qmbvf u0qmbvg u0qmby1 u0qmby4 u0qmby5"),
        (46.95560889645149, 0.6708710248315466, "u027pk0", "u027p5z u027p7b u027p7c u027php u027phr u027pk0 u027pk1 u027pk2 u027pk3"),
        (50.86730003481062, -4.510576326857207, "gchc3k5", "gchc37f gchc37g gchc37u gchc3k4 gchc3k5 gchc3k6 gchc3k7 gchc3kh gchc3kk"),
        (49.71299089867706, -0.9137825622911917, "gbz6g1y", "gbz6g1t gbz6g1v gbz6g1w gbz6g1x gbz6g1y gbz6g1z gbz6g4j gbz6g4n gbz6g4p"),
    ];

    #[test]
    fn encodes_and_expands_as_python_geohash() {
        for &(lat, lon, code, expanded) in VECTORS {
            let cell = encode(lat, lon).unwrap();
            assert_eq!(text(cell), code, "{lat}, {lon}");
            assert_eq!(parse(code), Some(cell));
            let mut cells: Vec<String> = expand(cell).into_iter().map(text).collect();
            cells.sort();
            assert_eq!(cells.join(" "), expanded, "{code}");
        }
    }

    #[test]
    fn rings_a_cell() {
        let cell = encode(46.0, 5.0).unwrap();
        assert_eq!(ring(cell, 0), [cell]);
        let mut first = ring(cell, 1);
        first.push(cell);
        first.sort();
        let mut expanded = expand(cell);
        expanded.sort();
        assert_eq!(first, expanded);
        // Ring 2: the cells around ring 1, but ring 1 and the cell itself.
        let mut second = ring(cell, 2);
        second.sort();
        let mut around: Vec<Cell> = expanded.iter().flat_map(|&cell| expand(cell)).filter(|cell| !expanded.contains(cell)).collect();
        around.sort();
        around.dedup();
        assert_eq!(second, around);
        assert_eq!(ring(cell, 5).len(), 40);
    }

    #[test]
    fn refuses_a_latitude_python_geohash_refuses() {
        assert_eq!(encode(90.0, 0.0), None);
        assert_eq!(encode(-90.1, 0.0), None);
        assert!(encode(-90.0, 0.0).is_some());
        assert_eq!(encode(10.0, 190.0), encode(10.0, -170.0));
    }
}
