import type { NodeDetailDto } from "../types";

interface Props {
  detail: NodeDetailDto;
  onSelect: (id: number) => void;
  onClose: () => void;
}

// The node under the cursor, in full: identity, payload, and both directions
// of every edge. Neighbor rows are buttons because clicking one is the same
// as clicking the node on the canvas.
export function DetailsPanel({ detail, onSelect, onClose }: Props) {
  const { node, neighbors } = detail;
  const payload = flatten(node.data);
  const outgoing = neighbors.filter((n) => n.direction === "out");
  const incoming = neighbors.filter((n) => n.direction === "in");

  return (
    <aside className="details">
      <div className="details-head">
        <div className="details-title" title={node.label}>
          {node.label}
        </div>
        <button className="close" onClick={onClose} aria-label="close" />
      </div>

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

      <EdgeGroup
        title={outgoingTitle(node.kind)}
        rows={outgoing}
        onSelect={onSelect}
      />
      <EdgeGroup
        title="read by"
        rows={incoming}
        onSelect={onSelect}
      />
    </aside>
  );
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

function outgoingTitle(kind: string): string {
  return kind === "column" || kind === "region" || kind === "formula group"
    ? "reads"
    : "points to";
}

function EdgeGroup({
  title,
  rows,
  onSelect,
}: {
  title: string;
  rows: NodeDetailDto["neighbors"];
  onSelect: (id: number) => void;
}) {
  if (rows.length === 0) return null;
  return (
    <div className="edge-group">
      <div className="section-title">
        {title} <span className="count">{rows.length}</span>
      </div>
      <ul className="neighbor-list">
        {rows.map((row, i) => (
          <li key={i}>
            <button className="neighbor" onClick={() => onSelect(row.id)}>
              <span className="neighbor-edge">
                {row.edge_kind}
                {row.weight > 1 && ` ×${row.weight.toLocaleString("en-US")}`}
              </span>
              <span className="neighbor-label">{row.label}</span>
              {row.a1 && <span className="neighbor-a1">{row.a1}</span>}
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
