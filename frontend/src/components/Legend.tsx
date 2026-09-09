import { useState } from "react";

import type { Filters } from "../graph/GraphView";
import {
  EDGE_COLORS,
  EDGE_KINDS,
  NODE_COLORS,
  NODE_KINDS,
} from "../graph/theme";
import type { GraphDto } from "../types";

interface Props {
  graph: GraphDto;
  filters: Filters;
  onToggleKind: (kind: string) => void;
  onToggleEdgeKind: (kind: string) => void;
  onSheet: (sheet: string | null) => void;
}

// The legend is the filter: a swatch names a kind, its count says what is
// there, and clicking either toggles that kind off the canvas.
export function Legend({ graph, filters, onToggleKind, onToggleEdgeKind, onSheet }: Props) {
  const [open, setOpen] = useState(true);

  return (
    <div className={"legend" + (open ? "" : " folded")}>
      <button className="legend-toggle" onClick={() => setOpen(!open)}>
        {open ? "hide legend" : "legend"}
      </button>
      {open && (
        <>
          <div className="legend-group">
            {NODE_KINDS.filter((kind) => (graph.node_kinds[kind] ?? 0) > 0).map(
              (kind) => (
                <button
                  key={kind}
                  className={
                    "legend-row" + (filters.kinds.has(kind) ? "" : " off")
                  }
                  onClick={() => onToggleKind(kind)}
                  title={filters.kinds.has(kind) ? "hide" : "show"}
                >
                  <span
                    className="swatch"
                    style={{ background: NODE_COLORS[kind] }}
                  />
                  <span className="legend-name">{kind}</span>
                  <span className="legend-count">
                    {graph.node_kinds[kind].toLocaleString("en-US")}
                  </span>
                </button>
              ),
            )}
          </div>
          <div className="legend-group">
            {EDGE_KINDS.filter((kind) => (graph.edge_kinds[kind] ?? 0) > 0).map(
              (kind) => (
                <button
                  key={kind}
                  className={
                    "legend-row" + (filters.edgeKinds.has(kind) ? "" : " off")
                  }
                  onClick={() => onToggleEdgeKind(kind)}
                  title={filters.edgeKinds.has(kind) ? "hide" : "show"}
                >
                  <span
                    className="swatch line"
                    style={{ background: EDGE_COLORS[kind] }}
                  />
                  <span className="legend-name">
                    {kind.replaceAll("_", " ").toLowerCase()}
                  </span>
                  <span className="legend-count">
                    {graph.edge_kinds[kind].toLocaleString("en-US")}
                  </span>
                </button>
              ),
            )}
          </div>
          {graph.sheets.length > 1 && (
            <div className="legend-group">
              <select
                className="sheet-select"
                value={filters.sheet ?? ""}
                onChange={(e) => onSheet(e.target.value === "" ? null : e.target.value)}
              >
                <option value="">all sheets</option>
                {graph.sheets.map((sheet) => (
                  <option key={sheet.node} value={sheet.name}>
                    {sheet.name}
                    {sheet.visible ? "" : " (hidden)"}
                  </option>
                ))}
              </select>
            </div>
          )}
        </>
      )}
    </div>
  );
}
