//! Posting lists: for each token, the documents it appears in and its score in
//! each. Intersecting them is the algorithmic core of a query: the
//! work addok hands to Redis as `ZINTERSTORE` then `ZREVRANGE`, and as
//! `SINTER` for fuzzy matching.
//!
//! A list holds its document ids sorted ascending, with the scores alongside:
//! intersected this way, they take 13% of Redis's time.

use std::cmp::Ordering;

/// A document's number in the index. The index numbers documents in
/// tie-break order (importance descending, then BAN id), so among equal scores the lower id ranks first.
pub type DocId = u32;

/// One token's documents and its score in each, read in place from the
/// index: ids ascending, each once, scores alongside. Or a set of documents,
/// a search filter, each scoring 1 as a Redis set does in `ZINTERSTORE`.
#[derive(Debug, Clone, Copy)]
pub struct PostingList<'a> {
    ids: &'a [DocId],
    scores: &'a [f64],
    set: bool,
}

impl<'a> PostingList<'a> {
    /// Ids ascending, each once, and their scores alongside. The index
    /// writes them so; `intersect` relies on it.
    pub fn new(ids: &'a [DocId], scores: &'a [f64]) -> Self {
        assert_eq!(ids.len(), scores.len(), "a score per document");
        PostingList {
            ids,
            scores,
            set: false,
        }
    }

    /// A set of documents, ids ascending, each once, each scoring 1.
    pub fn set(ids: &'a [DocId]) -> Self {
        PostingList {
            ids,
            scores: &[],
            set: true,
        }
    }

    /// The score of its `i`th document.
    fn score(&self, i: usize) -> f64 {
        if self.set { 1.0 } else { self.scores[i] }
    }

    /// How many documents it holds: addok's token frequency (`ZCARD`).
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether it holds a document.
    pub fn contains(&self, doc: DocId) -> bool {
        self.ids.binary_search(&doc).is_ok()
    }

    /// Its best score, 0 when empty: addok's `order_by_max_score` script.
    pub fn max_score(&self) -> f64 {
        match self.set {
            true if !self.ids.is_empty() => 1.0,
            _ => self.scores.iter().copied().fold(0.0, f64::max),
        }
    }
}

/// The best `limit` documents found in every list, by score summed across
/// the lists, then by id. Like `ZINTERSTORE`, it adds the shortest list's
/// score first, then the next shortest's: the same doubles in the same order
/// give the same sums, which rank documents at the cut as addok's
/// do, ties aside. Lists of equal length are added in the
/// caller's order; addok's order for them follows Python's hash
/// seed. addok keeps only the ids (`Search.intersect`), so no sum reaches a
/// result's score.
pub fn intersect(lists: &[PostingList], limit: usize) -> Vec<(DocId, f64)> {
    select(lists, limit, false).0
}

/// `intersect`, and the documents tied at its cut: when the limit falls
/// among documents of equal score, all of them, kept or not, since another
/// tie-break, such as addok's, could keep others.
pub fn intersect_with_ties(lists: &[PostingList], limit: usize) -> (Vec<(DocId, f64)>, Vec<DocId>) {
    select(lists, limit, true)
}

fn select(lists: &[PostingList], limit: usize, ties: bool) -> (Vec<(DocId, f64)>, Vec<DocId>) {
    let mut lists = lists.to_vec();
    lists.sort_by_key(|list| list.ids.len());
    let ids: Vec<&[DocId]> = lists.iter().map(|list| list.ids).collect();
    let mut found = Vec::new();
    for_each_common(&ids, |id, positions| {
        let mut scores = lists.iter().zip(positions).map(|(list, &i)| list.score(i));
        let first = scores.next().unwrap();
        found.push((id, scores.fold(first, |sum, score| sum + score)));
    });
    let mut tied = Vec::new();
    if found.len() > limit && limit > 0 {
        found.select_nth_unstable_by(limit - 1, by_rank);
        let cut = found[limit - 1].1;
        if ties && found[limit..].iter().any(|&(_, score)| score == cut) {
            let at_cut = found.iter().filter(|&&(_, score)| score == cut);
            tied = at_cut.map(|&(doc, _)| doc).collect();
        }
    }
    found.truncate(limit);
    found.sort_unstable_by(by_rank);
    (found, tied)
}

