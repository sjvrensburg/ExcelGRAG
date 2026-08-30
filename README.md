# ExcelGRAG

Excel → Graph → GraphRAG. Turns spreadsheets into a queryable property graph so
agents can explore them and ground answers in specific cells.

Written in Rust, and the reason is XLSB. A 170 MB binary workbook with 43.5
million populated cells loads in about 8 seconds. `openpyxl` cannot open XLSB at
all, and the only Python library that can, `pyxlsb`, does not surface formulas.

Every measurement below is against that workbook. It is confidential, so its
sheet names appear here as pseudonyms — consistently, so a name that recurs is
the same sheet.

## Status

Early, but a question now goes all the way from words to the part of a workbook
that answers it. `eg-model`, `eg-ingest`, `eg-structure`, `eg-graph` and
`eg-index` and `eg-retrieve` are implemented and tested: a question in words
comes back as a cited passage, and `eg-eval` follows a citation down to the
cells behind it and recomputes it from the cells under it. The MCP and CLI
layers are not yet built.

## Workspace

| Crate | Purpose | State |
|---|---|---|
| `eg-model` | Addressing, cell values, workbook model | implemented |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods via calamine | implemented |
| `eg-structure` | Region detection, header inference, formula grouping | implemented |
| `eg-graph` | Graph build, reference lifting, invariants, store | implemented |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes | implemented |
| `eg-retrieve` | Hybrid search, graph expansion, context rendering | implemented |
| `eg-eval` | Cell-level provenance, formula evaluation, what-if | provenance and recompute done |
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
fabricated 1,472 more, because the discarded prefix `TR450` is itself a valid
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

It runs the other way too. `HQ880` is decisive lexically and vague to an
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
on the reference workbook**, which at 384 dimensions is 1.07 MiB of `f32`, or
**1.2 MiB on disk** with the metadata beside it. Fifty such workbooks are 36,600
vectors and 56 MB. A full scan of the real corpus takes **0.11ms**, and it is
exact. An approximate index would add a build step, a
tuning parameter, a recall cliff and a second on-disk format in exchange for
beating a number too small to see, so there is no HNSW here: an array of floats
per workbook and a loop over it.

Formula groups are not embedded. There are 463,570 of them — 713 MB of vectors
and hours of model time to make near-identical formula text searchable by
meaning, when a formula is exact-token text and the lexical index already
covers it.

Embedding the 732 nodes takes **5.2s**, and the batching is why: batches are
padded to the longest text in them, so one wide table, whose document carries
every column header it has, was paying for the 255 short labels batched beside
it. Sorting by length before batching took it from **9.5s to 5.2s**, measured
around the model call alone — loading the graph and building the lexical index
are another 0.04s together, and folding them in would report a throughput that
is really a measure of tantivy.

## Retrieval

A hit is a door, not an answer. "The Revenue column of TR450" is the right node
and still says nothing about which table it is in, what feeds it, or what breaks
if it is wrong. `eg-retrieve` walks out from the hits to the nodes that explain
them, and records for every node it brings back which node pulled it in and
along which edge — an expansion nobody can check is an expansion nobody should
trust.

```sh
cargo run --release -p eg-retrieve --example retrieve -- index bad debt provision
cargo run --release -p eg-retrieve --example retrieve -- index --hops 3 --children 6 lookup rates
```

On the real workbook an expansion is **0.4ms** over the stored graph, and the
chain it prints is the point: a note headed *Provision for debtors with…* is
read by the 115,004-row working table, which in turn reads a dozen rate tables
on the TABLES sheet, each edge labelled with how many cell references stand
behind it.

### The measurement that shaped the walk

The graph's degree distribution decides whether a bounded k-hop expansion is
cheap or explosive, which is why `eg-graph` has been collecting it since P3a.
On the reference workbook the dependency layer is **161 edges across 732 nodes**
— sparse, with a maximum in-degree of 13. But the most connected nodes have
out-degrees of **136, 83, 82, 74 and 71**, and every one of them is a region
pointing at its own columns.

So the explosion is real and lives entirely in `CONTAINS`. A plain k-hop walk
from a column reaches its region in one hop and that region's 136 columns in
two: 19% of the workbook, none of it asked for. Containment is therefore
followed *inwards* — a column's table, its sheet, the workbook, a path of at
most three that costs no hop, because naming a node is not travelling away from
it — and outwards only when asked, and never from the workbook root, whose
children are the whole file.

Dependencies are followed in both directions, nearest first and heaviest within
a distance. Weight is the number of cell references behind the edge, so the
heaviest is what most of the workbook actually rests on — but taking weight
before distance loses nodes: a heavy detour reaches a node at two hops, the
one-hop step to it is then dropped as already-seen, and because it arrived at
the hop limit its own edges are never followed. Everything past it disappears.

