//! What a graph node looks like to a search index.
//!
//! A node is not a document. `Column("Revenue")` is three words on its own, and
//! there are hundreds of columns called Revenue in a corpus; what makes one of
//! them the right hit is the table it sits in and the sheet that table is on.
//! So each node is flattened into a document carrying its own name, the names
//! of the nodes above it, and whatever else it holds that someone might type.
//!
//! The split into `label`, `context` and `body` is what lets the query weigh
//! them differently: a match on a column's own header should beat a match on
//! the sheet name it happens to live under, or every node on a sheet called
//! Revenue outranks the Revenue column itself.
//!
//! A node also carries what it *holds*, where the corpus knows: a column's
//! profile records the distinct values of a column that has few enough of them,
//! and the bounds of one that is numeric. Nothing indexed those, which made a
//! whole class of question unanswerable — a workbook is asked about in the
//! vocabulary of its values at least as often as in the vocabulary of its
//! headers, and searching for a figure that is plainly in a cell came back
//! blind. They go in [`NodeDoc::values`], scored below the node's own name,
//! because a column *called* Retail is a better answer than one *containing*
//! the word.
//!
//! What that cannot cover is stated where the limit is: a profile abandons the
//! distinct list above [`eg_structure::ProfileOptions::max_distinct`], so a number in a
//! column of two hundred thousand measurements is in no index and never will
//! be. `eg_eval::cells_holding` scans the cells for those.
//!
//! This module knows nothing about tantivy. The vector index will want the same
//! flattening with the three fields joined instead of weighed, and building it
//! here keeps the two indexes describing the same nodes.

use eg_graph::{EdgeKind, Graph, Node, NodeKind};
use eg_model::{shown, RangeRef, SheetId};
use eg_structure::{ColumnProfile, Profiles};
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use rustc_hash::FxHashMap;

/// One node, flattened for indexing.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeDoc {
    /// The node's index in the graph it came from. Meaningless anywhere else,
    /// which is why a hit carries the workbook hash beside it.
    pub node: u32,
    pub kind: NodeKind,
    /// The sheet the node lives on, by name. `None` for the workbook root, an
    /// external workbook, and a workbook-scoped defined name.
    pub sheet: Option<String>,
    /// A fully-qualified citation of the cells the node covers, for the kinds
    /// that cover a rectangle.
    pub a1: Option<String>,
    /// The node's own name. What a searcher is most likely to type.
    pub label: String,
    /// The names of the nodes above it: sheet, and the region for a column or
    /// a formula group.
    pub context: String,
    /// Everything else worth matching — a formula, what a name refers to, the
    /// headers of a table's columns.
    pub body: String,
    /// What the node has been profiled to hold, as the strings a profile
    /// stores. Empty for every kind but a column, for a column no profile
    /// covers, and for a corpus indexed with values redacted.
    ///
    /// The kept distinct values where there are few enough of them; the
    /// minimum and the maximum where there were too many to keep and the
    /// column is numeric. Only those two — a sum or a mean is a number the
    /// workbook never wrote in a cell, and indexing it would offer a hit on a
    /// value nobody can go and read.
    pub values: Vec<String>,
    /// Whether those values read as a category — few of them, each repeated —
    /// rather than as a key column's identifiers.
    ///
    /// What decides whether they are worth putting in front of a sentence
    /// embedder. `Retail, Business, Wholesale` is a phrase about what the
    /// column means; two thousand account numbers is not, and would crowd the
    /// node's own name out of a 512-token window.
    pub categorical: bool,
    /// How many cells the node stands for.
    ///
    /// The tie-breaker. `VLOOKUP` appears in nearly every formula of a real
    /// workbook, so text relevance alone ranks a group covering three cells
    /// level with one covering 115,000, and the top of the list is then
    /// arbitrary. Same idea as an edge's weight: how much of the workbook rests
    /// on this. Zero where the node stands for no cells of its own.
    pub cells: u64,
}

impl NodeDoc {
    /// The node as one line of prose, for an embedding model.
    ///
    /// The kind is spelled out and the fields are joined rather than weighed,
    /// because a sentence embedder has no notion of fields — it reads the whole
    /// string. "column Revenue in Q3 Sales" is a thing a person might have
    /// written; the three fields side by side are not.
    pub fn embedding_text(&self) -> String {
        let mut out = format!("{} {}", self.kind.as_str(), self.label);
        if !self.context.is_empty() {
            out.push_str(" in ");
            out.push_str(&self.context);
        }
        if !self.body.is_empty() {
            out.push_str(": ");
            out.push_str(&self.body);
        }
        if self.categorical {
            let mut values = self.values.iter().take(EMBEDDED_VALUES).peekable();
            if values.peek().is_some() {
                out.push_str(", holding ");
                out.push_str(&values.cloned().collect::<Vec<_>>().join(", "));
            }
        }
        out
    }
}