/// The members found in every set, ascending. Each set is sorted ascending,
/// without duplicates.
pub fn intersect_sets(sets: &[&[u32]]) -> Vec<u32> {
    let mut sets = sets.to_vec();
    sets.sort_by_key(|set| set.len());
    let mut found = Vec::new();
    for_each_common(&sets, |id, _| found.push(id));
    found
}

/// The members of any of the sets, ascending, each once. Each set is sorted
/// ascending, without duplicates: merged two by two, in linear time, rather
/// than sorted again.
pub fn union_sets(sets: &[&[u32]]) -> Vec<u32> {
    let mut union: Vec<u32> = Vec::new();
    for set in sets {
        let mut merged = Vec::with_capacity(union.len() + set.len());
        let (mut i, mut j) = (0, 0);
        while i < union.len() && j < set.len() {
            let (x, y) = (union[i], set[j]);
            merged.push(x.min(y));
            i += usize::from(x <= y);
            j += usize::from(y <= x);
        }
        merged.extend_from_slice(&union[i..]);
        merged.extend_from_slice(&set[j..]);
        union = merged;
    }
    union
}

/// Best first: score descending, then id ascending.
fn by_rank(a: &(DocId, f64), b: &(DocId, f64)) -> Ordering {
    b.1.total_cmp(&a.1).then(a.0.cmp(&b.0))
}

/// Calls `found` with each id present in every one of `lists`, ascending, and
/// its position in each. Walks the first list and gallops through the others,
/// so the shortest list should come first: the cost then grows with its
/// length, barely with the others'.
fn for_each_common(lists: &[&[u32]], mut found: impl FnMut(u32, &[usize])) {
    let Some((first, rest)) = lists.split_first() else {
        return;
    };
    let mut positions = vec![0; lists.len()];
    'ids: for (i, &id) in first.iter().enumerate() {
        positions[0] = i;
        for (list, position) in rest.iter().zip(&mut positions[1..]) {
            *position = gallop(list, *position, id);
            match list.get(*position) {
                Some(&other) if other == id => {}
                Some(_) => continue 'ids,
                None => return,
            }
        }
        found(id, &positions);
    }
}

