//! Keeping the vectors, and searching them by brute force.
//!
//! The measurement decides this, as it did for the graph store. The nodes worth
//! embedding — sheets, tables, columns, defined names — are **732 on the
//! reference workbook**, and at 384 dimensions that is 1.1 MB of `f32`. Fifty
//! such workbooks are 36,600 vectors and 56 MB: a full scan is 14 million
//! multiply-adds, well under a millisecond, and it is exact. An approximate
//! index would add a build step, a tuning parameter, a recall cliff and a
//! second on-disk format, in exchange for beating a number that is already too
//! small to see.
//!
//! So there is no HNSW here, and no vector database. There is an array of
//! floats per workbook and a loop over it. If a corpus ever reaches the
//! millions of vectors where that stops being true, the thing to do is measure
//! again.
//!
//! **Formula groups are not embedded.** 463,570 of them on the reference
//! workbook: 713 MB of vectors, hours of model time, to make near-identical
//! formula text searchable by meaning. A formula is exact-token text, which is
//! what the lexical index is already good at. [`embeddable`] is where that
//! choice lives.
//!
//! Two files per workbook: the metadata as JSON, and the vectors as raw
//! little-endian `f32`. JSON for the numbers would be ten times the size and
//! lossy in the last decimal place, and a format that quietly perturbs the
//! vectors is a format that quietly changes the ranking.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use eg_graph::{Graph, NodeKind};
use serde::{Deserialize, Serialize};

use crate::doc::{docs_for, NodeDoc};
use crate::embed::similarity;
use crate::text::{Hit, IndexError, SearchOptions};

/// Bumped when the stored shape changes. A set from another version is dropped
/// rather than read, the same trade the graph store makes.
pub const FORMAT_VERSION: u32 = 1;

/// The nodes worth embedding.
///
/// Everything but formula groups. See the module note: a formula is exact
/// tokens, and there are half a million of them.
pub fn embeddable(graph: &Graph) -> Vec<NodeDoc> {
    docs_for(graph)
        .into_iter()
        .filter(|d| d.kind != NodeKind::FormulaGroup)
        .collect()
}

/// What a stored vector points back at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub node: u32,
    pub kind: NodeKind,
    pub sheet: Option<String>,
    pub a1: Option<String>,
    pub label: String,
}

/// One workbook's vectors and the entries they belong to.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    version: u32,
    /// The model that produced these. Vectors from two models are numbers of
    /// the same shape and no shared meaning.
    model: String,
    dim: usize,
    content_hash: String,
    path: String,
    entries: Vec<Entry>,
}

struct Set {
    path: String,
    entries: Vec<Entry>,
    /// `entries.len() * dim` floats, each row unit length.
    data: Vec<f32>,
}

/// A directory of workbook vectors, held in memory and scanned in full.
pub struct VectorIndex {
    dir: PathBuf,
    model: String,
    dim: usize,
    sets: BTreeMap<String, Set>,
}

impl VectorIndex {
    /// Open the vectors under `root`, which is the corpus directory: they go in
    /// `root/vectors`, beside the graphs and the lexical index.
    ///
    /// Anything stored by a different model, or a different format version, is
    /// skipped on load and overwritten when that workbook is indexed again.
    /// Silently reusing it would mix two vector spaces in one ranking, which
    /// produces plausible nonsense rather than an error.
    pub fn open(
        root: impl AsRef<Path>,
        model: &str,
        dim: usize,
    ) -> Result<VectorIndex, IndexError> {
        let dir = root.as_ref().join("vectors");
        fs::create_dir_all(&dir).map_err(io_err(format!("creating {}", dir.display())))?;

        let mut index = VectorIndex {
            dir,
            model: model.to_string(),
            dim,
            sets: BTreeMap::new(),
        };
        index.load()?;
        Ok(index)
    }

