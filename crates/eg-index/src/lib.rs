//! Finding the way into a corpus of workbook graphs.
//!
//! The graph says how a workbook hangs together, but a question does not arrive
//! as a node index. It arrives as words — "revenue", "the tax rate", "Q3
//! Sales" — and something has to turn those into the handful of nodes worth
//! traversing from. That is this crate.
//!
//! P4a is the lexical half: a tantivy index over every node of every stored
//! graph, keyed by the same content hash the corpus uses. A hit carries the
//! workbook hash and the node index, which is exactly what reopening the graph
//! and expanding outwards needs. The vector half comes next, over the same
//! [`NodeDoc`] flattening, so both indexes describe the same nodes.
//!
//! ```no_run
//! # use eg_index::{SearchOptions, TextIndex};
//! # use eg_graph::store::Corpus;
//! let corpus = Corpus::open("index")?;
//! let mut index = TextIndex::open("index")?;
//! let hashes: Vec<String> = corpus.entries().map(|(h, _)| h.to_string()).collect();
//! for hash in &hashes {
//!     if let Some(stored) = corpus.get(hash)? {
//!         index.index_stored(&stored)?;
//!     }
//! }
//! let hits = index.search("revenue", &SearchOptions::default())?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod doc;
pub mod text;
pub mod tokenize;

pub use doc::{docs_for, NodeDoc};
pub use text::{Hit, IndexError, SearchOptions, TextIndex};
