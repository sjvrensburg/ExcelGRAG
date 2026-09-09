//! What the frontend sees.
//!
//! The engine's types are the honest shapes, but they are not a web API: a
//! petgraph `NodeIndex` is meaningless without the graph it came from, node
//! payloads are externally-tagged enums, and `Search`/`Retrieved` grew for
//! agents, not browsers. Everything crossing the wire is restated here, in
//! one direction only, with node identity spelled `id` and every enum a
//! string the frontend can key on. The day a field here disagrees with the
//! engine, the fix is in this file.

use std::collections::HashMap;

use eg_graph::node::{EdgeKind, Node};
use eg_graph::store::{Entry, StoredGraph};
use eg_model::{RangeRef, SheetId};
use eg_retrieve::{Retrieved, Role, Search};
use petgraph::visit::EdgeRef;
use serde::Serialize;

use crate::app::App;

/// One workbook as the corpus lists it.
#[derive(Serialize, Clone)]
pub struct WorkbookDto {
    pub hash: String,
    pub path: String,
    pub sheets: usize,
    pub cells: u64,
    pub nodes: u64,
    pub edges: u64,
    pub formula_group_nodes: bool,
    pub profiled_columns: u64,
    pub profile_values: bool,
}

impl WorkbookDto {
    pub fn list(app: &App) -> Vec<WorkbookDto> {
        let state = app.engine();
        state
            .corpus
            .entries()
            .map(|(hash, entry)| WorkbookDto::new(hash, entry))
            .collect()
    }

    pub fn new(hash: &str, entry: &Entry) -> WorkbookDto {
        WorkbookDto {
            hash: hash.to_string(),
            path: entry.path.clone(),
            sheets: entry.sheets,
            cells: entry.cells,
            nodes: entry.nodes,
            edges: entry.edges,
            formula_group_nodes: entry.formula_group_nodes,
            profiled_columns: entry.profiled_columns,
            profile_values: entry.profile_values,
        }
    }
}

/// A whole stored graph, flattened into nodes and edges the client can lay
/// out. Node ids are petgraph indices; they are stable for as long as the
/// content hash is, which is as long as the corpus would serve this file.
#[derive(Serialize)]
pub struct GraphDto {
    pub hash: String,
    pub path: String,
    pub root: u32,
    pub formula_groups: bool,
    pub nodes: Vec<NodeDto>,
    pub edges: Vec<EdgeDto>,
    /// Node count per kind, for filters that start sensible.
    pub node_kinds: HashMap<String, u64>,
    pub edge_kinds: HashMap<String, u64>,
    /// The sheet layer, which is also the filter-by-sheet list.
    pub sheets: Vec<SheetDto>,
}

#[derive(Serialize, Clone)]
pub struct NodeDto {
    pub id: u32,
    pub kind: String,
    pub label: String,
    /// A fully-qualified citation, for the kinds that cover a rectangle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1: Option<String>,
    /// The sheet this node lives on, by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Cells covered, for the kinds that cover cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cells: Option<u64>,
    /// The node whose `CONTAINS` edge points here, when there is exactly one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<u32>,
    /// The node payload as stored, untouched. The flattened fields above are
    /// what the views read; this is for the details panel that wants
    /// everything.
    pub data: serde_json::Value,
}

#[derive(Serialize)]
pub struct EdgeDto {
    pub source: u32,
    pub target: u32,
    pub kind: String,
    /// How many cell references stand behind this edge. Structural edges
    /// carry 1; a lifted dependency carries its count, which is what makes it
    /// rankable.
    pub weight: u64,
}

#[derive(Serialize)]
pub struct SheetDto {
    pub node: u32,
    pub name: String,
    pub visible: bool,
    pub cells: u64,
    pub formula_cells: u64,
}

