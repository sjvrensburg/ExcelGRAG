import type {
  AskResponse,
  ChatTurnDto,
  GraphDto,
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
}

function query(params: QueryParams, extra: Record<string, string> = {}): string {
  const search = new URLSearchParams({ q: params.q, ...extra });
  if (params.workbook) search.set("workbook", params.workbook);
  if (params.sheet) search.set("sheet", params.sheet);
  if (params.lexicalOnly) search.set("lexical_only", "true");
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

// The shared chat session: a human's turn here and an agent's `chat` MCP
// tool call run the same pipeline server-side and land in the same log,
// broadcast to every tab as a `chat_turn` WsEvent.
export function postChat(message: string, sessionId?: string): Promise<ChatTurnDto> {
  return json(fetch("/api/chat", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ message, session_id: sessionId }),
  }));
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
