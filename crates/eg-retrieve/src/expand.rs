//! Walking out from a hit to the nodes that explain it.
//!
//! A hit is a door, not an answer. "The Revenue column of BP136" is the right
//! node and still not enough to act on: which table is it in, what feeds it,
//! and what breaks if it is wrong. Expansion is what turns one node into that.
//!
//! # Why the two edge classes are not walked the same way
//!
//! The measurement decides this. On the reference workbook the graph is 732
//! nodes and 892 edges, and the dependency layer is **161 edges** — 66
//! `DEPENDS_ON` and 95 `CROSS_SHEET_REF`, with a maximum in-degree of 13. That
//! is sparse, and walking it is cheap.
//!
//! Every hub in that graph is structural. The most connected nodes have
//! out-degrees of 136, 83, 82, 74 and 71, and every one of them is a region
//! pointing at its own columns. So a plain k-hop walk from a column reaches its
//! region in one hop and that region's 136 columns in two — 19% of the whole
//! workbook, none of it asked for. This is exactly the explosion
//! [`eg_graph::degree_stats`] was written to detect, and it lives entirely in
//! `CONTAINS`.
//!
//! So containment is followed *inwards* and dependencies *outwards*:
//!
//! - **Up the containment tree**, always, and unbounded — a column's region,
//!   its sheet, the workbook. That is a path of at most three, and it is what
//!   makes a node nameable: "the Revenue column of Q3 Sales".
//! - **Down the containment tree**, never by default. A region's columns are
//!   what the index just ranked; the ones that matter are already seeds, and
//!   pulling all 136 in would bury them. [`ExpandOptions::children`] opens it
//!   with a cap for callers who want the shape of a table.
//! - **Along dependencies**, both directions, k hops, under a node budget:
//!   nearest first, and heaviest within a distance. Weight is the number of
//!   cell references behind the edge, so the heaviest is what most of the
//!   workbook actually rests on — but taking weight before distance loses
//!   nodes, as [`Step`]'s ordering explains.
//!
//! # Dependencies hang off regions, so ancestors are walked from too
//!
//! [`eg_graph::build`] lifts every cell reference to the *region* containing
//! it, which means the dependency layer connects regions and nothing else. A
//! column node has no dependency edges at all; neither does a sheet. So a
//! column seed whose own edges were the only ones followed would come back with
//! its ancestry and nothing else — which is what the first version of this did.
//!
//! Every node brought in therefore contributes its dependencies, and so does
//! everything above it. Walking up costs no hop, so a column reaches its
//! table's dependencies at hop one, which is the right reading: the question
//! "what feeds this column" is answered at the granularity the graph kept.
//!
//! A sheet has the same problem in the other direction — its regions carry the
//! edges, and they are below it. So the walk also looks one containment level
//! down, and takes any child that actually carries a dependency. That does not
//! reopen the flood: the children of a region are columns, and a column carries
//! nothing, so a wide table still contributes only the columns asked for. It is
//! also why a workbook root is a poor seed — one level down is sheets, which
//! carry nothing either.
//!
//! That descent follows the nodes the caller asked about — the seeds, and what
//! a dependency reached — and not the ancestry above them. A sheet that is only
//! there to name a region has other regions under it, and they are siblings of
//! the answer rather than context for it. On the reference workbook, seeding a
//! region of SUMMARY and descending from SUMMARY spent an entire budget on the
//! seven other tables of that sheet before a single dependency was followed.
//!
//! That granularity is the honest caveat. A column's inputs are its table's
//! inputs, because lifting threw away which cell fed which — deliberately, and
//! recovering it is P6's job against the workbook itself.

use std::collections::BinaryHeap;

use eg_graph::store::{Corpus, StoredGraph};
use eg_graph::{EdgeKind, Graph, Node, NodeKind};
use eg_index::Hit;
use eg_model::{RangeRef, SheetId};
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum RetrieveError {
    #[error("reading the corpus: {0}")]
    Store(#[from] eg_graph::StoreError),
}

/// How far to walk, and how much to bring back.
#[derive(Debug, Clone)]
pub struct ExpandOptions {
    /// Dependency hops from a seed. Two is the useful default: one hop is what
    /// a node reads, two is what that reads in turn, which is where "the number
    /// is wrong because the rate table is stale" becomes visible.
    pub hops: usize,
    /// The most nodes to return per workbook, ancestry included. A cap and not
    /// a target: a well-aimed query on a sparse workbook returns far fewer.
    pub budget: usize,
    /// The most contained children to pull in, ranked by the cells they cover.
    /// Zero by default — see the module note on why this direction is not free.
    ///
    /// Applies to every node in the result except the workbook root, so a
    /// column seed shows the other columns of its table, which is the useful
    /// case. The root is excluded because its children are every sheet in the
    /// file and there is no ranking among them worth taking four of.
    pub children: usize,
    /// Ignore dependency edges standing for fewer than this many references.
    /// One keeps everything, which is the honest default: a single hand-written
    /// reference into another sheet is often the whole finding.
    pub min_weight: u64,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        ExpandOptions {
            hops: 2,
            budget: 40,
            children: 0,
            min_weight: 1,
        }
    }
}

