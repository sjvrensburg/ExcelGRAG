---
name: excelgrag
description: >-
  Ask questions about a spreadsheet (xlsx/xlsm/xlsb/xls/ods) and get an
  answer grounded in specific cells, via the `eg` CLI or the `eg-mcp` / `eg
  serve` MCP server. Use whenever the user wants something *found* or
  *computed* in a workbook — "what's the bad debt provision", "which cells
  feed this total", "does this formula still check out", "what happens if I
  change this rate" — rather than opening the file and reading it by eye.
  Built for workbooks too large to load into a spreadsheet app or an LLM's
  context: a 170 MB / 43M-cell XLSB is the reference case. Covers the
  index/search/ask workflow, checking a figure is grounded in a real cell
  (`eg where` / `find_value`) when search cannot reach it, the evidence
  system that stops a guess from reading like an answer, and the refusals
  (ambiguous column, unmodelled function, whole-column reference) that are
  findings, not bugs.
---

# ExcelGRAG

Turns a spreadsheet into a queryable property graph, so a question about it
can be answered in words and the answer checked against the exact cells it
came from. The reason this exists rather than "read the file": a real
workbook is gigabytes and millions of cells, and no context window holds
that — so the questions have to run *against* the file, not against a
summary of it typed out beforehand.

Two front doors to the same engine, pick whichever fits how you were
invoked:

- **`eg` (CLI)** — a shell binary. Use this if you have shell access.
- **`eg-mcp` / `eg serve`** — the same calls as MCP tools, for a client wired
  up as an MCP server rather than given a shell.

Everything below applies to both; CLI verb names and MCP tool names differ
in a few places (e.g. `cells` vs `read_cells`) and are cross-referenced.

## The workflow

1. **Index once.** `eg index <corpus-dir> <workbook-file>` reads the
   workbook, builds the graph, and writes a lexical + (optionally) semantic
   index into `<corpus-dir>`. This is the only verb allowed to create a
   corpus — every other verb refuses a directory that has never been
   indexed, rather than silently answering NOTHING MATCHED against an empty
   one. Re-run `eg index <corpus-dir>` with no workbook argument to bring
   the indexes up to date after something changed; add `--reindex` to force
   a workbook already in the corpus to be re-read.
2. **Search or ask, as many times as needed.** `search` (CLI) / `search`
   (MCP) lists ranked hits with citations, cheaply. `ask` (CLI) / `context`
   (MCP) does the same and then walks the graph out to what explains each
   hit, rendering a numbered, citable passage — this is usually the better
   first move for an actual question. Neither carries cell values, only
   structure and citations: **read the evidence line before trusting
   anything below it** (next section).
3. **Drop to cells for the actual numbers.** A citation like
   `'Q3 Sales'!B2:D40` is *where to look*, not what is there. Follow up
   with `cells` (CLI) / `read_cells` (MCP) to read the real values and
   formulas before quoting a number in an answer.
4. **`where` (CLI) / `find_value` (MCP)** to check a figure. Search covers
   labels, headers, sheet names, formula text — and a column's *values*
   only where its profile kept them (a few dozen distinct values at most,
   plus the minimum and maximum of a large numeric column). So searching
   for a number out of a big numeric column comes back blind, correctly and
   unhelpfully. `eg where <workbook|corpus> 1612` scans every populated
   cell and cites each one holding it. Expensive, like `dependents`, and
   exhaustive — a nil answer from it means the value genuinely is not
   there, which is what makes it worth the scan.