/// How many of a categorical column's values reach the embedding.
///
/// A cap rather than the whole list, because the point of putting them there
/// is that the node reads as a phrase about what the column means, and
/// `max_distinct` values is a list rather than a phrase. The
/// lexical half indexes all of them, which is where an exact value is meant to
/// be found anyway.
const EMBEDDED_VALUES: usize = 12;

/// Flatten every node of a graph, with nothing said about what it holds.
///
/// The order is the graph's own node order, so `node` is also the position.
pub fn docs_for(graph: &Graph) -> Vec<NodeDoc> {
    docs_for_with(graph, None)
}

/// Flatten every node of a graph, carrying the values its columns were
/// profiled to hold.
///
/// `profiles` is the same workbook's `profiles/` entry, or `None` for a corpus
/// that has none — indexed with `--no-profiles`, or stored before profiles
/// existed. A profile whose values were redacted carries none, so passing it is
/// the same as passing `None` and the caller does not have to know that.
///
/// A profile is matched to a column node by **range**, which is an identity and
/// not a guess: the graph builds a column node over the region body's rows at
/// one column, and `read_table` builds the profile over exactly that rectangle.
/// A column the profiler did not cover — a region's row-label columns, which
/// sit outside the body it profiles — simply gets no values, rather than the
/// values of whichever column happened to be next to it.
pub fn docs_for_with(graph: &Graph, profiles: Option<&Profiles>) -> Vec<NodeDoc> {
    let sheets = sheet_names(graph);
    let parents = parents(graph);
    let profiled = by_range(profiles);

    graph
        .node_indices()
        .map(|idx| doc_for(graph, idx, &sheets, &parents, &profiled))
        .collect()
}

/// Column profiles by the range they cover.
///
/// Empty when values were not collected: a profile built with
/// [`eg_structure::ProfileOptions::values`] off holds counts and types, which are structure
/// the graph already carries, and nothing a searcher would type.
fn by_range(profiles: Option<&Profiles>) -> FxHashMap<RangeRef, &ColumnProfile> {
    let Some(profiles) = profiles.filter(|p| p.values) else {
        return FxHashMap::default();
    };
    profiles.columns.iter().map(|c| (c.range, c)).collect()
}

/// What a profiled column holds, as the strings to index.
///
/// The kept distinct values, most frequent first, or — when there were too many
/// to keep — the bounds of a numeric column. The bounds are cells: some row
/// really does hold the minimum and some row the maximum, so a search for
/// either is a search that can be followed to a cell. A sum or a mean is not,
/// and is left out for that reason.
fn values_of(profile: &ColumnProfile) -> Vec<String> {
    if let Some(distinct) = &profile.distinct {
        return distinct.iter().map(|v| v.value.clone()).collect();
    }
    match &profile.numeric {
        // Rendered through the sheet's own fifteen digits, the way a profile
        // writes a distinct value, so the two spellings of a number that
        // reaches this index by two routes are one token.
        Some(n) => vec![format!("{}", shown(n.min)), format!("{}", shown(n.max))],
        None => Vec::new(),
    }
}

/// Sheet names by id, read off the sheet nodes rather than passed in, so a
/// graph reloaded from the store indexes exactly like a freshly built one.
fn sheet_names(graph: &Graph) -> FxHashMap<SheetId, String> {
    graph
        .node_weights()
        .filter_map(|n| match n {
            Node::Sheet(s) => Some((s.id, s.name.clone())),
            _ => None,
        })
        .collect()
}

/// The containing node of each node, following `CONTAINS` only.
///
/// `HEADER_OF` is deliberately skipped: a formula group is contained by its
/// region and *also* headed by a column, and taking the header as the parent
/// would make the ancestry depend on which edge came first.
fn parents(graph: &Graph) -> FxHashMap<NodeIndex, NodeIndex> {
    let mut parents = FxHashMap::default();
    for edge in graph.edge_references() {
        if edge.weight().kind == EdgeKind::Contains {
            parents.entry(edge.target()).or_insert(edge.source());
        }
    }
    parents
}

