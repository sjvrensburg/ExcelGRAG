//! Asking both indexes and fusing the answers.
//!
//! The two indexes disagree by design: BM25 finds the node whose text contains
//! the words, and cosine finds the node whose text means something similar. A
//! column headed `Prov. dbtfl dbts` is invisible to the first and obvious to
//! the second; a formula group is the other way round. So both are asked and
//! their *rankings* are fused, never their scores — BM25 and cosine are not on
//! one scale, and adding them would let whichever happens to be larger decide.
//!
//! Living here rather than in the front-end is what lets a measurement of
//! retrieval measure the thing the front-end actually runs. A copy would only
//! ever say how the copy scores.

use rustc_hash::FxHashSet;

use eg_index::vector::VectorIndex;
use eg_index::{fuse_weighted, Embedder, Hit, SearchOptions, TextIndex, RRF_K};

/// What went wrong before there was anything to rank.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("could not open the lexical index: {0}")]
    Text(String),
    #[error("lexical search failed: {0}")]
    Query(String),
}

/// How much the word ranking counts against the meaning ranking.
///
/// Two, because on a spreadsheet the words are usually the better evidence: a
/// column really is called `Total Debt`, and BM25 finds it. Meaning earns its
/// place on the questions where the workbook calls the thing something else,
/// which is a minority of them.
///
/// What the weight decides is narrower than it sounds, and worth stating
/// exactly. A node *both* halves ranked outscores either half's exclusive find
/// regardless — that is the fusion doing its job. The weight settles which of
/// two **exclusive** finds goes first, and unweighted that was a tie broken by
/// label: a node only the embeddings liked displacing one the words had ranked
/// first. On these questions that displacement was most of the lost precision.
///
/// Measured, not assumed. Over the twelve-question set, weighting the words at
/// 1 puts the right node first half the time; at 1.5 to 3 it is 58%, flat
/// across that range; at 5 the semantic half stops rescuing the questions it
/// exists for and passage recall falls. Two is the middle of the plateau rather
/// than the peak of it, because twelve questions cannot tell 1.5 from 2.
pub const LEXICAL_WEIGHT: f32 = 2.0;

/// How the two rankings are combined.
#[derive(Debug, Clone)]
pub struct Fusion {
    /// How deep each half is asked before fusing, whatever the caller wants back.
    ///
    /// Absence from a ranking is the only evidence fusion has that one half
    /// dislikes a node, and how strong that evidence is depends on how far the
    /// ranking was allowed to go — missing from a list of eight might only mean
    /// rank nine. Asking deeper than the answer needs costs one more index read
    /// and makes every absence mean something.
    pub depth: usize,
    /// The rank-fusion constant. See [`eg_index::RRF_K`].
    pub k: f32,
    /// How much the word ranking counts against the meaning ranking. See
    /// [`LEXICAL_WEIGHT`].
    pub lexical_weight: f32,
    /// Skip the semantic half entirely.
    pub lexical_only: bool,
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion {
            depth: 50,
            k: RRF_K,
            lexical_weight: LEXICAL_WEIGHT,
            lexical_only: false,
        }
    }
}

impl Fusion {
    /// By word only — for a corpus with no vectors, and for tests, which may
    /// not depend on a model download.
    pub fn lexical() -> Self {
        Fusion {
            lexical_only: true,
            ..Default::default()
        }
    }
}

/// The words a question is built from that say nothing about a workbook.
///
/// Not a general stopword list and not meant to be one. A question about a
/// spreadsheet is a content phrase wrapped in function words — "how is the debt
/// aged", "which cells feed this" — and those wrappers are absent from every
/// index because no column is called "which". Counting them as unmatched would
/// make every question look weak, so they are dropped before the counting and
/// nowhere else: they are not removed from the query, which the tokenizer and
/// BM25 are entitled to see in full.
const FRAME: &[&str] = &[
    "a", "about", "all", "an", "and", "any", "are", "as", "at", "be", "by", "can", "do", "does",
    "each", "for", "from", "has", "have", "how", "i", "in", "into", "is", "it", "its", "many",
    "me", "much", "my", "of", "on", "or", "show", "so", "some", "that", "the", "their", "then",
    "there", "these", "this", "to", "up", "was", "we", "were", "what", "when", "where", "which",
    "who", "why", "will", "with", "would", "you", "your",
];

