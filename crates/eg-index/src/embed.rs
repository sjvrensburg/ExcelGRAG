//! Turning nodes and questions into vectors.
//!
//! The lexical index finds what a workbook already says. This finds what it
//! means: a column headed `Recoverability` when the question asks about bad
//! debt, a sheet of ageing buckets when the question asks how old the debtors
//! are. Neither shares a token with the question, and neither is reachable by
//! any amount of stemming.
//!
//! The model runs locally, through ONNX. Nothing about a workbook leaves the
//! machine, which for the workbooks this is built for is not a preference.
//!
//! The first call downloads the model — about 130 MB — into a per-user cache,
//! so it happens once per machine and not once per corpus. Note that fastembed
//! would otherwise put it in `./.fastembed_cache`, relative to the working
//! directory: a 130 MB download landing in whatever directory the command was
//! run from, and a second copy for the next one. See [`cache_dir`].

use std::path::PathBuf;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::doc::NodeDoc;
use crate::text::IndexError;

/// Small, fast, and good at short noun phrases, which is all a node label ever
/// is. 384 dimensions, so a corpus of vectors stays in memory and brute force
/// stays cheaper than an approximate index.
pub const DEFAULT_MODEL: EmbeddingModel = EmbeddingModel::BGESmallENV15;

/// BGE is trained with an asymmetric objective: passages go in bare, and a
/// query is prefixed with this. Skipping it costs real accuracy, and nothing
/// warns you — the vectors come back the same shape either way.
const QUERY_INSTRUCTION: &str = "Represent this sentence for searching relevant passages: ";

/// How many texts go through the model at once.
const BATCH: usize = 256;

/// A loaded embedding model.
pub struct Embedder {
    model: TextEmbedding,
    name: String,
    dim: usize,
}

impl Embedder {
    /// Load [`DEFAULT_MODEL`], downloading it if this machine does not have it.
    pub fn new() -> Result<Embedder, IndexError> {
        Embedder::with_model(DEFAULT_MODEL)
    }

    pub fn with_model(model: EmbeddingModel) -> Result<Embedder, IndexError> {
        let name = model.to_string();
        let dim = TextEmbedding::get_model_info(&model)
            .map(|info| info.dim)
            .map_err(|e| IndexError::Embed {
                context: format!("looking up {name}"),
                detail: e.to_string(),
            })?;
        let model = TextEmbedding::try_new(
            TextInitOptions::new(model)
                .with_cache_dir(cache_dir())
                .with_show_download_progress(false),
        )
        .map_err(|e| IndexError::Embed {
            context: format!("loading {name}"),
            detail: e.to_string(),
        })?;
        Ok(Embedder { model, name, dim })
    }

    /// The model's name, stored beside the vectors it produced. Vectors from
    /// two models are not comparable, and nothing about the numbers themselves
    /// would ever reveal that they came from different ones.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed nodes, in the order given.
    pub fn embed_documents(&mut self, docs: &[NodeDoc]) -> Result<Vec<Vec<f32>>, IndexError> {
        let texts: Vec<String> = docs.iter().map(NodeDoc::embedding_text).collect();
        self.embed_texts(&texts)
    }

    pub fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, IndexError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Batches are padded to the longest text in them, so a single wide
        // table — whose document carries every one of its column headers —
        // pays for the 255 short labels batched with it. Sorting by length
        // first puts the long ones together, and on the reference workbook
        // that alone is most of the embedding time.
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let sorted: Vec<&str> = order.iter().map(|&i| texts[i].as_str()).collect();

        let embedded = self
            .model
            .embed(&sorted, Some(BATCH))
            .map_err(|e| IndexError::Embed {
                context: format!("embedding {} text(s)", texts.len()),
                detail: e.to_string(),
            })?;
        if embedded.len() != texts.len() {
            return Err(IndexError::Embed {
                context: format!("embedding {} text(s)", texts.len()),
                detail: format!("the model returned {} vectors", embedded.len()),
            });
        }

        // Back into the caller's order, which is the order the vectors are
        // stored in and lined up against their nodes.
        let mut out = vec![Vec::new(); texts.len()];
        for (slot, mut v) in order.into_iter().zip(embedded) {
            normalize(&mut v);
            out[slot] = v;
        }
        Ok(out)
    }

    /// Embed a question, with the instruction prefix the model expects.
    pub fn embed_query(&mut self, query: &str) -> Result<Vec<f32>, IndexError> {
        let prefixed = format!("{QUERY_INSTRUCTION}{query}");
        let mut out = self.embed_texts(std::slice::from_ref(&prefixed))?;
        out.pop().ok_or_else(|| IndexError::Embed {
            context: "embedding the query".to_string(),
            detail: "the model returned nothing".to_string(),
        })
    }
}

/// Where downloaded models live.
///
/// `$EG_MODEL_CACHE` if it is set, else the usual per-user cache directory.
/// Explicit because fastembed's own default is `./.fastembed_cache` — relative
/// to the working directory, so the model lands wherever the command happened
/// to be run from, and again for the next directory.
pub fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("EG_MODEL_CACHE") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("excelgrag").join("models");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("excelgrag")
            .join("models");
    }
    PathBuf::from(".fastembed_cache")
}

/// Scale a vector to unit length, so cosine similarity is a dot product.
///
/// A zero vector is left alone rather than divided by zero; it scores 0 against
/// everything, which is the honest answer for text that carried no signal.
pub fn normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v {
            *x /= norm;
        }
    }
}

/// The dot product of two vectors, which is their cosine when both are unit
/// length. Mismatched lengths score 0 rather than panicking: it means the
/// stored vectors and the query came from different models, and the caller
/// above this has already been told to rebuild.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_is_never_the_working_directory_when_home_is_known() {
        // The failure this guards against is silent: a 130 MB model appearing
        // in whatever directory the command was run from.
        let dir = cache_dir();
        if std::env::var_os("HOME").is_some() || std::env::var_os("XDG_CACHE_HOME").is_some() {
            assert!(dir.is_absolute(), "cache dir was {}", dir.display());
        }
    }

    #[test]
    fn normalising_gives_unit_length() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_vector_survives_normalising() {
        let mut v = vec![0.0, 0.0];
        normalize(&mut v);
        assert_eq!(v, vec![0.0, 0.0]);
        assert_eq!(similarity(&v, &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn similarity_is_one_for_a_vector_against_itself() {
        let mut v = vec![1.0, 2.0, 3.0];
        normalize(&mut v);
        assert!((similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn vectors_of_different_widths_do_not_panic() {
        assert_eq!(similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }
}