fn doc_for(
    graph: &Graph,
    idx: NodeIndex,
    sheets: &FxHashMap<SheetId, String>,
    parents: &FxHashMap<NodeIndex, NodeIndex>,
    profiled: &FxHashMap<RangeRef, &ColumnProfile>,
) -> NodeDoc {
    let node = &graph[idx];
    let sheet = node.sheet().and_then(|id| sheets.get(&id).cloned());
    let a1 = node.range().map(|r| cite(r, sheets));

    let mut context = Vec::new();
    if let Some(name) = &sheet {
        context.push(name.clone());
    }
    // A column and a formula group both sit inside a region, and the region's
    // title is the thing a person names when they mean it: "the Revenue column
    // of Q3 Sales". Titles only — a region with no title has a range for a
    // label, and an A1 range is noise in a text field.
    if let Some(&parent) = parents.get(&idx) {
        if let Node::Region(r) = &graph[parent] {
            if let Some(title) = &r.title {
                context.push(title.clone());
            }
        }
    }

    let body = match node {
        Node::Workbook(w) => w.format.clone().unwrap_or_default(),
        Node::Sheet(s) => {
            if s.visible {
                String::new()
            } else {
                "hidden".to_string()
            }
        }
        // A table is found by the columns it holds far more often than by its
        // own title, which is frequently absent. The column nodes are indexed
        // in their own right too; carrying the headers here is what makes a
        // search for "revenue" return the table as well as the column.
        Node::Region(r) => {
            let mut parts = vec![r.kind.as_str().to_string()];
            parts.extend(child_headers(graph, idx));
            parts.join(" ")
        }
        Node::Column(_) => String::new(),
        Node::FormulaGroup(g) => g.shape.clone(),
        Node::DefinedName(n) => n.refers_to.clone(),
        Node::ExternalWorkbook(_) => String::new(),
    };

    let profile = match node {
        Node::Column(c) => profiled.get(&c.range).copied(),
        _ => None,
    };

    NodeDoc {
        node: idx.index() as u32,
        cells: cells(node),
        kind: node.kind(),
        sheet,
        a1,
        label: node.label(),
        context: context.join(" "),
        body,
        values: profile.map(values_of).unwrap_or_default(),
        categorical: profile.is_some_and(ColumnProfile::is_categorical),
    }
}

/// How many cells a node stands for.
///
/// A column carries a range rather than a populated count, so its rows are the
/// closest honest answer. The workbook root, a defined name and an external
/// workbook stand for no cells of their own, and inflating them with the totals
/// beneath them would put the root at the top of every result.
fn cells(node: &Node) -> u64 {
    match node {
        Node::Sheet(s) => s.cells,
        Node::Region(r) => r.cell_count,
        Node::Column(c) => u64::from(c.range.rows()),
        Node::FormulaGroup(g) => g.cell_count,
        Node::Workbook(_) | Node::DefinedName(_) | Node::ExternalWorkbook(_) => 0,
    }
}

/// The headers of the columns a region contains.
fn child_headers(graph: &Graph, region: NodeIndex) -> Vec<String> {
    graph
        .edges_directed(region, Direction::Outgoing)
        .filter(|e| e.weight().kind == EdgeKind::Contains)
        .filter_map(|e| match &graph[e.target()] {
            Node::Column(c) => Some(c.header.clone()),
            _ => None,
        })
        .collect()
}

