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
cells behind it, recomputes it from the cells under it, and says what else
moves if one of them changes. `eg-mcp` serves all of that to an agent over MCP,
and `eg` is the same behind one command.

## Workspace

| Crate | Purpose | State |
|---|---|---|
| `eg-model` | Addressing, cell values, workbook model | implemented |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods via calamine | implemented |
| `eg-structure` | Region detection, header inference, formula grouping | implemented |
| `eg-graph` | Graph build, reference lifting, invariants, store | implemented |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes | implemented |
| `eg-retrieve` | Hybrid search, graph expansion, context rendering | implemented |
| `eg-eval` | Cell-level provenance, formula evaluation, what-if | implemented |
| `eg-mcp` | MCP server | implemented |
| `eg-cli` | Command-line front-end (`eg`) | implemented |

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

On a real 170 MB workbook: **6,793,166 formula cells become 1,272 groups**
(5,340x), in 11s, with one group covering 690,018 cells and only 724 formulas
one-off. It also finds cells that break a pattern — the classic hand-edited row
in an otherwise uniform column, 401 of them here.

That ratio used to read 464,131 groups and 14.6x. The difference is not this
code: before the calamine fork's fixes, relative references in XLSB were decoded
wrongly, so every row of a filled-down column normalised to a *different* shape
and almost nothing grouped. Grouping was reporting a reader defect as a property
of the workbook.

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

On the real 170 MB workbook: **2,007 nodes and 3,271 edges in 10.3s**, adding
0.4 MiB to the 6 GB the workbook itself occupies. Drop the formula-group nodes
and it is **735 nodes, 951 edges** — with the identical dependency layer,
because lifting reads formula cells and not group nodes.

That dependency layer is 212 edges standing for 3.76 million references, out of
24.5 million scanned: 22.2 million never leave the region they are written in,
which is what makes a region-level graph small enough to hold.

```sh
cargo run --release -p eg-graph --example graph -- private/book.xlsb
cargo run --release -p eg-graph --example graph -- private/book.xlsb --no-groups
```

The example reports node and edge counts by kind, the reference breakdown,
measured memory, the degree distribution, references to sheets the workbook does
not have, and whether every invariant holds.

### Checking the lifted edges against the cells

`check` proves the graph is *self-consistent* — nothing orphaned, every node on
one sheet, every edge standing for at least one reference — and it says plainly
what that misses. An edge lifted to the **wrong** region is still reachable,
still on one sheet, still positively weighted, and every invariant passes. An
earlier phase shipped exactly that bug.

So the edges are checked against the thing they were derived from. `audit` reads
every formula in the workbook, resolves every reference, and attaches each to
the regions it lands in — taking those regions out of the *graph's own nodes*,
not out of the builder's internals — then compares the two multisets both ways
round. An expected edge the graph lacks is a reference that lost its edge; an
edge nothing expects is one pointing where no formula does; a weight that
disagrees is an edge whose evidence is miscounted, which is what ranks it in
retrieval.

On the reference workbook, over 6,793,166 formulas and 24,480,367 references:

| | Edges | References behind them |
|---|---|---|
| the workbook expects | 212 | 3,760,745 |
| the graph holds | 212 | 3,760,745 |
| agreed exactly | **212** (100.0%) | — |

It takes 2.7s, against 7.0s to build the graph in the first place, so it is
cheap enough to run on every workbook indexed rather than saved for a special
occasion.

What it does *not* check matters more than the number. Reference scanning, range
geometry and region detection are one implementation each, used by both sides,
so a defect in any of them is invisible here — parity against a second reader
and the recompute sweep are what cover those. What is genuinely re-derived is
the lifting itself: which region owns a formula, which regions its references
land in, whether a self-reference is dropped, how the counts accumulate. Those
are the steps that pick a region, and picking the wrong one was the bug.

Because it reads regions and edges out of a graph rather than a builder, it
audits one loaded back from the corpus as readily as a freshly built one, which
puts the store's round-trip under the same check.

