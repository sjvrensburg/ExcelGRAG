import type { Selection } from "../graph/GraphView";
import { EDGE_DIRECTION_NOTE } from "../graph/theme";
import type { EdgeDto, GraphDto, NodeDetailDto, NodeDto } from "../types";

interface Props {
  graph: GraphDto;
  selection: Selection;
  // Present only for a node selection, and only once the fetch resolves —
  // `App` guards this against a stale response for a selection the user has
  // since abandoned.
  nodeDetail: NodeDetailDto | null;
  onSelectNode: (id: number) => void;
  onSelectEdge: (edge: EdgeDto) => void;
  onClose: () => void;
  onAskAboutSelection: () => void;
}

// One inspector for whatever is selected — a node (full facts + neighbors,
// fetched) or an edge (source/relation/target, entirely derivable from the
// already-loaded graph, so it needs no request and works with no LLM). Every
// relationship is grouped by what it actually means, not by raw direction:
// CONTAINS/HEADER_OF describe structure, DEPENDS_ON-family edges describe
// what reads what, and blurring the two under "read by" is what made a
// containing sheet or a formula group's own column read as if it were a
// dependency.
export function DetailsPanel({
  graph,
  selection,
  nodeDetail,
  onSelectNode,
  onSelectEdge,
  onClose,
  onAskAboutSelection,
}: Props) {
  if (selection.entity === "edge") {
    const edge = graph.edges.find((e) => e.id === selection.id);
    if (!edge) return null;
    return (
      <EdgeDetails
        graph={graph}
        edge={edge}
        onSelectNode={onSelectNode}
        onClose={onClose}
        onAskAboutSelection={onAskAboutSelection}
      />
    );
  }

  if (!nodeDetail) {
    return (
      <aside className="details">
        <div className="details-head">
          <div className="details-title">loading…</div>
          <button className="close" onClick={onClose} aria-label="close" />
        </div>
      </aside>
    );
  }

  const { node, neighbors } = nodeDetail;
  const payload = flatten(node.data);
  const groups = groupNeighbors(node.kind, neighbors);
  const edgeIdFor = (row: NodeDetailDto["neighbors"][number]) => {
    const [a, b] = row.direction === "out" ? [node.id, row.id] : [row.id, node.id];
    return graph.edges.find((e) => e.source === a && e.target === b && e.kind === row.edge_kind)?.id;
  };

  return (
    <aside className="details">
      <div className="details-head">
        <div className="details-title" title={node.label}>
          {node.label}
        </div>
        <button className="close" onClick={onClose} aria-label="close" />
      </div>

      <div className="details-description">{describeNode(node)}</div>

      <button className="explain-button" onClick={onAskAboutSelection}>
        Ask about this
      </button>

      <div className="details-facts">
        <div className="fact">
          <span className="fact-key">kind</span>
          <span className="fact-value">{node.kind}</span>
        </div>
        {node.a1 && (
          <div className="fact">
            <span className="fact-key">range</span>
            <span className="fact-value mono">{node.a1}</span>
          </div>
        )}
        {node.sheet && (
          <div className="fact">
            <span className="fact-key">sheet</span>
            <span className="fact-value">{node.sheet}</span>
          </div>
        )}
        {node.cells !== undefined && node.cells > 0 && (
          <div className="fact">
            <span className="fact-key">cells</span>
            <span className="fact-value mono">{node.cells.toLocaleString("en-US")}</span>
          </div>
        )}
        {payload.map(([key, value]) => (
          <div className="fact" key={key}>
            <span className="fact-key">{key}</span>
            <span className="fact-value mono" title={value}>
              {value}
            </span>
          </div>
        ))}
      </div>

      {groups.map((group) => (
        <EdgeGroup
          key={group.title}
          title={group.title}
          rows={group.rows}
          onSelect={onSelectNode}
          edgeIdFor={edgeIdFor}
          onSelectEdge={(id) => {
            const edge = graph.edges.find((e) => e.id === id);
            if (edge) onSelectEdge(edge);
          }}
        />
      ))}
    </aside>
  );
}