/// Why a node is in the result.
///
/// Kept per node, because an expansion nobody can check is an expansion nobody
/// should trust. Every node that is not a seed names the node that pulled it in
/// and the edge that did it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    /// The index returned it.
    Seed,
    /// It contains `of`, directly or transitively.
    Ancestor { of: u32 },
    /// It is contained by `of`.
    Child { of: u32 },
    /// `of` reads it: it is an input to `of`.
    Input {
        of: u32,
        kind: EdgeKind,
        weight: u64,
    },
    /// It reads `on`: it is a dependent of `on`.
    Dependent {
        on: u32,
        kind: EdgeKind,
        weight: u64,
    },
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Seed => "seed",
            Role::Ancestor { .. } => "contains",
            Role::Child { .. } => "within",
            Role::Input { .. } => "feeds",
            Role::Dependent { .. } => "reads",
        }
    }

    /// The node this one was reached from, if it was reached from one.
    pub fn from(&self) -> Option<u32> {
        match self {
            Role::Seed => None,
            Role::Ancestor { of } | Role::Child { of } | Role::Input { of, .. } => Some(*of),
            Role::Dependent { on, .. } => Some(*on),
        }
    }
}

/// One node of an expansion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievedNode {
    pub node: u32,
    pub kind: NodeKind,
    pub label: String,
    /// A fully-qualified citation, for the kinds that cover a rectangle.
    pub a1: Option<String>,
    pub sheet: Option<String>,
    /// The node containing this one, if it is in the result too.
    ///
    /// Structural, and kept apart from `role` on purpose. A node reached by two
    /// different children records one of them in its role — whichever arrived
    /// first — so a reader rebuilding the containment path from roles alone
    /// gets it right for that child and truncated for every other.
    pub parent: Option<u32>,
    pub role: Role,
    /// Dependency hops from the nearest seed, and it is the nearest: the walk
    /// takes every node at distance *n* before any at *n+1*. Ancestry does not
    /// count as a hop — naming a node is not travelling away from it.
    pub hops: usize,
    /// The seed's search score, carried on the seed only.
    pub score: Option<f32>,
}

impl RetrievedNode {
    pub fn is_seed(&self) -> bool {
        self.role == Role::Seed
    }
}

/// What one workbook contributed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbookContext {
    pub content_hash: String,
    pub path: String,
    pub nodes: Vec<RetrievedNode>,
    /// Whether the budget stopped the walk before it ran out of graph. Reported
    /// rather than hidden: a truncated expansion is a partial answer, and a
    /// caller that cannot tell will present it as a complete one.
    pub truncated: bool,
}

impl WorkbookContext {
    pub fn seeds(&self) -> impl Iterator<Item = &RetrievedNode> {
        self.nodes.iter().filter(|n| n.role == Role::Seed)
    }

    pub fn node(&self, index: u32) -> Option<&RetrievedNode> {
        self.nodes.iter().find(|n| n.node == index)
    }

    /// The containment path above a node, outermost first.
    ///
    /// Only the part of it that is in the result: a budget that stopped before
    /// the workbook root gives a shorter path, not a wrong one.
    pub fn ancestry(&self, of: u32) -> Vec<&RetrievedNode> {
        let mut path = Vec::new();
        let mut at = self.node(of).and_then(|n| n.parent);
        while let Some(index) = at {
            let Some(node) = self.node(index) else { break };
            // A containment cycle cannot happen in a graph built by eg-graph,
            // but this walks data read off disk. A path can visit each node at
            // most once, so reaching that many is already proof of a cycle —
            // the earlier `>` let it return one node more than the result holds.
            if path.len() >= self.nodes.len() {
                break;
            }
            path.push(node);
            at = node.parent;
        }
        path.reverse();
        path
    }
}

