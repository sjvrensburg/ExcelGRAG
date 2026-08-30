//! The lexical index: a tantivy index over the nodes of every stored graph.
//!
//! Lexical search is first because most of what a person types at a spreadsheet
//! is a word the spreadsheet already contains — a sheet name, a column header,
//! a defined name, a function. Those are exact tokens, and an embedding is a
//! worse way to find an exact token than an inverted index is.
//!
//! A hit is not an answer. It is a node index plus the content hash of the
//! workbook the node belongs to, which is exactly what is needed to reopen that
//! graph from the [`Corpus`](eg_graph::store::Corpus) and traverse outwards.
//! Retrieval is P5's job; this layer's job is to find the door.
//!
//! The index lives beside the corpus and is keyed the same way, by the blake3
//! of the source file: reindexing a workbook deletes every document carrying
//! its hash first, so a workbook indexed twice appears once, and a workbook
//! that has changed can never match under its old hash.
//!
//! ```no_run
//! # use eg_index::{SearchOptions, TextIndex};
//! let index = TextIndex::open("index")?;
//! for hit in index.search("revenue", &SearchOptions::default())? {
//!     println!("{} {} — {}", hit.kind.as_str(), hit.label, hit.a1.unwrap_or_default());
//! }
//! # Ok::<(), eg_index::IndexError>(())
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use eg_graph::store::StoredGraph;
use eg_graph::{BuiltGraph, Graph, NodeKind};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TantivyDocument, TextFieldIndexing, TextOptions, Value, FAST,
    INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, Stemmer, TextAnalyzer};
use tantivy::{Index, IndexWriter, ReloadPolicy, Score, TantivyError, Term};

use crate::doc::{docs_for, NodeDoc};
use crate::tokenize::{SpreadsheetTokenizer, TOKENIZER};

/// How much heap the writer may use before it flushes. Tantivy's floor is 15
/// MB; the reference workbook's region-level graph is 732 documents, so this is
/// sized for the formula-group case, which is 464,131.
const WRITER_HEAP: usize = 64 << 20;

/// Weights for the three text fields.
///
/// A node's own name is what a searcher typed, so it outweighs the sheet and
/// table names around it — otherwise every node on a sheet called Revenue
/// outranks the Revenue column. The workbook path is scored lowest: it matches
/// every node of a workbook at once, so it can only ever break ties.
const LABEL_BOOST: f32 = 4.0;
const CONTEXT_BOOST: f32 = 1.5;
const BODY_BOOST: f32 = 1.0;
const PATH_BOOST: f32 = 0.3;

/// How much a node's size may move it.
///
/// The final score is the text score times `1 + log10(1 + cells) / SIZE_SPREAD`,
/// so a node covering half a million cells is worth about 2.4 times one
/// covering none. That is enough to order a field of ties — `VLOOKUP` matches
/// nearly every formula in a real workbook, and without this the top of that
/// list is whichever group tantivy reached first — and far too little to put a
/// big irrelevant node above a small exact match, where the text scores differ
/// by much more than that.
const SIZE_SPREAD: f32 = 4.0;

/// The fast field the size weighting reads.
const CELLS: &str = "cells";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    /// Anything the embedding model could not do. Its error type is not in
    /// this crate's public interface, so it arrives as text: a caller can
    /// report it, and there is nothing useful to match on.
    #[error("{context}: {detail}")]
    Embed { context: String, detail: String },
    #[error("{context}: {source}")]
    Tantivy {
        context: String,
        #[source]
        source: TantivyError,
    },
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> IndexError {
    let context = context.into();
    move |source| IndexError::Io { context, source }
}

fn tantivy_err(context: impl Into<String>) -> impl FnOnce(TantivyError) -> IndexError {
    let context = context.into();
    move |source| IndexError::Tantivy { context, source }
}

/// The index fields, resolved once so a search is not doing string lookups.
#[derive(Clone, Copy)]
struct Fields {
    hash: Field,
    path: Field,
    node: Field,
    kind: Field,
    sheet: Field,
    a1: Field,
    label: Field,
    context: Field,
    body: Field,
    cells: Field,
}

