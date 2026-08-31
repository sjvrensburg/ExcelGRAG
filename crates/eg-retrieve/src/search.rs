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
    /// Hits that both halves ranked. Zero when only one half ran.
    pub corroborated: usize,
    /// Whether the semantic half ran at all.
    pub both_halves: bool,
}

impl Search {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }

    pub fn verdict(&self) -> Verdict {
        if self.hits.is_empty() {
            return Verdict::Nothing;
        }
        if self.covered.is_empty() {
            return Verdict::Blind;
        }
        if self.covered.len() < self.matched.len() || !self.unmatched.is_empty() {
            return Verdict::Partial;
        }
        Verdict::Full
    }

    /// What the top result was found on. Always worth printing, which is the
    /// point — a caveat that appears only sometimes is one a reader learns to
    /// skip, and the failure being fixed here is two answers looking alike.
    pub fn evidence(&self) -> String {
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
            .filter(|w| !self.covered.contains(w))
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
        if self.both_halves {
            out.push_str(&format!(
                "; {} of {} results found by word and by meaning",
                self.corroborated,
                self.hits.len()
            ));
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
                 entirely. Verify with `read_cells` before relying on it.",
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
    let mut opened = embedder(dir).ok();
    let semantic = opened
        .as_mut()
        .filter(|(_, vectors)| !vectors.is_empty())
        .map(|(embedder, vectors)| (embedder, &*vectors));
    Ok(find_in(&text, semantic, query, options, fusion))
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
) -> Search {
    let terms = term_hits(text, query, options);

    let lexical_only = fusion.lexical_only || semantic.is_none();
    if lexical_only {
        let hits = text.search(query, options).unwrap_or_default();
        let (unmatched, matched, covered) = coverage(&terms, hits.first());
        return Search {
            hits,
            matched,
            unmatched,
            covered,
            corroborated: 0,
            both_halves: false,
        };
    }
    let (embedder, vectors) = semantic.expect("checked above");
    // Both halves are asked for `depth` and the fused list is cut afterwards.
    // Fusing two lists already truncated to the answer's length throws away the
    // ranks that decide the answer.
    let deep = SearchOptions {
        limit: options.limit.max(fusion.depth),
        ..options.clone()
    };
    let lexical = text.search(query, &deep).unwrap_or_default();
    let semantic = embedder
        .embed_query(query)
        .map(|vector| vectors.search(&vector, &deep))
        .unwrap_or_default();
    if semantic.is_empty() {
        let mut hits = lexical;
        hits.truncate(options.limit);
        let (unmatched, matched, covered) = coverage(&terms, hits.first());
        return Search {
            hits,
            matched,
            unmatched,
            covered,
            corroborated: 0,
            both_halves: false,
        };
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

    let (unmatched, matched, covered) = coverage(&terms, hits.first());
    Search {
        hits,
        matched,
        unmatched,
        covered,
        corroborated,
        both_halves: true,
    }
}

/// What each content word of the question matches, as node keys.
///
/// One index probe per word, which is a fraction of a millisecond each against
/// a search that is already sub-millisecond. Asked through the same search path
/// as the query itself, so a word counts as known exactly when it could have
/// contributed — the custom tokenizer's case and letter-digit splitting
/// included, which is why this is not a lookup in a term dictionary.
fn term_hits(
    text: &TextIndex,
    query: &str,
    options: &SearchOptions,
) -> Vec<(String, FxHashSet<(String, u32)>)> {
    // Deep enough that a word common in the workbook still lists the node the
    // ranking chose. Shallower and a frequent word would look as if it missed.
    let probe = SearchOptions {
        limit: 200,
        ..options.clone()
    };
    let mut out = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for word in query.split(|c: char| !c.is_alphanumeric()) {
        let lower = word.to_lowercase();
        if lower.is_empty() || FRAME.contains(&lower.as_str()) || !seen.insert(lower.clone()) {
            continue;
        }
        let hits = text
            .search(word, &probe)
            .unwrap_or_default()
            .into_iter()
            .map(|h| (h.workbook, h.node))
            .collect();
        out.push((lower, hits));
    }
    out
}

/// Split the question's words into unknown, known, and carried by `top`.
fn coverage(
    terms: &[(String, FxHashSet<(String, u32)>)],
    top: Option<&Hit>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let key = top.map(|h| (h.workbook.clone(), h.node));
    let (mut unmatched, mut matched, mut covered) = (Vec::new(), Vec::new(), Vec::new());
    for (word, hits) in terms {
        if hits.is_empty() {
            unmatched.push(word.clone());
            continue;
        }
        matched.push(word.clone());
        if key.as_ref().is_some_and(|k| hits.contains(k)) {
            covered.push(word.clone());
        }
    }
    (unmatched, matched, covered)
}

/// The model and the vectors it wrote, for a corpus that has them.
pub fn embedder(dir: &str) -> Result<(Embedder, VectorIndex), String> {
    let embedder = Embedder::new().map_err(|e| e.to_string())?;
    let vectors =
        VectorIndex::open(dir, embedder.name(), embedder.dim()).map_err(|e| e.to_string())?;
    Ok((embedder, vectors))
}