/// How much of the question the best result actually accounts for.
///
/// Advisory, and deliberately not a prediction of whether the answer is right.
/// Two attempts were made at predicting that and both were worse than useless:
/// warning whenever a question used a word the workbook does not fires on four
/// questions in five, including ones answered perfectly, and thresholding on
/// coverage flags a rank-one answer whose column happens to be named in two
/// words out of five. Twelve questions cannot calibrate a classifier, so this
/// does not pretend to be one. What it is: a statement of what the top result
/// was found on, which an agent can weigh for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The question had no content word to search on at all — every word was
    /// a frame word (`FRAME`), or there was no word. Distinct from `Nothing`:
    /// that is "we looked and found nothing", this is "there was nothing to
    /// look for", and retrieval is not even run over a query like this.
    NoContentWords,
    /// Nothing matched at all.
    Nothing,
    /// The result was not found on anything the question asked about — either
    /// no word of it is in this corpus, or the best result carries none of the
    /// words that are. This one *is* unambiguous, and is the only one that
    /// raises a banner.
    Blind,
    /// The best result carries some of the words the corpus knows.
    Partial,
    /// It carries every content word of the question.
    Full,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::NoContentWords => "no content words",
            Verdict::Nothing => "nothing matched",
            Verdict::Blind => "blind",
            Verdict::Partial => "partial",
            Verdict::Full => "full",
        }
    }
}

/// A ranking, and the evidence behind it.
///
/// Every other layer of this project fails loudly — the reader is diffed
/// against a second reader, the graph's edges are re-derived from the cells, a
/// formula that cannot be evaluated is refused by name. Retrieval was the
/// exception: a passage that missed the right table read exactly like one that
/// found it, and an agent had no way to tell.
///
/// The fix is not a confidence score. It is that every answer now carries
/// [`Search::evidence`] — which words of the question this corpus knows, and
/// which of them the top result actually accounts for — so the two stop looking
/// alike. A passage found on `"debt"` out of `"debt aged buckets"` says so, and
/// an agent can decide what that is worth. Where the result was found on
/// nothing the question asked at all, there is a banner as well.
#[derive(Debug, Clone, Default)]
pub struct Search {
    pub hits: Vec<Hit>,
    /// Content words of the question that no document in this corpus contains.
    pub unmatched: Vec<String>,
    /// Content words that did match something.
    pub matched: Vec<String>,
    /// Of those, the ones the top result itself carries.
    pub covered: Vec<String>,
    /// Matched words whose own coverage probe hit its depth cap without
    /// finding the top result — so whether the top result carries the word
    /// is unknown, not negative. A word this common could easily rank the
    /// top result past the probe's depth on its own merits while still
    /// containing it. Left out of `covered` and out of the `Partial`/`Full`
    /// decision alike, rather than counted as a miss the probe never
    /// actually confirmed.
    pub uncertain: Vec<String>,
    /// Hits that both halves ranked. Zero when only one half ran.
    pub corroborated: usize,
    /// Whether the semantic half ran at all.
    pub both_halves: bool,
    /// Whether the per-word coverage probe failed for at least one content
    /// word. That word is left out of both `matched` and `unmatched` rather
    /// than reported unmatched — a probe failure means its coverage was never
    /// checked, and claiming the corpus lacks the word would be a guess, the
    /// same kind of mistake this whole evidence layer exists to avoid.
    pub probe_incomplete: bool,
    /// Whether the question had no content word at all — every word was a
    /// frame word, or there was no word. `hits` is empty for this the same
    /// way it is for a genuine `Nothing` miss, but the reason is different
    /// enough to need its own verdict: see [`Verdict::NoContentWords`].
    pub no_content_words: bool,
}

