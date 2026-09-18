import { useCallback, useEffect, useRef, useState } from "react";

import { connectEvents, getAsk, getChatHistory, getGraph, getNodeDetail, getSearch, postChat } from "./api";
import { NO_HIGHLIGHT, GraphView } from "./graph/GraphView";
import type { Filters, Highlight, Selection } from "./graph/GraphView";
import { visibleCounts } from "./graph/visibility";
import { EDGE_KINDS, NODE_KINDS, formatEdgeKind } from "./graph/theme";
import { ChatPanel } from "./components/ChatPanel";
import { DetailsPanel } from "./components/DetailsPanel";
import { Legend } from "./components/Legend";
import { QuickSearch } from "./components/QuickSearch";
import { Sidebar } from "./components/Sidebar";
import type {
  AskResponse,
  ChatTurnDto,
  EdgeDto,
  GraphDto,
  NodeDetailDto,
  SearchDto,
  WorkbookDto,
  WsEvent,
} from "./types";

const DEFAULT_SESSION = "default";

// What the chat box will attach to its next message, from "Explain"/"Ask
// about this" in the details panel. Distinct from canvas selection: a
// selection alone never sends a chat request (the workflow's own rule), and
// this is cleared after one turn rather than sticking silently to every
// follow-up after it.
interface ChatContext {
  workbook: string;
  node: number;
  label: string;
}