/// Text as this index reads it: split at compound boundaries, lowercased, and
/// stemmed, so `Revenues` in a header answers a search for `revenue`.
///
/// Naming the tokenizer in the schema rather than relying on the default is
/// also what makes a change to it a schema change, and so a rebuild.
fn text_field() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(TOKENIZER)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

fn analyzer() -> TextAnalyzer {
    // No length filter: the tokenizer drops an over-long run itself, before
    // splitting it, so a filter here would only re-check what it already did.
    TextAnalyzer::builder(SpreadsheetTokenizer)
        .filter(LowerCaser)
        .filter(Stemmer::default())
        .build()
}

impl Fields {
    fn build() -> (Schema, Fields) {
        let text = text_field();
        let stored_text = text_field().set_stored();
        let mut b = Schema::builder();
        let fields = Fields {
            // The workbook's content hash, and the sheet name, are terms to
            // filter on rather than prose to match, so neither is tokenised.
            hash: b.add_text_field("hash", STRING | STORED),
            path: b.add_text_field("path", stored_text.clone()),
            node: b.add_u64_field("node", INDEXED | STORED),
            kind: b.add_text_field("kind", STRING | STORED),
            sheet: b.add_text_field("sheet", STRING | STORED),
            // A1 is a citation to hand back, never something to match: nobody
            // searches for `$B$7`, and indexing it would let a stray `A1` in a
            // query pull in every node whose range happens to start there.
            a1: b.add_text_field("a1", STORED),
            label: b.add_text_field("label", stored_text),
            context: b.add_text_field("context", text.clone()),
            body: b.add_text_field("body", text),
            // Fast, not stored: it is read once per candidate document while
            // scoring, and never handed back.
            cells: b.add_u64_field(CELLS, FAST),
        };
        (b.build(), fields)
    }

    fn search_fields(&self) -> [(Field, f32); 4] {
        [
            (self.label, LABEL_BOOST),
            (self.context, CONTEXT_BOOST),
            (self.body, BODY_BOOST),
            (self.path, PATH_BOOST),
        ]
    }
}

/// What to search for, beyond the query text.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    /// Restrict to these node kinds. Empty means every kind.
    pub kinds: Vec<NodeKind>,
    /// Restrict to one workbook, by content hash.
    pub workbook: Option<String>,
    /// Restrict to one sheet, by exact name.
    pub sheet: Option<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            limit: 10,
            kinds: Vec::new(),
            workbook: None,
            sheet: None,
        }
    }
}

/// One result: a node, and where to go and read it.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub score: f32,
    /// The content hash of the workbook holding the node. With `node`, this is
    /// enough to reopen the graph from the corpus and traverse from here.
    pub workbook: String,
    /// Where that workbook was when it was indexed. A hint, not an identity.
    pub path: String,
    pub node: u32,
    pub kind: NodeKind,
    pub sheet: Option<String>,
    pub label: String,
    /// A fully-qualified citation, for the kinds that cover a rectangle.
    pub a1: Option<String>,
}

/// A lexical index over one corpus.
pub struct TextIndex {
    dir: PathBuf,
    index: Index,
    fields: Fields,
    writer: IndexWriter,
}

