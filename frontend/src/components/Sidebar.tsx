import { useEffect, useState } from "react";

import { postIndex, postLlm } from "../api";
import type {
  AskResponse,
  LlmPrivacy,
  LlmSettings,
  LlmStatusDto,
  SearchDto,
  WorkbookDto,
} from "../types";

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
  // The last job's outcome (nonce distinguishes two runs of one path):
  // a success clears the path field if it still holds that path, since the
  // same path re-submitted is the one thing the field can no longer
  // usefully hold. The field stays editable while a job runs, so a path
  // typed in the meantime is not the finished one and is kept.
  indexResult: { path: string; ok: boolean; nonce: number } | null;
  // The chat model as the server reports it (null until the hello event).
  llm: LlmStatusDto | null;
  onLlmChanged: (status: LlmStatusDto) => void;
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

  useEffect(() => {
    const result = props.indexResult;
    if (!result?.ok) return;
    setPath((current) => (current.trim() === result.path ? "" : current));
  }, [props.indexResult]);

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

      <LlmSection status={props.llm} onChanged={props.onLlmChanged} />

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

const PRIVACY_TIERS: { value: LlmPrivacy; label: string; note: string }[] = [
  { value: "off", label: "off", note: "no model call; answers are the rendered passage" },
  {
    value: "passage",
    label: "passage",
    note: "the rendered passage and citations are sent — never a cell value",
  },
  {
    value: "values",
    label: "values",
    note: "the cited cells' contents are sent too — the one mode that lets workbook data leave the machine",
  },
];

// The connection to an OpenAI-chat-completions-compatible endpoint — the
// same four things the `--llm-*` flags set, changeable without restarting
// the server. The key is named, not typed: the field takes the environment
// variable that holds it and the server reads that variable, so a secret
// never crosses the browser and nothing here is persisted anywhere.
function LlmSection({
  status,
  onChanged,
}: {
  status: LlmStatusDto | null;
  onChanged: (status: LlmStatusDto) => void;
}) {
  const current = status?.settings ?? null;
  const [open, setOpen] = useState(false);
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [privacy, setPrivacy] = useState<LlmPrivacy>("off");
  const [keyEnv, setKeyEnv] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Track the server's settings whenever they change underneath the form —
  // a flag at startup, another tab's apply — unless the form is open and
  // being edited.
  useEffect(() => {
    if (open) return;
    setBaseUrl(current?.base_url ?? "");
    setModel(current?.model ?? "");
    setPrivacy(current?.privacy ?? "off");
    setKeyEnv(current?.api_key_env ?? "");
  }, [current, open]);

  const redact = status?.redact_values ?? false;

  const apply = async () => {
    if (busy) return;
    setBusy(true);
    setError(null);
    const settings: LlmSettings = {
      base_url: baseUrl.trim(),
      model: model.trim(),
      privacy,
      ...(keyEnv.trim() ? { api_key_env: keyEnv.trim() } : {}),
    };
    try {
      onChanged(await postLlm(settings));
      setOpen(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const summary = !current || current.privacy === "off"
    ? "off — answers are the rendered passage"
    : `${current.privacy} → ${current.model} at ${current.base_url}` +
      (current.api_key_env ? (status?.key_present ? "" : " (key not found)") : "");

  return (
    <section className="side-section">
      <div className="section-title">
        Chat model
        <button className="link-button section-action" onClick={() => setOpen((o) => !o)}>
          {open ? "close" : "configure"}
        </button>
      </div>
      <div className={"llm-summary" + (current?.privacy === "values" ? " values" : "")} title={summary}>
        {summary}
      </div>
      {open && (
        <div className="llm-form">
          <label>
            base URL
            <input
              value={baseUrl}
              placeholder="http://127.0.0.1:8080/v1"
              onChange={(e) => setBaseUrl(e.target.value)}
              spellCheck={false}
            />
          </label>
          <label>
            model
            <input value={model} placeholder="model name" onChange={(e) => setModel(e.target.value)} spellCheck={false} />
          </label>
          <label>
            API key env var
            <input
              value={keyEnv}
              placeholder="OPENAI_API_KEY (optional)"
              onChange={(e) => setKeyEnv(e.target.value)}
              spellCheck={false}
            />
          </label>
          <div className="llm-privacy">
            {PRIVACY_TIERS.map((tier) => {
              const refused = tier.value === "values" && redact;
              return (
                <label key={tier.value} className={refused ? "refused" : ""} title={tier.note}>
                  <input
                    type="radio"
                    name="llm-privacy"
                    value={tier.value}
                    checked={privacy === tier.value}
                    disabled={refused}
                    onChange={() => setPrivacy(tier.value)}
                  />
                  {tier.label}
                </label>
              );
            })}
          </div>
          <div className="llm-note">{PRIVACY_TIERS.find((t) => t.value === privacy)?.note}</div>
          {redact && (
            <div className="llm-note">values is refused: this server runs --redact-values</div>
          )}
          <div className="llm-note">
            the key is read from that variable in eg gui's own environment; nothing typed here is saved
          </div>
          <div className="index-row">
            <button className="run" onClick={apply} disabled={busy || (privacy !== "off" && !baseUrl.trim())}>
              {busy ? "applying" : "apply"}
            </button>
          </div>
          {error && <div className="error-note">{error}</div>}
        </div>
      )}
    </section>
  );
}
