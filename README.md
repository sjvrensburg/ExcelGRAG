# ExcelGRAG

Excel → Graph → GraphRAG. Turns spreadsheets into a queryable property graph so
agents can explore them and ground answers in specific cells.

Written in Rust, and the reason is XLSB. A 170 MB binary workbook with 43.5
million populated cells loads in about 8 seconds. `openpyxl` cannot open XLSB at
all, and the only Python library that can, `pyxlsb`, does not surface formulas.

## Status

Early, but a workbook now goes all the way to a graph you can search, by word
and by meaning. `eg-model`, `eg-ingest`, `eg-structure`, `eg-graph` and
`eg-index` are implemented and tested; the retrieval, evaluation and MCP layers
are not yet built.

## Workspace

| Crate | Purpose | State |
|---|---|---|
| `eg-model` | Addressing, cell values, workbook model | implemented |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods via calamine | implemented |
| `eg-structure` | Region detection, header inference, formula grouping | implemented |
| `eg-graph` | Graph build, reference lifting, invariants, store | implemented |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes | implemented |
| `eg-retrieve` | Hybrid search, graph expansion, context rendering | stub |
| `eg-eval` | Formula evaluation and what-if | stub |
| `eg-mcp` | MCP server | stub |
| `eg-cli` | Command-line front-end | stub |

## The calamine fork

