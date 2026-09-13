import { useState } from "react";

import type { Filters } from "../graph/GraphView";
import {
  EDGE_COLORS,
  EDGE_DIRECTION_NOTE,
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
  onSheetMode: (mode: "only" | "context") => void;
}

// The legend is the filter: a swatch names a kind, its count says what is
// there, and clicking either toggles that kind off the canvas.
export function Legend({
  graph,
  filters,
  onToggleKind,
  onToggleEdgeKind,
  onSheet,
  onSheetMode,
}: Props) {
  const [open, setOpen] = useState(true);
  const [coverageOpen, setCoverageOpen] = useState(false);
  const c = graph.coverage;

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
            <div className="legend-note">{EDGE_DIRECTION_NOTE}</div>
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
              {filters.sheet !== null && (
                <div className="sheet-mode">
                  <button
                    className={"sheet-mode-option" + (filters.sheetMode === "only" ? " active" : "")}
                    onClick={() => onSheetMode("only")}
                    title="Show nothing off this sheet"
                  >
                    this sheet only
                  </button>
                  <button
                    className={"sheet-mode-option" + (filters.sheetMode === "context" ? " active" : "")}
                    onClick={() => onSheetMode("context")}
                    title="Also show one dependency hop off this sheet, defined names, and external targets"
                  >
                    + connected context
                  </button>
                </div>
              )}
            </div>
          )}
          <div className="legend-group coverage">
            <button className="legend-toggle small" onClick={() => setCoverageOpen(!coverageOpen)}>
              {coverageOpen ? "hide region overview coverage" : "region overview: what this omits"}
            </button>
            {coverageOpen && (
              <div className="coverage-body">
                <p>
                  This is a region overview, not a cell-by-cell map: references are
                  lifted to the region containing them, and identical
                  relationships merge into one edge carrying a reference count
                  as its weight.
                </p>
                <div className="fact">
                  <span className="fact-key">references scanned</span>
                  <span className="fact-value mono">{c.references_scanned.toLocaleString("en-US")}</span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ became edges</span>
                  <span className="fact-value mono">{c.references_lifted.toLocaleString("en-US")}</span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ within the same region</span>
                  <span className="fact-value mono">
                    {c.references_within_source_region.toLocaleString("en-US")}
                  </span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ cross-sheet (subset of edges)</span>
                  <span className="fact-value mono">{c.references_cross_sheet.toLocaleString("en-US")}</span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ external workbook</span>
                  <span className="fact-value mono">{c.references_external.toLocaleString("en-US")}</span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ missing-sheet break</span>
                  <span className="fact-value mono">{c.references_dangling.toLocaleString("en-US")}</span>
                </div>
                <div className="fact">
                  <span className="fact-key">→ empty target (not damage)</span>
                  <span className="fact-value mono">
                    {c.references_unpopulated_target.toLocaleString("en-US")}
                  </span>
                </div>
                {c.unknown_sheets.length > 0 && (
                  <p className="legend-note">
                    Missing sheets named by a break: {c.unknown_sheets
                      .map(([name, count]) => `${name} (${count.toLocaleString("en-US")})`)
                      .join(", ")}
                  </p>
                )}
                {!graph.formula_groups && (
                  <p className="legend-note">
                    Formula groups are above this corpus's storage cap and were
                    dropped from this graph; they are not shown here and there
                    is currently no in-GUI action to rebuild them on demand.
                  </p>
                )}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}
