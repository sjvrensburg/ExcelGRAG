//! Finding the way into a corpus of workbook graphs.
//!
//! The graph says how a workbook hangs together, but a question does not arrive
//! as a node index. It arrives as words — "revenue", "the tax rate", "Q3
//! Sales" — and something has to turn those into the handful of nodes worth
//! traversing from. That is this crate.
//!
//! There are two indexes, over one flattening of the graph's nodes.
//! [`TextIndex`] is lexical — tantivy, over every node of every stored graph.
//! [`VectorIndex`] is semantic — embeddings from a local model, scanned in
//! full. Both are keyed by the same content hash the corpus uses, and a hit
//! from either carries the workbook hash and the node index, which is exactly
//! what reopening the graph and expanding outwards needs.
//!
//! They fail in opposite directions, so neither is a fallback for the other:
//! run both and fuse the rankings with [`fuse`]. The [`hybrid`] module has the
//! argument for why that is done by rank and not by score.
//!
//! ```no_run
//! # use eg_index::{fuse, Embedder, SearchOptions, TextIndex, VectorIndex};
//! # use eg_index::vector::embeddable;
//! # use eg_graph::store::Corpus;
//! let corpus = Corpus::open("index")?;
//! let mut embedder = Embedder::new()?;
//! let mut text = TextIndex::open("index")?;
//! let mut vectors = VectorIndex::open("index", embedder.name(), embedder.dim())?;
//!
//! let hashes: Vec<String> = corpus.entries().map(|(h, _)| h.to_string()).collect();
//! for hash in &hashes {
//!     let Some(stored) = corpus.get(hash)? else { continue };
//!     text.index_stored(&stored)?;
//!     let docs = embeddable(&stored.graph);
//!     let made = embedder.embed_documents(&docs)?;
//!     vectors.put(hash, &stored.path, &docs, &made)?;
//! }
//!
//! let opts = SearchOptions::default();
//! let lexical = text.search("bad debt", &opts)?;
//! let semantic = vectors.search(&embedder.embed_query("bad debt")?, &opts);
//! let hits = fuse(&[&lexical, &semantic], opts.limit);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod doc;
pub mod embed;
pub mod hybrid;
pub mod text;
pub mod tokenize;
pub mod vector;

pub use doc::{docs_for, NodeDoc};
pub use embed::{Embedder, DEFAULT_MODEL};
pub use hybrid::fuse;
pub use text::{Hit, IndexError, SearchOptions, TextIndex};
pub use vector::{embeddable, VectorIndex};
