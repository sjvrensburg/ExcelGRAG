import { useEffect, useMemo, useRef, useState } from "react";
import Graph from "graphology";
import forceAtlas2 from "graphology-layout-forceatlas2";
import Sigma from "sigma";
import type { DisplayData } from "sigma/types";
import { EdgeArrowProgram, EdgeLineProgram } from "sigma/rendering";

import type { EdgeDto, GraphDto } from "../types";
import {
  DIM,
  DIM_EDGE,
  ROLE_COLORS,
  STRUCTURAL_EDGES,
  drawHighlightedLabel,
  edgeColor,
  edgeSize,
  nodeColor,
  nodeSize,
} from "./theme";

// Sigma's own edge programs: an arrowhead at the target for the directed,
// "X depends on Y" kinds, a plain line for the structural kinds (whose
// direction is already obvious from node kind/size — workbook > sheet >
// region > column).
//
// A curved-edge program (`@sigma/edge-curve`, which would additionally
// separate reciprocal/parallel edges between the same two nodes) was tried
// here first and made every edge disappear — its `EdgeCurveProgram` extends
// this Sigma version's `EdgeProgram`/`EdgeProgramType`, and something in
// that inheritance (module duplication under Vite's resolution, most likely
// — worth a closer look, not chased down here) produced a WebGL program
// that compiled without error but drew nothing, on this exact stack
// (sigma 3.0.3 + @sigma/edge-curve 3.1.0). Reverted to the built-in
// programs below, which render correctly; parallel-edge separation is
// deferred rather than shipped half-verified. See the GUI review response
// for the reasoning.
const DEPENDENCY_EDGE_PROGRAM = EdgeArrowProgram;
const STRUCTURAL_EDGE_PROGRAM = EdgeLineProgram;

export interface Filters {
  kinds: Set<string>;
  edgeKinds: Set<string>;
  sheet: string | null;
  // "only": nothing off-sheet is shown. "context": one dependency hop off the
  // sheet, plus workbook-scoped names and external targets, stays visible —
  // the sheet filter otherwise hides the exact relationships tracing needs.
  sheetMode: "only" | "context";
}

export type Selection =
  | { entity: "node"; id: number }
  | { entity: "edge"; id: string };

export interface Highlight {
  // node id -> role ("seed" | "within" | "feeds" | "reads" | "contains"),
  // from a search or an ask. Empty means no highlight.
  roles: Map<number, string>;
  // The exact edges that support the answer/search (by EdgeDto.id), not
  // "every edge of a kind that appears anywhere in the roles" — that used to
  // thicken unrelated same-kind edges across the whole graph. Empty with a
  // nonempty `roles` still dims every edge; it just supports none of them.
  edgeIds: Set<string>;
  workbook: string | null;
}

export const NO_HIGHLIGHT: Highlight = {
  roles: new Map(),
  edgeIds: new Set(),
  workbook: null,
};

interface Props {
  graph: GraphDto;
  filters: Filters;
  highlight: Highlight;
  selection: Selection | null;
  focus: { id: number; nonce: number } | null;
  onClear: () => void;
  onSelectNode: (id: number) => void;
  onSelectEdge: (edge: EdgeDto) => void;
  onHoverEdge: (edge: EdgeDto | null) => void;
  onReady: (ready: boolean) => void;
}

function edgeProgramType(kind: string): string {
  return STRUCTURAL_EDGES.has(kind) ? "structural" : "dependency";
}