`cargo test -p eg-graph --test audit` breaks a correct graph five ways — an edge
moved to another region, an edge deleted, a weight falsified, one dependency
split across two parallel edges, two regions laid over one cell — and asserts
for each that `check` stays silent and the audit does not.

```sh
cargo run --release -p eg-graph --example lifting -- private/book.xlsb
```

## The corpus

The graph of that 170 MB workbook is **520 KB of JSON**, so the store is a
directory rather than a database: `manifest.json` plus one file per workbook,
keyed by the blake3 of the source file. A workbook that has not changed is a hit
however it was copied; one that has changed cannot be.

| | Cold | Warm |
|---|---|---|
| 170 MB workbook | 19.9s | **1.4ms** |

The 6 GB in-memory workbook is never touched to answer a corpus-level question,
which is the whole reason to keep a store.

### The formula groups, and a number that stopped being true

For most of this project the formula-group layer was left out of the store, on a
measured reason: 464,131 nodes and 119 MiB, near-identical text by construction,
wanted only when drilling into one workbook. That reason no longer holds, and
the way it stopped holding is worth writing down.

The 464,131 came from the reference workbook read through calamine *before* the
fork's fixes. Mis-decoded relative references gave a filled-down column a
different R1C1 shape on every row, so almost nothing grouped. Read correctly the
same workbook has **1,272 groups**, compressing 6,793,166 formulas 5,340×, the
largest of them 690,018 cells. The layer that was 119 MiB is 397 KB.

| Stored | Nodes | Edges | JSON | Cold | Warm |
|---|---|---|---|---|---|
| regions only | 735 | 951 | 123 KB | 16.6s | 0.45ms |
| with formula groups | 2,007 | 3,271 | 520 KB | 19.9s | 1.4ms |

So they are stored. What it buys is not the disk: rebuilding the layer costs a
full ingest of the source file — ten seconds and 6 GB of memory — so without it
no question about a formula could be answered from the corpus alone. With it,
the lexical index holds every formula group in the workbook and `eg search` can
find one when the file is not even present.

The old number is kept as the reason for a ceiling rather than a flat yes. This
layer has no natural bound — a workbook of one-off formulas groups into nothing,
and its group layer is as large as its formula count — so above
`MAX_STORED_FORMULA_GROUPS` (20,000, fifteen times what the reference workbook
needs) the layer is dropped at index time and rebuilt on demand, as the whole
layer used to be. Each stored graph records which kind it is, so a loader never
guesses.

Formula groups are still not *embedded*. That choice was made on the same stale
number, but it survives the correction on its own merits: a formula is exact
tokens, not a sentence, and asking for one is a lexical query.

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

The formula-group layer is what lets a search reach an actual formula, and it is
cheap to index: **2,007 documents in 0.01s, 0.3 MiB on disk**, with a search over
them returning in **0.39ms**. That measurement is why the corpus stores the layer
rather than rebuilding it.

```sh
cargo run --release -p eg-index --example formulas -- private/formulas private/book.xlsb vlookup
```

That measurement also found the one thing text relevance cannot do here. Nearly
every formula in a real workbook is a lookup, so a query for `vlookup` matches a
great many groups at the same BM25 score, and which ones surface is then
arbitrary. Each node carries how many cells it stands for, and the score is
multiplied by `1 + log10(1 + cells) / 4` — the same idea as an edge's weight,
that how much of the workbook rests on something is part of how much it matters.
The `vlookup` list leads with the group covering 195,366 cells. The multiplier
tops out
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

Formula groups are stored and indexed lexically, and still not embedded. The
old reason was volume — 463,570 groups, 713 MB of vectors — and that number came
from the same pre-fork reader; the real figure is 1,272 groups and 2 MB, which
would embed in seconds. The reason that survives the correction is the one about
the text: `=VLOOKUP(S2,LOOKUP!E$1:F$1048576,2,FALSE)` is not a sentence, and a
sentence embedding has nothing to say about it. Asking for it is a lexical
query, and every group is in the lexical index.

