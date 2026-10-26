//! Pure fuzzy scorer for the picker: case-insensitive subsequence matching.
//!
//! Ranking (higher is better): a prefix match beats a match starting on a word boundary, which
//! beats a contiguous run, which beats a gappy match. Ties keep candidate order.

/// Scores `text` against `query`, or `None` if `query` is not a case-insensitive subsequence of
/// `text`. An empty query matches everything with score 0.
#[must_use]
pub fn score(query: &str, text: &str) -> Option<i64> {
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.chars().collect();
    let tl: Vec<char> = t.iter().flat_map(|c| c.to_lowercase()).collect();
    // Lowercasing can change length for exotic characters; fall back to the lowered text.
    let boundary = |i: usize| -> bool {
        i == 0
            || t.get(i - 1).is_some_and(|p| !p.is_alphanumeric())
            || (tl.len() == t.len() && t[i - 1].is_lowercase() && t[i].is_uppercase())
    };
    let mut total: i64 = 0;
    let mut qi = 0;
    let mut prev: Option<usize> = None;
    for (i, c) in tl.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            let mut s = 1;
            if i == 0 && qi == 0 {
                s += 40;
            } else if boundary(i) {
                s += if qi == 0 { 10 } else { 3 };
            }
            match prev {
                Some(p) if p + 1 == i => s += 15,
                Some(p) => s -= i64::try_from(i - p - 1).unwrap_or(i64::MAX).min(10),
                None => {}
            }
            total += s;
            prev = Some(i);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    // Shorter texts win ties between equally good matches.
    Some(total * 4 - i64::try_from(tl.len()).unwrap_or(i64::MAX).min(3))
}

/// Indices of `items` matching `query`, best first (stable: ties keep input order). An empty
/// query keeps every item in input order.
#[must_use]
pub fn filter(query: &str, items: &[&str]) -> Vec<usize> {
    let mut hits: Vec<(usize, i64)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, t)| score(query, t).map(|s| (i, s)))
        .collect();
    if !query.is_empty() {
        hits.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
    }
    hits.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_is_required_and_case_insensitive() {
        assert!(score("cde", "Crucible-Dev-Env").is_some());
        assert!(score("CDE", "crucible-dev-env").is_some());
        assert!(score("xyz", "crucible").is_none());
        assert!(score("ba", "ab").is_none());
    }

    #[test]
    fn empty_query_matches_all_in_order() {
        assert_eq!(filter("", &["b", "a", "c"]), vec![0, 1, 2]);
    }

    #[test]
    fn prefix_beats_boundary_beats_gappy() {
        let items = ["xxcrate", "crate", "x-crate", "c-r-a-t-e"];
        let order = filter("crate", &items);
        assert_eq!(order[0], 1, "prefix first: {order:?}");
        assert_eq!(order[1], 2, "boundary run second: {order:?}");
        assert_eq!(*order.last().unwrap(), 3, "gappy last: {order:?}");
    }

    #[test]
    fn ties_keep_candidate_order() {
        assert_eq!(filter("a", &["a1", "a2", "a3"]), vec![0, 1, 2]);
    }

    #[test]
    fn filter_drops_non_matches() {
        assert_eq!(filter("web", &["api", "webapp", "docs"]), vec![1]);
    }

    #[test]
    fn camel_case_boundary_counts() {
        let order = filter("mw", &["xmxw", "myWork"]);
        assert_eq!(order[0], 1);
    }

    #[test]
    fn unicode_does_not_panic() {
        assert!(score("é", "Café").is_some());
        assert!(score("ß", "straße").is_some());
    }
}
