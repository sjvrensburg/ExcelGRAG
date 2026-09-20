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
  coverage: CoverageDto;
}

// What the aggregate graph above omits, and why — see crates/eg-gui/src/dto.rs.
export interface CoverageDto {
  references_scanned: number;
  references_lifted: number;
  references_within_source_region: number;
  references_cross_sheet: number;
  references_external: number;
  references_dangling: number;
  references_unpopulated_target: number;
  names_resolved: number;
  names_not_defined: number;
  unknown_sheets: [string, number][];
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
  id: string;
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
  // Set when a human routed this turn to the attached agent instead of the
  // built-in search/LLM pipeline. An empty `answer` alongside this means the
  // question is still open — no MCP client attached, or it hasn't replied.
  directed_to?: "agent";
  // Set on an agent's turn that answers a `directed_to` one — the id of the
  // question it answers.
  reply_to?: number;
  // The tool calls an investigation made, when the model drove this turn
  // itself. Arguments and verdicts, never results.
  trail?: TrailStepDto[];
}

export interface TrailStepDto {
  turn: number;
  name: string;
  args: unknown;
  ok: boolean;
  refused: boolean;
}

// One step of an investigation in flight, streamed as it happens. Shown
// live under the pending turn; never persisted.
export type AgentStepDto =
  | { kind: "model_call"; turn: number }
  | { kind: "model_text"; turn: number; text: string }
  | { kind: "tool_call"; turn: number; name: string; args: unknown }
  | { kind: "tool_result"; turn: number; name: string; ok: boolean; refused: boolean; text: string }
  | { kind: "unknown_tool"; turn: number; name: string }
  | { kind: "sent_back"; turn: number; reason: string };

// The bundled model's sidecar, as the server reports it.
export type SidecarStatus =
  | { state: "stopped" }
  | { state: "downloading"; model: string; what: string; done: number; total: number }
  | { state: "verifying"; model: string }
  | { state: "starting"; model: string }
  | { state: "running"; model: string; port: number; pid: number }
  | { state: "failed"; model: string; error: string };

export interface SidecarModel {
  id: string;
  tier: string;
  note: string;
  file: string;
  size: number;
  needs_bytes: number;
  downloaded: boolean;
}

export interface SidecarInfo {
  status: SidecarStatus;
  runtime: { os: string; arch: string; accelerator: string; build: string } | null;
  models: SidecarModel[];
  cache_dir: string;
}

export type LlmPrivacy = "off" | "passage" | "values";

export interface LlmSettings {
  base_url: string;
  model: string;
  privacy: LlmPrivacy;
  // The *name* of the environment variable holding the key, read by the
  // server; the key itself never passes through the browser.
  api_key_env?: string;
}

export interface LlmStatusDto {
  settings: LlmSettings | null;
  key_present: boolean;
  redact_values: boolean;
}

export type WsEvent =
  | {
      type: "hello";
      dir: string;
      redact_values: boolean;
      workbooks: WorkbookDto[];
      llm: LlmStatusDto;
      sidecar: SidecarStatus;
    }
  | { type: "llm"; status: LlmStatusDto }
  | { type: "sidecar"; status: SidecarStatus }
  | { type: "agent_step"; session_id: string; step: AgentStepDto }
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
