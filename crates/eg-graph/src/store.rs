//! Keeping graphs between runs.
//!
//! P3a measured what there is to store, and the answer decided the design. The
//! graph of a 170 MB workbook, without formula-group nodes, is **732 nodes and
//! 892 edges** — a few hundred kilobytes of JSON. Fifty such workbooks are a
//! few megabytes. That is far below the scale at which an embedded key-value
//! store or a memory-mapped columnar format earns its complexity, so this is a
//! directory of files and a manifest.
//!
//! What is *not* stored is as deliberate:
//!
//! - **Formula-group nodes.** 464,131 of them on the reference workbook, 119
//!   MiB, and near-identical text by construction. They are rebuilt when a
//!   caller drills into one workbook, which is the only time they are wanted.
//!   [`StoredGraph::formula_group_nodes`] records which kind a file holds, so a
//!   loader is never guessing.
//! - **Cell values.** The workbook is 6 GB in memory. Nodes carry the ranges
//!   they stand for, so the cells are one read away, and a stored copy could
//!   only ever go stale.
//!
//! Freshness is by content hash, not timestamp: the key of a stored graph is
//! the blake3 of the source file, so a workbook that has not changed is a hit
//! however it was copied, and one that has changed can never be a hit.
//!
//! ```no_run
//! # use eg_graph::store::Corpus;
//! let mut corpus = Corpus::open("index")?;
//! if corpus.get("a1b2…")?.is_none() {
//!     // load, build, and `corpus.put(…)`
//! }
//! # Ok::<(), eg_graph::store::StoreError>(())
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};

use crate::build::{BuiltGraph, Graph};
use crate::report::BuildReport;

/// Bumped when the stored shape changes. A file from another version is
/// reported as a miss rather than deserialised into something plausible.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("could not read {path}: {source}")]
    Decode {
        path: String,
        #[source]
        source: serde_json::Error,
    },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}

/// One workbook graph, as written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredGraph {
    pub version: u32,
    /// blake3 of the source file.
    pub content_hash: String,
    /// Where the workbook was when it was indexed. A hint for a reader, not an
    /// identity — the hash is the identity.
    pub path: String,
    /// Whether formula groups are present. A graph stored without them is not a
    /// smaller version of one stored with them; it is missing a layer, and a
    /// caller that needs groups must rebuild rather than assume.
    pub formula_group_nodes: bool,
    pub root: u32,
    pub graph: Graph,
    pub report: BuildReport,
}

impl StoredGraph {
    pub fn into_built(self) -> BuiltGraph {
        BuiltGraph {
            graph: self.graph,
            root: NodeIndex::new(self.root as usize),
            report: self.report,
        }
    }
}

/// What the corpus holds, one line per workbook.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Keyed by content hash, so the same workbook under two paths is stored
    /// once and a changed workbook never masquerades as its old self.
    pub workbooks: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub sheets: usize,
    pub cells: u64,
    pub nodes: u64,
    pub edges: u64,
    pub formula_group_nodes: bool,
}

/// A directory of stored workbook graphs.
pub struct Corpus {
    root: PathBuf,
    manifest: Manifest,
}

impl Corpus {
    /// Open a corpus directory, creating it if it does not exist.
    pub fn open(root: impl AsRef<Path>) -> Result<Corpus, StoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("graphs"))
            .map_err(io_err(format!("creating {}", root.display())))?;

        let manifest_path = root.join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes)
                .map_err(|source| StoreError::Decode {
                    path: manifest_path.display().to_string(),
                    source,
                })
                .map(|m| {
                    // A manifest from another version is discarded rather than
                    // migrated. Rebuilding costs seconds; a wrong answer out of
                    // a half-understood file costs more.
                    if m.version == FORMAT_VERSION {
                        m
                    } else {
                        Manifest {
                            version: FORMAT_VERSION,
                            ..Default::default()
                        }
                    }
                })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest {
                version: FORMAT_VERSION,
                ..Default::default()
            },
            Err(e) => return Err(io_err(format!("reading {}", manifest_path.display()))(e)),
        };

        Ok(Corpus { root, manifest })
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.manifest
            .workbooks
            .iter()
            .map(|(hash, entry)| (hash.as_str(), entry))
    }

    pub fn len(&self) -> usize {
        self.manifest.workbooks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.manifest.workbooks.is_empty()
    }

    /// The stored graph for a content hash, if there is one.
    ///
    /// A file that is unreadable, or written by another format version, is a
    /// miss rather than an error: the caller can always rebuild, and refusing
    /// to start because a cache is stale would be the wrong trade.
    pub fn get(&self, content_hash: &str) -> Result<Option<StoredGraph>, StoreError> {
        if !self.manifest.workbooks.contains_key(content_hash) {
            return Ok(None);
        }
        let path = self.graph_path(content_hash);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(format!("reading {}", path.display()))(e)),
        };
        let stored: StoredGraph =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Decode {
                path: path.display().to_string(),
                source,
            })?;
        if stored.version != FORMAT_VERSION || stored.content_hash != content_hash {
            return Ok(None);
        }
        Ok(Some(stored))
    }

    /// Store a graph, replacing any earlier one for the same content hash.
    pub fn put(
        &mut self,
        content_hash: &str,
        path: &str,
        sheets: usize,
        cells: u64,
        formula_group_nodes: bool,
        built: &BuiltGraph,
    ) -> Result<(), StoreError> {
        let stored = StoredGraph {
            version: FORMAT_VERSION,
            content_hash: content_hash.to_string(),
            path: path.to_string(),
            formula_group_nodes,
            root: built.root.index() as u32,
            graph: built.graph.clone(),
            report: built.report.clone(),
        };
        let file = self.graph_path(content_hash);
        let bytes = serde_json::to_vec(&stored).expect("a graph of plain data serialises");
        write_atomically(&file, &bytes)?;

        self.manifest.workbooks.insert(
            content_hash.to_string(),
            Entry {
                path: path.to_string(),
                sheets,
                cells,
                nodes: built.report.total_nodes(),
                edges: built.report.total_edges(),
                formula_group_nodes,
            },
        );
        self.write_manifest()
    }

    /// Drop a workbook from the corpus. Returns whether it was there.
    pub fn forget(&mut self, content_hash: &str) -> Result<bool, StoreError> {
        if self.manifest.workbooks.remove(content_hash).is_none() {
            return Ok(false);
        }
        let path = self.graph_path(content_hash);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(format!("removing {}", path.display()))(e)),
        }
        self.write_manifest()?;
        Ok(true)
    }

    fn graph_path(&self, content_hash: &str) -> PathBuf {
        // The hash is hex from blake3, so it cannot escape the directory. Kept
        // to its first 32 characters, which is still far past collision.
        let stem: String = content_hash
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(32)
            .collect();
        self.root.join("graphs").join(format!("{stem}.json"))
    }

    fn write_manifest(&self) -> Result<(), StoreError> {
        let path = self.root.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&self.manifest).expect("the manifest serialises");
        write_atomically(&path, &bytes)
    }
}

/// Write through a temporary file and rename, so an interrupted write leaves
/// the previous version rather than a truncated one.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(io_err(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_err(format!("renaming into {}", path.display())))
}
