// Color and size are data here, not decoration: every hue on the canvas
// answers "what am I looking at", for a node kind, an edge kind, or a role
// an answer gave a node.

export const NODE_KINDS = [
  "workbook",
  "sheet",
  "region",
  "column",
  "formula group",
  "defined name",
  "external workbook",
] as const;

export const EDGE_KINDS = [
  "CONTAINS",
  "HEADER_OF",
  "DEPENDS_ON",
  "CROSS_SHEET_REF",
  "CROSS_WORKBOOK_REF",
  "REFERENCES_NAME",
] as const;

export const NODE_COLORS: Record<string, string> = {
  workbook: "#f2f4f8",
  sheet: "#e2b34d",
  region: "#41b287",
  column: "#5aa2e8",
  "formula group": "#9a86ee",
  "defined name": "#e06f92",
  "external workbook": "#79828f",
};

// Structural edges recede so the dependency edges they sit under can read.
export const EDGE_COLORS: Record<string, string> = {
  CONTAINS: "#272e3a",
  HEADER_OF: "#39424f",
  DEPENDS_ON: "#5aa2e8",
  CROSS_SHEET_REF: "#e2954d",
  CROSS_WORKBOOK_REF: "#e05b5b",
  REFERENCES_NAME: "#41b287",
};

export const STRUCTURAL_EDGES = new Set(["CONTAINS", "HEADER_OF"]);

export function nodeColor(kind: string): string {
  return NODE_COLORS[kind] ?? "#8b94a7";
}

export function nodeSize(kind: string): number {
  switch (kind) {
    case "workbook":
      return 11;
    case "sheet":
      return 8;
    case "region":
      return 6.5;
    default:
      return 4.5;
  }
}

// Edge weight spans one to hundreds of thousands; log keeps a weight-100000
// reference visibly heavier than a weight-3 one without flattening the rest.
export function edgeSize(weight: number, kind: string): number {
  const base = STRUCTURAL_EDGES.has(kind) ? 0.6 : 1.1;
  return base + Math.log10(1 + weight) * 0.45;
}

// Roles an `ask` result assigns, strongest first.
export const ROLE_COLORS: Record<string, string> = {
  seed: "#ffffff",
  within: "#ffd479",
  feeds: "#5aa2e8",
  reads: "#e2954d",
  contains: "#8b94a7",
};

// Receded, not invisible: the un-highlighted part of an answer view is the
// context the answer sits in, so it stays a step darker than lit nodes
// rather than melting into the background.
export const DIM = "#2b313c";
export const DIM_EDGE = "#20242c";
