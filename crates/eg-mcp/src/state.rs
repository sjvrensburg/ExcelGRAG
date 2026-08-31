//! What the server holds open between calls.
//!
//! Three resources, with three very different costs, which is the whole reason
//! this is a long-running server rather than a command:
//!
//! - The **corpus** and the **lexical index** are memory-mapped and cheap.
//! - The **embedder** is a model that takes seconds to start, so it is loaded
//!   once, on the first question that needs meaning rather than words, and kept.
//! - A **workbook** is the expensive one: the reference file is 170 MB on disk,
//!   ten seconds to read and about six gigabytes in memory. It is opened only
//!   when a tool needs cells rather than structure, and then kept, because the
//!   second question about a workbook is far likelier than the first.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use eg_graph::store::Corpus;
use eg_index::vector::VectorIndex;
use eg_index::{Embedder, TextIndex};
use eg_ingest::{load_with, LoadOptions, Loaded};

pub struct State {
    pub corpus: Corpus,
    pub text: TextIndex,
    /// The index directory, for opening the vector half against whatever model
    /// the embedder turns out to be.
    pub dir: String,
    /// Whether cell values may leave this server. Set once, at startup, so the
    /// policy is a property of the deployment rather than of each call.
    pub redact_values: bool,
    embedder: Option<Embedder>,
    /// `None` once loading has failed, so a machine that cannot reach the model
    /// is told once and then searches by word without trying again.
    embedder_failed: bool,
    workbooks: HashMap<String, Arc<Loaded>>,
}

impl State {
    pub fn open(dir: &str, redact_values: bool) -> Result<State, String> {
        let corpus = Corpus::open(dir).map_err(|e| format!("could not open the corpus: {e}"))?;
        let text =
            TextIndex::open(dir).map_err(|e| format!("could not open the lexical index: {e}"))?;
        Ok(State {
            corpus,
            text,
            dir: dir.to_string(),
            redact_values,
            embedder: None,
            embedder_failed: false,
            workbooks: HashMap::new(),
        })
    }

    /// The semantic half, if this machine can have one.
    ///
    /// Returns `None` rather than an error: a corpus with no vectors, or a
    /// machine that cannot reach the model, still answers questions by word,
    /// and saying so once is more useful than failing every call.
    pub fn semantic(&mut self) -> Option<(&mut Embedder, VectorIndex)> {
        if self.embedder_failed {
            return None;
        }
        if self.embedder.is_none() {
            match Embedder::new() {
                Ok(embedder) => self.embedder = Some(embedder),
                Err(_) => {
                    self.embedder_failed = true;
                    return None;
                }
            }
        }
        let embedder = self.embedder.as_mut()?;
        let vectors = VectorIndex::open(&self.dir, embedder.name(), embedder.dim()).ok()?;
        if vectors.is_empty() {
            return None;
        }
        Some((embedder, vectors))
    }

    /// Both indexes at once, for a search that needs them together.
    ///
    /// `semantic` borrows all of `self`, so a caller cannot hold it and reach
    /// for `text` as well. Handing back both from one call is what lets the
    /// search live in `eg-retrieve` rather than being copied in here.
    pub fn halves(&mut self) -> (&TextIndex, Option<(&mut Embedder, VectorIndex)>) {
        if !self.embedder_failed && self.embedder.is_none() {
            match Embedder::new() {
                Ok(embedder) => self.embedder = Some(embedder),
                Err(_) => self.embedder_failed = true,
            }
        }
        let semantic = match (self.embedder_failed, self.embedder.as_mut()) {
            (false, Some(embedder)) => {
                match VectorIndex::open(&self.dir, embedder.name(), embedder.dim()) {
                    Ok(vectors) if !vectors.is_empty() => Some((embedder, vectors)),
                    _ => None,
                }
            }
            _ => None,
        };
        (&self.text, semantic)
    }

    /// A workbook, loaded if this is the first time it has been asked for.
    ///
    /// The returned handle is reference-counted so a tool can work with it
    /// while the state is borrowed elsewhere.
    pub fn workbook(&mut self, path: &str) -> Result<(Arc<Loaded>, Option<f64>), String> {
        if let Some(loaded) = self.workbooks.get(path) {
            return Ok((Arc::clone(loaded), None));
        }
        if !Path::new(path).exists() {
            return Err(format!(
                "the corpus indexed {path}, and there is no file there now. \
                 The graph remembers where a workbook was, not where it went."
            ));
        }
        let started = Instant::now();
        let loaded = load_with(
            path,
            &LoadOptions {
                max_cells: None,
                ..Default::default()
            },
        )
        .map_err(|e| format!("could not load {path}: {e}"))?;
        let seconds = started.elapsed().as_secs_f64();
        let loaded = Arc::new(loaded);
        self.workbooks.insert(path.to_string(), Arc::clone(&loaded));
        Ok((loaded, Some(seconds)))
    }

    /// Resolve a workbook the caller named, or the only one there is.
    ///
    /// A corpus of one workbook needs no argument; a corpus of many will not
    /// guess, because answering a question about the wrong workbook is worse
    /// than asking which.
    pub fn resolve(&self, wanted: Option<&str>) -> Result<(String, String), String> {
        let entries: Vec<(String, String)> = self
            .corpus
            .entries()
            .map(|(hash, entry)| (hash.to_string(), entry.path.clone()))
            .collect();
        if entries.is_empty() {
            return Err("the corpus is empty — index a workbook first".to_string());
        }
        match wanted {
            None if entries.len() == 1 => Ok(entries.into_iter().next().expect("just checked")),
            None => Err(format!(
                "this corpus holds {} workbooks, so `workbook` is not optional. \
                 Call `workbooks` to see them.",
                entries.len()
            )),
            Some(want) => {
                let matches: Vec<&(String, String)> = entries
                    .iter()
                    .filter(|(hash, path)| {
                        hash.starts_with(want)
                            || path == want
                            || Path::new(path).file_name().is_some_and(|n| n == want)
                    })
                    .collect();
                match matches.as_slice() {
                    [one] => Ok((*one).clone()),
                    [] => Err(format!("no workbook in this corpus matches {want:?}")),
                    many => Err(format!(
                        "{want:?} matches {} workbooks; be more specific",
                        many.len()
                    )),
                }
            }
        }
    }
}
