//! From a question to the part of a workbook that answers it.
//!
//! `eg-index` finds the door: a ranked list of nodes whose text matches, by
//! word or by meaning. That is not yet enough to act on. "The Revenue column of
//! BP136" is the right node and still says nothing about which table it sits
//! in, what feeds it, or what breaks if it is wrong.
//!
//! P5a is the walk that answers those: [`expand`] takes the hits, reopens each
//! workbook's graph from the corpus, and follows the edges outwards under a
//! budget, recording for every node it brings back which node pulled it in and
//! along which edge. Rendering that into a passage is P5b; recovering the
//! individual cells behind it is P6, done on demand against the workbook.
//!
//! ```no_run
//! # use eg_graph::store::Corpus;
//! # use eg_index::{SearchOptions, TextIndex};
//! # use eg_retrieve::{expand, ExpandOptions};
//! let corpus = Corpus::open("index")?;
//! let index = TextIndex::open("index")?;
//! let hits = index.search("bad debt", &SearchOptions::default())?;
//! let found = expand(&corpus, &hits, &ExpandOptions::default())?;
//! for workbook in &found.workbooks {
//!     for node in &workbook.nodes {
//!         println!("{:>10} {}", node.role.as_str(), node.label);
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod context;
pub mod expand;
pub mod search;

pub use context::{render, RenderOptions, Rendered};
pub use expand::{
    expand, ExpandOptions, RetrieveError, Retrieved, RetrievedNode, Role, WorkbookContext,
};
pub use search::{embedder, find, Fusion, SearchError, LEXICAL_WEIGHT};