pub fn graph_dto(stored: &StoredGraph) -> GraphDto {
    let graph = &stored.graph;

    // Sheet names by id, so every ranged node can cite itself the way
    // `eg` does: `'Q3 Sales'!B2:D40`, sheet and all.
    let mut sheet_names: HashMap<SheetId, String> = HashMap::new();
    let mut sheets = Vec::new();
    for index in graph.node_indices() {
        if let Node::Sheet(sheet) = &graph[index] {
            sheet_names.insert(sheet.id, sheet.name.clone());
            sheets.push(SheetDto {
                node: index.index() as u32,
                name: sheet.name.clone(),
                visible: sheet.visible,
                cells: sheet.cells,
                formula_cells: sheet.formula_cells,
            });
        }
    }
    sheets.sort_by(|a, b| a.name.cmp(&b.name));

    let cite = |range: RangeRef| match sheet_names.get(&range.sheet) {
        Some(name) => range.to_a1_with_sheet(name),
        None => format!("{}!{}", range.sheet, range.to_a1()),
    };

    // Containment parents first, so node flattening can point at them.
    let mut parents: HashMap<u32, u32> = HashMap::new();
    for edge in graph.edge_references() {
        if edge.weight().kind == EdgeKind::Contains {
            parents
                .entry(edge.target().index() as u32)
                .or_insert(edge.source().index() as u32);
        }
    }

    let mut node_kinds: HashMap<String, u64> = HashMap::new();
    let nodes = graph
        .node_indices()
        .map(|index| {
            let node = &graph[index];
            *node_kinds
                .entry(node.kind().as_str().to_string())
                .or_default() += 1;
            NodeDto {
                id: index.index() as u32,
                kind: node.kind().as_str().to_string(),
                label: node.label(),
                a1: node.range().map(&cite),
                sheet: node.sheet().and_then(|id| sheet_names.get(&id).cloned()),
                cells: match node {
                    Node::Sheet(s) => Some(s.cells),
                    Node::Region(r) => Some(r.cell_count),
                    Node::FormulaGroup(g) => Some(g.cell_count),
                    _ => None,
                },
                parent: parents.get(&(index.index() as u32)).copied(),
                // Every node payload is plain data by construction; a failure
                // here is a bug in a `Serialize` impl, not bad input.
                data: serde_json::to_value(node).expect("a node serialises"),
            }
        })
        .collect();

    let mut edge_kinds: HashMap<String, u64> = HashMap::new();
    let edges = graph
        .edge_references()
        .map(|edge| {
            let weight = edge.weight();
            *edge_kinds
                .entry(weight.kind.as_str().to_string())
                .or_default() += 1;
            EdgeDto {
                source: edge.source().index() as u32,
                target: edge.target().index() as u32,
                kind: weight.kind.as_str().to_string(),
                weight: weight.weight,
            }
        })
        .collect();

    GraphDto {
        hash: stored.content_hash.clone(),
        path: stored.path.clone(),
        root: stored.root,
        formula_groups: stored.formula_group_nodes,
        nodes,
        edges,
        node_kinds,
        edge_kinds,
        sheets,
    }
}

/// One node and everything hanging off it, for the details panel.
#[derive(Serialize)]
pub struct NodeDetailDto {
    pub node: NodeDto,
    pub neighbors: Vec<NeighborDto>,
}

#[derive(Serialize)]
pub struct NeighborDto {
    /// `out` — an edge from the node; `in` — an edge to it.
    pub direction: &'static str,
    pub edge_kind: String,
    pub weight: u64,
    pub id: u32,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1: Option<String>,
}

pub fn node_detail_dto(
    stored: &StoredGraph,
    id: u32,
    graph_dto: &GraphDto,
) -> Option<NodeDetailDto> {
    let graph = &stored.graph;
    let index = petgraph::graph::NodeIndex::new(id as usize);
    if index.index() >= graph.node_count() {
        return None;
    }
    // The flattened node out of the DTO walk, so details agree with the map.
    let node = graph_dto
        .nodes
        .iter()
        .find(|n| n.id == id)
        .cloned()
        .expect("the graph DTO walked every node");

    let mut neighbors = Vec::new();
    for edge in graph.edges_directed(index, petgraph::Direction::Outgoing) {
        let other = &graph[edge.target()];
        neighbors.push(NeighborDto {
            direction: "out",
            edge_kind: edge.weight().kind.as_str().to_string(),
            weight: edge.weight().weight,
            id: edge.target().index() as u32,
            kind: other.kind().as_str().to_string(),
            label: other.label(),
            a1: other.range().map(|r| {
                match graph_dto
                    .nodes
                    .get(edge.target().index())
                    .and_then(|n| n.a1.clone())
                {
                    Some(a1) => a1,
                    None => r.to_a1(),
                }
            }),
        });
    }
    for edge in graph.edges_directed(index, petgraph::Direction::Incoming) {
        let other = &graph[edge.source()];
        neighbors.push(NeighborDto {
            direction: "in",
            edge_kind: edge.weight().kind.as_str().to_string(),
            weight: edge.weight().weight,
            id: edge.source().index() as u32,
            kind: other.kind().as_str().to_string(),
            label: other.label(),
            a1: graph_dto
                .nodes
                .get(edge.source().index())
                .and_then(|n| n.a1.clone()),
        });
    }
    Some(NodeDetailDto { node, neighbors })
}

/// A search result, flattened from `eg_index::text::Hit`.
#[derive(Serialize)]
pub struct HitDto {
    pub score: f32,
    pub workbook: String,
    pub node: u32,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1: Option<String>,
}

#[derive(Serialize)]
pub struct SearchDto {
    pub hits: Vec<HitDto>,
    pub verdict: String,
    pub evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    pub matched: Vec<String>,
    pub unmatched: Vec<String>,
    pub both_halves: bool,
}