Embedding the 735 nodes takes **5.4s**, and the batching is why: batches are
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
On the reference workbook the dependency layer is **212 edges across 735 nodes**
— sparse, with a maximum in-degree of 13. But the most connected nodes have
out-degrees of **136, 92, 91, 74 and 71**, and every one of them is a region
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
6,793,166 formulas in 25.1s (271,000 per second)
  agreed         6,792,963 (100.0%)
  differed             0 ( 0.0%)
  unsupported        203 ( 0.0%)
```

Zero disagreements. Every formula this crate can evaluate computes exactly what
Excel stored, to the last digit, six and three-quarter million times. What is
left unsupported is two honest gaps: 191 `GETPIVOTDATA()`, which asks a pivot
table rather than the grid, and 12 references into workbooks that are not open.

The 115,566 that were unsupported until recently were all one function. `PV()`
discounts an overdue amount by the days it has been outstanding, which is the
arithmetic this workbook exists to do: the impairment on a debtor is the
difference between what is owed and what that is worth now. Modelling one
function moved the sweep from 98.3% of formulas evaluated to 100.0%, and every
one of those 115,566 agrees with the value Excel stored beside it.

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
| references into other workbooks no longer resolved against this one | 98.3%, and nothing left disagreeing |
| `PV()` modelled rather than refused | **100.0%**, with 203 formulas left unevaluable |

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

## What if this number were different

Recomputing says whether one formula still agrees with the value beside it, and
it reads that formula's precedents as *stored* values on purpose, so a
disagreement is about one cell. A what-if is the opposite question and needs the
opposite of that limit: put a different number in a cell, and follow it as far
as it goes.

```sh
eg what-if book.xlsb 'RATES!BS9=0.175'
eg what-if book.xlsb 'RATES!BS9=0.175' --levels 2 --limit 5
```

Nothing is written. The workbook cannot be written — XLSB is read-only here,
because no Rust crate can serialise it — so a substituted value lives in an
overlay that every read goes through, and the answer is a report rather than a
file. That is a limitation on paper and rarely one in practice: the question is
almost always "what would this do", not "please change it".

Three costs the one-formula recompute did not have, and they are the whole
design:

- **Finding what is downstream.** Nothing records who reads a cell, so each
  level of the chain is a full scan of the workbook's formulas.
- **Order.** A cell must not be computed before the cells it reads, so each
  level is sorted by its own internal dependencies. What cannot be sorted is a
  cycle, and a cycle is reported rather than iterated to a fixed point —
  Excel's iterative calculation is a setting this cannot see.
- **Saying what it could not answer.** A cell whose formula this crate does not
  model has no new value, and neither does anything reading it. Those come back
  as *no answer*, never as *unchanged* — leaving the stored value in place would
  report a smaller impact than the change really has.

On the reference workbook, changing one interest rate — the one residential
debtors are discounted at:

```
1,197,300 cell(s) downstream over 6 level(s), 7 scans of 47.5M formulas in 207s
  moved           678,427
  unchanged       518,873
  blocked               0
```

The half that does not move is the interesting half, and the first level shows
it cleanly:

```
115,566 cell(s) downstream over 1 level(s): 98,374 moved, 17,192 unchanged
```

That level is exactly the `PV()` column. Every one of those formulas looks the
rate up in the whole table, `$BR$4:$BS$12`, so every one of them reads the cell
that changed — but 17,192 of them are looking up a different category and come
back with the number they had. A dependency graph is right that all 115,566
depend on that cell; only recomputing can say which of them a change actually
reaches.

A walk that has to stop says which limit stopped it, because "nothing else
moved" and "this did not look" are different answers:

```
  stopped at the ceiling on affected cells — the change reaches further than this
```

The cost is dominated by the scans, and the walk keeps them down by pruning:
a cell that recomputes to what it already held cannot move anything reading it,
so it does not travel in the next frontier. Matching a reference against that
frontier is the inner loop — tens of millions of times against a set of
hundreds of thousands of cells — so the frontier is held as sorted rows per
column, and a reference costs a bounding-box reject and then a binary search
per column it spans, rather than a walk over either side.

## One command

Everything above is reachable through each crate's examples, which is fine for
developing a library and poor for using one. `eg` is the same capabilities
behind nine verbs, in the order a question travels:

```sh
cargo install --path crates/eg-cli