    fn load(&mut self) -> Result<(), IndexError> {
        let entries =
            fs::read_dir(&self.dir).map_err(io_err(format!("reading {}", self.dir.display())))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(meta) = serde_json::from_slice::<Meta>(&bytes) else {
                continue;
            };
            if meta.version != FORMAT_VERSION || meta.model != self.model || meta.dim != self.dim {
                continue;
            }
            let Ok(raw) = fs::read(path.with_extension("f32")) else {
                continue;
            };
            let Some(data) = decode(&raw, meta.entries.len(), self.dim) else {
                continue;
            };
            self.sets.insert(
                meta.content_hash.clone(),
                Set {
                    path: meta.path,
                    entries: meta.entries,
                    data,
                },
            );
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Whether this workbook already has usable vectors.
    pub fn contains(&self, content_hash: &str) -> bool {
        self.sets.contains_key(content_hash)
    }

    /// How many vectors the index holds, across every workbook.
    pub fn len(&self) -> usize {
        self.sets.values().map(|s| s.entries.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn workbooks(&self) -> usize {
        self.sets.len()
    }

    pub fn size_on_disk(&self) -> u64 {
        fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    }

    /// Store one workbook's vectors, replacing anything held under the same
    /// content hash.
    ///
    /// `vectors` must line up with `docs`, one per document and each of the
    /// index's dimension. A mismatch is an error rather than a truncation: it
    /// means the embedder and the index disagree about the model, and a
    /// half-written set would be searched as if it were whole.
    pub fn put(
        &mut self,
        content_hash: &str,
        path: &str,
        docs: &[NodeDoc],
        vectors: &[Vec<f32>],
    ) -> Result<usize, IndexError> {
        if vectors.len() != docs.len() {
            return Err(IndexError::Embed {
                context: format!("storing vectors for {path}"),
                detail: format!("{} vectors for {} documents", vectors.len(), docs.len()),
            });
        }
        if let Some(bad) = vectors.iter().find(|v| v.len() != self.dim) {
            return Err(IndexError::Embed {
                context: format!("storing vectors for {path}"),
                detail: format!("a vector of {} floats, expected {}", bad.len(), self.dim),
            });
        }

        let entries: Vec<Entry> = docs
            .iter()
            .map(|d| Entry {
                node: d.node,
                kind: d.kind,
                sheet: d.sheet.clone(),
                a1: d.a1.clone(),
                label: d.label.clone(),
            })
            .collect();
        let mut data = Vec::with_capacity(entries.len() * self.dim);
        for v in vectors {
            data.extend_from_slice(v);
        }

        let meta = Meta {
            version: FORMAT_VERSION,
            model: self.model.clone(),
            dim: self.dim,
            content_hash: content_hash.to_string(),
            path: path.to_string(),
            entries: entries.clone(),
        };
        let stem = self.stem(content_hash);
        write_atomically(&self.dir.join(format!("{stem}.f32")), &encode(&data))?;
        // The metadata goes second. It is what `load` looks for, so a run
        // interrupted between the two leaves a stray float file that the next
        // `put` overwrites, rather than metadata promising vectors that are
        // not there.
        write_atomically(
            &self.dir.join(format!("{stem}.json")),
            &serde_json::to_vec(&meta).expect("plain data serialises"),
        )?;

        let count = entries.len();
        self.sets.insert(
            content_hash.to_string(),
            Set {
                path: path.to_string(),
                entries,
                data,
            },
        );
        Ok(count)
    }

    /// Drop a workbook's vectors. Returns whether it had any.
    pub fn forget(&mut self, content_hash: &str) -> Result<bool, IndexError> {
        let had = self.sets.remove(content_hash).is_some();
        let stem = self.stem(content_hash);
        for ext in ["json", "f32"] {
            let path = self.dir.join(format!("{stem}.{ext}"));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(format!("removing {}", path.display()))(e)),
            }
        }
        Ok(had)
    }

    /// The nearest nodes to a query vector, most similar first.
    ///
    /// A full scan, filtered as it goes. Filtering first and scoring second
    /// would be the same work, because the filter fields are already beside the
    /// vector.
    pub fn search(&self, query: &[f32], opts: &SearchOptions) -> Vec<Hit> {
        if query.len() != self.dim {
            return Vec::new();
        }
        let limit = opts.limit.max(1);
        let mut best: Vec<Hit> = Vec::new();

        for (hash, set) in &self.sets {
            if opts.workbook.as_deref().is_some_and(|w| w != hash) {
                continue;
            }
            for (i, entry) in set.entries.iter().enumerate() {
                if !opts.kinds.is_empty() && !opts.kinds.contains(&entry.kind) {
                    continue;
                }
                if let Some(sheet) = &opts.sheet {
                    if entry.sheet.as_deref() != Some(sheet.as_str()) {
                        continue;
                    }
                }
                let row = &set.data[i * self.dim..(i + 1) * self.dim];
                let score = similarity(query, row);
                // Kept in a sorted vector of at most `limit`, so the scan
                // allocates nothing per candidate.
                if best.len() == limit && score <= best[limit - 1].score {
                    continue;
                }
                let hit = Hit {
                    score,
                    workbook: hash.clone(),
                    path: set.path.clone(),
                    node: entry.node,
                    kind: entry.kind,
                    sheet: entry.sheet.clone(),
                    label: entry.label.clone(),
                    a1: entry.a1.clone(),
                };
                let at = best
                    .binary_search_by(|h| {
                        score
                            .partial_cmp(&h.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .unwrap_or_else(|i| i);
                best.insert(at, hit);
                best.truncate(limit);
            }
        }
        best
    }

    /// The hash is hex from blake3, so it cannot escape the directory.
    fn stem(&self, content_hash: &str) -> String {
        content_hash
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(32)
            .collect()
    }
}

fn encode(data: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 4);
    for x in data {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Read a float file back, or `None` if it is not the size the metadata says.
///
/// A short file means an interrupted write or a corrupted copy. Reading what is
/// there and scanning it would silently drop the last workbook's worth of
/// nodes; a miss makes the next `put` rewrite it.
fn decode(raw: &[u8], count: usize, dim: usize) -> Option<Vec<f32>> {
    if raw.len() != count * dim * 4 {
        return None;
    }
    Some(
        raw.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> IndexError {
    let context = context.into();
    move |source| IndexError::Io { context, source }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), IndexError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(io_err(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_err(format!("renaming into {}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_survive_the_round_trip_exactly() {
        let data = vec![0.1, -0.25, 1.0 / 3.0, f32::MIN_POSITIVE];
        let raw = encode(&data);
        assert_eq!(decode(&raw, 2, 2), Some(data));
    }

    #[test]
    fn a_float_file_of_the_wrong_size_is_a_miss() {
        let raw = encode(&[0.1, 0.2, 0.3]);
        assert_eq!(decode(&raw, 2, 2), None);
        assert_eq!(decode(&raw[..7], 3, 1), None);
    }
}