function EdgeDetails({
  graph,
  edge,
  onSelectNode,
  onClose,
  onAskAboutSelection,
}: {
  graph: GraphDto;
  edge: EdgeDto;
  onSelectNode: (id: number) => void;
  onClose: () => void;
  onAskAboutSelection: () => void;
}) {
  const source = graph.nodes.find((n) => n.id === edge.source);
  const target = graph.nodes.find((n) => n.id === edge.target);
  const structural = edge.kind === "CONTAINS" || edge.kind === "HEADER_OF";

  return (
    <aside className="details">
      <div className="details-head">
        <div className="details-title">{edge.kind.replaceAll("_", " ").toLowerCase()}</div>
        <button className="close" onClick={onClose} aria-label="close" />
      </div>

      <div className="details-description">{describeEdge(edge, source, target)}</div>

      {!structural && <div className="details-note">{EDGE_DIRECTION_NOTE}</div>}

      <button className="explain-button" onClick={onAskAboutSelection}>
        Explain this connection
      </button>

      <div className="edge-endpoints">
        <button className="neighbor" onClick={() => source && onSelectNode(source.id)}>
          <span className="neighbor-edge">source</span>
          <span className="neighbor-label">{source?.label ?? edge.source}</span>
          {source?.a1 && <span className="neighbor-a1">{source.a1}</span>}
        </button>
        <button className="neighbor" onClick={() => target && onSelectNode(target.id)}>
          <span className="neighbor-edge">target</span>
          <span className="neighbor-label">{target?.label ?? edge.target}</span>
          {target?.a1 && <span className="neighbor-a1">{target.a1}</span>}
        </button>
      </div>

      <div className="details-facts">
        <div className="fact">
          <span className="fact-key">weight</span>
          <span className="fact-value mono">{edge.weight.toLocaleString("en-US")}</span>
        </div>
      </div>
      {!structural && (
        <div className="details-note">
          Weight is a reference count, not a confidence or an importance score —
          how many cell references this edge stands for, after identical
          references merged into one.
        </div>
      )}
    </aside>
  );
}

function describeNode(node: NodeDto): string {
  switch (node.kind) {
    case "workbook":
      return `The workbook root.`;
    case "sheet":
      return `The sheet ${node.label}.`;
    case "region":
      return `A table or block on ${node.sheet ?? "this sheet"}${node.a1 ? ` (${node.a1})` : ""}.`;
    case "column":
      return `The ${node.label} column${node.sheet ? ` of a table on ${node.sheet}` : ""}.`;
    case "formula group":
      return `A group of formula cells sharing one shape, represented by ${node.label}.`;
    case "defined name":
      return `The defined name ${node.label}${node.sheet ? ` (scoped to ${node.sheet})` : " (workbook scope)"}.`;
    case "external workbook":
      return `A reference to another workbook this corpus has not resolved (token ${node.label}).`;
    default:
      return node.label;
  }
}

function describeEdge(edge: EdgeDto, source: NodeDto | undefined, target: NodeDto | undefined): string {
  const s = source?.label ?? `node ${edge.source}`;
  const t = target?.label ?? `node ${edge.target}`;
  const times = edge.weight > 1 ? ` (aggregating ${edge.weight.toLocaleString("en-US")} references)` : "";
  switch (edge.kind) {
    case "CONTAINS":
      return `${s} contains ${t}.`;
    case "HEADER_OF":
      return `${s} heads the formula group ${t}.`;
    case "DEPENDS_ON":
      return `${s} reads ${t}, same sheet${times}.`;
    case "CROSS_SHEET_REF":
      return `${s} reads ${t} on another sheet${times}.`;
    case "CROSS_WORKBOOK_REF":
      return `${s} reads another workbook (${t})${times}.`;
    case "REFERENCES_NAME":
      return `${s} uses the defined name ${t}${times}.`;
    default:
      return `${s} → ${t}${times}.`;
  }
}

