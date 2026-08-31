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
    /// Opens (creating if absent) the corpus at `dir`. The two binary entry
    /// points — `eg serve` and standalone `eg-mcp` — refuse a directory with
    /// no `manifest.json` before calling this, the same guard `eg ask`/
    /// `search`/`workbooks` apply (see `require_corpus` in `eg-cli`); this
    /// constructor stays permissive so tests can build a `State` over a fresh
    /// empty corpus without indexing a workbook first.
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
        let resolved = self.resolve_workbook_path(path)?;
        let started = Instant::now();
        let loaded = load_with(
            &resolved,
            &LoadOptions {
                max_cells: None,
                ..Default::default()
            },
        )
        .map_err(|e| format!("could not load {resolved}: {e}"))?;
        let seconds = started.elapsed().as_secs_f64();
        let loaded = Arc::new(loaded);
        // Cached under the path as stored — a caller always asks by that
        // same string, whatever location it actually resolved to.
        self.workbooks.insert(path.to_string(), Arc::clone(&loaded));
        Ok((loaded, Some(seconds)))
    }

    /// Where the stored path actually points.
    ///
    /// Indexing now stores an absolute path (see `eg-cli`'s `corpus::index`),
    /// so this fallback exists for corpora indexed before that fix, or with a
    /// relative path passed some other way. A relative path is relative to
    /// wherever `eg index` was run — not necessarily this process's own
    /// working directory, since the documented MCP deployment starts the
    /// server with the *client's* cwd — so its most likely surviving location
    /// is beside the corpus directory, or beside whatever holds that.
    fn resolve_workbook_path(&self, path: &str) -> Result<String, String> {
        if Path::new(path).exists() {
            return Ok(path.to_string());
        }
        if Path::new(path).is_absolute() {
            return Err(format!(
                "the corpus indexed {path}, and there is no file there now. \
                 The graph remembers where a workbook was, not where it went — \
                 re-run `eg index` if it has moved."
            ));
        }
        let corpus_dir = Path::new(&self.dir);
        for candidate_dir in [Some(corpus_dir), corpus_dir.parent()]
            .into_iter()
            .flatten()
        {
            let candidate = candidate_dir.join(path);
            if candidate.exists() {
                return Ok(candidate.to_string_lossy().into_owned());
            }
        }
        Err(format!(
            "the corpus indexed {path} (a relative path), which resolves from \
             neither this process's working directory nor the corpus directory. \
             Either the file is actually gone, or this server is simply running \
             from somewhere other than where `eg index` was run — re-index with \
             an absolute path to fix it for good."
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn state(corpus_dir: &Path) -> State {
        State::open(corpus_dir.to_str().unwrap(), false).expect("an empty corpus opens")
    }

    #[test]
    fn a_relative_path_resolves_beside_the_corpus_directory() {
        let root = tempfile::tempdir().unwrap();
        let corpus_dir = root.path().join("corpus");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        // The workbook sits next to the corpus directory, the way `eg index
        // mycorpus/ book.xlsx` leaves things when both are typed relative to
        // the same invocation directory.
        std::fs::write(root.path().join("book.xlsx"), b"not a real workbook").unwrap();

        let state = state(&corpus_dir);
        let resolved = state.resolve_workbook_path("book.xlsx").unwrap();
        assert!(Path::new(&resolved).is_absolute());
        assert!(Path::new(&resolved).exists());
    }

    #[test]
    fn a_relative_path_resolves_inside_the_corpus_directory_too() {
        let root = tempfile::tempdir().unwrap();
        let corpus_dir = root.path().join("corpus");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        std::fs::write(corpus_dir.join("book.xlsx"), b"not a real workbook").unwrap();

        let state = state(&corpus_dir);
        let resolved = state.resolve_workbook_path("book.xlsx").unwrap();
        assert!(Path::new(&resolved).exists());
    }

    #[test]
    fn an_absolute_path_that_is_actually_gone_says_so_without_a_fallback() {
        let root = tempfile::tempdir().unwrap();
        let corpus_dir = root.path().join("corpus");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        let state = state(&corpus_dir);

        let missing = root.path().join("nowhere").join("book.xlsx");
        let err = state
            .resolve_workbook_path(missing.to_str().unwrap())
            .unwrap_err();
        assert!(err.contains("there is no file there now"), "{err}");
        assert!(!err.contains("resolves from neither"), "{err}");
    }

    #[test]
    fn a_relative_path_that_resolves_nowhere_names_the_ambiguity() {
        let root = tempfile::tempdir().unwrap();
        let corpus_dir = root.path().join("corpus");
        std::fs::create_dir_all(&corpus_dir).unwrap();
        let state = state(&corpus_dir);

        let err = state
            .resolve_workbook_path("eg-mcp-test-fixture-that-does-not-exist-anywhere.xlsx")
            .unwrap_err();
        assert!(
            err.contains("resolves from neither"),
            "must distinguish this from a plainly-gone absolute path: {err}"
        );
        assert!(err.contains("re-index"), "{err}");
    }
}
