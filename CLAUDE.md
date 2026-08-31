# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ExcelGRAG turns spreadsheets (xlsx/xlsm/xlsb/xls/ods) into a queryable property
graph so an agent can search a workbook in words and ground the answer in
specific cells. Rust, because the target is XLSB: a 170 MB binary workbook with
43.5M populated cells, which no Python library reads with formulas.

`README.md` is the design document — it carries the reasoning and the measured
numbers behind nearly every decision below, and is worth reading before changing
anything structural.

## Commands

```sh
cargo build --release            # release matters: debug builds are unusably slow on real workbooks
cargo test --workspace
cargo test -p eg-eval --test calc                 # one test file
cargo test -p eg-eval --test calc -- arithmetic_follows_excels_precedence # one test
cargo clippy --workspace --all-targets
cargo fmt
```

The front door is the `eg` binary (`crates/eg-cli`), nine verbs in the order a
question travels:

```sh
cargo run --release -p eg-cli -- index corpus/ book.xlsb   # ingest, store graph, index
cargo run --release -p eg-cli -- ask corpus/ bad debt provision
cargo run --release -p eg-cli -- search corpus/ bad debt --limit 3
cargo run --release -p eg-cli -- cells book.xlsb 'LOOKUP!AE53:AG89'
cargo run --release -p eg-cli -- trace book.xlsb 'LOOKUP!AE53' --dependents
cargo run --release -p eg-cli -- check book.xlsb           # sweep: do formulas still agree
cargo run --release -p eg-cli -- what-if book.xlsb 'RATES!B4=0.15'
cargo run --release -p eg-cli -- serve corpus/             # MCP over stdio
```

Each crate also has `examples/` that exercise its layer directly and print
measurements; those are the development surface (`cargo run --release -p
eg-graph --example graph -- private/book.xlsb`). The CLI deliberately wraps
library calls only — diagnostics (`raw_cells`, `why_unpopulated`, format probes)
stay as examples in the crate they belong to.

## Pipeline

Data flows one way through the workspace; each crate depends only on the ones
above it.

- `eg-model` — addresses (`CellRef`, `RangeRef`, A1 parse/quote), `CellValue`,
  formula scanning (`scan_references`, `to_r1c1_shape`), `Workbook`. Dependency-light
  and side-effect free; every other crate speaks these types.
- `eg-ingest` — one entry point, `load()`, for all five formats, plus
  `Capabilities` saying what a format could not provide. calamine exposes **no
  cell styling for any format**, so structural analysis may never depend on
  presentation — only value kinds, blank runs, and formula-shape homogeneity.
- `eg-structure` — regions (tables/blocks recovered from blank runs and value-kind
  contrast) and formula grouping (formulas normalised to an R1C1 *shape*, so a
  filled-down column of 575k cells collapses to one node).
- `eg-graph` — nodes are aggregates (workbook, sheet, region, column, formula
  group, defined name), not cells. Every cell reference is **lifted** to the
  region containing it, and identical lifted references merge into one edge
  carrying a weight = number of references behind it. `check()` asserts the
  structural invariants and says plainly what they miss; `audit()` covers the
  main omission by re-deriving every dependency edge from the cells and
  comparing, which is the only thing that catches an edge lifted to the *wrong*
  region. `store::Corpus` is a directory (`manifest.json`, `graphs/<blake3>.json`)
  keyed by the blake3 of the source file, holding the region layer *and* the
  formula groups (1,272 of them, 520 KB total) up to
  `MAX_STORED_FORMULA_GROUPS`; past that the group layer is dropped and rebuilt
  on demand. The README explains why the old "119 MiB, never store them" figure
  was an artifact of the pre-fork reader.
- `eg-index` — `TextIndex` (tantivy) and `VectorIndex` (fastembed,
  `bge-small-en-v1.5` via ONNX, full scan, no ANN) over the same node flattening,
  keyed by the same blake3. Rankings are fused by reciprocal rank (`fuse`), not by
  score — BM25 and cosine are not on one scale. Custom tokenizer indexes each run
  whole *and* split at case/letter-digit boundaries (`NetRevenue` → `net`,
  `revenue`, `netrevenue`).
- `eg-retrieve` — `expand()` walks out from hits under a budget, recording for
  every node which node pulled it in and along which edge; `render()` turns the
  subgraph into a numbered, citable passage. Passages carry **no cell values** —
  they say where to look.