/// The position of the first id at or after `from` that is not below
/// `target`: doubling steps, then a binary search within the last one.
fn gallop(ids: &[u32], from: usize, target: u32) -> usize {
    let (mut low, mut high, mut step) = (from, from, 1);
    while high < ids.len() && ids[high] < target {
        low = high + 1;
        high += step;
        step *= 2;
    }
    let high = high.min(ids.len());
    low + ids[low..high].partition_point(|&id| id < target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries sorted by id, to view as a posting list.
    struct Entries(Vec<DocId>, Vec<f64>);

    impl Entries {
        fn view(&self) -> PostingList<'_> {
            PostingList::new(&self.0, &self.1)
        }
    }

    fn list(entries: &[(DocId, f64)]) -> Entries {
        let mut entries = entries.to_vec();
        entries.sort_unstable_by_key(|&(id, _)| id);
        let (ids, scores) = entries.into_iter().unzip();
        Entries(ids, scores)
    }

    #[test]
    fn sums_the_scores_of_documents_in_every_list() {
        let a = list(&[(3, 3.0), (1, 1.0), (2, 2.0)]);
        let b = list(&[(2, 10.0), (4, 1.0), (3, 20.0)]);
        assert_eq!(intersect(&[a.view(), b.view()], 10), [(3, 23.0), (2, 12.0)]);
    }

    #[test]
    fn keeps_the_best_limit() {
        let a = list(&[(1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)]);
        let b = list(&[(1, 1.0), (2, 1.0), (3, 1.0), (4, 1.0)]);
        assert_eq!(intersect(&[a.view(), b.view()], 2), [(4, 5.0), (3, 4.0)]);
    }

    #[test]
    fn ranks_the_lower_id_first_among_equal_scores() {
        let a = list(&[(9, 1.0), (5, 1.0), (7, 1.0)]);
        let b = list(&[(5, 1.0), (7, 1.0), (9, 1.0)]);
        assert_eq!(intersect(&[a.view(), b.view()], 2), [(5, 2.0), (7, 2.0)]);
    }

    #[test]
    fn sums_the_shortest_list_first() {
        let long = list(&[(1, 0.3), (2, 0.0), (3, 0.0)]);
        let middle = list(&[(1, 0.2), (2, 0.0)]);
        let short = list(&[(1, 0.1)]);
        assert_ne!(
            0.1 + 0.2 + 0.3,
            0.3 + 0.2 + 0.1,
            "the order shows in the last bit"
        );
        assert_eq!(
            intersect(&[long.view(), middle.view(), short.view()], 10),
            [(1, 0.1 + 0.2 + 0.3)]
        );
    }

    #[test]
    fn finds_documents_far_apart_in_a_long_list() {
        let long: Vec<(DocId, f64)> = (0..10_000).step_by(3).map(|id| (id, 1.0)).collect();
        let long = list(&long);
        let short = list(&[0, 2, 3, 2997, 2999, 9999, 10_000].map(|id| (id, 1.0)));
        let found = intersect(&[long.view(), short.view()], 100);
        let found: Vec<DocId> = found.into_iter().map(|(id, _)| id).collect();
        assert_eq!(found, [0, 3, 2997, 9999]);
    }

    #[test]
    fn an_empty_list_matches_nothing() {
        let a = list(&[(1, 1.0)]);
        let empty = list(&[]);
        assert!(intersect(&[a.view(), empty.view()], 10).is_empty());
        assert!(intersect(&[], 10).is_empty());
    }

    #[test]
    fn a_single_list_gives_its_best() {
        let a = list(&[(1, 1.0), (2, 3.0), (3, 2.0)]);
        assert_eq!(intersect(&[a.view()], 2), [(2, 3.0), (3, 2.0)]);
    }

    #[test]
    fn a_limit_of_zero_keeps_nothing() {
        let a = list(&[(1, 1.0)]);
        assert!(intersect(&[a.view(), a.view()], 0).is_empty());
    }

    #[test]
    fn tells_the_documents_tied_at_its_cut() {
        let a = list(&[(1, 3.0), (2, 2.0), (3, 2.0), (4, 2.0), (5, 1.0)]);
        let (best, mut tied) = intersect_with_ties(&[a.view()], 2);
        assert_eq!(best, [(1, 3.0), (2, 2.0)]);
        tied.sort_unstable();
        assert_eq!(tied, [2, 3, 4]);
        // A cut between two scores ties nothing.
        assert!(intersect_with_ties(&[a.view()], 4).1.is_empty());
        assert!(intersect_with_ties(&[a.view()], 10).1.is_empty());
    }

    #[test]
    fn scores_each_document_of_a_set_1() {
        let a = list(&[(1, 0.5), (2, 0.25), (3, 0.125)]);
        let filter = [2, 3, 4];
        let found = intersect(&[a.view(), PostingList::set(&filter)], 10);
        assert_eq!(found, [(2, 1.25), (3, 1.125)]);
    }

    #[test]
    fn unites_sets() {
        let sets: [&[u32]; 3] = [&[1, 4, 9], &[2, 4, 10], &[]];
        assert_eq!(union_sets(&sets), [1, 2, 4, 9, 10]);
        assert!(union_sets(&[]).is_empty());
    }

    #[test]
    fn intersects_sets() {
        let long: Vec<u32> = (0..10_000).step_by(3).collect();
        let sets: [&[u32]; 3] = [&[1, 2, 3, 5, 9999], &[2, 3, 4, 5, 8, 9999], &long];
        assert_eq!(intersect_sets(&sets), [3, 9999]);
        assert!(intersect_sets(&[&[1, 2], &[]]).is_empty());
    }
}
