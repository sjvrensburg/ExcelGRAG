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
) -> Result<Vec<Hit>, SearchError> {
    let text = TextIndex::open(dir).map_err(|e| SearchError::Text(e.to_string()))?;
    if fusion.lexical_only {
        return text
            .search(query, options)
            .map_err(|e| SearchError::Query(e.to_string()));
    }
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
    let semantic = match embedder(dir) {
        Ok((mut embedder, vectors)) if !vectors.is_empty() => embedder
            .embed_query(query)
            .map(|vector| vectors.search(&vector, &deep))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if semantic.is_empty() {
        let mut lexical = lexical;
        lexical.truncate(options.limit);
        return Ok(lexical);
    }
    Ok(fuse_weighted(
        &[&lexical, &semantic],
        &[fusion.lexical_weight, 1.0],
        options.limit,
        fusion.k,
    ))
}

/// The model and the vectors it wrote, for a corpus that has them.
pub fn embedder(dir: &str) -> Result<(Embedder, VectorIndex), String> {
    let embedder = Embedder::new().map_err(|e| e.to_string())?;
    let vectors =
        VectorIndex::open(dir, embedder.name(), embedder.dim()).map_err(|e| e.to_string())?;
    Ok((embedder, vectors))
}