- `eg-eval` — the cell layer the graph dropped, recovered on demand:
  `precedents_of` (cheap, in the formula's own text), `dependents_of` (expensive,
  scans every formula), and `recompute`/`check` which evaluate a formula and
  compare with the value Excel stored. `whatif::what_if` substitutes values
  through an `Overrides` overlay — the workbook is never mutated, since XLSB
  cannot be written — and walks the closure a level per full formula scan.
- `eg-mcp` — MCP server over the whole stack (`workbooks`, `search`, `context`,
  `read_cells`, `precedents`, `dependents`, `recompute`, `what_if`). Hand-written stdio JSON
  protocol, no SDK, because the workspace is synchronous. A failing tool returns a
  *result* with `isError`, never a protocol error.
- `eg-cli` — `eg`.

## Invariants worth not breaking

- **Containment is followed inwards for free, outwards only on request.** The
  explosion in a k-hop walk lives entirely in `CONTAINS` (a region has up to 136
  columns). Dependencies go both ways, nearest first, heaviest within a distance —
  taking weight before distance silently drops nodes.
- **Recompute never recurses.** Precedents are read as stored values, so a
  disagreement is about one formula, not a chain. No dependency order, no cycle
  detection.
- **A what-if never reports a cell it could not evaluate as unchanged.** A
  blocked formula, and everything reading it, comes back as *no answer*; so does
  a cycle, which is reported rather than iterated to a fixed point. Silently
  keeping the stored value would understate the impact.
- **Unsupported functions are refused by name, not guessed.** ~50 of Excel's
  functions are modelled; volatile functions (`TODAY()`) are refused too, because
  "differs" would be the wrong verdict.
- **A sheet carries 15 significant digits.** Comparison, `ROUND`, and cancellation
  to exact zero all operate on the number a sheet *shows*, which is why
  `10.13+6.75 == 16.88` here and in Excel but in no language with doubles. See
  `eg-eval/src/calc.rs`.
- **Examples redact cell values by default; `eg` and `eg serve` show them** (and
  take `--redact-values`). The asymmetry is deliberate: example output ends up in
  commit messages and READMEs.

## The calamine fork

`[patch.crates-io]` in the workspace `Cargo.toml` points calamine at
`sjvrensburg/calamine` branch `excelgrag`, pinned by the committed `Cargo.lock`.
Five fixes ride on it (two upstream as #712/#713, three not yet submitted);
without them 70% of a real XLSB workbook's formulas are lost and comparisons are
inverted. `docs/upstream-issues.md` documents all seven defects. Delete the patch
section once they land in a published release. `Cargo.lock` is committed on
purpose — it is what pins the fork to an exact commit.

## Testing the reader

Three independent checks, because they fail differently:

1. **Format parity** (`crates/eg-ingest/tests/parity.rs`) — the same logical
   workbook read as `.xlsx` and as `.xlsb` must give identical values *and*
   formulas. Fixtures in `tests/fixtures/vendor/`, authored by real Excel because
   no open-source tool writes XLSB. This cannot catch a defect that reads both
   formats the same way — four of the seven were exactly that.
2. **A second reader** — `eg-ingest --example dump_cells` writes the schema
   `sheet-oracle` (SheetJS, at `../sheet-oracle`) writes, and the two dumps are
   diffed. Diff both readers before committing a change to the XLSB/ingest path.
3. **The lifted edges against the cells** (`eg-graph --example lifting`) — every
   dependency edge re-derived from the formulas and compared with the graph's,
   both ways round. `cargo test -p eg-graph --test audit` breaks a correct graph
   five ways and asserts `check` stays silent about each.

The sharpest check on the whole stack is `eg check <workbook>`: recompute every
formula and compare with what Excel cached. That sweep found four reader defects
and one here; it currently agrees on 100% of what it can evaluate, so **any new
disagreement is a regression**.

## Confidential workbooks

`private/` is gitignored in full and holds a real 170 MB XLSB containing
commercial and personal data. Never commit a real spreadsheet, and never put
anything derived from it — sheet names, cell values, labels — into git, a commit
message or the README without asking; the README uses consistent pseudonyms for
that reason. To inspect one safely:

```sh
cargo run --release -p eg-ingest --example audit -- private/book.xlsb   # counts and A1 addresses only
```

The embedding model downloads once to `~/.cache/excelgrag/models` (~130 MB);
`EG_MODEL_CACHE` overrides. Nothing about a workbook leaves the machine.
