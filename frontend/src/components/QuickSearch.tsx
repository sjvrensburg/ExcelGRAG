import { useEffect, useRef, useState } from "react";

import { getSearch } from "../api";
import type { HitDto } from "../types";

interface Props {
  // The workbook on screen, or null if none is open yet. Scopes the search
  // to it when present — the same corpus-wide fallback `getSearch` already
  // has covers the no-workbook case, and a hit from elsewhere still opens
  // its own workbook via `onSelect`.
  workbook: string | null;
  onSelect: (workbook: string, node: number) => void;
}

const DEBOUNCE_MS = 200;
const LIMIT = 8;

// A lightweight "jump to a node" bar, always in the topbar: distinct from
// the sidebar's search/ask, which carries a scored hit list and a passage.
// This one is just `getSearch` plus a dropdown, for the common case of
// knowing roughly what you're looking for and wanting the graph to jump
// there.
export function QuickSearch({ workbook, onSelect }: Props) {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<HitDto[] | null>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const requestId = useRef(0);

  useEffect(() => {
    const query = q.trim();
    if (!query) {
      setHits(null);
      setError(null);
      return;
    }
    const id = ++requestId.current;
    const timer = window.setTimeout(() => {
      getSearch({ q: query, workbook: workbook ?? undefined, limit: LIMIT })
        .then((found) => {
          if (id !== requestId.current) return;
          setHits(found.hits);
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
  };

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
        <ul className="quick-search-results">
          {error && <li className="quick-search-error-row">{error}</li>}
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
