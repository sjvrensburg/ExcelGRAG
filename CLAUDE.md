# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ExcelGRAG turns spreadsheets (xlsx/xlsm/xlsb/xls/ods) into a queryable property
graph so an agent can search a workbook in words and ground the answer in
specific cells. Rust, because the target is XLSB: a binary workbook of hundreds
of megabytes and tens of millions of populated cells, which no Python library
reads with formulas.

`docs/design.md` is the design document — it carries the reasoning behind
nearly every decision below, and is worth reading before changing anything
structural. The measurements that reasoning rests on are deliberately not in it
(see **Confidential workbooks**); re-run the examples to see them.

`README.md` is a README: what the project is, how to install and run it, and
where the design document is. Keep it that way — the reasoning, the discarded
approaches and the measurements that settled a constant go in `docs/design.md`,
and the working rules go here.

## Commands

```sh
cargo build --release            # release matters: debug builds are unusably slow on real workbooks
cargo test --workspace
cargo test -p eg-eval --test calc                 # one test file
cargo test -p eg-eval --test calc -- arithmetic_follows_excels_precedence # one test
cargo clippy --workspace --all-targets
cargo fmt
```

The front door is the `eg` binary (`crates/eg-cli`), ten verbs in the order a
question travels:

```sh
cargo run --release -p eg-cli -- index corpus/ book.xlsb   # ingest, store graph, index
cargo run --release -p eg-cli -- ask corpus/ bad debt provision
cargo run --release -p eg-cli -- search corpus/ bad debt --limit 3
cargo run --release -p eg-cli -- cells book.xlsb 'LOOKUP!AE53:AG89'
cargo run --release -p eg-cli -- where book.xlsb 1612          # which cells hold it
cargo run --release -p eg-cli -- trace book.xlsb 'LOOKUP!AE53' --dependents
cargo run --release -p eg-cli -- check book.xlsb           # sweep: do formulas still agree
cargo run --release -p eg-cli -- what-if book.xlsb 'RATES!B4=0.15'
cargo run --release -p eg-cli -- serve corpus/             # MCP over stdio
```

`eg-fixtures` generates the demo workbook every one of those can be run
against — a fictional distributor's trade debtor impairment, deterministic
from a fixed seed. Its *structure* mirrors the workbook this project was built
against, which is the point of it; its subject matter deliberately does not:

```sh
cargo run --release -p eg-fixtures -- --rows 2000 --out tests/fixtures/demo
cargo run --release -p eg-fixtures -- --rows 400000 --out demo --formats xlsx
```

It writes flat ODS with formulas and **no cached values**, and LibreOffice
computes them on conversion, so `eg check` against it compares this evaluator
with one that shares no code. Never fill those values in from `eg-eval` — the
fixture would then agree with us by construction and test nothing. LibreOffice
cannot write XLSB, so that format still rests on the vendor fixtures.

All three formats it can write are committed under `tests/fixtures/demo`, from
one generator run, so they are the same spreadsheet by construction. The `.xls`
is committed because format parity caught issue 9: its formulas once named the
wrong real sheets while their cached values stayed correct. The vendored reader
now fixes that path and parity requires complete agreement with the XLSX twin.

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
  and side-effect free; every other crate speaks these types. It also settles
  **which sheets a reference names** (`ParsedRef::spanned_sheets`, returning a
  `SheetSpan`), because a 3-D reference answered two ways is two answers: the
  graph lifted `Jan:Dec!B2` to every sheet it spans while the cell layer read
  it as `Jan!B2`, and a what-if then reported the other sheets' readers as
  unaffected.