impl Search {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn verdict(&self) -> Verdict {
        if self.no_content_words {
            return Verdict::NoContentWords;
        }
        if self.hits.is_empty() {
            return Verdict::Nothing;
        }
        // Every matched word partitions into confirmed-covered, confirmed
        // NOT covered by the top result, or `uncertain` (its own probe hit
        // the depth cap before finding the top result, so absence there
        // proves nothing). Only a confirmed miss may downgrade the verdict —
        // an all-`uncertain` empty `covered` is unresolved, not `Blind`.
        let confirmed_miss = self.matched.len() - self.covered.len() - self.uncertain.len();
        if self.covered.is_empty() {
            return if confirmed_miss > 0 || self.matched.is_empty() {
                Verdict::Blind
            } else {
                Verdict::Partial
            };
        }
        if confirmed_miss > 0 || !self.unmatched.is_empty() {
            return Verdict::Partial;
        }
        Verdict::Full
    }

    /// What the top result was found on. Always worth printing, which is the
    /// point — a caveat that appears only sometimes is one a reader learns to
    /// skip, and the failure being fixed here is two answers looking alike.
    pub fn evidence(&self) -> String {
        if self.no_content_words {
            return "the question carried no content word to search on".to_string();
        }
        if self.hits.is_empty() {
            return "nothing matched".to_string();
        }
        let mut out = if self.covered.is_empty() {
            "the top result matches none of the question's words".to_string()
        } else {
            format!(
                "the top result matches {} of {}",
                quoted(&self.covered),
                self.matched.len() + self.unmatched.len()
            )
        };
        let missed: Vec<String> = self
            .matched
            .iter()
            .filter(|w| !self.covered.contains(w) && !self.uncertain.contains(w))
            .cloned()
            .collect();
        if !missed.is_empty() {
            out.push_str(&format!("; {} matched elsewhere", quoted(&missed)));
        }
        if !self.unmatched.is_empty() {
            out.push_str(&format!(
                "; {} not in this corpus at all",
                quoted(&self.unmatched)
            ));
        }
        if !self.uncertain.is_empty() {
            out.push_str(&format!(
                "; whether the top result also carries {} could not be checked \
                 (too common to probe that deep)",
                quoted(&self.uncertain)
            ));
        }
        if self.both_halves {
            out.push_str(&format!(
                "; {} of {} results found by word and by meaning",
                self.corroborated,
                self.hits.len()
            ));
        }
        if self.probe_incomplete {
            out.push_str(
                "; coverage for at least one word could not be checked (index probe failed)",
            );
        }
        out
    }

    /// The banner, for the case where there is no room to argue.
    ///
    /// `None` otherwise, including for a partial match: a warning on most
    /// answers is a warning on none, and [`Search::evidence`] carries the
    /// nuance without shouting.
    pub fn warning(&self) -> Option<String> {
        match self.verdict() {
            Verdict::Partial | Verdict::Full => None,
            Verdict::NoContentWords => Some(
                "every word in the question is a frame word — ask with the terms you want found."
                    .to_string(),
            ),
            Verdict::Nothing => Some("NOTHING MATCHED.".to_string()),
            Verdict::Blind if self.matched.is_empty() => Some(format!(
                "BLIND MATCH: none of {} appears anywhere in this corpus, so nothing \
                 below was found on the question. Treat it as a guess; `workbooks` \
                 says what is actually indexed.",
                quoted(&self.unmatched)
            )),
            Verdict::Blind => Some(format!(
                "BLIND MATCH: the best result carries none of {} — the words of the \
                 question this corpus does know. It was found on something else \
                 entirely. Read the cells it names before relying on it.",
                quoted(&self.matched)
            )),
        }
    }
}