eg index corpus/ book.xlsb                  # read it, store its graph, index it
eg ask corpus/ bad debt provision           # a question, as a cited passage
eg search corpus/ bad debt --limit 3        # or just what matched
eg cells book.xlsb 'LOOKUP!AE53:AG89'       # the cells behind a citation
eg trace book.xlsb 'LOOKUP!AE53' --dependents
eg check book.xlsb                          # do the formulas still agree
eg what-if book.xlsb 'RATES!BS9=0.175'      # and what moves if one changes
eg serve corpus/                            # the same, to an agent over MCP
```

The verbs wrap library calls only. The diagnostics — `raw_cells`,
`why_unpopulated`, the format probes — stay as examples in the crate they
belong to, because they exist to develop this code rather than to use it.

`eg` and `eg serve` both **show** cell values, and take `--redact-values` to
turn them into kinds. That is the opposite default from the examples, which
redact unless asked, because their output ends up in commit messages and
READMEs — while a person who types `eg cells` is asking to see the cells.

## Serving it to an agent

`eg-mcp` is an MCP server over the whole stack: a corpus in, eight tools out.

```sh
cargo run --release -p eg-graph --example corpus -- index private/book.xlsb
cargo run --release -p eg-index --example semantic -- index warm up the indexes
claude mcp add excelgrag -- "$PWD/target/release/eg-mcp" "$PWD/index"
```

| Tool | Answers |
|---|---|
| `workbooks` | what is in this corpus |
| `search` | which parts of a workbook match a question, by word and by meaning |
| `context` | that question, as a cited passage explaining what was found |
| `read_cells` | the formulas and values of a range |
| `precedents` | what a formula reads |
| `dependents` | what reads a cell — the expensive direction, and it says so |
| `recompute` | whether a formula still agrees with the value stored beside it |
| `what_if` | what else moves if a cell held a different number |

The surface is the pipeline: search, then read the context, then go down to
cells when the answer needs a number. A passage carries no values — it says
where to look — so an agent that wants one has to ask for it by citation, which
is also what makes the answer checkable.

Three resources with three costs sit behind that, which is why this is a server
and not a command. The corpus and the lexical index are memory-mapped and
cheap. The embedder is loaded on the first question that needs meaning rather
than words. A workbook is the expensive one — ten seconds and six gigabytes for
the reference file — so it is opened only when a tool needs cells, and then held:
the second question about a workbook is far likelier than the first.

```
read_cells  INDICATORS!A45:D47   (opened private/book.xlsb in 9.7s)
recompute   'TR450-6-WORK DOC'!AJ5   =V5*$AH5
              agrees with the stored value 295.86915555555555
                V5      589.12
                $AH5    0.5022222222222222
```

Start it with `--redact-values` and every value becomes its kind — `<number>`,
`<text>` — while formulas and structure still answer. The policy is set once, at
startup, so a caller cannot talk its way past it.

MCP's stdio transport is one JSON object per line, so the protocol is a
`read_line` and a `serde_json::from_str`; it is written out rather than taken
from an SDK because the rest of this workspace is synchronous, and an SDK would
bring an async runtime along for something this size. A tool that fails comes
back as a *result* with `isError`, not as a protocol error: the model can act on
"no sheet called that, here are the ones there are" and cannot act on -32603.

## Testing

```sh
cargo test --workspace
```

The most important test is format parity: the same logical workbook read as
`.xlsx` and as `.xlsb` must produce identical values *and* formulas. It has
already caught three real bugs — see `docs/upstream-issues.md`.

Fixtures live in `tests/fixtures/vendor` and were authored by real Excel, because
no open-source tool can write XLSB.

Two further checks stand behind the layers above the reader: the graph's lifted
edges are re-derived from the cells and compared, in [Checking the lifted edges
against the cells](#checking-the-lifted-edges-against-the-cells), and every
formula is recomputed and compared with what Excel cached, in [What the workbook
says about itself](#what-the-workbook-says-about-itself).

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