pub fn search_dto(found: &Search) -> SearchDto {
    SearchDto {
        hits: found
            .hits
            .iter()
            .map(|hit| HitDto {
                score: hit.score,
                workbook: hit.workbook.clone(),
                node: hit.node,
                kind: hit.kind.as_str().to_string(),
                sheet: hit.sheet.clone(),
                label: hit.label.clone(),
                a1: hit.a1.clone(),
            })
            .collect(),
        verdict: found.verdict().as_str().to_string(),
        evidence: found.evidence(),
        warning: found.warning().map(|w| w.to_string()),
        matched: found.matched.clone(),
        unmatched: found.unmatched.clone(),
        both_halves: found.both_halves,
    }
}

/// One node an `ask` brought back, with the role it played.
#[derive(Serialize)]
pub struct RetrievedNodeDto {
    pub node: u32,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// `seed`, `contains`, `within`, `feeds`, or `reads`.
    pub role: String,
    /// The node this role is relative to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weight: Option<u64>,
    pub hops: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Serialize)]
pub struct RetrievedWorkbookDto {
    pub hash: String,
    pub path: String,
    pub nodes: Vec<RetrievedNodeDto>,
    pub truncated: bool,
}

pub fn retrieved_dto(found: &Retrieved) -> Vec<RetrievedWorkbookDto> {
    found
        .workbooks
        .iter()
        .map(|workbook| RetrievedWorkbookDto {
            hash: workbook.content_hash.clone(),
            path: workbook.path.clone(),
            nodes: workbook
                .nodes
                .iter()
                .map(|node| {
                    let (via, edge_kind, weight) = match &node.role {
                        Role::Ancestor { of } | Role::Child { of } => (Some(*of), None, None),
                        Role::Input { of, kind, weight } => {
                            (Some(*of), Some(kind.as_str().to_string()), Some(*weight))
                        }
                        Role::Dependent { on, kind, weight } => {
                            (Some(*on), Some(kind.as_str().to_string()), Some(*weight))
                        }
                        Role::Seed => (None, None, None),
                    };
                    RetrievedNodeDto {
                        node: node.node,
                        kind: node.kind.as_str().to_string(),
                        label: node.label.clone(),
                        a1: node.a1.clone(),
                        sheet: node.sheet.clone(),
                        role: node.role.as_str().to_string(),
                        via,
                        edge_kind,
                        weight,
                        hops: node.hops,
                        score: node.score,
                    }
                })
                .collect(),
            truncated: workbook.truncated,
        })
        .collect()
}

/// Who spoke a chat turn: the human in the browser, or an agent calling the
/// `chat` MCP tool. This is what makes the log visibly shared rather than
/// two logs interleaved by accident.
#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnSourceDto {
    Human,
    Agent,
}

/// One turn of a chat session, as the wire sees it. Deliberately the same
/// shape regardless of which LLM privacy tier produced it — `answer` is
/// either the LLM's composed reply or, with no LLM configured, the rendered
/// passage itself; it never carries a cell value beyond what the passage
/// already carries (none, by `render()`'s own invariant).
#[derive(Serialize, Clone)]
pub struct ChatTurnDto {
    pub id: u64,
    pub source: TurnSourceDto,
    /// What was asked, verbatim.
    pub message: String,
    /// The standalone query actually searched, if an LLM resolved a
    /// follow-up ("what about that one") into one. Absent when no LLM ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_query: Option<String>,
    pub evidence: String,
    pub citations: Vec<String>,
    pub answer: String,
    pub timestamp: u64,
}

/// What the WebSocket sends. One JSON object per message, tagged by `type`.
#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    /// Sent once, on connect.
    Hello {
        dir: String,
        redact_values: bool,
        workbooks: Vec<WorkbookDto>,
    },
    /// The corpus changed on disk — possibly underneath an open graph.
    Corpus {
        added: Vec<String>,
        removed: Vec<String>,
        /// Same hash, rewritten file: a workbook re-indexed.
        changed: Vec<String>,
        workbooks: Vec<WorkbookDto>,
    },
    /// A line of progress from an index job.
    Log { line: String },
    /// An index job finished, one way or the other.
    IndexDone {
        path: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// A chat turn was appended — by a human's `POST /api/chat` or an
    /// agent's `chat` MCP tool call. Every open tab gets this, which is what
    /// makes the session shared rather than per-tab.
    ChatTurn {
        session_id: String,
        turn: ChatTurnDto,
    },
    /// An agent asked the GUI to point the camera at a node, without running
    /// a full chat turn (it already knows exactly what to show).
    Navigate {
        session_id: String,
        node: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        workbook: Option<String>,
    },
}