// The map. One Sigma instance per loaded graph; filters, selection and
// highlight never rebuild it, they flow through reducers, which is Sigma's
// intended way to restyle without touching layout state.
export function GraphView({
  graph,
  filters,
  highlight,
  selection,
  focus,
  onClear,
  onSelectNode,
  onSelectEdge,
  onHoverEdge,
  onReady,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const sigmaRef = useRef<Sigma | null>(null);
  const [status, setStatus] = useState<string>("");
  const [ready, setReady] = useState(false);

  // Latest values for the reducers, which Sigma captures once at
  // construction: a ref lets every render restyle without rebuilding.
  const view = useRef({ filters, highlight, selection });
  view.current = { filters, highlight, selection };

  // One hop of dependency/name/external context out from each sheet, so
  // "this sheet + context" can keep the relationships tracing actually
  // needs instead of amputating every off-sheet endpoint. Recomputed only
  // when the graph itself changes — it does not depend on which sheet is
  // currently selected, since it is cheap enough to precompute for all of
  // them at once and index by sheet name.
  const sheetContext = useMemo(() => {
    const bySheet = new Map<string, Set<number>>();
    const nodeSheet = new Map<number, string | undefined>();
    for (const node of graph.nodes) nodeSheet.set(node.id, node.sheet);
    const DEP_KINDS = new Set([
      "DEPENDS_ON",
      "CROSS_SHEET_REF",
      "CROSS_WORKBOOK_REF",
      "REFERENCES_NAME",
    ]);
    for (const edge of graph.edges) {
      if (!DEP_KINDS.has(edge.kind)) continue;
      const sourceSheet = nodeSheet.get(edge.source);
      const targetSheet = nodeSheet.get(edge.target);
      if (sourceSheet) {
        const set = bySheet.get(sourceSheet) ?? new Set<number>();
        set.add(edge.target);
        bySheet.set(sourceSheet, set);
      }
      if (targetSheet) {
        const set = bySheet.get(targetSheet) ?? new Set<number>();
        set.add(edge.source);
        bySheet.set(targetSheet, set);
      }
    }
    return bySheet;
  }, [graph]);

  // Nodes with no sheet of their own that a "this sheet only" view would
  // otherwise erase outright — workbook-scoped names and external targets —
  // kept in context mode regardless of which sheet is selected.
  const sheetless = useMemo(
    () =>
      new Set(
        graph.nodes
          .filter((n) => !n.sheet && n.kind !== "workbook")
          .map((n) => n.id),
      ),
    [graph],
  );

  // Build + lay out. Synchronous ForceAtlas2: a few hundred iterations on
  // graphs of this size (hundreds to ~20k nodes) costs well under a second
  // on the workbooks this has been run against — not independently
  // benchmarked, so treat that as a working assumption to revisit if a
  // much larger graph makes it feel slow, not a measured guarantee.
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
      g.addDirectedEdgeWithKey(edge.id, String(edge.source), String(edge.target), {
        kind: edge.kind,
        weight: edge.weight,
        color: edgeColor(edge.kind),
        size: edgeSize(edge.weight, edge.kind),
        type: edgeProgramType(edge.kind),
      });
    }
    // A reciprocal pair (A depends on B and B depends on A) still renders as
    // one straight line with arrowheads at both ends rather than as two
    // separable edges — see the edge-program comment above for why a curved
    // program that would fix this was tried and reverted. Truthful (it does
    // show the relationship is mutual) but not a full fix; each edge is
    // still independently selectable and correctly described once picked,
    // via Sigma's own edge picking, which does not depend on the edges
    // being visually offset.
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

  // Whether the node filter (kind + sheet scope) hides a node — the edge
  // reducer needs this exact predicate so an edge is never drawn between two
  // nodes that visibly aren't there.
  const nodeVisible = (kind: string, sheet: string | undefined, id: number, f: Filters): boolean => {
    if (!f.kinds.has(kind)) return false;
    if (f.sheet === null || kind === "workbook") return true;
    if (sheet === f.sheet) return true;
    if (f.sheetMode === "context") {
      if (sheetless.has(id)) return true;
      if (sheetContext.get(f.sheet)?.has(id)) return true;
    }
    return false;
  };

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    setReady(false);
    onReady(false);
    setStatus("laying out");
    let sigma: Sigma | null = null;
    let disposed = false;
    // Let the status paint before Sigma mounts. The layout above already ran
    // synchronously in `useMemo`, before this effect — this timeout delays
    // renderer construction, not the layout computation itself.
    const handle = window.setTimeout(() => {
      if (disposed || !containerRef.current) return;
      sigma = new Sigma(layout, containerRef.current, {
        allowInvalidContainer: true,
        renderEdgeLabels: false,
        enableEdgeEvents: true,
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
        defaultEdgeType: "structural",
        edgeProgramClasses: {
          dependency: DEPENDENCY_EDGE_PROGRAM,
          structural: STRUCTURAL_EDGE_PROGRAM,
        },
        defaultNodeColor: "#8b94a7",
        nodeReducer(node, data) {
          const { filters: f, highlight: h, selection: s } = view.current;
          const kind = String(data.kind ?? "");
          const id = Number(node);
          if (!nodeVisible(kind, data.sheet as string | undefined, id, f)) {
            return { ...data, hidden: true } as DisplayData;
          }
          const out = { ...data } as DisplayData & Record<string, unknown>;
          const role = h.roles.get(id) ?? null;
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
          if (s?.entity === "node" && id === s.id) {
            out.highlighted = true;
            out.forceLabel = true;
            out.zIndex = 3;
          }
          if (s?.entity === "edge") {
            const isEndpoint =
              layout.source(s.id) === node || layout.target(s.id) === node;
            if (isEndpoint) {
              out.highlighted = true;
              out.forceLabel = true;
              out.zIndex = 3;
            }
          }
          return out as DisplayData;
        },
        edgeReducer(edge, data) {
          const { filters: f, highlight: h, selection: s } = view.current;
          const kind = String(data.kind ?? "");
          if (!f.edgeKinds.has(kind)) {
            return { ...data, hidden: true } as DisplayData;
          }
          const sourceId = Number(layout.source(edge));
          const targetId = Number(layout.target(edge));
          const sourceKind = String(layout.getNodeAttribute(layout.source(edge), "kind"));
          const targetKind = String(layout.getNodeAttribute(layout.target(edge), "kind"));
          const sourceSheet = layout.getNodeAttribute(layout.source(edge), "sheet") as
            | string
            | undefined;
          const targetSheet = layout.getNodeAttribute(layout.target(edge), "sheet") as
            | string
            | undefined;
          // Hide edges whose endpoints the node filter hid; a visible edge
          // between invisible nodes is a lie about the graph.
          if (
            !nodeVisible(sourceKind, sourceSheet, sourceId, f) ||
            !nodeVisible(targetKind, targetSheet, targetId, f)
          ) {
            return { ...data, hidden: true } as DisplayData;
          }
          const out = { ...data } as DisplayData;
          if (h.roles.size > 0) {
            if (h.edgeIds.has(edge)) {
              out.size = (out.size ?? 1) * 1.8;
              out.color = "#ffffff";
              out.zIndex = 2;
            } else {
              out.color = DIM_EDGE;
              out.size = 0.4;
            }
          }
          if (s?.entity === "edge" && s.id === edge) {
            out.size = (out.size ?? 1) * 2;
            out.color = "#ffffff";
            out.zIndex = 3;
          }
          return out;
        },
      });

      sigma.on("clickNode", ({ node }) => onSelectNode(Number(node)));
      sigma.on("clickEdge", ({ edge }) => {
        const dto = graph.edges.find((e) => e.id === edge);
        if (dto) onSelectEdge(dto);
      });
      sigma.on("enterEdge", ({ edge }) => {
        const dto = graph.edges.find((e) => e.id === edge);
        onHoverEdge(dto ?? null);
      });
      sigma.on("leaveEdge", () => onHoverEdge(null));
      sigma.on("clickStage", () => onClear());
      sigmaRef.current = sigma;
      // A debug affordance: the live renderer, for the console and for tests
      // that need to compute where a node actually is on screen.
      (window as unknown as { __sigma?: unknown }).__sigma = sigma;
      setStatus("");
      setReady(true);
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
  }, [filters, highlight, selection]);

  // Camera focus, requested by clicks elsewhere in the app. Queued rather
  // than dropped when it arrives before Sigma exists (construction is
  // deferred behind the 30ms timeout above): recording `lastFocus.current`
  // without checking readiness let a focus request land, be marked
  // consumed, and never actually move the camera — the map stayed blank
  // after the very first navigation on a freshly opened graph.
  const lastFocus = useRef(0);
  const pendingFocus = useRef<{ id: number; nonce: number } | null>(null);
  const applyFocus = (target: { id: number; nonce: number }) => {
    const sigma = sigmaRef.current;
    const node = String(target.id);
    if (!sigma || !layout.hasNode(node)) return false;
    // `getNodeDisplayData`, not `layout.getNodeAttribute`: the camera's
    // x/y are in Sigma's normalized graph space, but `layout` still holds
    // ForceAtlas2's raw pre-normalization coordinates (which can run into
    // the hundreds) — animating the camera to those flew it off to empty
    // space, leaving the canvas blank after every focus.
    const display = sigma.getNodeDisplayData(node);
    if (!display) return false;
    sigma
      .getCamera()
      .animate({ x: display.x, y: display.y, ratio: 0.35 }, { duration: 350 });
    return true;
  };
  useEffect(() => {
    if (!focus || focus.nonce === lastFocus.current) return;
    lastFocus.current = focus.nonce;
    if (!ready || !applyFocus(focus)) {
      pendingFocus.current = focus;
      return;
    }
    pendingFocus.current = null;
  }, [focus, layout, ready]);
  useEffect(() => {
    if (ready && pendingFocus.current) {
      applyFocus(pendingFocus.current);
      pendingFocus.current = null;
    }
  }, [ready]);

  // No built-in "fit to content" in Sigma 3 — computed from the rendered
  // (post-normalization) positions of the nodes the current filters leave
  // visible, so hiding most of the graph and fitting recenters on what's
  // actually shown rather than the whole unfiltered layout.
  const fitGraph = () => {
    const sigma = sigmaRef.current;
    if (!sigma) return;
    let minX = Infinity;
    let maxX = -Infinity;
    let minY = Infinity;
    let maxY = -Infinity;
    let any = false;
    layout.forEachNode((node) => {
      const display = sigma.getNodeDisplayData(node);
      if (!display || display.hidden) return;
      any = true;
      minX = Math.min(minX, display.x);
      maxX = Math.max(maxX, display.x);
      minY = Math.min(minY, display.y);
      maxY = Math.max(maxY, display.y);
    });
    if (!any) return;
    const x = (minX + maxX) / 2;
    const y = (minY + maxY) / 2;
    const spread = Math.max(maxX - minX, maxY - minY, 0.05);
    sigma.getCamera().animate({ x, y, ratio: spread * 0.65 }, { duration: 300 });
  };

  return (
    <div className="stage-inner">
      <div ref={containerRef} className="sigma-container" />
      {status && <div className="stage-status">{status}</div>}
      <div className="stage-controls">
        <button className="stage-control" onClick={fitGraph} title="Fit graph">
          fit
        </button>
      </div>
    </div>
  );
}