impl TextIndex {
    /// Open the index under `root`, creating it if it is not there.
    ///
    /// `root` is the corpus directory: the index goes in `root/text`, so one
    /// path names the graphs and the index over them.
    ///
    /// An index whose schema is not the current one is deleted and rebuilt
    /// empty, the same trade the corpus makes for a stored graph from another
    /// format version. Reindexing costs seconds, and there is no honest way to
    /// read documents laid out by a schema we no longer have.
    pub fn open(root: impl AsRef<Path>) -> Result<TextIndex, IndexError> {
        let dir = root.as_ref().join("text");
        let (schema, fields) = Fields::build();

        fs::create_dir_all(&dir).map_err(io_err(format!("creating {}", dir.display())))?;

        let index = match Index::open_in_dir(&dir) {
            Ok(existing) if existing.schema() == schema => existing,
            Ok(_) => {
                fs::remove_dir_all(&dir).map_err(io_err(format!("clearing {}", dir.display())))?;
                fs::create_dir_all(&dir).map_err(io_err(format!("creating {}", dir.display())))?;
                Index::create_in_dir(&dir, schema.clone()).map_err(tantivy_err(format!(
                    "creating an index in {}",
                    dir.display()
                )))?
            }
            // Not an index yet, or one we cannot read. Either way there is
            // nothing to preserve, so create over it.
            Err(_) => Index::create_in_dir(&dir, schema.clone()).map_err(tantivy_err(format!(
                "creating an index in {}",
                dir.display()
            )))?,
        };

        // Registered on the index itself, so the writer and the query parser
        // both see it. An index whose text was written by one tokenizer and
        // searched with another matches nothing, and says nothing about it.
        index.tokenizers().register(TOKENIZER, analyzer());

        let writer = index.writer(WRITER_HEAP).map_err(tantivy_err(format!(
            "opening a writer on {}",
            dir.display()
        )))?;

        Ok(TextIndex {
            dir,
            index,
            fields,
            writer,
        })
    }

    /// Where the index sits on disk.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Total bytes the index occupies.
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

    /// How many documents the index holds.
    pub fn len(&self) -> Result<u64, IndexError> {
        Ok(self.searcher()?.num_docs())
    }

    pub fn is_empty(&self) -> Result<bool, IndexError> {
        Ok(self.len()? == 0)
    }

    /// Index a graph loaded from the corpus.
    pub fn index_stored(&mut self, stored: &StoredGraph) -> Result<usize, IndexError> {
        self.index_graph(&stored.graph, &stored.content_hash, &stored.path)
    }

    /// Index a graph that has just been built.
    pub fn index_built(
        &mut self,
        built: &BuiltGraph,
        content_hash: &str,
        path: &str,
    ) -> Result<usize, IndexError> {
        self.index_graph(&built.graph, content_hash, path)
    }

    /// Index every node of a graph, replacing anything held under the same
    /// content hash. Returns the number of documents written.
    pub fn index_graph(
        &mut self,
        graph: &Graph,
        content_hash: &str,
        path: &str,
    ) -> Result<usize, IndexError> {
        // Delete first, in the same commit as the insert: a workbook rebuilt
        // with formula groups has more nodes than one without, and adding over
        // the old documents would leave the surplus behind as hits pointing at
        // node indices that no longer mean what they did.
        self.writer
            .delete_term(Term::from_field_text(self.fields.hash, content_hash));

        let docs = docs_for(graph);
        for doc in &docs {
            self.writer
                .add_document(self.to_tantivy(doc, content_hash, path))
                .map_err(tantivy_err("adding a document"))?;
        }
        self.writer.commit().map_err(tantivy_err("committing"))?;
        Ok(docs.len())
    }

    /// Drop a workbook from the index.
    pub fn forget(&mut self, content_hash: &str) -> Result<(), IndexError> {
        self.writer
            .delete_term(Term::from_field_text(self.fields.hash, content_hash));
        self.writer.commit().map_err(tantivy_err("committing"))?;
        Ok(())
    }

