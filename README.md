# ExcelGRAG

Excel → Graph → GraphRAG. Turns a spreadsheet into a queryable property graph,
so an agent can search a workbook in words and ground the answer in specific
cells.

Built for workbooks too large to open: a question runs *against* the file
rather than against a summary of it pasted into a context window. Written in
Rust because the target is XLSB — a binary workbook of hundreds of megabytes
and tens of millions of populated cells. `openpyxl` cannot open XLSB at all,
and `pyxlsb`, the only Python library that can, does not surface formulas.

```console
$ eg index demo/ tests/fixtures/demo/impairment.xlsx
$ eg ask demo/ how is the provision calculated

Matched: the top result matches "provision" of 2; "calculated" not in what this
corpus indexes; 5 of 5 results found by word and by meaning

18 node(s). A `*` marks something the search matched; the rest were reached from
it. Every range below is a live location in this workbook, not a value.

[1]  * column "Provision"   Debtors!J2:J2001
       in: Debtors > A1:M2001
[2]  * column "Provision Percent"   Rates!E4:E8
       in: Rates > D3:E8
[3]  * column "Provision Percent"   Debtors!I2:I2001
       in: Debtors > A1:M2001
...
[5]  * region    Debtors!A1:M2001
       reads: [9] (2,000 refs, another sheet)
       read by: [10] (8 refs, another sheet)
```

A passage carries **no cell values** — it says where to look. Ask for the
numbers by citation, which is what makes the answer checkable. And it says what
it was found on: `"calculated"` is not a word this workbook uses, and the answer
does not pretend otherwise.

## What it does

- **Reads** xlsx, xlsm, xlsb, xls and ods through one entry point, with
  formulas, and says what a format could not provide.
- **Recovers structure** the file does not record: tables, header rows, the
  column a table is keyed by, and columns of filled-down formulas collapsed to
  one shape.
- **Builds a graph** of aggregates — workbook, sheet, region, column, formula
  group, defined name — where every cell reference is lifted to the region
  containing it, so the graph stays smaller than the workbook.
- **Searches** it by word (tantivy) and by meaning (a local ONNX embedding
  model), fusing the two rankings, and **says what the answer was found on** so
  a guess cannot read like an answer.
- **Drops to cells** on demand: what a formula reads, what reads a cell, which
  cells hold a value, whether a formula still agrees with its stored result,
  and what else moves if one number changes.
- **Serves all of it to an agent** over MCP.

Nothing about a workbook leaves the machine. The embedding model is downloaded
once and runs locally.

## Install

Rust 1.85 or later.

```sh
git clone https://github.com/sjvrensburg/ExcelGRAG
cd ExcelGRAG
cargo install --path crates/eg-cli
```

Release builds matter: debug builds are unusably slow on real workbooks.