`eg-ingest` depends on a [forked calamine](https://github.com/sjvrensburg/calamine/tree/xlsb-shared-formulas),
wired in via `[patch.crates-io]` and pinned by `Cargo.lock`. Nothing extra to
clone — `cargo build` fetches it. Each fix is submitted upstream on its own
branch; the workspace points at an `excelgrag` branch carrying all of them,
because `[patch.crates-io]` takes a single source.

The fork fixes two silent bugs in both binary formats: dropped shared and array
formulas, and transposed `>=` / `>`. Without them 70% of the formulas in a real
XLSB workbook go missing, and every comparison is inverted.

A third fix, submitted separately as
[#713](https://github.com/tafia/calamine/pull/713), quotes sheet names that need
quoting: `'Q3 SALES'!A1` was arriving as `Q3 SALES!A1`, reading as a sheet named
`SALES`. On the reference workbook that lost 1,663 real references and
fabricated 1,472 more, because the discarded prefix `BP136` is itself a valid
cell reference.

Submitted upstream as [tafia/calamine#712](https://github.com/tafia/calamine/pull/712).
Once it lands in a published release, delete the `[patch.crates-io]` section.
See `docs/upstream-issues.md`, which also records what a pre-PR review caught.

## Formula grouping

A filled-down column is one idea written ten thousand times. `eg-structure`
collapses it to a single node by normalising each formula to an R1C1 *shape*, so
the graph is built over groups rather than cells.

On a real 170 MB workbook: **6,793,166 formula cells become 464,131 groups**
(14.6x), in 10s, with one group covering 575,005 cells. It also finds cells that
break a pattern — the classic hand-edited row in an otherwise uniform column.

```sh
cargo run --release -p eg-structure --example group -- private/book.xlsb
```

## Region detection

Recovering the tables and blocks a sheet is built from, so an answer can say
"the Revenue column of the Q3 Sales table" rather than "some cells near D5".

It runs without styling, because calamine exposes none for any format — using
blank rows and columns, value-kind contrast, and declared Excel tables where a
format has them. On the real 170 MB workbook: **168 regions across 43.5M cells
in 2.4s**, every populated cell covered by exactly one region.

```sh
cargo run --release -p eg-structure --example regions -- private/book.xlsb
cargo run --release -p eg-structure --example check_regions -- private/book.xlsb
```

## The graph

Sheets, regions, columns and formula groups become nodes; every cell reference is
**lifted** to the region containing it, and identical lifted references merge
into one edge carrying their count. A column of 100,000 formulas reading a lookup
table becomes one edge of weight 100,000 — smaller than 100,000 edges, and a
better answer, because the weight says how much rests on that table.

On the real 170 MB workbook: **464,863 nodes and 926,970 edges in 10.7s**, using
119 MiB. Drop the formula-group nodes and it is **732 nodes, 892 edges, 0.1 MiB**
— with the identical dependency layer, because lifting reads formula cells and
not group nodes.

That dependency layer is 161 edges, standing for 2.6 million references. Most
references never leave the region they are written in.

```sh
cargo run --release -p eg-graph --example graph -- private/book.xlsb
cargo run --release -p eg-graph --example graph -- private/book.xlsb --no-groups
```

The example reports node and edge counts by kind, the reference breakdown,
measured memory, the degree distribution, references to sheets the workbook does
not have, and whether every invariant holds.

## The corpus

The region-level graph of that 170 MB workbook is **122 KB of JSON**, so the
store is a directory rather than a database: `manifest.json` plus one file per
workbook, keyed by the blake3 of the source file. A workbook that has not
changed is a hit however it was copied; one that has changed cannot be.

| | Cold | Warm |
|---|---|---|
| 170 MB workbook | 17.4s | **0.36ms** |

The 6 GB in-memory workbook is never touched to answer a corpus-level question,
which is the whole reason to keep a store. Formula groups are deliberately not
stored — 119 MiB, and wanted only when drilling into one workbook, at which
point they are rebuilt.

```sh
cargo run --release -p eg-graph --example corpus -- index private/*.xlsb  # add
cargo run --release -p eg-graph --example corpus -- index                # list
```

## The lexical index

Search over every node of every stored graph, so a question that arrives as
words — "revenue", "the tax rate" — becomes the handful of nodes worth
traversing from. A hit is a content hash plus a node index, which is exactly
what reopening that graph and expanding outwards needs.

Each node is flattened into its own name, the names of the nodes above it, and
whatever else it holds, and the three are weighed differently. Otherwise every
node on a sheet called Revenue outranks the Revenue column itself. A table also
carries the headers of its columns, so searching for a column finds the table it
belongs to as well.

Tokenisation is the part worth knowing about. A spreadsheet writes `NetRevenue`,
`FY2024` and `Sheet1`, and tantivy's default tokenizer splits on punctuation
only — so `sheet` matches no sheet in the corpus. Each run is indexed whole
*and* split at its case and letter/digit boundaries, then stemmed, so `net`,
`revenue` and `netrevenue` all reach the same column.

On the real 170 MB workbook the region-level index is **732 documents, 0.1 MiB,
built in 0.04s**, and a query returns in **0.4ms**.

```sh
cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
cargo run --release -p eg-index --example search -- index revenue
cargo run --release -p eg-index --example search -- index --kind column net revenue
```

The index is keyed by the same blake3 the corpus is, so reindexing a workbook
replaces it rather than doubling it, and a changed workbook can never match
under its old hash. It prints node labels and A1 ranges, never cell values.

### Indexing the formulas

Formula groups are the layer the corpus deliberately does not store — 119 MiB of
near-identical text. Whether they are worth *indexing* is a different question,
and the answer is yes: **464,302 documents in 1.73s, 35.6 MiB on disk**, and a
search over them still returns in about **10ms**.

```sh
cargo run --release -p eg-index --example formulas -- private/formulas private/book.xlsb vlookup
```

That measurement also found the one thing text relevance cannot do here. Nearly
every formula in a real workbook is a lookup, so `vlookup` matches 463,570
groups with the same score, and which ones surface is then arbitrary. Each node
carries how many cells it stands for, and the score is multiplied by
`1 + log10(1 + cells) / 4` — the same idea as an edge's weight, that how much of
the workbook rests on something is part of how much it matters. The `vlookup`
list now leads with the group covering 195,366 cells. The multiplier tops out
near 2.4x, far below the spread of real text scores, so it orders ties without
ever putting a big irrelevant node above a small exact match.

## The vector index

Words are not always what a question shares with the answer. `recoverability`
matches nothing in the reference workbook lexically — not one node, at any
stemming — while the same query against embeddings returns the column headed
`Indicators of impairment` and the one headed `DEBTOR CLASSIFICATION`. Neither
shares a token with the question.

It runs the other way too. `GS560` is decisive lexically and vague to an
embedder, because an identifier has no meaning to embed: the nearest thing in
vector space to a label is another label.

So neither index is a default and neither is a fallback. Both run, and their
rankings are fused by reciprocal rank — by rank rather than by score, because a
BM25 score of 46 and a cosine of 0.71 are not on one scale, and any constant
that claims to put them there quietly makes the weighting depend on how many
workbooks are indexed.

```sh
cargo run --release -p eg-index --example semantic -- index bad debt written off
cargo run --release -p eg-index --example semantic -- index how old are the outstanding amounts
```

The example prints all three lists for one query, which is the only honest way
to show what fusing bought.

The model is `bge-small-en-v1.5`, run locally through ONNX. It is downloaded
once per machine into `~/.cache/excelgrag/models`, about 130 MB — set
`EG_MODEL_CACHE` to put it elsewhere. After that nothing about a workbook leaves
the machine, which for the workbooks this is built for is not a preference.

### Why there is no vector database

The nodes worth embedding — sheets, tables, columns, defined names — are **732
on the reference workbook**, which at 384 dimensions is **1.2 MiB**. Fifty such
workbooks are 36,600 vectors and 56 MB. A full scan of the real corpus takes
**0.11ms**, and it is exact. An approximate index would add a build step, a
tuning parameter, a recall cliff and a second on-disk format in exchange for
beating a number too small to see, so there is no HNSW here: an array of floats
per workbook and a loop over it.

Formula groups are not embedded. There are 463,570 of them — 713 MB of vectors
and hours of model time to make near-identical formula text searchable by
meaning, when a formula is exact-token text and the lexical index already
covers it.

Embedding the 732 nodes takes **5.3s**, and the batching is why: batches are
padded to the longest text in them, so one wide table, whose document carries
every column header it has, was paying for the 255 short labels batched beside
it. Sorting by length before batching took it from 9.7s to 5.3s.

## Testing

```sh
cargo test --workspace
```

The most important test is format parity: the same logical workbook read as
`.xlsx` and as `.xlsb` must produce identical values *and* formulas. It has
already caught three real bugs — see `docs/upstream-issues.md`.

Fixtures live in `tests/fixtures/vendor` and were authored by real Excel, because
no open-source tool can write XLSB.

## Confidential workbooks

Put them in `private/`, which is gitignored in full. To inspect one without
exposing its contents:

```sh
cargo run --release -p eg-ingest --example audit -- private/book.xlsb
```

The audit reports counts, coverage and A1 addresses only — never cell contents.
Sheet names are redacted unless you pass `--show-names`.
