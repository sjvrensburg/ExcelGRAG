import { useEffect, useRef, useState } from "react";

import { getSearch } from "../api";
import type { HitDto, WorkbookDto } from "../types";

interface Props {
  // The workbook on screen, or null if none is open yet. Scopes the search
  // to it when present — the same corpus-wide fallback `getSearch` already
  // has covers the no-workbook case, and a hit from elsewhere still opens
  // its own workbook via `onSelect`.
  workbook: string | null;
  // Every workbook in the corpus, to name the one a hit came from when the
  // search wasn't scoped to the open workbook — two workbooks with the same
  // sheet names (the demo's .ods and .xlsx twins) otherwise produce
  // identical-looking rows.
  workbooks: WorkbookDto[];
  onSelect: (workbook: string, node: number) => void;
}

const DEBOUNCE_MS = 200;
const LIMIT = 8;

// A lightweight "jump to a node" bar, always in the topbar: distinct from
// the sidebar's search/ask, which carries a scored hit list and a passage.
// This one is just `getSearch` plus a dropdown, for the common case of
// knowing roughly what you're looking for and wanting the graph to jump
// there.
export function QuickSearch({ workbook, workbooks, onSelect }: Props) {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<HitDto[] | null>(null);
  // `Search::evidence()` for the current hits: the vector half always
  // returns *something*, so a nonsense query still gets a full dropdown of
  // low-scored sheets. The sidebar prints the evidence line above every hit
  // list for exactly this reason; the bar must not drop it.
  const [evidence, setEvidence] = useState<{ verdict: string; text: string } | null>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const requestId = useRef(0);

  useEffect(() => {
    const query = q.trim();
    if (!query) {
      setHits(null);
      setEvidence(null);
      setError(null);
      return;
    }
    const id = ++requestId.current;
    const timer = window.setTimeout(() => {
      getSearch({ q: query, workbook: workbook ?? undefined, limit: LIMIT })
        .then((found) => {
          if (id !== requestId.current) return;
          setHits(found.hits);
          setEvidence({ verdict: found.verdict, text: found.evidence });
          setActive(0);
          setError(null);
          setOpen(true);
        })
        .catch((e) => {
          if (id !== requestId.current) return;
          setHits(null);
          setError(e instanceof Error ? e.message : String(e));
          setOpen(true);
        });
    }, DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [q, workbook]);

  useEffect(() => {
    const onClick = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onClick);
    return () => document.removeEventListener("mousedown", onClick);
  }, []);

  const pick = (hit: HitDto) => {
    onSelect(hit.workbook, hit.node);
    setOpen(false);
    setQ("");
    setHits(null);
    setEvidence(null);
  };

  // A blind verdict means no hit was found on any word of the query — what
  // follows is nearest-by-meaning filler, shown dimmed rather than hidden so
  // a near-miss spelling can still be picked.
  const blind = evidence !== null && evidence.verdict !== "full" && evidence.verdict !== "partial";
  const nameOf = (hash: string) =>
    (workbooks.find((w) => w.hash === hash)?.path ?? hash).split("/").pop() ?? hash;

  return (
    <div className="quick-search" ref={rootRef}>
      <input
        className="quick-search-input"
        value={q}
        placeholder="jump to…"
        spellCheck={false}
        onChange={(e) => setQ(e.target.value)}
        onFocus={() => hits && hits.length > 0 && setOpen(true)}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            setOpen(false);
            return;
          }
          if (!hits || hits.length === 0) return;
          if (e.key === "ArrowDown") {
            e.preventDefault();
            setActive((a) => Math.min(a + 1, hits.length - 1));
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setActive((a) => Math.max(a - 1, 0));
          } else if (e.key === "Enter") {
            e.preventDefault();
            pick(hits[active]);
          }
        }}
      />
      {open && (error || (hits && hits.length > 0)) && (
        <ul className={"quick-search-results" + (blind ? " blind" : "")}>
          {error && <li className="quick-search-error-row">{error}</li>}
          {!error && evidence && (
            <li className="quick-search-evidence">
              {blind ? "no match — nearest by meaning only; " : ""}
              {evidence.text}
            </li>
          )}
          {!error &&
            hits!.map((hit, i) => (
              <li key={`${hit.workbook}:${hit.node}`}>
                <button
                  className={"quick-search-hit" + (i === active ? " active" : "")}
                  onMouseEnter={() => setActive(i)}
                  onClick={() => pick(hit)}
                >
                  <span className="hit-top">
                    <span className="hit-kind">{hit.kind}</span>
                    {!workbook && <span className="hit-workbook">{nameOf(hit.workbook)}</span>}
                    <span className="hit-score">{hit.score.toFixed(2)}</span>
                  </span>
                  <span className="hit-label">{hit.label}</span>
                  {hit.a1 && <span className="hit-a1">{hit.a1}</span>}
                </button>
              </li>
            ))}
        </ul>
      )}
      {open && hits && hits.length === 0 && !error && (
        <div className="quick-search-empty">nothing matched</div>
      )}
    </div>
  );
}
