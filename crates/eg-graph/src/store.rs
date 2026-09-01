//! Keeping graphs between runs.
//!
//! P3a measured what there is to store, and the answer decided the design. The
//! graph of a 170 MB workbook is **2,007 nodes and 3,271 edges** — 520 KB of
//! JSON, reloaded in 1.4ms. Fifty such workbooks are a few tens of megabytes.
//! That is far below the scale at which an embedded key-value store or a
//! memory-mapped columnar format earns its complexity, so this is a directory
//! of files and a manifest.
//!
//! Formula groups used to be left out of that, on a measurement that no longer
//! holds. They were 464,131 nodes and 119 MiB — but that was a workbook read
//! through calamine before the fork's fixes, where mis-decoded relative
//! references gave a filled-down column a different shape every row, so almost
//! nothing grouped. Read correctly the same workbook has **1,272 groups**, and
//! keeping them costs 397 KB and 0.9ms on reload. Rebuilding them instead costs
//! a full ingest of the source file: ten seconds and 6 GB. So they are stored,
//! up to [`MAX_STORED_FORMULA_GROUPS`], and the lexical index can then find a
//! formula without the workbook being present at all.
//!
//! What is still *not* stored is as deliberate:
//!
//! - **Cell values.** The workbook is 6 GB in memory. Nodes carry the ranges
//!   they stand for, so the cells are one read away, and a stored copy could
//!   only ever go stale. A formula-group node carries the formula *text* and
//!   its R1C1 shape, which are structure; no value it computed is written here.
//! - **A formula-group layer past the budget.** The old number is a reminder
//!   that this layer has no natural bound: a workbook of one-off formulas
//!   groups into nothing, and its group layer would be as large as its formula
//!   count. Past the budget the layer goes back to being rebuilt on demand, and
//!   [`StoredGraph::formula_group_nodes`] records which kind a file holds, so a
//!   loader is never guessing.
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
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs4::fs_std::FileExt;
use petgraph::graph::NodeIndex;
use serde::{Deserialize, Serialize};

use eg_structure::Profiles;

use crate::build::{BuiltGraph, Graph};
use crate::report::BuildReport;

/// Bumped when the stored shape changes. A file from another version is
/// reported as a miss rather than deserialised into something plausible.
///
/// "The stored shape" is not only these structs: it is everything a stored
/// graph is *derived* from. `get` serves a stored graph whenever the version
/// and the source file's hash both match, and the file does not change when
/// the reader does — so a fix to what a formula decodes to, or a new field on
/// a formula group's shape, has to move this or the corpus goes on answering
/// from graphs built before the fix. Version 2 is the vendored reader's
/// sheet-qualifier fixes plus `R1C1Ref::end_sheet_name`.
pub const FORMAT_VERSION: u32 = 2;

/// How many formula-group nodes are worth keeping in a stored graph.
///
/// The layer is cheap on a workbook that groups well — 1,272 nodes and 397 KB
/// on the reference file — and unbounded on one that does not, since a workbook
/// of one-off formulas groups into nothing. At roughly 320 bytes a group, this
/// ceiling is about 6 MB of JSON and tens of milliseconds to reload, which is
/// where a store whose whole point is a sub-millisecond warm read stops being
/// one. Past it, the layer is rebuilt from the source file on demand, as the
/// whole layer used to be.
///
/// Fifteen times what the reference workbook needs, so it is a guard against a
/// pathological workbook and not a limit anything normal meets.
pub const MAX_STORED_FORMULA_GROUPS: usize = 20_000;

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
    /// Something was offered for a workbook this corpus does not hold. The
    /// manifest is the corpus — a file written beside it for a hash it does
    /// not list is unreachable, not stored.
    #[error("{content_hash} is not in this corpus: store the graph before what hangs off it")]
    NotInCorpus { content_hash: String },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> StoreError {
    let context = context.into();
    move |source| StoreError::Io { context, source }
}

/// The format version a stored file announces, ignoring every other field.
///
/// Read on its own so that a file whose *shape* belongs to another version is
/// recognised as such. Deserialising the whole file to reach its `version`
/// would fail first, on the very fields the version bump changed — which would
/// make the version gate unreachable in exactly the case it exists for.
/// `None` means the file is not a versioned document of ours at all.
fn stored_version(bytes: &[u8]) -> Option<u32> {
    #[derive(Deserialize)]
    struct Versioned {
        version: u32,
    }
    serde_json::from_slice::<Versioned>(bytes)
        .ok()
        .map(|v| v.version)
}