5. **`trace` / `precedents` / `dependents`** for provenance: what a cell's
   formula reads (cheap — it's in the formula text) or what reads a cell
   (expensive — every formula in the workbook gets scanned).
6. **`check` / `recompute`** to verify arithmetic still holds: recomputes a
   formula from the stored values of the cells it reads and compares
   against what the workbook cached. Refuses by name what it cannot model
   (a handful of Excel functions, whole-column references, volatile
   functions like `TODAY()`) rather than guessing — see Refusals below.
7. **`what-if` / `what_if`** for scenario analysis: substitute a value into
   a cell (in memory only — nothing is written, XLSB can't be written
   anyway) and see every cell that moves, and how far the walk got before
   giving up.
8. **`tables` → `query_table`** and **`schema`** for tabular questions —
   "total X where Y", "which orders are late" — and for following a lookup
   from one table to another. `query_table` needs a table's exact range
   from `tables` first; it does not guess at table boundaries.

Cheap vs expensive, roughly: `search`/`context`/`cells`/`precedents` are
fast (index reads or formula-text reads). `dependents`/`what_if`/`check`
scan every formula in the workbook, and `where`/`find_value` scans every
cell — seconds to tens of seconds on a large file, not milliseconds. Don't
call the expensive ones speculatively in a loop.

## Read the evidence — this is the part that actually matters

Every search-backed answer (`ask`/`context`, and `search` itself) carries a
verdict about what the top result was actually found on, because a passage
that missed the right table reads exactly like one that found it. **Always
read this before using the passage as a source of fact:**

- **Full** — the top result carries every content word of the question. No
  banner; safe to use.
- **Partial** — carries some of them. No banner either (a warning on most
  answers is a warning on none) — the nuance is in the evidence line itself,
  e.g. `Matched: the top result matches "debt", "aged" of 3; "buckets"
  matched elsewhere`. Read it; a word that "matched elsewhere" means a
  *different* part of the corpus knows that word, not this passage.
- **Blind** — banner `BLIND MATCH: ...`. Either none of the question's words
  are in what this corpus indexes, or the best result was found on something
  else entirely. **Treat the passage as a guess, not an answer** — say so, or
  go look elsewhere (`workbooks` shows what is actually indexed).
- **Nothing** — banner `NOTHING MATCHED.` Say so plainly; do not invent a
  number to fill the gap.

**A missing number is not an absent number.** When the word that missed is a
bare figure, both banners add a sentence saying so and naming the scan:
*"A number can be in a cell and in no index — scan the cells (`find_value`,
`eg where`)."* Read that literally. The index carries a column's values only
where its profile kept them, so a figure out of a large numeric column is
absent from the index by construction, and reporting it as absent from the
*workbook* would be exactly the confident wrong answer this whole evidence
system exists to stop. Run the scan before saying a figure is not there.
- **No content words** — every word of the question was a frame word ("how
  is the total", "what does this show") with nothing to search on. Ask
  again with the specific term.

A word can also come back **uncertain** rather than confirmed either way —
too common in this corpus to probe deep enough to say for sure whether the
top result carries it. That's stated in the evidence line too, and is
deliberately *not* counted against the verdict: don't read "uncertain" as
"missing."

The same discipline applies to `recompute`/`check`: a formula it could not
model is reported as **unsupported by name**, never silently treated as
agreeing. And to `what_if`: a cell it could not evaluate — because the
formula uses something unmodelled, or a cycle was hit — is reported
**Blocked**, never silently assumed unchanged. If a `what_if` report says it
`stopped at <n> cells`, the walk hit its budget or level limit; results past
that point are simply not there, not zero.

## Refusals are findings, not bugs

This project is built around refusing an ambiguous or unsupported case
rather than guessing at an answer that would look as confident as a right
one. When one of these fires, it is telling you something true about the
workbook — don't work around it by re-asking a slightly different way
until something answers:

- **Whole-column/row references** (`SUM(A:A)`, `A:A` alone) are refused by
  `recompute`/`check`/`what_if`, not evaluated. On the reference workbook
  this is the single largest "unsupported" bucket in `check` — expected, not
  a defect.
- **`query_table`** refuses a header that names two columns ("which
  `Total`?") and a total requested over a column that isn't numeric, rather
  than picking one or skipping the non-numeric cells. Both are real problems
  with the workbook's structure.
- **`schema`** reports an *approximate* lookup (`VLOOKUP(...,TRUE)` or
  `MATCH(...,1)`) as a **banding** — a set of thresholds, not an equality
  key — because treating it as a join key would silently be wrong for every
  row that doesn't land exactly on a threshold.
- **An unmodelled or volatile function** (`TODAY()`, and a few dozen others)
  is refused by name in `recompute`/`check`/`what_if`. "Unsupported" here
  means exactly that function, not "something went wrong."

## Citations, values, and confidentiality

A citation (`'Sheet Name'!B2:D40`) names a range, never a value — `ask` and
`search` passages are deliberately built this way so they don't go stale
the moment a cell changes and so a corpus can be shared without its data.
To get an actual number, always go to `cells`/`read_cells`.

If the corpus or server was started with `--redact-values` /
`redact_values`, every value comes back as its kind (`<number>`, `<text>`)
instead of the value itself, and formula literals are redacted the same
way (`IF(A2=<text>,B2*<number>,0)`) while references stay visible. This is
normal for a confidential workbook — don't try to reconstruct the real
value from context, and don't be surprised that formulas still show their
structure.

A workbook is identified by its content hash everywhere (`workbooks` lists
it); most tools also accept the stored path or a bare filename for
`workbook`, resolving it the same way — ambiguous if it matches more than
one, an error if it matches none.

## Exact syntax

Flags and MCP argument schemas are intentionally not repeated in full here
— they're one lookup away and change slightly over time:

- CLI: `eg <verb> --help`, or `eg --help` for the verb list.
- MCP: the `tools/list` response carries every tool's full JSON schema and
  description live from the running server.

Verb ↔ tool cross-reference, for translating between the two:

| CLI verb            | MCP tool       | What it does |
|----------------------|----------------|--------------|
| `index`              | *(none — index before serving)* | Build/update a corpus |
| `search`             | `search`       | Ranked hits with citations |
| `ask`                | `context`      | Hits, expanded and rendered as a passage |
| `workbooks`          | `workbooks`    | What's in the corpus |
| `cells`              | `read_cells`   | Values and formulas in a range |
| `where`              | `find_value`   | Which cells hold a value, by scanning them |
| `trace` (no `--dependents`) | `precedents` | What a formula reads |
| `trace --dependents` | `dependents`   | What reads a range |
| `check`              | `recompute`    | Does the arithmetic still hold |
| `what-if`            | `what_if`      | What moves if a cell changes |
| *(no CLI equivalent)* | `tables`      | A workbook's tables and columns |
| *(no CLI equivalent)* | `query_table` | Filter/group/aggregate a table |
| *(no CLI equivalent)* | `schema`      | Foreign keys the lookups declare |
| `serve`              | *(is the server)* | Run the MCP server |

`query_table`/`tables`/`schema` are MCP-only today; from a shell, reach the
same query engine through `eg-eval`'s library or by scripting `cells`
output. `find_value` scans one workbook; `eg where` also takes a corpus
directory and scans every workbook in it, which is deliberately not offered
over MCP — a corpus of large files is minutes of scanning per call.

## Starting a session

```sh
# CLI, once per workbook:
eg index corpus/ book.xlsb
eg ask corpus/ bad debt provision
eg cells book.xlsb "'Provision'!B12:D12"
eg check book.xlsb

# MCP, already indexed:
claude mcp add excelgrag -- /path/to/eg-mcp /path/to/corpus
# or, as a subcommand of eg, no second binary:
eg serve corpus/
```

`eg check <workbook>` exits `2` if any formula disagreed with its stored
value (CI-friendly — a disagreement is always worth investigating) and `1`
on a tool error; `0` means clean or nothing to check.
