// Mirrors of the server's DTOs (crates/eg-gui/src/dto.rs). The server is the
// single source of truth for these shapes; when they change there, they
// change here.

export interface WorkbookDto {
  hash: string;
  path: string;
  sheets: number;
  cells: number;
  nodes: number;
  edges: number;
  formula_group_nodes: boolean;
  profiled_columns: number;
  profile_values: boolean;
}

export interface GraphDto {
  hash: string;
  path: string;
  root: number;
  formula_groups: boolean;
  nodes: NodeDto[];
  edges: EdgeDto[];
  node_kinds: Record<string, number>;
  edge_kinds: Record<string, number>;
  sheets: SheetDto[];
}

export interface NodeDto {
  id: number;
  kind: string;
  label: string;
  a1?: string;
  sheet?: string;
  cells?: number;
  parent?: number;
  data: Record<string, unknown>;
}

export interface EdgeDto {
  source: number;
  target: number;
  kind: string;
  weight: number;
}

export interface SheetDto {
  node: number;
  name: string;
  visible: boolean;
  cells: number;
  formula_cells: number;
}

export interface NodeDetailDto {
  node: NodeDto;
  neighbors: NeighborDto[];
}

export interface NeighborDto {
  direction: "out" | "in";
  edge_kind: string;
  weight: number;
  id: number;
  kind: string;
  label: string;
  a1?: string;
}

export interface HitDto {
  score: number;
  workbook: string;
  node: number;
  kind: string;
  sheet?: string;
  label: string;
  a1?: string;
}

export interface SearchDto {
  hits: HitDto[];
  verdict: string;
  evidence: string;
  warning?: string;
  matched: string[];
  unmatched: string[];
  both_halves: boolean;
}

export interface RetrievedNodeDto {
  node: number;
  kind: string;
  label: string;
  a1?: string;
  sheet?: string;
  role: "seed" | "contains" | "within" | "feeds" | "reads";
  via?: number;
  edge_kind?: string;
  weight?: number;
  hops: number;
  score?: number;
}

export interface RetrievedWorkbookDto {
  hash: string;
  path: string;
  nodes: RetrievedNodeDto[];
  truncated: boolean;
}

export interface AskResponse {
  search: SearchDto;
  workbooks: RetrievedWorkbookDto[];
  passage: {
    text: string;
    citations: string[];
    omitted: number;
  };
}

export interface ChatTurnDto {
  id: number;
  source: "human" | "agent";
  message: string;
  resolved_query?: string;
  evidence: string;
  citations: string[];
  answer: string;
  timestamp: number;
}

export type WsEvent =
  | { type: "hello"; dir: string; redact_values: boolean; workbooks: WorkbookDto[] }
  | {
      type: "corpus";
      added: string[];
      removed: string[];
      changed: string[];
      workbooks: WorkbookDto[];
    }
  | { type: "log"; line: string }
  | { type: "index_done"; path: string; ok: boolean; error?: string }
  | { type: "chat_turn"; session_id: string; turn: ChatTurnDto }
  | { type: "navigate"; session_id: string; node: number; workbook?: string }
  | { type: "lagged"; skipped: number };