fn quoted(words: &[String]) -> String {
    words
        .iter()
        .map(|w| format!("{w:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Search a corpus by word, by meaning, and fuse the two.
///
/// The semantic half is optional in the strong sense: a corpus indexed without
/// it, or a machine that cannot load the model, still answers by word. That is
/// a worse answer and a great deal better than an error, so it is not one.
pub fn find(
    dir: &str,
    query: &str,
    options: &SearchOptions,
    fusion: &Fusion,
) -> Result<Search, SearchError> {
    let text = TextIndex::open(dir).map_err(|e| SearchError::Text(e.to_string()))?;
    // `embedder(dir)` loads the ONNX model — seconds, and on a fresh machine
    // an attempt at the ~130 MB download — exactly the costs `lexical_only`
    // exists to avoid, so it is not even called on that path.
    let mut opened = if fusion.lexical_only {
        None
    } else {
        embedder(dir).ok()
    };
    let semantic = opened
        .as_mut()
        .filter(|(_, vectors)| !vectors.is_empty())
        .map(|(embedder, vectors)| (embedder, &*vectors));
    find_in(&text, semantic, query, options, fusion)
}

/// As [`find`], against indexes the caller is already holding open.
///
/// The long-running server keeps its indexes and its model between calls, and
/// having it keep a *copy of this function* instead is how the fusion weighting
/// came to be missing from the surface an agent actually talks to. So there is
/// one implementation and two ways in.
pub fn find_in(
    text: &TextIndex,
    semantic: Option<(&mut Embedder, &VectorIndex)>,
    query: &str,
    options: &SearchOptions,
    fusion: &Fusion,
) -> Result<Search, SearchError> {
    let (terms, probe_incomplete) = term_hits(text, query, options);
    if terms.is_empty() && !probe_incomplete {
        // Every word of the query is a frame word (`FRAME`), or there was no
        // word: there is no content to search on. Retrieval is not run at
        // all — a thresholdless top-k semantic search over a query like
        // "show me all" returns hits that are neighbours of nothing the
        // question actually named, and a caller could easily mistake them
        // for an answer.
        //
        // `terms` can also come back empty because *every* probe failed
        // (`probe_incomplete`), which is not the same situation and must not
        // be reported as one: that is the index itself in trouble, and
        // falling through to the real search below is what lets that surface
        // as the error H3 exists to propagate, instead of a false "nothing to
        // search for" here.
        return Ok(Search {
            no_content_words: true,
            ..Search::default()
        });
    }

    let lexical_only = fusion.lexical_only || semantic.is_none();
    if lexical_only {
        let hits = text
            .search(query, options)
            .map_err(|e| SearchError::Query(e.to_string()))?;
        let (unmatched, matched, covered, uncertain) = coverage(&terms, hits.first());
        return Ok(Search {
            hits,
            matched,
            unmatched,
            covered,
            uncertain,
            corroborated: 0,
            both_halves: false,
            probe_incomplete,
            no_content_words: false,
        });
    }
    let (embedder, vectors) = semantic.expect("checked above");
    // Both halves are asked for `depth` and the fused list is cut afterwards.
    // Fusing two lists already truncated to the answer's length throws away the
    // ranks that decide the answer.
    let deep = SearchOptions {
        limit: options.limit.max(fusion.depth),
        ..options.clone()
    };
    let lexical = text
        .search(query, &deep)
        .map_err(|e| SearchError::Query(e.to_string()))?;
    // Failing to *embed* this one query is not a lexical index failure — the
    // lexical half above already ran and is trustworthy — so this degrades to
    // a lexical-only answer rather than failing the whole search, the same
    // trade `find` makes when there is no semantic half to ask at all.
    let semantic = embedder
        .embed_query(query)
        .map(|vector| vectors.search(&vector, &deep))
        .unwrap_or_default();
    if semantic.is_empty() {
        let mut hits = lexical;
        hits.truncate(options.limit);
        let (unmatched, matched, covered, uncertain) = coverage(&terms, hits.first());
        return Ok(Search {
            hits,
            matched,
            unmatched,
            covered,
            uncertain,
            corroborated: 0,
            both_halves: false,
            probe_incomplete,
            no_content_words: false,
        });
    }

    let hits = fuse_weighted(
        &[&lexical, &semantic],
        &[fusion.lexical_weight, 1.0],
        options.limit,
        fusion.k,
    );
    // Which of the answers both halves actually found. A node one half ranked
    // and the other never saw is a real answer and a lonelier one, and that is
    // worth saying rather than averaging away.
    let seen: FxHashSet<(&str, u32)> = semantic
        .iter()
        .map(|h| (h.workbook.as_str(), h.node))
        .collect();
    let in_words: FxHashSet<(&str, u32)> = lexical
        .iter()
        .map(|h| (h.workbook.as_str(), h.node))
        .collect();
    let corroborated = hits
        .iter()
        .filter(|h| {
            let key = (h.workbook.as_str(), h.node);
            seen.contains(&key) && in_words.contains(&key)
        })
        .count();

    let (unmatched, matched, covered, uncertain) = coverage(&terms, hits.first());
    Ok(Search {
        hits,
        matched,
        unmatched,
        covered,
        uncertain,
        corroborated,
        both_halves: true,
        probe_incomplete,
        no_content_words: false,
    })
}

/// What each content word of the question matches, as node keys.
///
/// One index probe per word, which is a fraction of a millisecond each against
/// a search that is already sub-millisecond. Asked through the same search path
/// as the query itself, so a word counts as known exactly when it could have
/// contributed — the custom tokenizer's case and letter-digit splitting
/// included, which is why this is not a lookup in a term dictionary.
/// Returns the per-word hits, and whether the probe failed for any word.
///
/// A probe failure degrades rather than propagates, unlike the main search
/// above: this is diagnostic evidence about a search that already ran, not
/// the search itself, so a probe error should not turn a real, useful ranking
/// into a hard failure. But the failure must still be visible — a word left
/// out here must not silently fall into "unmatched" in [`coverage`], which
/// would tell the caller the corpus lacks a word nobody actually checked.
/// A content word, and the node keys it matched — what each of [`term_hits`]
/// and [`coverage`] pass between them.
/// A content word, the node keys it matched (up to the probe depth), and
/// whether that list was cut short by the depth cap rather than exhausted.
type TermHits = Vec<(String, FxHashSet<(String, u32)>, bool)>;

fn term_hits(text: &TextIndex, query: &str, options: &SearchOptions) -> (TermHits, bool) {
    // Deep enough that a word common in the workbook still lists the node the
    // ranking chose. Shallower and a frequent word would look as if it missed.
    // Not deep enough to promise that for every corpus — L14: a word common
    // enough to blow past this on its own merits still gets probed only this
    // far, so `coverage` treats a capped, top-result-not-found probe as
    // unresolved rather than as proof the top result lacks the word.
    const PROBE_DEPTH: usize = 200;
    let probe = SearchOptions {
        limit: PROBE_DEPTH,
        ..options.clone()
    };
    let mut out = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    let mut probe_incomplete = false;
    for word in query.split(|c: char| !c.is_alphanumeric()) {
        let lower = word.to_lowercase();
        if lower.is_empty() || FRAME.contains(&lower.as_str()) || !seen.insert(lower.clone()) {
            continue;
        }
        match text.search(word, &probe) {
            Ok(hits) => {
                let capped = hits.len() >= PROBE_DEPTH;
                let hits = hits.into_iter().map(|h| (h.workbook, h.node)).collect();
                out.push((lower, hits, capped));
            }
            Err(_) => probe_incomplete = true,
        }
    }
    (out, probe_incomplete)
}

/// Split the question's words into unmatched, matched, carried by `top`, and
/// matched-but-unresolved-for-`top` (see [`Search::uncertain`]).
fn coverage(
    terms: &TermHits,
    top: Option<&Hit>,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let key = top.map(|h| (h.workbook.clone(), h.node));
    let (mut unmatched, mut matched, mut covered, mut uncertain) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (word, hits, capped) in terms {
        if hits.is_empty() {
            unmatched.push(word.clone());
            continue;
        }
        matched.push(word.clone());
        if key.as_ref().is_some_and(|k| hits.contains(k)) {
            covered.push(word.clone());
        } else if *capped {
            uncertain.push(word.clone());
        }
    }
    (unmatched, matched, covered, uncertain)
}

/// The model and the vectors it wrote, for a corpus that has them.
pub fn embedder(dir: &str) -> Result<(Embedder, VectorIndex), String> {
    let embedder = Embedder::new().map_err(|e| e.to_string())?;
    let vectors =
        VectorIndex::open(dir, embedder.name(), embedder.dim()).map_err(|e| e.to_string())?;
    Ok((embedder, vectors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_graph::NodeKind;

    fn hit(workbook: &str, node: u32) -> Hit {
        Hit {
            score: 1.0,
            workbook: workbook.to_string(),
            path: "book.xlsx".into(),
            node,
            kind: NodeKind::Column,
            sheet: None,
            label: "label".into(),
            a1: None,
        }
    }

    fn hits(pairs: &[(&str, u32)]) -> FxHashSet<(String, u32)> {
        pairs.iter().map(|(wb, n)| (wb.to_string(), *n)).collect()
    }

    #[test]
    fn a_capped_probe_that_never_found_the_top_result_is_uncertain_not_a_miss() {
        // L14: at exactly the probe's depth cap, absence from the sample
        // proves nothing — the true match set may run deeper and include the
        // top result anyway.
        let terms: TermHits = vec![("debt".to_string(), hits(&[("wbA", 1), ("wbA", 2)]), true)];
        let (unmatched, matched, covered, uncertain) = coverage(&terms, Some(&hit("wbA", 99)));
        assert!(unmatched.is_empty());
        assert_eq!(matched, vec!["debt".to_string()]);
        assert!(covered.is_empty());
        assert_eq!(uncertain, vec!["debt".to_string()]);
    }

    #[test]
    fn an_uncapped_probe_that_never_found_the_top_result_is_a_confirmed_miss() {
        // The whole match set fit under the cap, so its absence is certain —
        // must not be softened into `uncertain` just because some *other*
        // word in the same query happened to be capped.
        let terms: TermHits = vec![("debt".to_string(), hits(&[("wbA", 1), ("wbA", 2)]), false)];
        let (_, matched, covered, uncertain) = coverage(&terms, Some(&hit("wbA", 99)));
        assert_eq!(matched, vec!["debt".to_string()]);
        assert!(covered.is_empty());
        assert!(uncertain.is_empty());
    }

    #[test]
    fn an_uncertain_word_does_not_turn_a_real_result_blind() {
        // Before L14 this configuration was `Blind`, wrongly asserting the
        // top result "carries none of" a word whose coverage was never
        // actually resolved.
        let search = Search {
            hits: vec![hit("wbA", 99)],
            matched: vec!["debt".to_string()],
            uncertain: vec!["debt".to_string()],
            ..Search::default()
        };
        assert_eq!(search.verdict(), Verdict::Partial);
    }

    #[test]
    fn an_uncertain_word_does_not_block_a_full_verdict() {
        let search = Search {
            hits: vec![hit("wbA", 99)],
            matched: vec!["debt".to_string(), "aged".to_string()],
            covered: vec!["debt".to_string()],
            uncertain: vec!["aged".to_string()],
            ..Search::default()
        };
        assert_eq!(search.verdict(), Verdict::Full);
    }

    #[test]
    fn a_confirmed_miss_alongside_an_uncertain_word_is_still_partial() {
        let search = Search {
            hits: vec![hit("wbA", 99)],
            matched: vec!["debt".to_string(), "aged".to_string(), "bucket".to_string()],
            covered: vec!["debt".to_string()],
            uncertain: vec!["aged".to_string()],
            ..Search::default()
        };
        // "bucket" is matched, not covered, not uncertain — a confirmed miss.
        assert_eq!(search.verdict(), Verdict::Partial);
    }

    #[test]
    fn evidence_reports_an_uncertain_word_separately_from_a_confirmed_miss() {
        let search = Search {
            hits: vec![hit("wbA", 99)],
            matched: vec!["debt".to_string(), "aged".to_string()],
            covered: vec!["debt".to_string()],
            uncertain: vec!["aged".to_string()],
            ..Search::default()
        };
        let evidence = search.evidence();
        assert!(evidence.contains("could not be checked"), "{evidence}");
        assert!(
            !evidence.contains("matched elsewhere"),
            "an uncertain word is not a confirmed miss: {evidence}"
        );
    }
}