/// A citation for a range, falling back to the sheet index when the graph holds
/// no sheet node for it, so a citation is never silently dropped.
fn cite(range: RangeRef, sheets: &FxHashMap<SheetId, String>) -> String {
    match sheets.get(&range.sheet) {
        Some(name) => range.to_a1_with_sheet(name),
        None => format!("{}!{}", range.sheet, range.to_a1()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eg_graph::build;
    use eg_model::{
        Cell, CellValue, DefinedName, RangeRef, Sheet, SheetId, Workbook, WorkbookFormat,
    };

    fn grid(id: u16, name: &str, rows: &[&str]) -> Sheet {
        let mut sheet = Sheet::new(SheetId(id), name);
        for (r, line) in rows.iter().enumerate() {
            for (c, tok) in line.split_whitespace().enumerate() {
                if tok == "." {
                    continue;
                }
                let cell = match tok.strip_prefix('=') {
                    Some(f) => Cell {
                        value: CellValue::Number(0.0),
                        formula: Some(f.to_string()),
                        format: Default::default(),
                    },
                    None => match tok.parse::<f64>() {
                        Ok(n) => Cell::literal(CellValue::Number(n)),
                        Err(_) => Cell::literal(CellValue::Text(tok.to_string())),
                    },
                };
                sheet.set(r as u32, c as u16, cell);
            }
        }
        sheet
    }

    fn sample() -> Workbook {
        Workbook {
            path: "book.xlsx".into(),
            format: Some(WorkbookFormat::Xlsx),
            content_hash: "hash".into(),
            sheets: vec![grid(
                0,
                "Q3 Sales",
                &[
                    "Region Revenue Net",
                    "North 10 =B2*2",
                    "South 20 =B3*2",
                    "East 30 =B4*2",
                ],
            )],
            defined_names: vec![DefinedName {
                name: "TaxRate".into(),
                refers_to: "'Q3 Sales'!$B$1".into(),
                scope: None,
            }],
            external_links: Vec::new(),
        }
    }

    fn of_kind(docs: &[NodeDoc], kind: NodeKind) -> Vec<&NodeDoc> {
        docs.iter().filter(|d| d.kind == kind).collect()
    }

    #[test]
    fn a_column_carries_its_sheet_as_context() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        let revenue = of_kind(&docs, NodeKind::Column)
            .into_iter()
            .find(|d| d.label == "Revenue")
            .expect("the Revenue column is indexed");

        assert_eq!(revenue.sheet.as_deref(), Some("Q3 Sales"));
        assert!(revenue.context.contains("Q3 Sales"));
        // The sheet name needs quoting, and a citation that loses the quotes
        // reads as a sheet called Sales.
        assert!(revenue.a1.as_deref().unwrap().starts_with("'Q3 Sales'!"));
    }

    #[test]
    fn a_region_is_findable_by_the_columns_it_holds() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        let region = of_kind(&docs, NodeKind::Region)[0];
        assert!(
            region.body.contains("Revenue"),
            "body was {:?}",
            region.body
        );
        assert!(region.body.contains("Net"));
    }

    #[test]
    fn a_defined_name_carries_what_it_refers_to() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        let name = of_kind(&docs, NodeKind::DefinedName)[0];
        assert_eq!(name.label, "TaxRate");
        assert!(name.body.contains("$B$1"));
        // Workbook-scoped: it belongs to no sheet, and saying it does would be
        // a filter that quietly excludes it.
        assert_eq!(name.sheet, None);
    }

    #[test]
    fn a_node_reads_as_a_phrase_for_an_embedder() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        let revenue = of_kind(&docs, NodeKind::Column)
            .into_iter()
            .find(|d| d.label == "Revenue")
            .unwrap();
        assert_eq!(revenue.embedding_text(), "column Revenue in Q3 Sales");

        let name = of_kind(&docs, NodeKind::DefinedName)[0];
        assert_eq!(
            name.embedding_text(),
            "defined name TaxRate: 'Q3 Sales'!$B$1"
        );
    }

    #[test]
    fn a_node_carries_how_many_cells_it_stands_for() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        assert_eq!(of_kind(&docs, NodeKind::Sheet)[0].cells, 12);
        assert_eq!(of_kind(&docs, NodeKind::Region)[0].cells, 12);
        // Three body rows under the header.
        assert_eq!(
            of_kind(&docs, NodeKind::Column)
                .into_iter()
                .find(|d| d.label == "Revenue")
                .unwrap()
                .cells,
            3
        );
        // A name stands for no cells of its own, whatever it points at.
        assert_eq!(of_kind(&docs, NodeKind::DefinedName)[0].cells, 0);
    }

    #[test]
    fn a_formula_group_takes_its_context_from_its_region_not_its_column() {
        // `parents` follows CONTAINS only. A formula group is contained by its
        // region and *also* headed by a column, and taking the header as the
        // parent would make the ancestry depend on which edge came first.
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        let groups = of_kind(&docs, NodeKind::FormulaGroup);
        assert!(
            !groups.is_empty(),
            "the fixture should have a formula group"
        );

        for group in groups {
            let parent = parents(&built.graph)
                .get(&NodeIndex::new(group.node as usize))
                .map(|&p| built.graph[p].kind());
            assert_eq!(
                parent,
                Some(NodeKind::Region),
                "{} is parented by {parent:?}",
                group.label
            );
        }
    }

    #[test]
    fn a_citation_falls_back_rather_than_being_dropped() {
        // A range whose sheet has no node in the graph still has to cite
        // something: silently dropping the citation would leave a hit that
        // cannot be traced back to a cell.
        let sheets = FxHashMap::default();
        let range = RangeRef::new(SheetId(7), 0, 0, 2, 2);
        let cited = cite(range, &sheets);
        // `#7` rather than a bare `7`, so the fallback can never be misread as
        // a sheet actually named 7.
        assert!(cited.starts_with("#7!"), "cited as {cited}");
        assert!(cited.contains(&range.to_a1()));
    }

    #[test]
    fn every_node_becomes_exactly_one_document_at_its_own_index() {
        let built = build(&sample());
        let docs = docs_for(&built.graph);
        assert_eq!(docs.len(), built.graph.node_count());
        for (i, doc) in docs.iter().enumerate() {
            assert_eq!(doc.node as usize, i);
            assert_eq!(doc.kind, built.graph[NodeIndex::new(i)].kind());
        }
    }
}
