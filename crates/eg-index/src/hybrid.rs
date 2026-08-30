//! Putting the two indexes together.
//!
//! They fail in opposite directions. The lexical index cannot find a column
//! headed `Recoverability` from a question about bad debt, because they share
//! no token. The vector index cannot reliably find a sheet called `GS560`,
//! because an identifier has no meaning to embed — it is a label, and the
//! nearest thing in vector space to a label is another label.
//!
//! So neither is a default and neither is a fallback. Both are run and their
//! rankings are fused.
//!
//! Fusion is by rank, not by score. A BM25 score of 34 and a cosine of 0.61 are
//! not on one scale, and there is no honest constant that puts them there —
//! BM25 has no upper bound and moves with the corpus, while cosine is pinned to
//! [-1, 1]. Normalising them against each other looks principled and quietly
//! makes the weighting depend on how many workbooks are indexed. Reciprocal
//! rank fusion throws the scores away and keeps the order, which is the part
//! both indexes agree is meaningful.

use rustc_hash::FxHashMap;

use crate::text::Hit;

/// The rank-fusion constant. 60 is the value the method was published with, and
/// what it buys is that the gap between rank 1 and rank 2 is small — a node
/// both indexes place near the top beats one that either places first alone.
/// That is the whole point of running both.
pub const RRF_K: f32 = 60.0;

/// Fuse two rankings into one.
///
/// The fused score is `sum over rankings of 1 / (K + rank)`, so it is a small
/// number in a fixed range and not comparable to either input score. It is an
/// ordering, and reading it as a similarity would be reading it wrong.
pub fn fuse(rankings: &[&[Hit]], limit: usize) -> Vec<Hit> {
    let mut scores: FxHashMap<(String, u32), (f32, Hit)> = FxHashMap::default();

    for ranking in rankings {
        for (rank, hit) in ranking.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f32 + 1.0);
            let key = (hit.workbook.clone(), hit.node);
            scores
                .entry(key)
                .and_modify(|(score, _)| *score += contribution)
                .or_insert_with(|| (contribution, hit.clone()));
        }
    }

    let mut fused: Vec<Hit> = scores
        .into_values()
        .map(|(score, hit)| Hit { score, ..hit })
        .collect();
    // Ties broken by label, so the same corpus and the same query give the same
    // list every time. A hash map's order is not an order.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.node.cmp(&b.node))
    });
    fused.truncate(limit);
    fused
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_graph::NodeKind;

    fn hit(node: u32, label: &str) -> Hit {
        Hit {
            score: 1.0,
            workbook: "hash".into(),
            path: "book.xlsx".into(),
            node,
            kind: NodeKind::Column,
            sheet: None,
            label: label.into(),
            a1: None,
        }
    }

    #[test]
    fn a_node_both_rankings_like_beats_one_either_ranks_first() {
        let lexical = [hit(1, "alone-lexical"), hit(2, "both")];
        let semantic = [hit(3, "alone-semantic"), hit(2, "both")];
        let fused = fuse(&[&lexical, &semantic], 10);

        assert_eq!(fused[0].label, "both");
        assert_eq!(fused.len(), 3, "the same node must not appear twice");
    }

    #[test]
    fn one_empty_ranking_leaves_the_other_in_order() {
        let lexical = [hit(1, "a"), hit(2, "b"), hit(3, "c")];
        let fused = fuse(&[&lexical, &[]], 10);
        assert_eq!(
            fused.iter().map(|h| h.label.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn the_same_node_in_two_workbooks_stays_two_nodes() {
        let a = hit(1, "Revenue");
        let mut b = hit(1, "Revenue");
        b.workbook = "other".into();
        let fused = fuse(&[&[a], &[b]], 10);
        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn ties_come_out_in_the_same_order_every_time() {
        let lexical = [hit(1, "b"), hit(2, "a")];
        let first = fuse(&[&lexical], 10);
        for _ in 0..8 {
            let again = fuse(&[&lexical], 10);
            assert_eq!(
                first.iter().map(|h| h.node).collect::<Vec<_>>(),
                again.iter().map(|h| h.node).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn the_limit_is_respected() {
        let many: Vec<Hit> = (0..50).map(|i| hit(i, "x")).collect();
        assert_eq!(fuse(&[&many], 7).len(), 7);
    }
}
