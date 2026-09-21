import type {
  AskResponse,
  ChatTurnDto,
  GraphDto,
  LlmSettings,
  LlmStatusDto,
  SidecarInfo,
  NodeDetailDto,
  SearchDto,
  WorkbookDto,
} from "./types";

async function json<T>(request: Promise<Response>): Promise<T> {
  const response = await request;
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body && typeof body.error === "string") message = body.error;
    } catch {
      // not JSON; keep the status line
    }
    throw new Error(message);
  }
  return response.json() as Promise<T>;
}

export function getWorkbooks(): Promise<{ dir: string; redact_values: boolean; workbooks: WorkbookDto[] }> {
  return json(fetch("/api/workbooks"));
}

export function getGraph(hash: string): Promise<GraphDto> {
  return json(fetch(`/api/graph/${encodeURIComponent(hash)}`));
}

export function getNodeDetail(hash: string, id: number): Promise<NodeDetailDto> {
  return json(fetch(`/api/graph/${encodeURIComponent(hash)}/node/${id}`));
}

export interface QueryParams {
  q: string;
  workbook?: string;
  sheet?: string;
  lexicalOnly?: boolean;
  limit?: number;
}

function query(params: QueryParams, extra: Record<string, string> = {}): string {
  const search = new URLSearchParams({ q: params.q, ...extra });
  if (params.workbook) search.set("workbook", params.workbook);
  if (params.sheet) search.set("sheet", params.sheet);
  if (params.lexicalOnly) search.set("lexical_only", "true");
  if (params.limit) search.set("limit", String(params.limit));
  return search.toString();
}

export function getSearch(params: QueryParams): Promise<SearchDto> {
  return json(fetch(`/api/search?${query(params)}`));
}

export function getAsk(params: QueryParams): Promise<AskResponse> {
  return json(fetch(`/api/ask?${query(params)}`));
}

export async function postIndex(body: {
  path: string;
  lexical_only?: boolean;
  profiles?: boolean;
}): Promise<void> {
  await json(fetch("/api/index", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  }));
}

export function getLlm(): Promise<LlmStatusDto> {
  return json(fetch("/api/llm"));
}

// `null` turns the model off. The server applies the same checks the
// startup flags get (values under --redact-values, a missing key
// variable) and answers 400 with the reason.
export function postLlm(settings: LlmSettings | null): Promise<LlmStatusDto> {
  return json(fetch("/api/llm", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(settings),
  }));
}

// The shared chat session: a human's turn here and an agent's `chat` MCP
// tool call run the same pipeline server-side and land in the same log,
// broadcast to every tab as a `chat_turn` WsEvent. `context`, when given,
// names the node the user had selected on the canvas — it settles which
// entity the turn is about, overriding both free-text search and any
// session scope carried from an earlier turn (see chat::EntityContext).
export function postChat(
  message: string,
  sessionId?: string,
  context?: { workbook: string; node: number },
  toAgent?: boolean,
  investigate?: boolean,
): Promise<ChatTurnDto> {
  return json(fetch("/api/chat", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      message,
      session_id: sessionId,
      workbook: context?.workbook,
      node: context?.node,
      to_agent: toAgent,
      investigate,
    }),
  }));
}

// The bundled model: what the manifest offers, what is on disk, and the
// sidecar's state. Starting one returns as soon as the job is accepted;
// progress arrives as `sidecar` WsEvents.
export function getSidecar(): Promise<SidecarInfo> {
  return json(fetch("/api/sidecar"));
}

export function postSidecar(model: string): Promise<SidecarInfo> {
  return json(fetch("/api/sidecar", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model }),
  }));
}

export function deleteSidecar(): Promise<SidecarInfo> {
  return json(fetch("/api/sidecar", { method: "DELETE" }));
}

export function getChatHistory(sessionId: string): Promise<{ session_id: string; turns: ChatTurnDto[] }> {
  return json(fetch(`/api/chat/${encodeURIComponent(sessionId)}`));
}

// One WebSocket for the app's lifetime, reconnecting with a pause. Callbacks
// fire for every event; `status` reports the connection itself.
export function connectEvents(
  onEvent: (event: WsLike) => void,
  onStatus: (connected: boolean) => void,
): () => void {
  let socket: WebSocket | null = null;
  let closed = false;
  let retry: number | undefined;

  const open = () => {
    const protocol = location.protocol === "https:" ? "wss:" : "ws:";
    socket = new WebSocket(`${protocol}//${location.host}/ws`);
    socket.onopen = () => onStatus(true);
    socket.onmessage = (message) => {
      try {
        onEvent(JSON.parse(message.data));
      } catch {
        // not JSON; nothing to do with it
      }
    };
    const reconnect = () => {
      if (closed) return;
      onStatus(false);
      if (!closed) retry = window.setTimeout(open, 2000);
    };
    socket.onclose = reconnect;
    socket.onerror = () => socket?.close();
  };

  open();
  return () => {
    closed = true;
    if (retry !== undefined) window.clearTimeout(retry);
    socket?.close();
  };
}

// Structural copy of WsEvent that avoids importing the type twice.
type WsLike = import("./types").WsEvent;