export default function App() {
  const [connected, setConnected] = useState(false);
  const [dir, setDir] = useState("");
  const [redactValues, setRedactValues] = useState(false);
  const [workbooks, setWorkbooks] = useState<WorkbookDto[]>([]);
  const [indexing, setIndexing] = useState(false);

  const [graph, setGraph] = useState<GraphDto | null>(null);
  const [graphBusy, setGraphBusy] = useState(false);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [nodeDetail, setNodeDetail] = useState<NodeDetailDto | null>(null);
  const [hoverEdge, setHoverEdge] = useState<EdgeDto | null>(null);
  const [focus, setFocus] = useState<{ id: number; nonce: number } | null>(null);
  const focusNonce = useRef(0);
  // A generation counter guards every selection-triggered fetch: a slow
  // response for a selection the user has since abandoned (clicked B, closed
  // the panel, switched workbooks) is dropped rather than reopening the
  // panel or overwriting what's now shown. Node ids are local to one graph,
  // so `detail.node.id === selected` alone cannot tell a stale cross-graph
  // response apart from a fresh one.
  const selectionGeneration = useRef(0);

  const [filters, setFilters] = useState<Filters>({
    kinds: new Set(NODE_KINDS),
    edgeKinds: new Set(EDGE_KINDS),
    sheet: null,
    sheetMode: "context",
  });
  const [highlight, setHighlight] = useState<Highlight>(NO_HIGHLIGHT);

  const [mode, setMode] = useState<"search" | "ask" | null>(null);
  const [queryBusy, setQueryBusy] = useState(false);
  const [search, setSearch] = useState<SearchDto | null>(null);
  const [ask, setAsk] = useState<AskResponse | null>(null);
  const [queryError, setQueryError] = useState<string | null>(null);
  const [logs, setLogs] = useState<string[]>([]);

  // --- Chat: the shared session an agent's `chat` MCP tool call and this
  // browser's chat box both append to. ------------------------------------
  const [chatTurns, setChatTurns] = useState<ChatTurnDto[]>([]);
  const [chatBusy, setChatBusy] = useState(false);
  const [chatError, setChatError] = useState<string | null>(null);
  const [chatContext, setChatContext] = useState<ChatContext | null>(null);

  useEffect(() => {
    getChatHistory(DEFAULT_SESSION)
      .then((history) => setChatTurns(history.turns))
      .catch(() => undefined);
  }, []);

  const sendChat = useCallback((text: string, toAgent: boolean) => {
    setChatBusy(true);
    setChatError(null);
    const context = chatContext;
    // A directed-at-agent turn carries no engine context of its own — the
    // engine never ran for it — so the canvas selection is dropped along
    // with the free text rather than sent nowhere.
    postChat(
      text,
      DEFAULT_SESSION,
      !toAgent && context ? { workbook: context.workbook, node: context.node } : undefined,
      toAgent,
    )
      .then((turn) => {
        setChatTurns((prev) => appendTurn(prev, turn));
        // Cleared only on success: a failed request leaves the context chip
        // in place so retrying the same message keeps the same entity
        // attached instead of silently falling back to a bare text search.
        setChatContext(null);
      })
      .catch((e) => setChatError(message(e)))
      // A turn directed at the agent returns immediately (no engine, no
      // LLM) — this only ever reflects the network round-trip, never "the
      // agent is thinking"; the turn itself shows "waiting for an agent to
      // answer…" until a reply lands.
      .finally(() => setChatBusy(false));
  }, [chatContext]);

  const log = useCallback((line: string) => {
    setLogs((previous) => [...previous.slice(-499), line]);
  }, []);

  // --- WebSocket -----------------------------------------------------------
  const onWsEvent = useCallback(
    (event: WsEvent) => {
      switch (event.type) {
        case "hello":
          setDir(event.dir);
          setRedactValues(event.redact_values);
          setWorkbooks(event.workbooks);
          break;
        case "corpus":
          setWorkbooks(event.workbooks);
          {
            const open = currentHash.current;
            if (open && event.removed.includes(open)) {
              setGraphAndFilters(null);
            } else if (open && event.changed.includes(open)) {
              void refreshGraph.current?.(open);
            }
            for (const hash of event.added) {
              log(`workbook added: ${name(event.workbooks, hash)}`);
            }
          }
          break;
        case "log":
          log(event.line);
          break;
        case "index_done":
          setIndexing(false);
          log(
            event.ok
              ? `indexed ${event.path}`
              : `indexing ${event.path} failed: ${event.error ?? "unknown error"}`,
          );
          break;
        case "lagged":
          log(`fell behind (${event.skipped} events skipped); refreshing`);
          if (currentHash.current) {
            setGraphBusy(true);
            void getGraph(currentHash.current)
              .then(setGraphAndFilters)
              .catch(() => setGraph(null))
              .finally(() => setGraphBusy(false));
          }
          break;
        case "chat_turn":
          if (event.session_id === DEFAULT_SESSION) {
            setChatTurns((prev) => appendTurn(prev, event.turn));
          }
          break;
        case "navigate":
          if (event.session_id === DEFAULT_SESSION) {
            if (event.workbook && event.workbook !== currentHash.current) {
              pendingAsk.current = () => setFocus({ id: event.node, nonce: ++focusNonce.current });
              openWorkbook(event.workbook);
            } else {
              setFocus({ id: event.node, nonce: ++focusNonce.current });
            }
          }
          break;
      }
    },
    [log],
  );

  useEffect(() => connectEvents(onWsEvent, setConnected), [onWsEvent]);

  // --- Graph loading --------------------------------------------------------
  const currentHash = useRef<string | null>(null);
  const refreshGraph = useRef<((hash: string) => Promise<void>) | null>(null);

  const setGraphAndFilters = useCallback((loaded: GraphDto | null) => {
    setGraph(loaded);
    selectionGeneration.current += 1;
    setSelection(null);
    setNodeDetail(null);
    setHighlight(NO_HIGHLIGHT);
    setChatContext(null);
    currentHash.current = loaded?.hash ?? null;
    if (loaded) {
      setFilters((f) => ({
        kinds: new Set(
          NODE_KINDS.filter((kind) => (loaded.node_kinds[kind] ?? 0) > 0),
        ),
        edgeKinds: new Set(
          EDGE_KINDS.filter((kind) => (loaded.edge_kinds[kind] ?? 0) > 0),
        ),
        sheet: null,
        sheetMode: f.sheetMode,
      }));
    }
  }, []);

  const openWorkbook = useCallback(
    (hash: string) => {
      if (currentHash.current === hash) return;
      setGraphBusy(true);
      getGraph(hash)
        .then(setGraphAndFilters)
        .catch((e) => log(`could not open the graph: ${message(e)}`))
        .finally(() => setGraphBusy(false));
    },
    [log, setGraphAndFilters],
  );

  refreshGraph.current = async (hash: string) => {
    await getGraph(hash)
      .then(setGraphAndFilters)
      .catch((e) => log(`could not reload the graph: ${message(e)}`));
  };

  // --- Selection ------------------------------------------------------------
  const closeSelection = useCallback(() => {
    selectionGeneration.current += 1;
    setSelection(null);
    setNodeDetail(null);
  }, []);

  const selectNode = useCallback(
    (id: number) => {
      const hash = currentHash.current;
      if (!hash) return;
      setFocus({ id, nonce: ++focusNonce.current });
      setSelection({ entity: "node", id });
      setNodeDetail(null);
      const generation = ++selectionGeneration.current;
      getNodeDetail(hash, id)
        .then((detail) => {
          // Dropped, not applied, if the user moved on while this was in
          // flight (another click, a close, a workbook switch) — a late
          // response for A must never reopen or overwrite B's panel.
          if (generation !== selectionGeneration.current) return;
          setNodeDetail(detail);
        })
        .catch((e) => {
          if (generation !== selectionGeneration.current) return;
          log(`could not read the node: ${message(e)}`);
        });
    },
    [log],
  );

  const selectEdge = useCallback((edge: EdgeDto) => {
    selectionGeneration.current += 1;
    setSelection({ entity: "edge", id: edge.id });
    setNodeDetail(null);
  }, []);

  // --- Queries ---------------------------------------------------------------
  const runSearch = useCallback(
    (q: string) => {
      setQueryBusy(true);
      setQueryError(null);
      setMode("search");
      setSearch(null);
      setAsk(null);
      setHighlight(NO_HIGHLIGHT);
      getSearch({ q, workbook: currentHash.current ?? undefined })
        .then((found) => {
          setSearch(found);
          applyHighlightFromSearch(found, currentHash.current, setHighlight);
        })
        .catch((e) => setQueryError(message(e)))
        .finally(() => setQueryBusy(false));
    },
    [],
  );

  const runAsk = useCallback(
    (q: string) => {
      setQueryBusy(true);
      setQueryError(null);
      setMode("ask");
      setSearch(null);
      setAsk(null);
      setHighlight(NO_HIGHLIGHT);
      getAsk({ q, workbook: currentHash.current ?? undefined })
        .then((answer) => {
          setAsk(answer);
          setSearch(answer.search);
          const book =
            answer.workbooks.find((w) => w.hash === currentHash.current) ??
            answer.workbooks[0];
          if (book) {
            const apply = () => {
              const roles = new Map(book.nodes.map((n) => [n.node, n.role]));
              // The exact supporting edges, not "every edge of a kind that
              // shows up anywhere in the roles": reconstruct each retrieved
              // node's edge to the node that pulled it in (`via`), matched
              // against the graph's actual edges in either direction, since
              // a dependency edge's `via` parent can be either endpoint
              // depending on which way the walk crossed it.
              //
              // Two lookups, not one: a reciprocal pair (A depends on B and
              // B depends on A, same kind) would otherwise collide on one
              // Map key and silently lose whichever edge was indexed first —
              // every key here maps to a list, not a single id, so both
              // survive. A containment hop (`Role::Ancestor`/`Child`) carries
              // no `edge_kind` at all (see `dto::retrieved_dto`), so it falls
              // back to "the edge between these two nodes, any kind" instead
              // of being dropped from the highlight outright.
              const edgeIds = new Set<string>();
              if (currentGraph.current) {
                const push = (m: Map<string, string[]>, key: string, id: string) => {
                  const list = m.get(key);
                  if (list) list.push(id);
                  else m.set(key, [id]);
                };
                const byKindPair = new Map<string, string[]>();
                const byPair = new Map<string, string[]>();
                for (const e of currentGraph.current.edges) {
                  push(byKindPair, `${e.source}:${e.target}:${e.kind}`, e.id);
                  push(byKindPair, `${e.target}:${e.source}:${e.kind}`, e.id);
                  push(byPair, `${e.source}:${e.target}`, e.id);
                  push(byPair, `${e.target}:${e.source}`, e.id);
                }
                for (const n of book.nodes) {
                  if (n.via === undefined) continue;
                  const ids = n.edge_kind
                    ? byKindPair.get(`${n.node}:${n.via}:${n.edge_kind}`)
                    : byPair.get(`${n.node}:${n.via}`);
                  ids?.forEach((id) => edgeIds.add(id));
                }
              }
              setHighlight({ roles, edgeIds, workbook: book.hash });
              const seed = book.nodes.find((n) => n.role === "seed");
              if (seed) setFocus({ id: seed.node, nonce: ++focusNonce.current });
            };
            if (book.hash !== currentHash.current) {
              openWorkbook(book.hash);
              // The graph load is async; the highlight applies to it when it
              // lands, via the pending-answer ref.
              pendingAsk.current = apply;
            } else {
              apply();
            }
          }
        })
        .catch((e) => setQueryError(message(e)))
        .finally(() => setQueryBusy(false));
    },
    [openWorkbook],
  );

  // Kept in a ref alongside `graph` state so `runAsk`'s closure (memoized on
  // `openWorkbook` alone) always reads the graph current at apply-time,
  // not the one current when the callback was created.
  const currentGraph = useRef<GraphDto | null>(null);
  currentGraph.current = graph;

  const pendingAsk = useRef<(() => void) | null>(null);
  useEffect(() => {
    if (graph && pendingAsk.current) {
      const apply = pendingAsk.current;
      pendingAsk.current = null;
      apply();
    }
  }, [graph]);

  const clearHighlight = useCallback(() => {
    setHighlight(NO_HIGHLIGHT);
    setMode(null);
    setSearch(null);
    setAsk(null);
  }, []);

  // --- Filters ---------------------------------------------------------------
  const toggleKind = useCallback((kind: string) => {
    setFilters((f) => {
      const kinds = new Set(f.kinds);
      if (kinds.has(kind)) kinds.delete(kind);
      else kinds.add(kind);
      return { ...f, kinds };
    });
  }, []);

  const toggleEdgeKind = useCallback((kind: string) => {
    setFilters((f) => {
      const edgeKinds = new Set(f.edgeKinds);
      if (edgeKinds.has(kind)) edgeKinds.delete(kind);
      else edgeKinds.add(kind);
      return { ...f, edgeKinds };
    });
  }, []);

  const setSheet = useCallback((sheet: string | null) => {
    setFilters((f) => ({ ...f, sheet }));
  }, []);

  const setSheetMode = useCallback((sheetMode: "only" | "context") => {
    setFilters((f) => ({ ...f, sheetMode }));
  }, []);

  const resetFilters = useCallback(() => {
    if (!graph) return;
    setFilters({
      kinds: new Set(NODE_KINDS.filter((kind) => (graph.node_kinds[kind] ?? 0) > 0)),
      edgeKinds: new Set(EDGE_KINDS.filter((kind) => (graph.edge_kinds[kind] ?? 0) > 0)),
      sheet: null,
      sheetMode: "context",
    });
  }, [graph]);

  // Shared by the sidebar's hit list and the topbar's quick search: open the
  // hit's workbook first if it isn't the one on screen, then select the node
  // once that graph has loaded (via `pendingAsk`, the same relay `runAsk`
  // uses for a cross-workbook answer).
  const jumpToHit = useCallback(
    (workbook: string, node: number) => {
      if (workbook !== currentHash.current) {
        pendingAsk.current = () => selectNode(node);
        openWorkbook(workbook);
      } else {
        selectNode(node);
      }
    },
    [openWorkbook, selectNode],
  );

  const askAboutSelection = useCallback(() => {
    const hash = currentHash.current;
    if (!hash || !selection || !graph) return;
    if (selection.entity === "node") {
      const node = graph.nodes.find((n) => n.id === selection.id);
      setChatContext({ workbook: hash, node: selection.id, label: node?.label ?? `node ${selection.id}` });
      return;
    }
    // No edge-scoped entity resolution on the server (see chat::EntityContext
    // — it takes a node, not an edge). The source node — the region whose
    // formula the edge came from — is the closer of the two endpoints to
    // "explain this relationship", and the edge's own description already
    // named both ends deterministically before this button was even shown.
    const edge = graph.edges.find((e) => e.id === selection.id);
    if (!edge) return;
    const source = graph.nodes.find((n) => n.id === edge.source);
    const target = graph.nodes.find((n) => n.id === edge.target);
    const label = `${source?.label ?? edge.source} → ${target?.label ?? edge.target} (${formatEdgeKind(edge.kind)})`;
    setChatContext({ workbook: hash, node: edge.source, label });
  }, [graph, selection]);

  const current = workbooks.find((w) => w.hash === currentHash.current) ?? null;
  // Computed once per render, not once per topbar stat: `visibleCounts` walks
  // every node and edge, and the two figures used to each redo that walk.
  const counts = graph ? visibleCounts(graph, filters) : null;

  return (
    <div className="app">
      <Sidebar
        dir={dir}
        workbooks={workbooks}
        currentHash={currentHash.current}
        redactValues={redactValues}
        onOpenWorkbook={openWorkbook}
        onIndexStarted={() => setIndexing(true)}
        onSearch={runSearch}
        onAsk={runAsk}
        busy={queryBusy}
        search={search}
        ask={ask}
        mode={mode}
        onHit={jumpToHit}
        logs={logs}
        indexing={indexing}
      />

      <main className="main">
        <div className="topbar">
          <div className="topbar-title">
            {current ? fileName(current.path) : graphBusy ? "opening…" : "no workbook open"}
          </div>
          <QuickSearch workbook={currentHash.current} onSelect={jumpToHit} />
          {graph && counts && (
            <div className="topbar-stats">
              {fmt(counts.nodes)}/{fmt(graph.nodes.length)} nodes ·{" "}
              {fmt(counts.edges)}/{fmt(graph.edges.length)} edges
              {!graph.formula_groups && " · formula groups omitted above the storage cap"}
              {(filters.kinds.size < NODE_KINDS.length ||
                filters.edgeKinds.size < EDGE_KINDS.length ||
                filters.sheet !== null) && (
                <button className="link-button" onClick={resetFilters} title="Show every kind, every sheet">
                  reset filters
                </button>
              )}
              {highlight.roles.size > 0 && (
                <button className="link-button" onClick={clearHighlight} title="Clear the answer/search highlight">
                  clear highlight
                </button>
              )}
            </div>
          )}
          <div className={"connection" + (connected ? "" : " down")}>
            {connected ? "live" : "offline"}
          </div>
        </div>
        <div className="stage">
          {graph ? (
            <>
              <GraphView
                graph={graph}
                filters={filters}
                highlight={highlight}
                selection={selection}
                focus={focus}
                onClear={closeSelection}
                onSelectNode={selectNode}
                onSelectEdge={selectEdge}
                onHoverEdge={setHoverEdge}
                onReady={() => undefined}
              />
              <Legend
                graph={graph}
                filters={filters}
                onToggleKind={toggleKind}
                onToggleEdgeKind={toggleEdgeKind}
                onSheet={setSheet}
                onSheetMode={setSheetMode}
              />
              {hoverEdge && !selection && (
                <div className="edge-tooltip">{edgeTooltip(graph, hoverEdge)}</div>
              )}
            </>
          ) : (
            <div className="stage-empty">
              {workbooks.length === 0
                ? "The corpus is empty. Index a workbook in the sidebar, or run eg index in a terminal, and it will appear here."
                : "Select a workbook on the left."}
            </div>
          )}
          {queryError && <div className="stage-error">{queryError}</div>}
        </div>
      </main>

      {selection && graph && (
        <DetailsPanel
          graph={graph}
          selection={selection}
          nodeDetail={selection.entity === "node" ? nodeDetail : null}
          onSelectNode={selectNode}
          onSelectEdge={selectEdge}
          onClose={closeSelection}
          onAskAboutSelection={askAboutSelection}
        />
      )}

      <aside className="side chat-dock">
        <ChatPanel
          turns={chatTurns}
          busy={chatBusy}
          error={chatError}
          onSend={sendChat}
          context={chatContext}
          onClearContext={() => setChatContext(null)}
          redactValues={redactValues}
        />
      </aside>
    </div>
  );
}