/// Whether a stored file was written by a format version we do not understand.
fn from_another_version(bytes: &[u8]) -> bool {
    stored_version(bytes).is_some_and(|v| v != FORMAT_VERSION)
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

/// One workbook's column profiles, as written to disk.
///
/// A separate file from the graph, and deliberately so. Everything in
/// `graphs/` is structure — ranges, headers, counts — and a reader can hand the
/// whole directory to someone who may not see the workbook. A profile carries
/// distinct values and sums, which are the workbook's data. Keeping them apart
/// means `profiles/` can be withheld or deleted without touching the graph, and
/// that the invariant about the graph stays true rather than becoming a
/// footnote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProfiles {
    pub version: u32,
    pub content_hash: String,
    pub path: String,
    pub profiles: Profiles,
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
    /// Columns profiled for this workbook, zero when it has no profiles.
    ///
    /// Defaulted rather than versioned: a manifest written before profiles
    /// existed is still a manifest, and reading it as "no profiles" is exactly
    /// right. Bumping the format for a field whose absence has an obvious
    /// meaning would drop every corpus on disk.
    #[serde(default)]
    pub profiled_columns: u64,
    /// Whether those profiles carry values — distinct lists and sums — or only
    /// counts and types.
    #[serde(default)]
    pub profile_values: bool,
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
        fs::create_dir_all(root.join("profiles"))
            .map_err(io_err(format!("creating {}", root.display())))?;

        let manifest_path = root.join("manifest.json");
        let manifest = match fs::read(&manifest_path) {
            // A manifest from another version is discarded rather than
            // migrated. Rebuilding costs seconds; a wrong answer out of a
            // half-understood file costs more. The version is read on its own
            // first: a version change is exactly when the rest of the shape
            // changes, so deserialising the whole file to reach the version
            // field would fail before ever reaching it.
            Ok(bytes) if from_another_version(&bytes) => Manifest {
                version: FORMAT_VERSION,
                ..Default::default()
            },
            Ok(bytes) => {
                serde_json::from_slice::<Manifest>(&bytes).map_err(|source| StoreError::Decode {
                    path: manifest_path.display().to_string(),
                    source,
                })?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest {
                version: FORMAT_VERSION,
                ..Default::default()
            },
            Err(e) => return Err(io_err(format!("reading {}", manifest_path.display()))(e)),
        };

        Ok(Corpus { root, manifest })
    }

    /// Serialize manifest mutations across processes and refresh this handle's
    /// snapshot after acquiring the lock. Atomic rename prevents torn files;
    /// this lock plus refresh prevents clean last-writer-wins lost updates.
    fn lock_and_reload(&mut self) -> Result<File, StoreError> {
        let lock_path = self.root.join("manifest.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(io_err(format!("opening {}", lock_path.display())))?;
        lock.lock_exclusive()
            .map_err(io_err(format!("locking {}", lock_path.display())))?;

        let fresh = Corpus::open(&self.root)?;
        self.manifest = fresh.manifest;
        Ok(lock)
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
        // Checked before deserialising, not after: a version bump is what a
        // shape change is announced by, so a file from another version would
        // fail to parse long before its `version` field could be compared.
        if from_another_version(&bytes) {
            return Ok(None);
        }
        let stored: StoredGraph =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Decode {
                path: path.display().to_string(),
                source,
            })?;
        if stored.version != FORMAT_VERSION || stored.content_hash != content_hash {
            return Ok(None);
        }
        // `root` is a raw index, and a petgraph index means nothing except
        // against the graph it came from. One past the end would panic in the
        // first caller that looked the node up, a long way from the bad file.
        if stored.root as usize >= stored.graph.node_count() {
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
        let _lock = self.lock_and_reload()?;
        write_atomically(&file, &bytes)?;

        let previous_entry = self.manifest.workbooks.get(content_hash).cloned();
        self.manifest.workbooks.insert(
            content_hash.to_string(),
            Entry {
                path: path.to_string(),
                sheets,
                cells,
                nodes: built.report.total_nodes(),
                edges: built.report.total_edges(),
                formula_group_nodes,
                profiled_columns: self
                    .manifest
                    .workbooks
                    .get(content_hash)
                    .map_or(0, |e| e.profiled_columns),
                profile_values: self
                    .manifest
                    .workbooks
                    .get(content_hash)
                    .is_some_and(|e| e.profile_values),
            },
        );
        // As in `forget`: an operation that returns `Err` must leave this
        // handle agreeing with the manifest on disk, or a later `put_profiles`
        // passes its "already in the corpus" check against an entry no
        // manifest lists. The graph file stays where it is — `get` reads
        // through the manifest, so an entry the manifest does not carry is
        // unreachable, and a retry overwrites it.
        if let Err(error) = self.write_manifest() {
            match previous_entry {
                Some(entry) => {
                    self.manifest
                        .workbooks
                        .insert(content_hash.to_string(), entry);
                }
                None => {
                    self.manifest.workbooks.remove(content_hash);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// Drop a workbook from the corpus. Returns whether it was there.
    pub fn forget(&mut self, content_hash: &str) -> Result<bool, StoreError> {
        let _lock = self.lock_and_reload()?;
        let Some(entry) = self.manifest.workbooks.remove(content_hash) else {
            return Ok(false);
        };
        // The manifest goes first. It is what `get` consults, so once the entry
        // is gone the workbook is forgotten whatever happens to the file; the
        // other order leaves a manifest on disk that still lists it while this
        // `Corpus` believes it does not.
        if let Err(error) = self.write_manifest() {
            self.manifest
                .workbooks
                .insert(content_hash.to_string(), entry);
            return Err(error);
        }
        let path = self.graph_path(content_hash);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(format!("removing {}", path.display()))(e)),
        }
        let profiles_path = self.profiles_path(content_hash);
        match fs::remove_file(&profiles_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(format!("removing {}", profiles_path.display()))(e)),
        }
        Ok(true)
    }

    /// Store the column profiles for a workbook already in the corpus.
    ///
    /// Separate from [`Corpus::put`] because it is separately refusable: a
    /// corpus can hold every graph and no profile, which is what a caller that
    /// will not write cell values to disk wants.
    ///
    /// "Already in the corpus" is checked, not assumed. [`Corpus::profiles`]
    /// reads through the manifest, so a file written for a hash the manifest
    /// does not list could never be read back — and this is the one file the
    /// store writes that holds the workbook's own values, sitting in
    /// `profiles/` where [`Corpus::forget`] will never reach it, because
    /// `forget` works from the manifest too. Refused before anything is
    /// written rather than after.
    pub fn put_profiles(
        &mut self,
        content_hash: &str,
        path: &str,
        profiles: &Profiles,
    ) -> Result<(), StoreError> {
        let _lock = self.lock_and_reload()?;
        if !self.manifest.workbooks.contains_key(content_hash) {
            return Err(StoreError::NotInCorpus {
                content_hash: content_hash.to_string(),
            });
        }
        let stored = StoredProfiles {
            version: FORMAT_VERSION,
            content_hash: content_hash.to_string(),
            path: path.to_string(),
            profiles: profiles.clone(),
        };
        let path = self.profiles_path(content_hash);
        let previous_entry = self.manifest.workbooks.get(content_hash).cloned();
        let bytes = serde_json::to_vec(&stored).expect("profiles of plain data serialise");
        write_atomically(&path, &bytes)?;
        let entry = self
            .manifest
            .workbooks
            .get_mut(content_hash)
            .expect("checked before the write");
        entry.profiled_columns = profiles.len() as u64;
        entry.profile_values = profiles.values;
        if let Err(error) = self.write_manifest() {
            if let Some(entry) = previous_entry {
                self.manifest
                    .workbooks
                    .insert(content_hash.to_string(), entry);
            }
            // The file goes, rather than reverting to what it held before.
            // This is the one stored file carrying the workbook's own values,
            // and both directions of a revert are a disclosure: putting a
            // `--redact-values` run's predecessor back restores the values the
            // run existed to remove, and leaving this run's file under the old
            // manifest entry serves values that entry says are not there.
            // Neither happens if the corpus simply holds no profiles for the
            // workbook; `profiles` reads that as `Ok(None)`, and a retry
            // writes the file again.
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(())
    }

    /// The profiles for a workbook, if any were stored.
    ///
    /// `Ok(None)` for a workbook profiled by nobody, which is an ordinary state
    /// and not an error — the graph is the corpus, the profiles are an extra.
    ///
    /// Checked against the manifest first, the same way [`Corpus::get`] is: a
    /// hash forgotten (or never profiled) is a miss even if a stale file
    /// happens to sit on disk, and the `content_hash` inside the file is
    /// compared too, so a filename-prefix collision cannot serve the wrong
    /// workbook's values.
    pub fn profiles(&self, content_hash: &str) -> Result<Option<Profiles>, StoreError> {
        if !self
            .manifest
            .workbooks
            .get(content_hash)
            .is_some_and(|e| e.profile_values || e.profiled_columns > 0)
        {
            return Ok(None);
        }
        let file = self.profiles_path(content_hash);
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(format!("reading {}", file.display()))(e)),
        };
        if from_another_version(&bytes) {
            return Ok(None);
        }
        let stored: StoredProfiles =
            serde_json::from_slice(&bytes).map_err(|source| StoreError::Decode {
                path: file.display().to_string(),
                source,
            })?;
        if stored.content_hash != content_hash {
            return Ok(None);
        }
        Ok(Some(stored.profiles))
    }

    /// Forget one workbook's profiles, leaving its graph.
    ///
    /// The manifest goes first and the file second, the order [`Corpus::forget`]
    /// uses and for a sharper reason: this file holds the workbook's own cell
    /// values. Removing it first and rolling back on a failed manifest write
    /// meant a scrub that failed *put the values back on disk* — and reported
    /// an error while doing it, so the corpus went on serving what the caller
    /// had asked it to forget. With the manifest first, a failure leaves
    /// exactly the state the call started in, and nothing is ever written back.
    pub fn forget_profiles(&mut self, content_hash: &str) -> Result<(), StoreError> {
        let _lock = self.lock_and_reload()?;
        let path = self.profiles_path(content_hash);
        let previous_entry = self.manifest.workbooks.get(content_hash).cloned();
        if let Some(entry) = self.manifest.workbooks.get_mut(content_hash) {
            entry.profiled_columns = 0;
            entry.profile_values = false;
        }
        if let Err(error) = self.write_manifest() {
            match previous_entry {
                Some(entry) => {
                    self.manifest
                        .workbooks
                        .insert(content_hash.to_string(), entry);
                }
                None => {
                    self.manifest.workbooks.remove(content_hash);
                }
            }
            return Err(error);
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            // The manifest no longer lists the profiles, so nothing reads
            // them; but the file is still there, still holding values, and
            // the caller is the only one who can do anything about that.
            Err(e) => Err(io_err(format!(
                "removing {} — it may still hold the workbook's values",
                path.display()
            ))(e)),
        }
    }

    /// Where a workbook's profiles are written.
    pub fn profiles_path(&self, content_hash: &str) -> PathBuf {
        self.root.join("profiles").join(self.stem(content_hash))
    }

    /// Where a workbook's graph is written. Public so a caller can measure what
    /// the store costs, or say where an answer came from.
    pub fn graph_path(&self, content_hash: &str) -> PathBuf {
        self.root.join("graphs").join(self.stem(content_hash))
    }

    /// A hash as a filename.
    ///
    /// The hash is hex from blake3, so it cannot escape the directory. Kept to
    /// its first 32 characters, which is still far past collision.
    fn stem(&self, content_hash: &str) -> String {
        let stem: String = content_hash
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(32)
            .collect();
        format!("{stem}.json")
    }

    fn write_manifest(&self) -> Result<(), StoreError> {
        let path = self.root.join("manifest.json");
        let bytes = serde_json::to_vec_pretty(&self.manifest).expect("the manifest serialises");
        write_atomically(&path, &bytes)
    }
}

/// Write through a temporary file and rename, so an interrupted write leaves
/// the previous version rather than a truncated one.
///
/// Manifest mutations hold `manifest.lock`; other files still use unique
/// temporary names so independent workbook writes cannot collide. A per-call
/// sequence also separates two handles in the same process.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), sequence));
    fs::write(&tmp, bytes).map_err(io_err(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(io_err(format!("renaming into {}", path.display())))
}