// The payload of a node is an externally-tagged enum: {"Region": {...}}.
// Flattened to key/value rows, it is exactly the details worth showing —
// minus the fields the facts above already carry, and the raw range, whose
// internal form adds nothing to the citation.
function flatten(data: Record<string, unknown>): [string, string][] {
  const inner = Object.values(data)[0];
  if (inner === null || typeof inner !== "object") return [];
  return Object.entries(inner as Record<string, unknown>)
    .filter(([key]) => key !== "range" && key !== "sheet")
    .map(([key, value]) => [
      key,
      value === null
        ? "none"
        : typeof value === "object"
          ? JSON.stringify(value)
          : String(value),
    ]);
}

interface Group {
  title: string;
  rows: NodeDetailDto["neighbors"];
}

// Direction alone ("in"/"out") conflates two different questions —
// structure ("what contains this / what does this contain") and dependency
// ("what does this read / what reads this") — under one misleading "read
// by" heading, regardless of edge kind. This groups by what the edge
// actually means instead.
function groupNeighbors(nodeKind: string, neighbors: NodeDetailDto["neighbors"]): Group[] {
  const contains = neighbors.filter((n) => n.edge_kind === "CONTAINS" && n.direction === "out");
  const containedIn = neighbors.filter((n) => n.edge_kind === "CONTAINS" && n.direction === "in");
  const heads = neighbors.filter((n) => n.edge_kind === "HEADER_OF" && n.direction === "out");
  const headedBy = neighbors.filter((n) => n.edge_kind === "HEADER_OF" && n.direction === "in");
  const DEP_KINDS = new Set(["DEPENDS_ON", "CROSS_SHEET_REF", "CROSS_WORKBOOK_REF"]);
  const reads = neighbors.filter((n) => DEP_KINDS.has(n.edge_kind) && n.direction === "out");
  const readBy = neighbors.filter((n) => DEP_KINDS.has(n.edge_kind) && n.direction === "in");
  const usesNames = neighbors.filter((n) => n.edge_kind === "REFERENCES_NAME" && n.direction === "out");
  const usedByNames = neighbors.filter((n) => n.edge_kind === "REFERENCES_NAME" && n.direction === "in");

  const groups: Group[] = [];
  if (contains.length) groups.push({ title: containsTitle(nodeKind), rows: contains });
  if (containedIn.length) groups.push({ title: "Contained in", rows: containedIn });
  if (heads.length) groups.push({ title: "Heads formula groups", rows: heads });
  if (headedBy.length) groups.push({ title: "Headed by", rows: headedBy });
  if (reads.length) groups.push({ title: "Reads", rows: reads });
  if (readBy.length) groups.push({ title: "Read by", rows: readBy });
  if (usesNames.length) groups.push({ title: "Uses defined names", rows: usesNames });
  if (usedByNames.length) groups.push({ title: "Named by", rows: usedByNames });
  return groups;
}

function containsTitle(kind: string): string {
  switch (kind) {
    case "workbook":
      return "Sheets";
    case "sheet":
      return "Contains";
    case "region":
      return "Columns";
    default:
      return "Contains";
  }
}

function EdgeGroup({
  title,
  rows,
  onSelect,
  edgeIdFor,
  onSelectEdge,
}: {
  title: string;
  rows: NodeDetailDto["neighbors"];
  onSelect: (id: number) => void;
  edgeIdFor: (row: NodeDetailDto["neighbors"][number]) => string | undefined;
  onSelectEdge: (id: string) => void;
}) {
  if (rows.length === 0) return null;
  return (
    <div className="edge-group">
      <div className="section-title">
        {title} <span className="count">{rows.length}</span>
      </div>
      <ul className="neighbor-list">
        {rows.map((row, i) => {
          const edgeId = edgeIdFor(row);
          return (
            <li key={i} className="neighbor-row">
              <button className="neighbor" onClick={() => onSelect(row.id)}>
                <span className="neighbor-edge">
                  {row.edge_kind}
                  {row.weight > 1 && ` ×${row.weight.toLocaleString("en-US")}`}
                </span>
                <span className="neighbor-label">{row.label}</span>
                {row.a1 && <span className="neighbor-a1">{row.a1}</span>}
              </button>
              {edgeId && (
                <button
                  className="neighbor-inspect"
                  title="Inspect this relationship itself"
                  aria-label="inspect relationship"
                  onClick={() => onSelectEdge(edgeId)}
                >
                  ↔
                </button>
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
