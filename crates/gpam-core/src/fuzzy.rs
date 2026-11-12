use nucleo_matcher::{
    Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};

/// Rank `items` against `query`. Returns indices into `items` ordered by
/// descending score. An empty query returns the input order unchanged.
pub fn rank<F>(items: &[F], query: &str) -> Vec<usize>
where
    F: AsRef<str>,
{
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored: Vec<(usize, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let mut buf = Vec::new();
            let hay = nucleo_matcher::Utf32Str::new(s.as_ref(), &mut buf);
            pattern.score(hay, &mut matcher).map(|score| (i, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_input_order() {
        let items = vec!["a", "b", "c"];
        assert_eq!(rank(&items, ""), vec![0, 1, 2]);
    }

    #[test]
    fn ranks_matches_over_non_matches() {
        let items = vec!["alpha", "beta", "alpine"];
        let ranked = rank(&items, "alp");
        assert_eq!(ranked, vec![0, 2]);
    }
}