/// An expansion, grouped by the workbook each node came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Retrieved {
    pub workbooks: Vec<WorkbookContext>,
    /// Seeds whose workbook is no longer in the corpus. Never silently dropped:
    /// an index that outlived its graphs is a real condition, and the fix is to
    /// reindex rather than to return fewer results without saying so.
    pub missing_workbooks: Vec<String>,
}

impl Retrieved {
    pub fn total_nodes(&self) -> usize {
        self.workbooks.iter().map(|w| w.nodes.len()).sum()
    }

    pub fn truncated(&self) -> bool {
        self.workbooks.iter().any(|w| w.truncated)
    }
}

/// Expand a ranked list of hits into the context around them.
///
/// Hits are grouped by workbook, so each stored graph is read once however many
/// hits landed in it, and the order of the workbooks follows the best hit in
/// each.
pub fn expand(
    corpus: &Corpus,
    seeds: &[Hit],
    opts: &ExpandOptions,
) -> Result<Retrieved, RetrieveError> {
    let mut order: Vec<&str> = Vec::new();
    let mut grouped: FxHashMap<&str, Vec<&Hit>> = FxHashMap::default();
    for hit in seeds {
        let key = hit.workbook.as_str();
        if !grouped.contains_key(key) {
            order.push(key);
        }
        grouped.entry(key).or_default().push(hit);
    }

    let mut out = Retrieved::default();
    for hash in order {
        let hits = &grouped[hash];
        match corpus.get(hash)? {
            Some(stored) => out.workbooks.push(expand_one(&stored, hits, opts)),
            None => out.missing_workbooks.push(hash.to_string()),
        }
    }
    Ok(out)
}

/// What an expansion has collected so far.
///
/// Bundled because every step of the walk needs all of it, and threading six
/// parameters through each one made the signatures longer than the bodies.
struct Collected {
    budget: usize,
    taken: FxHashSet<NodeIndex>,
    nodes: Vec<RetrievedNode>,
}

impl Collected {
    fn full(&self) -> bool {
        self.nodes.len() >= self.budget
    }

    /// Add a node unless it is already there or the budget is spent. Returns
    /// whether it was added, and `full` distinguishes the two refusals.
    fn add(
        &mut self,
        graph: &Graph,
        idx: NodeIndex,
        sheets: &FxHashMap<SheetId, String>,
        role: Role,
        hops: usize,
        score: Option<f32>,
    ) -> bool {
        if self.taken.contains(&idx) || self.full() {
            return false;
        }
        self.taken.insert(idx);
        self.nodes
            .push(describe(graph, idx, sheets, role, hops, score));
        true
    }
}

/// One dependency step waiting to be taken, ordered by the weight of the edge
/// that would take it.
struct Step {
    weight: u64,
    hops: usize,
    from: NodeIndex,
    to: NodeIndex,
    kind: EdgeKind,
    /// Whether `to` is read by `from`, rather than the other way round.
    inbound: bool,
}

