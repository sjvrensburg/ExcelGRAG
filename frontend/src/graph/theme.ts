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

// Sigma's built-in hover/highlight label renderer draws a light pill behind
// the label, but reuses the same fixed `labelColor` (tuned for text sitting
// directly on the dark canvas) for the text inside it — pale-on-pale,
// unreadable. This draws the same style of pill with a fixed dark text
// color instead, so a highlighted or selected node's label stays legible
// regardless of what `labelColor` is set to.
export function drawHighlightedLabel(
  context: CanvasRenderingContext2D,
  data: { x: number; y: number; size: number; label?: string | null },
  settings: { labelSize: number; labelFont: string; labelWeight: string },
): void {
  if (!data.label) return;
  const size = settings.labelSize;
  context.font = `${settings.labelWeight} ${size}px ${settings.labelFont}`;
  const paddingX = 4;
  const paddingY = 2;
  const textWidth = context.measureText(data.label).width;
  const boxX = data.x + data.size + 3;
  const boxY = data.y - size / 2 - paddingY;
  const boxWidth = textWidth + paddingX * 2;
  const boxHeight = size + paddingY * 2;
  const radius = 3;
  context.fillStyle = "#f2f4f8";
  context.beginPath();
  if (context.roundRect) {
    context.roundRect(boxX, boxY, boxWidth, boxHeight, radius);
  } else {
    context.rect(boxX, boxY, boxWidth, boxHeight);
  }
  context.fill();
  context.fillStyle = "#12151a";
  context.fillText(data.label, boxX + paddingX, data.y + size / 3);
}