    /// Search, most relevant first.
    pub fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<Hit>, IndexError> {
        let searcher = self.searcher()?;
        let Some(query) = self.build_query(query, opts) else {
            return Ok(Vec::new());
        };

        let top = searcher
            .search(
                &query,
                &TopDocs::with_limit(opts.limit.max(1)).tweak_score(size_weighted),
            )
            .map_err(tantivy_err("searching"))?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, address) in top {
            let doc: TantivyDocument = searcher
                .doc(address)
                .map_err(tantivy_err("reading a stored document"))?;
            if let Some(hit) = self.to_hit(score, &doc) {
                hits.push(hit);
            }
        }
        Ok(hits)
    }

    fn searcher(&self) -> Result<tantivy::Searcher, IndexError> {
        // Built per call rather than held: a reader caches the segments it saw
        // when it was made, and this index is written and read by the same
        // process in the same breath, so a cached view would miss the write
        // that just happened.
        let reader = self
            .index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(tantivy_err("opening a reader"))?;
        Ok(reader.searcher())
    }

    /// The user's query, with the filters wrapped around it.
    ///
    /// `None` when the query text produces nothing to match, which is not an
    /// error: an empty query is an empty result, and returning the whole corpus
    /// filtered by kind would be a different question than the one asked.
    fn build_query(&self, text: &str, opts: &SearchOptions) -> Option<Box<dyn Query>> {
        let fields = self.fields.search_fields();
        let mut parser = QueryParser::for_index(&self.index, fields.iter().map(|f| f.0).collect());
        for (field, boost) in fields {
            parser.set_field_boost(field, boost);
        }

        // Lenient, because the text of a spreadsheet is full of characters the
        // query grammar reserves — `Rates!A2`, `SUM(B:B)`, `'Q3 Sales'`. A
        // person typing a formula they saw should get the nodes holding it, not
        // a parse error.
        let (parsed, _errors) = parser.parse_query_lenient(text);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, parsed)];

        if let Some(hash) = &opts.workbook {
            clauses.push((Occur::Must, self.term_query(self.fields.hash, hash)));
        }
        if let Some(sheet) = &opts.sheet {
            clauses.push((Occur::Must, self.term_query(self.fields.sheet, sheet)));
        }
        if !opts.kinds.is_empty() {
            let any: Vec<(Occur, Box<dyn Query>)> = opts
                .kinds
                .iter()
                .map(|k| (Occur::Should, self.term_query(self.fields.kind, k.as_str())))
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(any))));
        }

        Some(Box::new(BooleanQuery::new(clauses)))
    }

    fn term_query(&self, field: Field, value: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        ))
    }

    fn to_tantivy(&self, doc: &NodeDoc, content_hash: &str, path: &str) -> TantivyDocument {
        let f = self.fields;
        let mut out = TantivyDocument::new();
        out.add_text(f.hash, content_hash);
        out.add_text(f.path, path);
        out.add_u64(f.node, doc.node as u64);
        out.add_text(f.kind, doc.kind.as_str());
        if let Some(sheet) = &doc.sheet {
            out.add_text(f.sheet, sheet);
        }
        if let Some(a1) = &doc.a1 {
            out.add_text(f.a1, a1);
        }
        out.add_text(f.label, &doc.label);
        out.add_text(f.context, &doc.context);
        out.add_text(f.body, &doc.body);
        out.add_u64(f.cells, doc.cells);
        out
    }

    /// A stored document back into a hit.
    ///
    /// `None` when a required field is missing or unrecognised, which can only
    /// mean the document was not written by this schema. Dropped rather than
    /// filled in with a default: a hit whose node index or kind was guessed
    /// would send the retrieval layer to the wrong node.
    fn to_hit(&self, score: f32, doc: &TantivyDocument) -> Option<Hit> {
        let f = self.fields;
        let text = |field: Field| doc.get_first(field).and_then(|v| v.as_str());
        Some(Hit {
            score,
            workbook: text(f.hash)?.to_string(),
            path: text(f.path).unwrap_or_default().to_string(),
            node: doc.get_first(f.node).and_then(|v| v.as_u64())? as u32,
            kind: NodeKind::parse(text(f.kind)?)?,
            sheet: text(f.sheet).map(str::to_string),
            label: text(f.label).unwrap_or_default().to_string(),
            a1: text(f.a1).map(str::to_string),
        })
    }
}

/// Weight a text score by how many cells the node stands for.
///
/// A segment with no `cells` column at all scores every document as if it stood
/// for none, which is the right fallback: it degrades to plain text relevance
/// rather than failing the search.
fn size_weighted(segment: &tantivy::SegmentReader) -> impl Fn(tantivy::DocId, Score) -> Score {
    let cells = segment.fast_fields().u64(CELLS).ok();
    move |doc, score| {
        let n = cells.as_ref().and_then(|c| c.first(doc)).unwrap_or(0);
        score * (1.0 + ((1 + n) as f32).log10() / SIZE_SPREAD)
    }
}