impl Ord for Step {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Nearest first, and heaviest within a distance.
        //
        // Weight-first looks like the right priority and quietly loses nodes.
        // With a heavy S→A, a light S→C and a middling A→C, weight order takes
        // C through A and records it at two hops; the one-hop step to C is then
        // dropped as already-taken, and because C arrived at the hop limit its
        // own edges were never queued. Everything past C disappears, and the
        // `hops` recorded is not the distance to the nearest seed either.
        //
        // Ordering by distance first makes every node arrive at its shortest
        // one, which is what the field claims and what the hop limit means.
        other
            .hops
            .cmp(&self.hops)
            .then_with(|| self.weight.cmp(&other.weight))
            .then_with(|| other.to.index().cmp(&self.to.index()))
    }
}
impl PartialOrd for Step {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Eq for Step {}
impl PartialEq for Step {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

fn expand_one(stored: &StoredGraph, hits: &[&Hit], opts: &ExpandOptions) -> WorkbookContext {
    let graph = &stored.graph;
    let sheets = sheet_names(graph);
    let mut got = Collected {
        budget: opts.budget.max(1),
        taken: FxHashSet::default(),
        nodes: Vec::new(),
    };
    // Nodes whose own edges have already been queued, so a node reached twice
    // does not queue its neighbours twice.
    let mut walked: FxHashSet<NodeIndex> = FxHashSet::default();
    let mut queue: BinaryHeap<Step> = BinaryHeap::new();
    // The flag says whether the caller asked about this node, or whether it is
    // context added to name one. Only the former is descended from.
    let mut pending: Vec<(NodeIndex, usize, bool)> = Vec::new();
    let mut truncated = false;

    // Seeds first, in the order the index ranked them, so a budget that runs
    // out never costs a better-ranked hit for a worse one.
    for hit in hits {
        let Some(idx) = valid_index(graph, hit.node) else {
            continue;
        };
        if got.full() {
            truncated = true;
            break;
        }
        if got.add(graph, idx, &sheets, Role::Seed, 0, Some(hit.score)) {
            pending.push((idx, 0, true));
        }
    }

    loop {
        // Everything newly included gets named, and everything that names it
        // becomes somewhere to walk from.
        while let Some((idx, hops, asked_about)) = pending.pop() {
            if walked.insert(idx) && hops < opts.hops {
                push_dependencies(graph, idx, hops, opts, &mut queue);
                let descend = if asked_about {
                    dependency_carrying_children(graph, idx, opts)
                } else {
                    Vec::new()
                };
                // The dependency layer is region-granular, so a sheet's edges
                // are its regions'. Only children that carry one are taken, and
                // they are walked from in turn.
                for child in descend {
                    if got.taken.contains(&child) {
                        continue;
                    }
                    if got.full() {
                        truncated = true;
                        break;
                    }
                    let role = Role::Child {
                        of: idx.index() as u32,
                    };
                    if got.add(graph, child, &sheets, role, hops, None) {
                        pending.push((child, hops, true));
                    }
                }
            }

            let (added, stopped) = add_ancestry(graph, idx, hops, &sheets, &mut got);
            pending.extend(added.into_iter().map(|a| (a, hops, false)));
            truncated |= stopped;

            if opts.children > 0 {
                truncated |= !add_children(graph, idx, hops, opts, &sheets, &mut got);
            }
        }

        let Some(step) = queue.pop() else { break };
        if got.taken.contains(&step.to) {
            continue;
        }
        if got.full() {
            truncated = true;
            break;
        }

        let from = step.from.index() as u32;
        let role = if step.inbound {
            Role::Dependent {
                on: from,
                kind: step.kind,
                weight: step.weight,
            }
        } else {
            Role::Input {
                of: from,
                kind: step.kind,
                weight: step.weight,
            }
        };
        if got.add(graph, step.to, &sheets, role, step.hops, None) {
            pending.push((step.to, step.hops, true));
        }
    }

    WorkbookContext {
        content_hash: stored.content_hash.clone(),
        path: stored.path.clone(),
        nodes: got.nodes,
        truncated,
    }
}

/// Queue every dependency edge at a node, in both directions.
fn push_dependencies(
    graph: &Graph,
    at: NodeIndex,
    hops: usize,
    opts: &ExpandOptions,
    queue: &mut BinaryHeap<Step>,
) {
    for (direction, inbound) in [(Direction::Outgoing, false), (Direction::Incoming, true)] {
        for edge in graph.edges_directed(at, direction) {
            let weight = edge.weight();
            if weight.kind.is_structural() || weight.weight < opts.min_weight {
                continue;
            }
            let to = if inbound {
                edge.source()
            } else {
                edge.target()
            };
            queue.push(Step {
                weight: weight.weight,
                hops: hops + 1,
                from: at,
                to,
                kind: weight.kind,
                inbound,
            });
        }
    }
}

/// Walk up the containment tree, adding whatever is not already there.
///
/// Returns the nodes it added and whether the budget stopped it partway.
/// Unbounded in depth on purpose: the tree is workbook → sheet → region →
/// column, so this is at most three steps, and stopping halfway would leave a
/// column whose table is unnamed.
fn add_ancestry(
    graph: &Graph,
    from: NodeIndex,
    hops: usize,
    sheets: &FxHashMap<SheetId, String>,
    got: &mut Collected,
) -> (Vec<NodeIndex>, bool) {
    let mut added = Vec::new();
    let mut child = from;
    // A containment cycle cannot happen in a graph eg-graph built, but this
    // walks one read off disk, and a corrupt file should give a short answer
    // rather than a hung process. `WorkbookContext::ancestry` guards the same
    // walk; this is the other place that does it.
    let mut steps = 0;
    while let Some(parent) = containing(graph, child) {
        steps += 1;
        if steps > graph.node_count() {
            return (added, false);
        }
        if got.taken.contains(&parent) {
            child = parent;
            continue;
        }
        if got.full() {
            return (added, true);
        }
        let role = Role::Ancestor {
            of: child.index() as u32,
        };
        got.add(graph, parent, sheets, role, hops, None);
        added.push(parent);
        child = parent;
    }
    (added, false)
}

/// The children of a node that carry a dependency edge of their own.
///
/// Empty for a region, whose children are columns, and empty for a workbook,
/// whose children are sheets — so this reaches regions from a sheet and nothing
/// else, which is exactly the level the edges live at.
fn dependency_carrying_children(
    graph: &Graph,
    of: NodeIndex,
    opts: &ExpandOptions,
) -> Vec<NodeIndex> {
    graph
        .edges_directed(of, Direction::Outgoing)
        .filter(|e| e.weight().kind == EdgeKind::Contains)
        .map(|e| e.target())
        .filter(|&child| carries_dependency(graph, child, opts))
        .collect()
}

fn carries_dependency(graph: &Graph, node: NodeIndex, opts: &ExpandOptions) -> bool {
    [Direction::Outgoing, Direction::Incoming]
        .into_iter()
        .flat_map(|d| graph.edges_directed(node, d))
        .any(|e| !e.weight().kind.is_structural() && e.weight().weight >= opts.min_weight)
}

/// Add the largest contained children of a node, up to the cap.
///
/// Ranked by the cells they cover, because a table's widest columns are the
/// ones a reader means when they ask what is in it, and an alphabetical or
/// insertion order would be an arbitrary slice of 136.
///
/// Returns false if the budget stopped it. Running out of children is not
/// truncation: that is the cap doing its job.
fn add_children(
    graph: &Graph,
    of: NodeIndex,
    hops: usize,
    opts: &ExpandOptions,
    sheets: &FxHashMap<SheetId, String>,
    got: &mut Collected,
) -> bool {
    // Enumerating the root means listing the file. Whatever cap is put on that
    // is an arbitrary slice of every sheet there is, in build order.
    if matches!(graph[of], Node::Workbook(_)) {
        return true;
    }
    let mut children: Vec<(u64, NodeIndex)> = graph
        .edges_directed(of, Direction::Outgoing)
        .filter(|e| e.weight().kind == EdgeKind::Contains)
        .map(|e| (cells(&graph[e.target()]), e.target()))
        .filter(|(_, idx)| !got.taken.contains(idx))
        .collect();
    children.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.index().cmp(&b.1.index())));