### Rendering it for an agent

The expansion is a subgraph; an agent needs a passage. The constraint that
shapes the rendering is not prose quality but *checkability* — a passage that
reads well and cannot be verified invites an answer nobody can trace.

```sh
cargo run --release -p eg-retrieve --example retrieve -- index --passage bad debt provision
```

Each node is listed once with a number, and relations are given as numbers:

```
[1]  * region "Provision for debtors with:"   SIGNALS!A45:D47
       in: SIGNALS
       read by: [7] (115,003 refs, another sheet), [26] (390 refs, another sheet)
[7]    region    'TR450-6-WORK DOC'!A1:BM115004
       in: TR450-6-WORK DOC
       reads: [9] (460,004 refs, another sheet), [11] (345,005 refs), [1] (115,003 refs, …)
```

Nesting each node under whatever reached it would repeat the same table once per
path that found it, and an agent reading that cannot tell two mentions are one
table. Numbering also gives it a handle: *"the figure comes from [4]"* is
checkable against the list in a way that *"the figure comes from the rates
table"* is not. On the real workbook, 30 nodes render to **3.6 KB in 0.04ms**,
with the citations handed back as a list so a caller can check an answer's
references against what it was actually given.

No cell values appear. The workbook is 6 GB in memory and the ranges are one
read away, so a passage that inlined values would be both enormous and stale.
This says where to look; P6 is what looks.

### Where the granularity bites

Lifting attached every reference to the *region* containing it, so the
dependency layer connects regions and nothing else — a column node has no
dependency edges, and neither does a sheet. The walk therefore collects edges
from the regions a node overlaps: upwards for a column, and one level down for a
sheet. A column's inputs are its table's inputs, which is as fine as the graph
kept. Recovering which cell fed which is P6, done on demand against the
workbook.

## Cell-level provenance

The graph lifts every reference to the region containing it, which is the right
granularity to traverse and the wrong one to explain a number with. So the cell
layer is not stored — it is recovered from the workbook on demand, which is what
the node ranges exist for.

The two directions do not cost the same, and that is why they are separate
functions rather than one call that hides which you asked for. What a cell reads
is in its own text. What reads a cell is written down nowhere, so finding it
means scanning every formula in the file.

```sh
cargo run --release -p eg-eval --example trace -- private/book.xlsb "'TR450-6-WORK DOC'!AQ2:AQ4"
cargo run --release -p eg-eval --example trace -- private/book.xlsb 'TABLES!AE53:AG89' --dependents
```

On the real workbook, after a 9.8s load: precedents come back in **0.08ms**;
dependents take **2.4s to scan 6,793,166 formulas** and find 115,566 references
into that one lookup table.

That closes the loop from the retrieval layer. Searching for "bad debt
provision" surfaces `TABLES!AE53:AG89` as the RATES table; this says which cells
read it and what each of them reads in turn.

The example prints addresses and formulas, and value *kinds* rather than values,
unless `--show-values` says otherwise. A formula is structure; a value is the
workbook's data.

## Recomputing a number

Provenance says which cells a formula stands on. The other half of P6 works out
what they add up to, and compares that with the number the workbook stored —
the one Excel last calculated.

```sh
cargo run --release -p eg-eval --example recompute -- private/book.xlsb 'JOURNAL_PROV!D7'
cargo run --release -p eg-eval --example recompute -- private/book.xlsb --check
```

Precedents are read as *stored values* and never recursively recomputed. That
is a limit and it is also the point: a disagreement is then about this one
formula rather than about a chain of a thousand cells, and each precedent is
itself a cell you can check the same way. It also means no dependency order to
compute, no cycles to detect, and no stale value quietly poisoning everything
downstream of it.

A spreadsheet has hundreds of functions and this models about fifty. The rest
are refused by name — an outcome, not a guess, because an evaluator that
returned a plausible number for a function it does not implement would be worse
than one that returns nothing. Volatile functions are refused too: `TODAY()`
recomputed today is not what the workbook computed when it was saved, so
"differs" would be the wrong verdict even when both numbers are right.

Three places where a spreadsheet is not floating-point arithmetic, all of them
the same rule: a sheet carries 15 significant digits, and two numbers equal in
all of them are the same number. Comparison is made on the number a sheet
*shows*, so `10.13+6.75=16.88` is true, as it is in Excel and in no language
with doubles — an ageing bucket picking its label depends on it. `ROUND` rounds
that same shown number, so `ROUND(2.675,2)` is 2.68 rather than 2.67. And
operands that cancel at 15 digits subtract to exactly zero, not to the
`1.49e-8` between the doubles, which is why a column of differences reads as
empty instead of filling with dust.

