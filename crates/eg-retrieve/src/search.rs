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
use eg_index::{fuse, Embedder, Hit, SearchOptions, TextIndex};

/// What went wrong before there was anything to rank.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("could not open the lexical index: {0}")]
    Text(String),
    #[error("lexical search failed: {0}")]
    Query(String),
}

/// Search a corpus by word, by meaning, and fuse the two.
///
/// The semantic half is optional in the strong sense: a corpus indexed without
/// it, or a machine that cannot load the model, still answers by word. That is
/// a worse answer and a great deal better than an error, so it is not one —
/// which is also why `lexical_only` is a plain flag rather than two functions.
pub fn find(
    dir: &str,
    query: &str,
    options: &SearchOptions,
    lexical_only: bool,
) -> Result<Vec<Hit>, SearchError> {
    let text = TextIndex::open(dir).map_err(|e| SearchError::Text(e.to_string()))?;
    let lexical = text
        .search(query, options)
        .map_err(|e| SearchError::Query(e.to_string()))?;
    if lexical_only {
        return Ok(lexical);
    }
    let semantic = match embedder(dir) {
        Ok((mut embedder, vectors)) if !vectors.is_empty() => embedder
            .embed_query(query)
            .map(|vector| vectors.search(&vector, options))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if semantic.is_empty() {
        return Ok(lexical);
    }
    Ok(fuse(&[&lexical, &semantic], options.limit))
}

/// The model and the vectors it wrote, for a corpus that has them.
pub fn embedder(dir: &str) -> Result<(Embedder, VectorIndex), String> {
    let embedder = Embedder::new().map_err(|e| e.to_string())?;
    let vectors =
        VectorIndex::open(dir, embedder.name(), embedder.dim()).map_err(|e| e.to_string())?;
    Ok((embedder, vectors))
}