    for &(_, idx) in children.iter().take(opts.children) {
        if got.full() {
            return false;
        }
        let role = Role::Child {
            of: of.index() as u32,
        };
        got.add(graph, idx, sheets, role, hops, None);
    }
    true
}

/// The node containing this one, following `CONTAINS` only.
fn containing(graph: &Graph, node: NodeIndex) -> Option<NodeIndex> {
    graph
        .edges_directed(node, Direction::Incoming)
        .find(|e| e.weight().kind == EdgeKind::Contains)
        .map(|e| e.source())
}

fn cells(node: &Node) -> u64 {
    match node {
        Node::Sheet(s) => s.cells,
        Node::Region(r) => r.cell_count,
        Node::Column(c) => u64::from(c.range.rows()),
        Node::FormulaGroup(g) => g.cell_count,
        Node::Workbook(_) | Node::DefinedName(_) | Node::ExternalWorkbook(_) => 0,
    }
}

/// A node index from an index hit, checked against the graph it claims to be
/// from. A stale index can name a node that no longer exists, and a raw
/// petgraph index means nothing except against the graph it came from.
fn valid_index(graph: &Graph, node: u32) -> Option<NodeIndex> {
    let idx = NodeIndex::new(node as usize);
    (idx.index() < graph.node_count()).then_some(idx)
}

fn describe(
    graph: &Graph,
    idx: NodeIndex,
    sheets: &FxHashMap<SheetId, String>,
    role: Role,
    hops: usize,
    score: Option<f32>,
) -> RetrievedNode {
    let node = &graph[idx];
    RetrievedNode {
        node: idx.index() as u32,
        kind: node.kind(),
        label: node.label(),
        a1: node.range().map(|r| cite(r, sheets)),
        sheet: node.sheet().and_then(|id| sheets.get(&id).cloned()),
        parent: containing(graph, idx).map(|p| p.index() as u32),
        role,
        hops,
        score,
    }
}

fn sheet_names(graph: &Graph) -> FxHashMap<SheetId, String> {
    graph
        .node_weights()
        .filter_map(|n| match n {
            Node::Sheet(s) => Some((s.id, s.name.clone())),
            _ => None,
        })
        .collect()
}

fn cite(range: RangeRef, sheets: &FxHashMap<SheetId, String>) -> String {
    match sheets.get(&range.sheet) {
        Some(name) => range.to_a1_with_sheet(name),
        None => format!("{}!{}", range.sheet, range.to_a1()),
    }
}
