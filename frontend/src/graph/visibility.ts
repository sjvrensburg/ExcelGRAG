import type { GraphDto } from "../types";

// The single node/edge visibility predicate the canvas (GraphView's
// reducers) and the topbar count (App's visibleCount) both need. Before this
// module existed the two reimplemented it separately, and the topbar version
// omitted the sheet filter's context walk entirely — in "context" mode with
// a sheet selected, it counted every edge of an enabled kind anywhere in the
// graph, not just the ones actually drawn. One shared implementation means a
// change to what "context" reveals only has to be made once.

export interface Filters {
  kinds: Set<string>;
  edgeKinds: Set<string>;
  sheet: string | null;
  // "only": nothing off-sheet is shown. "context": one dependency hop off the
  // sheet, plus workbook-scoped names and external targets, stays visible —
  // the sheet filter otherwise hides the exact relationships tracing needs.
  sheetMode: "only" | "context";
}

const DEP_KINDS = new Set([
  "DEPENDS_ON",
  "CROSS_SHEET_REF",
  "CROSS_WORKBOOK_REF",
  "REFERENCES_NAME",
]);

export interface SheetContext {
  // Sheet name -> ids of nodes one dependency hop off that sheet.
  bySheet: Map<string, Set<number>>;
  // Nodes with no sheet of their own (workbook-scoped names, external
  // targets) that a "this sheet only" view would otherwise erase outright.
  sheetless: Set<number>;
}

// One hop of dependency/name/external context out from each sheet, so "this
// sheet + context" can keep the relationships tracing actually needs instead
// of amputating every off-sheet endpoint. Cheap enough to precompute for
// every sheet at once and index by sheet name, rather than per selected
// sheet.
export function computeSheetContext(graph: GraphDto): SheetContext {
  const bySheet = new Map<string, Set<number>>();
  const nodeSheet = new Map<number, string | undefined>();
  for (const node of graph.nodes) nodeSheet.set(node.id, node.sheet);
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
  const sheetless = new Set(
    graph.nodes.filter((n) => !n.sheet && n.kind !== "workbook").map((n) => n.id),
  );
  return { bySheet, sheetless };
}

// Whether the node filter (kind + sheet scope) hides a node.
export function nodeVisible(
  kind: string,
  sheet: string | undefined,
  id: number,
  filters: Filters,
  ctx: SheetContext,
): boolean {
  if (!filters.kinds.has(kind)) return false;
  if (filters.sheet === null || kind === "workbook") return true;
  if (sheet === filters.sheet) return true;
  if (filters.sheetMode === "context") {
    if (ctx.sheetless.has(id)) return true;
    if (ctx.bySheet.get(filters.sheet)?.has(id)) return true;
  }
  return false;
}

export function visibleNodeIds(graph: GraphDto, filters: Filters): Set<number> {
  const ctx = computeSheetContext(graph);
  const ids = new Set<number>();
  for (const n of graph.nodes) {
    if (nodeVisible(n.kind, n.sheet, n.id, filters, ctx)) ids.add(n.id);
  }
  return ids;
}

// Exactly what GraphView actually renders: an edge counts only when its kind
// is enabled AND both endpoints are in the visible node set (context mode
// included), not "every edge of an enabled kind, anywhere."
export function visibleCounts(graph: GraphDto, filters: Filters): { nodes: number; edges: number } {
  const ids = visibleNodeIds(graph, filters);
  let edges = 0;
  for (const e of graph.edges) {
    if (!filters.edgeKinds.has(e.kind)) continue;
    if (ids.has(e.source) && ids.has(e.target)) edges += 1;
  }
  return { nodes: ids.size, edges };
}