- `eg-ingest` — one entry point, `load()`, for all five formats, plus
  `Capabilities` saying what a format could not provide. calamine exposes **no
  cell styling for any format**, so structural analysis may never depend on
  presentation — only value kinds, blank runs, and formula-shape homogeneity.
  It is also where a format's formula dialect stops: ODS arrives as
  OpenFormula (`of:=VLOOKUP([.D2];[Rates.$A$4:.$B$7];2;FALSE())`, and
  `$Rates.$B$11` for a defined name's target), and `odf::to_a1` translates it,
  so **one formula syntax exists downstream and it is A1**. Left untranslated
  it did not fail loudly — `scan_references` found nothing, so the graph had no
  dependency edges; `to_r1c1_shape` normalised nothing, so every formula was
  its own group; and every cell was refused. What the translation cannot do it
  leaves bracketed and counts into a warning, because a wrong edge is worse
  than a missing formula.
- `eg-structure` — regions (tables/blocks recovered from blank runs and value-kind
  contrast; a region's leading **row-label columns are named** by
  `label_headers` and kept out of `headers`, so the column a table is keyed by
  gets a node without becoming something a caller might total), formula grouping
  (formulas normalised to an R1C1 *shape*, so a
  filled-down column of hundreds of thousands of cells collapses to one node),
  `read_table` (a region as typed columns, kind by strict majority, rows lazy
  and gap-filled) and
  `profile_table` (what a column holds: counts, errors, distinct values where
  there are few, sum/min/max where numeric). Both take **one row-major pass** per
  region — a sheet is an ordered map keyed by (row, column), so asking it for a
  single column costs a probe per row, and doing that per column made indexing
  several times slower before it was noticed.
- `eg-graph` — nodes are aggregates (workbook, sheet, region, column, formula
  group, defined name), not cells. Every cell reference is **lifted** to the
  region containing it, and identical lifted references merge into one edge
  carrying a weight = number of references behind it. `check()` asserts the
  structural invariants and says plainly what they miss; `audit()` covers the
  main omission by re-deriving every dependency edge from the cells and
  comparing, which is the only thing that catches an edge lifted to the *wrong*
  region. `store::Corpus` is a directory (`manifest.json`, `graphs/<blake3>.json`)
  keyed by the blake3 of the source file, plus `profiles/<blake3>.json`, holding
  the region layer *and* the formula groups, up to `MAX_STORED_FORMULA_GROUPS`;
  past that the group layer is dropped in place
  (`BuiltGraph::drop_formula_groups`) and rebuilt on demand. `docs/design.md`
  explains why the old "never store them" measurement was an artifact of the
  pre-fork reader.
- `eg-index` — `TextIndex` (tantivy) and `VectorIndex` (fastembed,
  `bge-small-en-v1.5` via ONNX, full scan, no ANN) over the same node flattening,
  keyed by the same blake3. A column also carries **what it was profiled to
  hold** — the distinct values a profile kept, and the min and max of one whose
  list it abandoned, which are cells; never the sum, which is a number no cell
  holds. Weighed below the node's own name (`VALUES_BOOST`), because a column
  *called* Retail beats one *containing* the word. Only a categorical column's
  values reach the embedded sentence, capped, or a key column's identifiers
  would crowd out its name. **The lexical index therefore holds cell values**,
  on the same terms `profiles/` does: they arrive only through a profile, so
  `--redact-values`/`--no-profiles` keep them out with no separate enforcement,
  and `eg index` rewrites a re-read workbook's documents so a later redacted run
  cannot leave an earlier run's values behind. Rankings are fused by reciprocal rank (`fuse`), not by
  score — BM25 and cosine are not on one scale. The word ranking is weighted
  `LEXICAL_WEIGHT` (2) against the meaning ranking and both are asked 50 deep
  before fusing; both numbers were measured with the answer scorer, not chosen,
  and `RRF_K` stays at 60 because sweeping it changes nothing. Custom tokenizer indexes each run
  whole *and* split at case/letter-digit boundaries (`NetRevenue` → `net`,
  `revenue`, `netrevenue`).
- `eg-retrieve` — `find()`/`find_in()` is the **one** hybrid search — the CLI,
  the MCP server and the scorer all call it, because the server keeping its own
  copy is how the fusion weighting came to be missing from the surface agents
  talk to. Every answer carries `Search::evidence()`, which words of the
  question the corpus knows and which the top result accounts for; that is
  printed above every passage, and a `Verdict::Blind` raises a banner. It is
  deliberately not a confidence score — see `docs/design.md` for the two that
  were tried and discarded. A miss is reported against **what the corpus indexes**,
  never against the workbook: an unmatched word that parses as a number earns a
  sentence saying a column's values are indexed only where its profile kept
  them, and naming the scan (`find_value`, `eg where`) that can settle it —
  otherwise "1612 is not in this corpus" reads as "1612 is not in the workbook",
  which is the exact class of mistake this layer exists to prevent.
  `expand()` walks out from
  hits under a budget, recording for every node which node pulled it in and
  along which edge; `render()` turns the subgraph into a numbered, citable
  passage. Passages carry **no cell values** — they say where to look.
  `tests/answers.rs` is the retrieval floor on a made-up workbook,
  `tests/demo_answers.rs` the same on the generated demo one, and
  `examples/answers.rs` scores any corpus against any question file. The public
  questions are `tests/fixtures/demo/answers.json`; the reference workbook's
  live in `private/answers.json`.
- `eg-eval` — the cell layer the graph dropped, recovered on demand, plus the two
  things that read *across* a table: `query::query` (filter/group/aggregate over
  one `Table`, living here so its arithmetic is the evaluator's — it accumulates
  raw and rounds once, and refuses an ambiguous header or a total over a
  non-numeric column rather than guessing) and `schema::infer_schema` (the
  foreign keys a workbook declares in its `VLOOKUP`/`INDEX-MATCH` formulas; an
  approximate lookup is a *banding* over thresholds, not a key). Also:
  `precedents_of` (cheap, in the formula's own text), `dependents_of` (expensive,
  scans every formula), `cells_holding` (expensive, scans every *cell* — the
  direction nothing indexes, and the answer to a figure the profile was never
  going to keep; formula cells match on their value, numbers through the sheet's
  fifteen digits, text without regard to case, never across types, and the cells
  that only *look* like the probe are counted rather than folded in), and
  `recompute`/`check` which evaluate a formula and compare with the value Excel
  stored. `whatif::what_if` substitutes values
  through an `Overrides` overlay — the workbook is never mutated, since XLSB
  cannot be written — and walks the closure a level per full formula scan. The
  overlay is indexed by column: a what-if's overlay holds every cell it has
  recomputed, not the handful a caller typed, so any range read over it must not
  be a scan. `calc::Evaluator` is the reusable context the walk holds so it is
  not rebuilding a sheet-name map and a lookup index per cell; tell it
  `invalidate` whenever an override changes, or a cached lookup column outlives
  the values behind it.
- `eg-mcp` — MCP server over the whole stack (`workbooks`, `search`, `context`,
  `read_cells`, `precedents`, `dependents`, `find_value`, `recompute`, `tables`,
  `query_table`, `schema`, `what_if`). Hand-written stdio JSON
  protocol, no SDK, because the workspace is synchronous. A failing tool returns a
  *result* with `isError`, never a protocol error.
- `eg-cli` — `eg`.

## Invariants worth not breaking

- **Containment is followed inwards for free, outwards only on request.** The
  explosion in a k-hop walk lives entirely in `CONTAINS` (a wide region has well
  over a hundred columns). Dependencies go both ways, nearest first, heaviest
  within a distance — taking weight before distance silently drops nodes.
- **Recompute never recurses.** Precedents are read as stored values, so a
  disagreement is about one formula, not a chain. No dependency order, no cycle
  detection.
- **A query answer names the cells it was computed over.** Region boundaries
  are inferred, so a totals row swept into a body doubles every sum; the range
  is the only thing that lets a caller notice. For the same reason a query
  refuses an ambiguous column or a non-numeric total rather than producing a
  number nobody can check.
- **A what-if never reports a cell it could not evaluate as unchanged.** A
  blocked formula, and everything reading it, comes back as *no answer*; so does
  a cycle, which is reported rather than iterated to a fixed point. Silently
  keeping the stored value would understate the impact.
- **A miss is a fact about the index, never about the workbook.** Every name in
  a workbook is indexed; a column's *values* are indexed only where its profile
  kept them, so a figure out of a large numeric column is absent by
  construction. Search says "not in what this corpus indexes", and an unmatched
  number earns the sentence that says why and names the scan that settles it.
  `cells_holding` is that scan, and it is exhaustive so that its nil answer is
  worth something.
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

## Licensing

`MIT OR Apache-2.0`, declared in `[workspace.package]` and carried by
`LICENSE-MIT` and `LICENSE-APACHE`. `THIRD-PARTY.md` is generated —
`cargo about generate about.hbs -o THIRD-PARTY.md` — and `about.toml`'s
`accepted` list is a policy: a dependency whose licence is not on it fails the
run. Every licence on that list is permissive; adding a copyleft one to get a
green build would be the wrong fix. Regenerate the notice when dependencies
move.

## The vendored calamine patch

`[patch.crates-io]` in the workspace `Cargo.toml` points calamine at
`vendor/calamine`. The source is committed so every reader fix is reproducible
without an unpublished remote branch, and every one of them rides on it: without
them 70% of a real XLSB workbook's formulas are lost and comparisons are
inverted.
`docs/upstream-issues.md` documents the defects, one numbered section each.
Delete the patch and vendored directory once they land in a published release.

## Testing the reader

Four independent checks, because they fail differently:

1. **Format parity** (`crates/eg-ingest/tests/parity.rs`) — the same logical
   workbook read as `.xlsx` and as `.xlsb` must give identical values *and*
   formulas. Fixtures in `tests/fixtures/vendor/`, authored by real Excel because
   no open-source tool writes XLSB. This cannot catch a defect that reads both
   formats the same way — four of the seven were exactly that.
2. **A second reader** — `eg-ingest --example dump_cells` writes the schema
   `sheet-oracle` (SheetJS, at `../sheet-oracle`) writes, and the two dumps are
   diffed. Diff both readers before committing a change to the XLSB/ingest path.
3. **The lifted edges against the cells** — every dependency edge re-derived
   from the formulas and compared with the graph's, both ways round. `eg index`
   runs this on every workbook before storing it, at a fraction of what the
   build costs, and stores it anyway if it fails, loudly; `eg-graph --example
   lifting` is the same thing on its own. `cargo test -p eg-graph --test audit`
   breaks a correct graph five ways and asserts `check` stays silent about each.

4. **Whether the answers are any good, on a workbook with depth** (`cargo test
   -p eg-retrieve --test demo_answers`) — the same scoring as item 5, against
   the generated demo workbook, from `tests/fixtures/demo/answers.json`. That
   file is also what `eg-retrieve --example answers` reads, so the committed
   floor and the scorer anyone can run are the same questions. Questions carry
   a `known_gap` when retrieval cannot answer them; they stay in the file, are
   still scored, and the suite asserts the misses are exactly those.

5. **The demo workbook, against a second engine** (`cargo test -p eg-eval
   --test demo`, `cargo test -p eg-ingest --test parity`) — thousands of
   formulas whose values LibreOffice computed, asserting no disagreements and
   that the only refusals are the two the fixture plants. It caught three
   reader/schema defects in its first hour; two originated in calamine and are
   recorded in `docs/upstream-issues.md` as issues 9 and 10. Both are fixed in
   the vendored reader. The parity half compares
   the same workbook as `.xlsx` and as `.ods` — the two files come from one
   generator run, so they are the same spreadsheet by construction — down to
   **formula text**, cell for cell. That is the only check on the ODF
   translation that is not its own author's opinion, and the sweep is run over
   both, asserting the same refusals from each and that ODS error cells remain
   errors rather than disappearing as empty strings.

6. **Whether the answers are any good** (`cargo test -p eg-retrieve --test
   answers`, and `eg-retrieve --example answers` against a real corpus) —
   questions with known answers, scored by rank and by whether the rendered
   passage cites the answer. The suite asserts the unanswered set is *exactly*
   the gaps recorded in it, so a new miss fails and a fixed one does too.

The sharpest check on the whole stack is `eg check <workbook>`: recompute every
formula and compare with what Excel cached. That sweep found four reader defects
and one here; it currently agrees on everything it can evaluate, so **any new
disagreement is a regression**.

## Confidential workbooks

`private/` is gitignored in full and holds a real XLSB containing commercial and
personal data. **A corpus's `profiles/` directory holds cell values** — distinct
values, sums, minima — unlike `graphs/`, which holds only structure. So does
`text/`, since those values are indexed: the distinct lists and the numeric
bounds, in an inverted index that can be read back. Both come from the same
place, so both are governed by the same two flags — index with `--redact-values`
for a corpus that must not carry the workbook's contents, or `--no-profiles` for
none at all. Re-running `eg index` with either flag over a corpus built without
them rewrites the documents rather than leaving the old ones in place; a corpus
whose `profiles/` was deleted by hand is *not* scrubbed, and wants a reindex.

The demo workbook is the way out of this: it is synthetic and committed, so its
figures, sheet names and cell values may be quoted freely — in the README, in a
commit message, anywhere. Prefer measuring it when a number needs to be written
down. Its subject matter is deliberately **not** the reference workbook's, and
must stay that way: the structure is what makes it a fair test, and giving it
the real one's domain while calling it "the same shape as the real thing" would
disclose by conjunction what neither sentence says alone. That is not a
hypothetical — it is why the demo was rethemed in 4eaf780.

Never commit a real spreadsheet, and never put anything derived from it into
git. Above all, never state what **sector or industry** it comes from, in
committed text or anywhere public; that constraint is why the paragraph above
exists, and the vocabulary gives it away as readily as the word would.
Otherwise the line is drawn at what identifies: no personal or entity names, no
monetary amounts, and none of its **measurements** in a commit message or in
any committed document — cell and formula counts, node and edge totals,
timings, index sizes, agreement percentages. That covers `README.md` and
`docs/design.md` alike: the design document names *which* measurement settled a
decision, never the number it came out at. Report those on the terminal, where
the person who asked for them is; a commit message says the check passed.
Sheet names appear in committed prose only as consistent pseudonyms.

**The workbook code-names in committed text were reviewed and kept.**
`BP136-6-WORK DOC` (`docs/upstream-issues.md`, `eg-ingest/examples/unquoted_sheets.rs`,
`eg-eval/examples/trace.rs`, `eg-retrieve`), `TR450-6-WORK DOC` (`docs/design.md`),
`BZ200`, `HQ880_20240630`, `GS560` and `INDICATORS` were put to the user before
the repository was made public, along with the fact that `BP136` and `TR450`
name the same sheet two ways and so cannot both be the consistent pseudonym the
rule above asks for. The decision was to publish as they stand. Do not re-open
this, and do not scrub them on your own initiative — they are in the history
back to `e6a0aab` regardless, so a working-tree change would achieve nothing
but noise. Reconciling the two spellings is still worth doing if the file is
being edited anyway; disclosure is not the reason.

`docs/upstream-issues.md` and the calamine reports it links are the deliberate
exception, reviewed and kept on those terms: their measurements and cell values
are the evidence behind live bug reports, and a report without its evidence is
one nobody can act on. Sheet names are pseudonymised in what goes upstream,
which costs nothing — the arithmetic never depended on them. Don't scrub that
file.

To inspect a confidential workbook safely:

```sh
cargo run --release -p eg-ingest --example audit -- private/book.xlsb   # counts and A1 addresses only
```

The embedding model downloads once to `~/.cache/excelgrag/models` (~130 MB);
`EG_MODEL_CACHE` overrides. Nothing about a workbook leaves the machine.