// Highlight from a bare search: every hit on the open workbook gets the seed
// color; hits elsewhere stay in the list for their own click.
function applyHighlightFromSearch(
  found: SearchDto,
  openHash: string | null,
  setHighlight: (h: Highlight) => void,
) {
  if (!openHash) return;
  const roles = new Map(
    found.hits
      .filter((hit) => hit.workbook === openHash)
      .map((hit) => [hit.node, "seed"]),
  );
  setHighlight({ roles, edgeIds: new Set(), workbook: openHash });
}

function edgeTooltip(graph: GraphDto, edge: EdgeDto): string {
  const source = graph.nodes.find((n) => n.id === edge.source);
  const target = graph.nodes.find((n) => n.id === edge.target);
  const weight = edge.weight > 1 ? ` ×${edge.weight.toLocaleString("en-US")}` : "";
  return `${source?.label ?? edge.source} — ${formatEdgeKind(edge.kind)}${weight} → ${target?.label ?? edge.target}`;
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// Append or replace by id — a turn this tab just posted arrives twice (the
// POST response, then the broadcast), and the two must collapse into one.
function appendTurn(turns: ChatTurnDto[], turn: ChatTurnDto): ChatTurnDto[] {
  if (turns.some((t) => t.id === turn.id)) {
    return turns.map((t) => (t.id === turn.id ? turn : t));
  }
  return [...turns, turn];
}

function fileName(path: string): string {
  const slash = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return slash >= 0 ? path.slice(slash + 1) : path;
}

function name(workbooks: WorkbookDto[], hash: string): string {
  return fileName(workbooks.find((w) => w.hash === hash)?.path ?? hash);
}

function fmt(n: number): string {
  return n.toLocaleString("en-US");
}
