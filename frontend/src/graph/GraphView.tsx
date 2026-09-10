import { useEffect, useMemo, useRef, useState } from "react";
import Graph from "graphology";
import forceAtlas2 from "graphology-layout-forceatlas2";
import Sigma from "sigma";
import type { DisplayData } from "sigma/types";

import type { GraphDto } from "../types";
import {
  DIM,
  DIM_EDGE,
  ROLE_COLORS,
  drawHighlightedLabel,
  edgeSize,
  nodeColor,
  nodeSize,
} from "./theme";

export interface Filters {
  kinds: Set<string>;
  edgeKinds: Set<string>;
  sheet: string | null;
}

export interface Highlight {
  // node id -> role ("seed" | "within" | "feeds" | "reads" | "contains"),
  // from a search or an ask. Empty means no highlight.
  roles: Map<number, string>;
  // Edge kinds that carry the highlighted answer; edges of other kinds dim.
  edgeKinds: Set<string>;
  workbook: string | null;
}

export const NO_HIGHLIGHT: Highlight = {
  roles: new Map(),
  edgeKinds: new Set(),
  workbook: null,
};

interface Props {
  graph: GraphDto;
  filters: Filters;
  highlight: Highlight;
  selected: number | null;
  focus: { id: number; nonce: number } | null;
  onClear: () => void;
  onSelect: (id: number) => void;
  onReady: (ready: boolean) => void;
}