`eg-ingest` depends on a [forked calamine](https://github.com/sjvrensburg/calamine),
wired in through `[patch.crates-io]` and pinned by `Cargo.lock`. There is
nothing extra to clone — `cargo build` fetches it. Without those fixes most of
a real XLSB workbook's formulas go missing and every comparison is inverted.
Each is submitted upstream on its own branch; delete the `[patch.crates-io]`
section once they land in a published release. Every defect found while
building this is recorded in
[`docs/upstream-issues.md`](docs/upstream-issues.md).

The first question that needs semantic search downloads `bge-small-en-v1.5`
(~130 MB) to `~/.cache/excelgrag/models`; `EG_MODEL_CACHE` overrides the
location, and `--lexical-only` skips it entirely.

## Try it without a workbook

`eg-fixtures` generates a demo workbook — a fictional distributor's trade
debtor impairment, deterministic from a fixed seed, committed under
`tests/fixtures/demo` as `.xlsx`, `.ods` and `.xls` from one generator run — so
they are the same spreadsheet by construction. Everything here runs against it.

```sh
cargo run --release -p eg-fixtures -- --rows 2000 --out tests/fixtures/demo

eg index demo/ tests/fixtures/demo/impairment.xlsx
eg ask demo/ how is the provision calculated
eg where tests/fixtures/demo/impairment.xlsx 1612
```

## Usage

Ten verbs, in the order a question travels.

```sh
eg index corpus/ book.xlsb              # read it, store its graph, index it
eg ask corpus/ bad debt provision       # a question, as a cited passage
eg search corpus/ bad debt --limit 3    # or just what matched
eg workbooks corpus/                    # what is actually indexed

eg cells book.xlsb 'LOOKUP!AE53:AG89'   # the cells behind a citation
eg where book.xlsb 1612                 # which cells hold a value
eg trace book.xlsb 'LOOKUP!AE53' --dependents   # and what reads them
eg check book.xlsb                      # do the formulas still agree
eg what-if book.xlsb 'RATES!BS9=0.175'  # what moves if one changes

eg serve corpus/                        # the same, to an agent over MCP
```

`eg <verb> --help` for the flags. Three worth knowing:

| Flag | Effect |
|---|---|
| `--redact-values` | print each value's *kind* (`<number>`, `<text>`) instead of the value, and redact formula literals. For a workbook whose contents must not leave the machine. |
| `--lexical-only` | skip the embedding model; search by word alone. |
| `--no-profiles` | do not record what the columns hold. |

`eg` and `eg serve` show cell values by default — a person who types `eg cells`
is asking to see the cells. The crate examples do the opposite, and redact
unless asked, because their output ends up in commit messages and READMEs.

### Read the evidence

Every search-backed answer states what it was found on, because a passage that
missed the right table otherwise reads exactly like one that found it.

```
Matched: the top result matches "debt", "aged" of 3; "buckets" not in what
this corpus indexes; 2 of 2 results found by word and by meaning
```

It is deliberately not a confidence score. Where a result carries none of the
question's words there is a banner as well. A miss is always reported against
what the corpus *indexes*, never against the workbook — a column's values are
indexed only where its profile kept them, so a figure out of a large numeric
column is absent from the index by construction, and `eg where` is what
settles it.

### Refusals are findings

An ambiguous or unmodelled case is refused by name rather than guessed at: a
header naming two columns, a total over a column that is not numbers, a
whole-column reference, a volatile function like `TODAY()`, a cycle. Each one
is telling you something true about the workbook.

## Serving it to an agent

`eg-mcp` is an MCP server over the whole stack: a corpus in, twelve tools out.

```sh
eg index corpus/ book.xlsb
claude mcp add excelgrag -- "$(which eg)" serve "$PWD/corpus"
```

| Tool | Answers |
|---|---|
| `workbooks` | what is in this corpus |
| `search` | which parts of a workbook match a question, by word and by meaning |
| `context` | that question, as a cited passage explaining what was found |
| `read_cells` | the formulas and values of a range |
| `precedents` | what a formula reads |
| `dependents` | what reads a cell — the expensive direction, and it says so |
| `find_value` | which cells hold a value, by scanning every one of them |
| `recompute` | whether a formula still agrees with the value stored beside it |
| `tables` | the tables of a workbook and the columns of each, with their types |
| `query_table` | a total, count or average over the rows of one table |
| `schema` | which column keys into which table, read out of the lookup formulas |
| `what_if` | what else moves if a cell held a different number |

Start it with `--redact-values` and every value becomes its kind while formulas
and structure still answer. The policy is set once, at startup, so a caller
cannot talk its way past it.

There is also a [Claude Code skill](.claude/skills/excelgrag/SKILL.md)
describing the workflow, the evidence system and the refusals.

## How it works

Data flows one way through the workspace; each crate depends only on the ones
above it.

| Crate | Purpose |
|---|---|
| `eg-model` | Addressing, cell values, formula scanning, the workbook model |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods through one entry point |
| `eg-structure` | Region detection, header inference, formula grouping, column profiles |
| `eg-graph` | Graph build, reference lifting, invariants, the corpus store |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes |
| `eg-retrieve` | Hybrid search, graph expansion, passage rendering |
| `eg-eval` | Cell provenance, formula evaluation, queries, what-if |
| `eg-mcp` | MCP server |
| `eg-cli` | The `eg` binary |
| `eg-fixtures` | Generates the committed demo workbook |

A corpus is a directory: `manifest.json`, `graphs/<blake3>.json`,
`profiles/<blake3>.json`, and the two indexes, all keyed by the hash of the
source file. `graphs/` holds structure only; `profiles/` and the lexical index
hold cell values, and are what `--redact-values` and `--no-profiles` withhold.

**[`docs/design.md`](docs/design.md) is the design document** — why the graph
is built from aggregates rather than cells, how the region detector decides a
header row, what the two discarded confidence scores were, why there is no
vector database, and the measurements behind every constant.

## Confidential workbooks

Put them in `private/`, which is gitignored in full. To inspect one without
exposing its contents:

```sh
cargo run --release -p eg-ingest --example audit -- private/book.xlsb
```

The audit reports counts, coverage and A1 addresses only — never cell contents.
Sheet names are redacted unless you pass `--show-names`.

For a corpus that must not carry a workbook's data, index with
`--redact-values` (counts and types, no values) or `--no-profiles` (nothing at
all). Re-running `eg index` with either flag over a corpus built without them
rewrites the stored documents rather than leaving the old ones behind.

## Testing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

Six checks, because they fail differently:

1. **Format parity** — the same workbook read as `.xlsx` and as `.xlsb` must
   give identical values *and* formulas.
2. **A second reader** — the cell dump diffed against one written with SheetJS,
   which shares no code with calamine.
3. **The lifted edges against the cells** — every dependency edge re-derived
   from the formulas and compared with the graph's, both ways round. `eg index`
   runs this on every workbook before storing it.
4. **A second engine** — the demo workbook's thousands of formulas were
   computed by LibreOffice, so recomputing them here is a genuine second
   opinion.
5. **Whether the answers are any good** — retrieval scored by rank and by
   whether the rendered passage cites the answer, against questions with known
   answers. Questions retrieval cannot answer stay in the file as recorded
   gaps, and the suite asserts the misses are exactly those.
6. **`eg check <workbook>`** — recompute every formula and compare with what
   Excel cached. The sharpest check on the whole stack.

[`docs/design.md`](docs/design.md#how-this-is-tested) says what each one catches
that the others cannot.

## Status

Early, but complete end to end: a question in words comes back as a cited
passage, a citation follows down to the cells behind it, those cells recompute
and agree, and a changed number propagates. Every crate above is implemented and
tested. Interfaces are still moving; nothing is published to crates.io yet.

## Contributing

Issues and pull requests are welcome.

- `cargo test --workspace`, `cargo clippy --workspace --all-targets` and
  `cargo fmt` all pass before a commit.
- Diff both readers before committing a change to the XLSB or ingest path — see
  [`docs/design.md`](docs/design.md#how-this-is-tested).
- Never commit a real spreadsheet, or anything measured from one. The demo
  workbook is synthetic and committed for exactly this reason: quote its
  figures freely.
- `CLAUDE.md` carries the working rules and the invariants worth not breaking.

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT licence ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work by you, as defined in the Apache-2.0 licence, shall
be dual licensed as above, without any additional terms or conditions.

Every dependency is permissively licensed. The workbooks under
`tests/fixtures/vendor/` come from the calamine test suite and are MIT — see
[`LICENSE-calamine.md`](tests/fixtures/vendor/LICENSE-calamine.md) beside them.
