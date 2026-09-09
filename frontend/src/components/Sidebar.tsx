import { useState } from "react";

import { postIndex } from "../api";
import type { AskResponse, SearchDto, WorkbookDto } from "../types";

interface Props {
  dir: string;
  workbooks: WorkbookDto[];
  currentHash: string | null;
  redactValues: boolean;
  onOpenWorkbook: (hash: string) => void;
  onSearch: (q: string) => void;
  onAsk: (q: string) => void;
  busy: boolean;
  search: SearchDto | null;
  ask: AskResponse | null;
  mode: "search" | "ask" | null;
  onHit: (workbook: string, node: number) => void;
  logs: string[];
  indexing: boolean;
  onIndexStarted: () => void;
}

export function Sidebar(props: Props) {
  const {
    dir,
    workbooks,
    currentHash,
    onOpenWorkbook,
    onSearch,
    onAsk,
    busy,
    search,
    ask,
    mode,
    onHit,
    logs,
    onIndexStarted,
  } = props;

  const [q, setQ] = useState("");
  const [path, setPath] = useState("");
  const [profiles, setProfiles] = useState(true);
  const [lexicalOnly, setLexicalOnly] = useState(false);
  const [indexError, setIndexError] = useState<string | null>(null);

  const submit = (what: "search" | "ask") => {
    const query = q.trim();
    if (!query || busy) return;
    if (what === "search") onSearch(query);
    else onAsk(query);
  };

  const submitIndex = async () => {
    const target = path.trim();
    if (!target || props.indexing) return;
    setIndexError(null);
    try {
      await postIndex({ path: target, profiles, lexical_only: lexicalOnly });
      onIndexStarted();
    } catch (e) {
      setIndexError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <aside className="side">
      <div className="side-head">
        <span className="wordmark">ExcelGRAG</span>
        <span className="corpus-path" title={dir}>
          {dir}
          {props.redactValues ? " · values redacted" : ""}
        </span>
      </div>

      <section className="side-section">
        <div className="section-title">Workbooks</div>
        {workbooks.length === 0 && (
          <div className="empty-note">
            The corpus is empty. Index a workbook below, or run
            <code> eg index</code> in a terminal.
          </div>
        )}
        <ul className="workbook-list">
          {workbooks.map((w) => (
            <li key={w.hash}>
              <button
                className={
                  "workbook" + (w.hash === currentHash ? " current" : "")
                }
                onClick={() => onOpenWorkbook(w.hash)}
              >
                <span className="workbook-name">
                  {fileName(w.path) || w.path}
                </span>
                <span className="workbook-stats">
                  {w.sheets} sheets · {fmt(w.cells)} cells · {fmt(w.nodes)}{" "}
                  nodes · {fmt(w.edges)} edges
                </span>
              </button>
            </li>
          ))}
        </ul>
      </section>

      <section className="side-section">
        <div className="section-title">Index a workbook</div>
        <div className="index-row">
          <input
            value={path}
            placeholder="/path/to/workbook.xlsb"
            onChange={(e) => setPath(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submitIndex()}
            spellCheck={false}
          />
          <button
            className="run"
            onClick={submitIndex}
            disabled={!path.trim() || props.indexing}
          >
            {props.indexing ? "working" : "index"}
          </button>
        </div>
        <div className="index-flags">
          <label>
            <input
              type="checkbox"
              checked={profiles}
              onChange={(e) => setProfiles(e.target.checked)}
            />
            profile columns
          </label>
          <label>
            <input
              type="checkbox"
              checked={lexicalOnly}
              onChange={(e) => setLexicalOnly(e.target.checked)}
            />
            words only
          </label>
        </div>
        {indexError && <div className="error-note">{indexError}</div>}
      </section>

      <section className="side-section grow">
        <div className="section-title">Ask</div>
        <div className="query-row">
          <input
            value={q}
            placeholder="bad debt provision"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit("ask");
            }}
            spellCheck={false}
          />
        </div>
        <div className="query-actions">
          <button
            className="run"
            onClick={() => submit("search")}
            disabled={!q.trim() || busy}
          >
            search
          </button>
          <button
            className="run primary"
            onClick={() => submit("ask")}
            disabled={!q.trim() || busy}
          >
            ask
          </button>
        </div>

        {busy && <div className="result-note">asking the corpus…</div>}

        {mode === "search" && search && (
          <div className="results">
            <div className="result-note">
              {search.verdict} — {search.evidence}
            </div>
            {search.hits.length === 0 && (
              <div className="empty-note">nothing matched</div>
            )}
            <ul className="hit-list">
              {search.hits.map((hit, i) => (
                <li key={i}>
                  <button
                    className="hit"
                    onClick={() => onHit(hit.workbook, hit.node)}
                  >
                    <span className="hit-top">
                      <span className="hit-kind">{hit.kind}</span>
                      <span className="hit-score">{hit.score.toFixed(2)}</span>
                    </span>
                    <span className="hit-label">{hit.label}</span>
                    {hit.a1 && <span className="hit-a1">{hit.a1}</span>}
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}

        {mode === "ask" && ask && (
          <div className="results">
            <div className="result-note">
              {ask.search.verdict} — {ask.search.evidence}
            </div>
            {ask.workbooks.map((workbook) => (
              <div key={workbook.hash} className="passage-block">
                {workbook.truncated && (
                  <div className="result-note">budget stopped the walk</div>
                )}
                <pre className="passage">{ask.passage.text}</pre>
              </div>
            ))}
            {ask.search.unmatched.length > 0 && (
              <div className="result-note">
                not in this corpus: {ask.search.unmatched.join(", ")}
              </div>
            )}
          </div>
        )}
      </section>

      <section className="side-section log-section">
        <div className="section-title">Log</div>
        <div className="log">
          {logs.length === 0 && <div className="log-line dim">listening…</div>}
          {logs.slice(-200).map((line, i) => (
            <div key={i} className="log-line">
              {line}
            </div>
          ))}
        </div>
      </section>
    </aside>
  );
}

function fileName(path: string): string {
  const slash = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return slash >= 0 ? path.slice(slash + 1) : path;
}

function fmt(n: number): string {
  return n.toLocaleString("en-US");
}