// The map. One Sigma instance per loaded graph; filters, selection and
// highlight never rebuild it, they flow through reducers, which is Sigma's
// intended way to restyle without touching layout state.
export function GraphView({
  graph,
  filters,
  highlight,
  selected,
  focus,
  onClear,
  onSelect,
  onReady,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const [status, setStatus] = useState<string>("");

  // Latest values for the reducers, which Sigma captures once at
  // construction: a ref lets every render restyle without rebuilding.
  const view = useRef({ filters, highlight, selected });
  view.current = { filters, highlight, selected };

  // Build + lay out. Synchronous ForceAtlas2: a few hundred iterations on
  // graphs of this size (hundreds to ~20k nodes) costs well under a second,
  // and a stable converged layout beats an animated one.
  const layout = useMemo(() => {
    const g = new Graph({ multi: true, type: "directed" });
    for (const node of graph.nodes) {
      g.addNode(String(node.id), {
        label: node.label,
        kind: node.kind,
        color: nodeColor(node.kind),
        size: nodeSize(node.kind),
        x: 0,
        y: 0,
        a1: node.a1,
        sheet: node.sheet,
      });
    }
    for (const edge of graph.edges) {
      g.addDirectedEdge(String(edge.source), String(edge.target), {
        kind: edge.kind,
        weight: edge.weight,
        color: DIM_EDGE,
        size: edgeSize(edge.weight, edge.kind),
      });
    }
    // A ring to start: FA2 separates structure from there, and a ring keeps
    // the first iterations from folding the graph through the origin.
    const count = g.order;
    let position = 0;
    g.forEachNode((node) => {
      const angle = (2 * Math.PI * position) / Math.max(count, 1);
      position += 1;
      g.setNodeAttribute(node, "x", Math.cos(angle) * count);
      g.setNodeAttribute(node, "y", Math.sin(angle) * count);
    });
    forceAtlas2.assign(g, {
      iterations: Math.min(300, Math.max(60, Math.round(120_000 / Math.max(count, 1)))),
      settings: {
        ...forceAtlas2.inferSettings(g),
        gravity: 0.4,
        scalingRatio: 12,
      },
    });
    return g;
  }, [graph]);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    onReady(false);
    setStatus("laying out");
    let sigma: Sigma | null = null;
    let disposed = false;
    // Let the status paint before the layout work runs.
    const handle = window.setTimeout(() => {
      if (disposed || !containerRef.current) return;
      sigma = new Sigma(layout, containerRef.current, {
        allowInvalidContainer: true,
        renderEdgeLabels: false,
        minCameraRatio: 0.02,
        maxCameraRatio: 20,
        labelDensity: 0.5,
        labelGridCellSize: 90,
        labelRenderedSizeThreshold: 9,
        labelFont: "system-ui, sans-serif",
        labelSize: 12,
        labelColor: { color: "#c7cdd8" },
        // Sigma's built-in hover renderer would draw this same pale
        // `labelColor` text on its own light pill background — see
        // `drawHighlightedLabel`'s comment for why that's illegible.
        defaultDrawNodeHover: drawHighlightedLabel,
        defaultEdgeColor: DIM_EDGE,
        defaultNodeColor: "#8b94a7",
        nodeReducer(node, data) {
          const { filters: f, highlight: h, selected: s } = view.current;
          const kind = String(data.kind ?? "");
          if (!f.kinds.has(kind)) {
            return { ...data, hidden: true } as DisplayData;
          }
          if (f.sheet !== null && kind !== "workbook" && data.sheet !== f.sheet) {
            return { ...data, hidden: true } as DisplayData;
          }
          const out = { ...data } as DisplayData & Record<string, unknown>;
          const role = h.roles.get(Number(node)) ?? null;
          if (h.roles.size > 0 && role === null) {
            // Everything outside the answer recedes; labels off keeps the
            // canvas readable at answer scale.
            out.color = DIM;
            out.label = null;
            out.zIndex = 0;
          }
          if (role !== null) {
            out.color = ROLE_COLORS[role] ?? data.color;
            out.forceLabel = true;
            out.zIndex = 2;
            if (role === "seed") out.highlighted = true;
          }
          if (s !== null && node === String(s)) {
            out.highlighted = true;
            out.forceLabel = true;
            out.zIndex = 3;
          }
          return out as DisplayData;
        },
        edgeReducer(edge, data) {
          const { filters: f, highlight: h } = view.current;
          const kind = String(data.kind ?? "");
          if (!f.edgeKinds.has(kind)) {
            return { ...data, hidden: true } as DisplayData;
          }
          // Hide edges whose endpoints the node filter hid; a visible edge
          // between invisible nodes is a lie about the graph.
          const sourceKind = String(layout.getNodeAttribute(layout.source(edge), "kind"));
          const targetKind = String(layout.getNodeAttribute(layout.target(edge), "kind"));
          if (!f.kinds.has(sourceKind) || !f.kinds.has(targetKind)) {
            return { ...data, hidden: true } as DisplayData;
          }
          const out = { ...data } as DisplayData;
          if (h.roles.size > 0) {
            if (h.edgeKinds.has(kind)) {
              out.size = (out.size ?? 1) * 1.5;
              out.zIndex = 2;
            } else {
              out.color = DIM_EDGE;
              out.size = 0.4;
            }
          }
          return out;
        },
      });

      sigma.on("clickNode", ({ node }) => onSelect(Number(node)));
      sigma.on("clickStage", () => onClear());
      sigmaRef.current = sigma;
      // A debug affordance: the live renderer, for the console and for tests
      // that need to compute where a node actually is on screen.
      (window as unknown as { __sigma?: unknown }).__sigma = sigma;
      setStatus("");
      onReady(true);
    }, 30);

    return () => {
      disposed = true;
      window.clearTimeout(handle);
      sigma?.kill();
      sigmaRef.current = null;
    };
    // Rebuild only for a new graph. Handlers are stable in practice; the
    // eslint disable is deliberate.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [layout]);

  // Restyle when the view changes: refresh re-runs both reducers.
  useEffect(() => {
    sigmaRef.current?.refresh();
  }, [filters, highlight, selected]);

  // Camera focus, requested by clicks elsewhere in the app.
  const lastFocus = useRef(0);
  useEffect(() => {
    if (!focus || focus.nonce === lastFocus.current) return;
    lastFocus.current = focus.nonce;
    const sigma = sigmaRef.current;
    const node = String(focus.id);
    if (!sigma || !layout.hasNode(node)) return;
    // `getNodeDisplayData`, not `layout.getNodeAttribute`: the camera's
    // x/y are in Sigma's normalized graph space, but `layout` still holds
    // ForceAtlas2's raw pre-normalization coordinates (which can run into
    // the hundreds) — animating the camera to those flew it off to empty
    // space, leaving the canvas blank after every focus.
    const display = sigma.getNodeDisplayData(node);
    if (!display) return;
    sigma
      .getCamera()
      .animate({ x: display.x, y: display.y, ratio: 0.35 }, { duration: 350 });
  }, [focus, layout]);

  return (
    <div className="stage-inner">
      <div ref={containerRef} className="sigma-container" />
      {status && <div className="stage-status">{status}</div>}
    </div>
  );
}