### What the workbook says about itself

Sweeping the reference workbook is one pass, and it costs about as much as
loading it:

```
6,793,166 formulas in 25.6s (266,000 per second)
  agreed         6,677,397 (98.3%)
  differed               0 ( 0.0%)
  unsupported      115,769 ( 1.7%)
```

Zero disagreements. Every formula this crate can evaluate computes exactly what
Excel stored, to the last digit, six and a half million times. The unsupported
column is three honest gaps: 115,566 `PV()`, 191 `GETPIVOTDATA()`, and 12
references into workbooks that are not open.

It did not start out that way. The first sweep agreed with 71.9%, and the
distance from there to here is four defects in the XLSB *reader* and one in
this crate. None of them failed. Each was found by the same method — recompute
a formula and disagree with the number Excel had cached — and none of them
could have been found by reading formulas, because a wrong formula looks
exactly like a right one.

| | agreed |
|---|---|
| first sweep | 71.9% |
| relativity flags no longer read as part of a column | 83.8% |
| the two flags read the right way round | 97.6% |
| formula cells whose cached value is an error no longer skipped | 98.3% |
| references into other workbooks no longer resolved against this one | **98.3%, and nothing left disagreeing** |

Two of those are worth the detail. An XLSB reference stores its column in 14
bits and its relativity in the other two, and three of the four decoding paths
read the field whole, so a relative column 2 arrived as column 16,386 —
`=VLOOKUP(B2,HQ880_20240630!$XFF$1:$XFM$1048576,5,FALSE)`, where Excel's last
column is `XFD`. Masking the flags off made 855,637 formulas readable and
*raised* the disagreement count, because the two flags then turned out to mean
the opposite of what the reader believed:

```
=V5*BQ$1     column relative, row absolute — 589.12 × 159.49    = 93,958
=V5*$AH5     column absolute, row relative — 589.12 × 0.502222  = 295.86915555555555
```

Excel stored 295.86915555555555. `AH` is that sheet's "% to provide" column, and
ageing bucket times percentage is what a provision is. Two bits, 934,118
formulas.

The last one is the quietest defect in this repository. A reference into another
workbook carries two indices — which workbook, and which sheet of it — and only
the second was being used, so a sheet index meant for last year's copy of this
file named a sheet of *this* one. `JOURNAL_PROV!$D$21` was a real sheet, a real
cell, and a dependency the graph recorded and nobody could have questioned. Ten
formulas gave the wrong number and two gave the right one by coincidence, the
local cell happening to hold what the foreign cell held.

`docs/upstream-issues.md` has all seven, and the four found this way are in the
calamine fork.

## Testing

```sh
cargo test --workspace
```

The most important test is format parity: the same logical workbook read as
`.xlsx` and as `.xlsb` must produce identical values *and* formulas. It has
already caught three real bugs — see `docs/upstream-issues.md`.

Fixtures live in `tests/fixtures/vendor` and were authored by real Excel, because
no open-source tool can write XLSB.

### Asking a second reader

Parity compares this reader with itself across two formats, which cannot catch
a defect that reads both the same way — and four of the seven were exactly
that. The other check is an independent implementation. `dump_cells` writes a
sheet's formulas and cached values in the schema [sheet-oracle] writes from
SheetJS, and the two dumps are diffed:

```sh
cargo run --release -p eg-ingest --example dump_cells -- private/book.xlsb \
  --sheet 'Work Doc' --range A2:BZ200 --out ours.json
node ../sheet-oracle/bin/sheet-oracle.js private/book.xlsb \
  --sheet 'Work Doc' --max-rows 200 --range A2:BZ200 --out theirs.json
node ../sheet-oracle/bin/sheet-oracle-compare.js ours.json theirs.json
```

On 10,137 formulas of the reference workbook the two agree on 9,543. Of the
rest, 590 are SheetJS naming a column past `XFD` — the same defect this reader
had, in the same field, found the same way — and 4 are the two of them
spelling "a sheet in a workbook that is not open" differently. Neither reader
is an authority. They simply do not have the same bugs in the same places.

`--no-values` dumps formulas and value kinds without the data, for a workbook
that is confidential.

[sheet-oracle]: ../sheet-oracle

## Confidential workbooks

Put them in `private/`, which is gitignored in full. To inspect one without
exposing its contents:

```sh
cargo run --release -p eg-ingest --example audit -- private/book.xlsb
```

The audit reports counts, coverage and A1 addresses only — never cell contents.
Sheet names are redacted unless you pass `--show-names`.
