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

/// The rank-fusion constant. 60 is the value the method was published with.
///
/// It was suspected of a lot and measured to be worth almost nothing here. The
/// theory was that 60 over a ranking of eight flattens rank into noise —
/// `1/(60+1)` to `1/(60+8)` spans 11%, while a second appearance adds 100% — so
/// the fusion becomes a vote on set membership. The span is real; the effect is
/// not. Sweeping `k` from 0 to 60 moves mean reciprocal rank by 0.005 once the
/// rankings are weighted and asked deep enough, which are the two things that
/// did matter. So it stays at the published value: changing a constant that
/// buys nothing is just a second number to explain.
///
/// `crates/eg-retrieve/examples/answers.rs --sweep` is where that was measured.
pub const RRF_K: f32 = 60.0;

/// Fuse two rankings into one.
///
/// The fused score is `sum over rankings of 1 / (K + rank)`, so it is a small
/// number in a fixed range and not comparable to either input score. It is an
/// ordering, and reading it as a similarity would be reading it wrong.
pub fn fuse(rankings: &[&[Hit]], limit: usize) -> Vec<Hit> {
    fuse_with(rankings, limit, RRF_K)
}

/// Fuse with a chosen constant, for measuring what the constant is worth.
pub fn fuse_with(rankings: &[&[Hit]], limit: usize, k: f32) -> Vec<Hit> {
    fuse_weighted(rankings, &[], limit, k)
}

/// Fuse with a say in how much each ranking counts.
///
/// Plain reciprocal-rank fusion treats its inputs as equals, which is right
/// when they are. These two are not: on a spreadsheet, words are usually the
/// better evidence — a column really is called `Total Debt` — and meaning earns
/// its place by finding the ones that are called something else. Weighting is
/// how that asymmetry is stated, rather than hoping a shared constant expresses
/// it.
///
/// A weight per ranking, in the same order; a missing or empty list means every
/// ranking counts once, which is [`fuse`].
pub fn fuse_weighted(rankings: &[&[Hit]], weights: &[f32], limit: usize, k: f32) -> Vec<Hit> {
    let mut scores: FxHashMap<(String, u32), (f32, Hit)> = FxHashMap::default();

    for (i, ranking) in rankings.iter().enumerate() {
        let weight = weights.get(i).copied().unwrap_or(1.0);
        for (rank, hit) in ranking.iter().enumerate() {
            let contribution = weight / (k + rank as f32 + 1.0);
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
    // Ties broken by label, then node, then workbook, so the same corpus and
    // the same query give the same list every time. A hash map's order is
    // not an order. `node` alone does not finish the job — it is a
    // workbook-local index, so two hits from *different* workbooks can share
    // both a label and a node number, and without the workbook hash as the
    // last tiebreaker those two would still order however the hash map
    // happened to iterate.
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.node.cmp(&b.node))
            .then_with(|| a.workbook.cmp(&b.workbook))
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
    fn weighting_decides_between_what_only_one_half_found() {
        // Where the weight actually bites. At the published `K` a node both
        // halves ranked outscores either half's exclusive find, whatever the
        // weight — that is the fusion working. The open question is which of
        // two *exclusive* finds goes first, and unweighted the answer is a
        // coin flip settled by label. On a spreadsheet the words are the better
        // evidence, so the word ranking's find should win that.
        let words = [hit(1, "total debt"), hit(9, "shared")];
        let meaning = [hit(2, "some near thing"), hit(9, "shared")];

        let equal = fuse(&[&words, &meaning], 3);
        let equal: Vec<&str> = equal.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(equal[0], "shared", "both halves agreeing still comes first");
        assert_eq!(
            equal[1], "some near thing",
            "and between two exclusive finds, nothing but the label decides: {equal:?}"
        );

        let weighted = fuse_weighted(&[&words, &meaning], &[2.0, 1.0], 3, RRF_K);
        let weighted: Vec<&str> = weighted.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(weighted[0], "shared", "agreement still comes first");
        assert_eq!(
            weighted[1], "total debt",
            "the word ranking's exclusive find now outranks the other's: {weighted:?}"
        );
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

    #[test]
    fn a_tie_on_label_and_node_across_two_workbooks_still_orders_deterministically() {
        // L15: `node` is a workbook-local index, so two hits from different
        // workbooks can share both a label and a node number. Without the
        // workbook hash as the last tiebreaker, these two would order
        // however the fusion's hash map happened to iterate — different
        // between runs of the very same query.
        let mut a = hit(1, "Total");
        a.workbook = "aaaa".into();
        let mut b = hit(1, "Total");
        b.workbook = "bbbb".into();
        let first = fuse(&[&[a.clone()], &[b.clone()]], 10);
        for _ in 0..8 {
            let again = fuse(&[&[a.clone()], &[b.clone()]], 10);
            assert_eq!(
                first.iter().map(|h| h.workbook.clone()).collect::<Vec<_>>(),
                again.iter().map(|h| h.workbook.clone()).collect::<Vec<_>>()
            );
        }
        // And the order is the workbook comparison itself, not a coin flip.
        assert_eq!(first[0].workbook, "aaaa");
        assert_eq!(first[1].workbook, "bbbb");
    }
}
