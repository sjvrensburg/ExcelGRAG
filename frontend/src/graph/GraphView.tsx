import { useEffect, useMemo, useRef, useState } from "react";
import Graph from "graphology";
import forceAtlas2 from "graphology-layout-forceatlas2";
import Sigma from "sigma";
import type { DisplayData } from "sigma/types";
import type { RenderParams } from "sigma/types";
import {
  EdgeRectangleProgram,
  createEdgeArrowHeadProgram,
  createEdgeClampedProgram,
  createEdgeCompoundProgram,
} from "sigma/rendering";

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
import { computeSheetContext, nodeVisible as sharedNodeVisible } from "./visibility";
import type { Filters } from "./visibility";

export type { Filters } from "./visibility";

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
//
// Both are built from Sigma's own program pieces, wrapped so that the
// *picking* pass — the offscreen render Sigma reads a click's pixel back
// from — draws every edge at least `PICK_MIN_THICKNESS` px wide while the
// visible pass keeps its true width. Without this an edge is clickable only
// across the pixels it paints, and a dependency edge of weight 1 is
// ~1.3 px: live-testing found only the heaviest edge on the demo canvas
// could be selected by mouse at all. The structural kinds used Sigma's
// `EdgeLineProgram` (GL_LINES, always one pixel, no thickness uniform to
// widen) and were unclickable outright; the rectangle program is what
// Sigma itself uses for its default "line" type and looks the same at these
// sizes.
const PICK_MIN_THICKNESS = 9;

// Room around a fitted graph, per side, so the outermost nodes and their
// labels don't sit on the edge of the canvas (a node's radius and label
// extend past its centre, which is all the extent below measures).
const FIT_MARGIN_PX = 48;

type EdgeProgramClass = ReturnType<typeof createEdgeClampedProgram>;

// What the wrapper below needs of a program: Sigma's public `EdgeProgramType`
// hides `setUniforms`, so the class is widened to this shape and narrowed
// back, both as casts — the runtime classes do have the method.
interface UniformSetter {
  setUniforms(params: RenderParams, programInfo: { isPicking: boolean }): void;
}

function withPickTolerance(Base: EdgeProgramClass): EdgeProgramClass {
  // `setUniforms(params, programInfo)` is called once per pass with
  // `programInfo.isPicking` telling the two apart; `minEdgeThickness` is
  // the same setting Sigma reads from `settings.minEdgeThickness`, only
  // raised for the pass nobody sees.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const Widened = Base as unknown as new (...args: any[]) => UniformSetter;
  class Tolerant extends Widened {
    setUniforms(params: RenderParams, programInfo: { isPicking: boolean }): void {
      super.setUniforms(
        programInfo.isPicking
          ? { ...params, minEdgeThickness: Math.max(params.minEdgeThickness, PICK_MIN_THICKNESS) }
          : params,
        programInfo,
      );
    }
  }
  return Tolerant as unknown as EdgeProgramClass;
}

const DEPENDENCY_EDGE_PROGRAM = createEdgeCompoundProgram([
  withPickTolerance(createEdgeClampedProgram()),
  withPickTolerance(createEdgeArrowHeadProgram()),
]);
const STRUCTURAL_EDGE_PROGRAM = withPickTolerance(EdgeRectangleProgram);

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
  // needs instead of amputating every off-sheet endpoint. Shared with
  // `visibleCount` (App.tsx) via `./visibility` so the canvas and the
  // topbar count agree on what "context" mode actually reveals. Recomputed
  // only when the graph itself changes — it does not depend on which sheet
  // is currently selected, since it is cheap enough to precompute for all
  // of them at once and index by sheet name.
  const sheetCtx = useMemo(() => computeSheetContext(graph), [graph]);

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
  // nodes that visibly aren't there. Delegates to the shared predicate in
  // `./visibility` rather than a second copy of the sheet-context logic.
  const nodeVisible = (kind: string, sheet: string | undefined, id: number, f: Filters): boolean =>
    sharedNodeVisible(kind, sheet, id, f, sheetCtx);

  useEffect(() => {
    const element = containerRef.current;
    if (!element) return;
    setReady(false);
    onReady(false);
    setStatus("laying out");
    // A focus queued for the graph this effect is about to replace must not
    // survive into the new one: node ids are local to one graph, so an id
    // queued for graph A can silently resolve to an unrelated node in graph
    // B once B's Sigma instance becomes ready.
    pendingFocus.current = null;
    lastFocus.current = 0;
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
          // `layout.hasEdge` guards a selection left over from a graph this
          // `layout` no longer represents (e.g. a selection whose reset
          // hasn't committed yet) — `layout.source`/`target` throw on an
          // unknown edge key, which would otherwise break every node's
          // reducer call for the whole canvas at once.
          if (s?.entity === "edge" && layout.hasEdge(s.id)) {
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
          const sourceNode = layout.source(edge);
          const targetNode = layout.target(edge);
          const sourceId = Number(sourceNode);
          const targetId = Number(targetNode);
          const sourceKind = String(layout.getNodeAttribute(sourceNode, "kind"));
          const targetKind = String(layout.getNodeAttribute(targetNode, "kind"));
          const sourceSheet = layout.getNodeAttribute(sourceNode, "sheet") as
            | string
            | undefined;
          const targetSheet = layout.getNodeAttribute(targetNode, "sheet") as
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
    // Sigma's camera `ratio` is a zoom-out factor (larger shows more), in
    // units of its own normalised graph space, and it is not the fraction
    // of the stage a graph fills — a previous `spread * 0.65` here *cropped*
    // a freshly loaded graph on every press, the one button that promises
    // to show everything showing about two thirds of it. Rather than
    // reproduce Sigma's matrix (stage padding, aspect correction), measure
    // the visible extent in pixels under the *current* camera and scale
    // that camera's ratio by how far the extent overshoots the stage on
    // its worse axis. Whatever the transform is, it is linear in `ratio`.
    const camera = sigma.getCamera();
    const { width, height } = sigma.getDimensions();
    const a = sigma.framedGraphToViewport({ x: minX, y: minY });
    const b = sigma.framedGraphToViewport({ x: maxX, y: maxY });
    const spanX = Math.max(Math.abs(b.x - a.x), 1);
    const spanY = Math.max(Math.abs(b.y - a.y), 1);
    const usableX = Math.max(width - 2 * FIT_MARGIN_PX, 1);
    const usableY = Math.max(height - 2 * FIT_MARGIN_PX, 1);
    const ratio = camera.ratio * Math.max(spanX / usableX, spanY / usableY);
    camera.animate({ x, y, ratio: camera.getBoundedRatio(ratio) }, { duration: 300 });
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
