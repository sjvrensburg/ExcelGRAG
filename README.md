# ExcelGRAG

Excel → Graph → GraphRAG. Turns spreadsheets into a queryable property graph so
agents can explore them and ground answers in specific cells.

Written in Rust, and the reason is XLSB. A binary workbook of hundreds of
megabytes and tens of millions of populated cells loads in seconds. `openpyxl`
cannot open XLSB at all, and the only Python library that can, `pyxlsb`, does
not surface formulas.

Every decision below was measured, against a real workbook of that kind. It is
confidential, so the figures are not repeated here: each example prints its own
on the terminal, which is where the numbers belong and where anyone with a
workbook of their own can reproduce them. Sheet names appear as pseudonyms —
consistently, so a name that recurs is the same sheet.

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
formulas, and transposed `>=` / `>`. Without them most of the formulas in a real
XLSB workbook go missing, and every comparison is inverted.

A third fix, submitted separately as
[#713](https://github.com/tafia/calamine/pull/713), quotes sheet names that need
quoting: `'Q3 SALES'!A1` was arriving as `Q3 SALES!A1`, reading as a sheet named
`SALES`. On a real workbook that both loses real references and fabricates
others, because a discarded prefix like `TR450` is itself a valid cell
reference.

Submitted upstream as [tafia/calamine#712](https://github.com/tafia/calamine/pull/712).
Once it lands in a published release, delete the `[patch.crates-io]` section.
See `docs/upstream-issues.md`, which also records what a pre-PR review caught.

## Formula grouping

A filled-down column is one idea written ten thousand times. `eg-structure`
collapses it to a single node by normalising each formula to an R1C1 *shape*, so
the graph is built over groups rather than cells.

On a real workbook the collapse is thousands-fold: millions of formula cells
become a low thousands of groups, a single filled-down column accounting for
much of one group, and only a handful of formulas genuinely one-off. It also
finds the cells that break a pattern — the classic hand-edited row in an
otherwise uniform column.

That ratio used to be two orders of magnitude worse. The difference is not this
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
format has them. On a real workbook it recovers a couple of hundred regions in
seconds, with every populated cell covered by exactly one of them.

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

On a real workbook the whole graph is a couple of thousand nodes and edges,
built in seconds, adding a rounding error to the gigabytes the workbook itself
occupies. Drop the formula-group nodes and about a third of it remains — with
the identical dependency layer, because lifting reads formula cells and not
group nodes.

That dependency layer is the striking part: a few hundred edges standing for
millions of references, out of tens of millions scanned. The overwhelming
majority never leave the region they are written in, which is what makes a
region-level graph small enough to hold.

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

On a real workbook, over millions of formulas and tens of millions of
references, the two multisets agree exactly — every edge the cells expect,
every edge the graph holds, and every weight. The example prints both sides and
their difference, so a disagreement arrives as a number rather than as a
feeling.

It costs a fraction of what building the graph costs in the first place, so `eg
index` runs it — along with the structural invariants — on every workbook
before storing it, rather than leaving it for whoever remembers the example. A
workbook that fails is still stored, loudly. The finding is about this code,
not about the spreadsheet, and a corpus missing an edge is more use than no
corpus at all.

What it does *not* check matters more than the agreement. Reference scanning, range
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

## Reading a region as a table

Region detection already recovers the rectangle, the header rows, the row-label
columns and one header per body column. That is a table definition in all but
name, and nothing could turn it back into rows: the graph said *where* a table
was, and a caller wanting what is in it had a rectangle of cells.

`read_table` closes that. A column is a header, the cells beneath it, and a type
read off those cells — by majority, because there is no formatting to read it
off. A strict majority, not a plurality: a column that is 40% number, 35% text
and 25% error is `Mixed` and says so, because summing it and reading it as text
are both wrong and picking whichever kind was counted first is how a total ends
up silently incorrect.

Rows are produced lazily and their gaps are filled in. A sheet holds only its
populated cells, so the third cell of a row that skipped it is *absent* rather
than blank — a reader that let the row shorten would misalign every column after
the gap, for that row only.

## What the columns hold

The graph says where a table is and the table says what shape it has. Neither
says what is *in* it, and that is the gap a person's question falls into:
nothing in the index knew that a `Debt Type` column holds `Residential`,
`Business` and `Indigent`, so a question naming a value rather than a header
matched nothing.

`eg index` now profiles every column: how many rows, how many blank, how many
hold an error value, and — where a column has few enough of them — its distinct
values with counts, plus the range and total of a numeric one.

`eg index` reports how many columns it profiled and how many of them read as
categories — few values, each repeated, which is what a question names. On a
real workbook that is most of a second's work and a few hundred kilobytes, and
it turned up something nobody had asked for on the way: whole columns holding
nothing but error values.

A profile is bounded on purpose. The distinct list is abandoned above a
threshold, so a key column of customer numbers profiles to a count and nothing
else; listing it would be storing the column. Text values are cut to 64
characters and say that they were.

**It is also the one thing a corpus holds that is the workbook's data.**
Everything in `graphs/` is structure — ranges, headers, counts — and that
directory can be handed to someone who may not see the spreadsheet. Distinct
values and sums cannot. So profiles are written to their own `profiles/`
directory, deletable on their own, and the graph's invariant stays true rather
than becoming a footnote. `--no-profiles` skips them; `--redact-values` keeps
the counts and the types and drops everything that came out of a cell.

```sh
cargo run --release -p eg-structure --example profile -- private/book.xlsb
eg index corpus/ book.xlsb --redact-values     # shape without contents
eg index corpus/ book.xlsb --no-profiles
```

## Asking a table a question

Everything else here answers *where*: which cells feed this, what moves if that
changes, does this formula still agree with its inputs. None of it answers "what
is the total debt outstanding for residential debtors", which is the question a
person actually has — and which the workbook only answers if somebody already
wrote a cell that computes it.

`query_table` filters the rows of one table, groups them, and totals them. On a
real workbook, grouping the working sheet by debt category and summing the debt
column produces a handful of numbers that exist nowhere in the file.

It lives in `eg-eval` rather than beside `read_table` for one reason: the
arithmetic has to be the *evaluator's*. A sheet carries fifteen significant
digits, and a total here that used plain `f64` would disagree with the `SUM()`
in the cell next to it — a second opinion that is wrong, which is the one thing
this project must not produce. It accumulates raw and rounds once at the end;
rounding every step would be a regime Excel does not have, and over a column of
that length it would drift away from the sheet rather than towards it.

**Refusing is most of the design**, because the failure mode of a query engine
over a spreadsheet is a confident wrong number.

| | |
|---|---|
| a header naming two columns | refused — `Total` under both `Q3` and `Q4` is a coin toss |
| a total over a column that is not numbers | refused by name and kind, rather than computed by skipping the text |
| a column the table does not have | fails before reading a cell, rather than returning an empty answer that reads like "nothing matched" |
| error cells inside a total | counted and reported, never quietly dropped |

And every answer names the cells it was computed over:

```
over 'Work Doc'!A2:BM…
… row(s) scanned, … matched
```

That is not decoration. Region boundaries are *inferred* from blank runs and
value-kind contrast, so a totals row swept into a table's body would double every
sum — and the only defence is that the caller can see what was summed and check
it against the sheet.

## The schema a workbook writes down without meaning to

A spreadsheet has no schema and states one anyway. `VLOOKUP(C2, $BR$4:$BS$12, 2,
FALSE)` filled down a column is a declaration: *this column's values are keys
into that table, and the answer is its second column.* A real workbook writes
that hundreds of thousands of times. Nothing had ever read it.

Formulas are already grouped by R1C1 shape, so recovering the schema costs
parsing a few hundred representative formulas rather than millions, and the
number of cells behind each relation comes free with the grouping.

The example reports how many groups were examined, how many of them do lookups,
how many relations came back, and how many shapes it could not read — the last
being the number that matters, since it is the one that says what the schema is
missing.

Each relation is a key column, a table and what comes back — `Debtors!E:E` into
`Rates!G1:H19`, returning column `H`, with the formula cells behind it counted.
The key is reported as its *column*, not as the cell the group's representative
happened to sit on: the row is an accident of where the shape was sampled, and
the column is the thing that joins.

Two distinctions it insists on. `INDEX(range, MATCH(key, keys, 0))` is the same
relation written to survive a column being inserted, and is recovered as one
relation rather than as a table with no key. And an **approximate** lookup — a
`VLOOKUP` with no `FALSE` — is recorded as a *banding*: it asks for the last row
not past its argument, so the first column is a set of thresholds and joining it
on equality would be wrong. A shape it cannot read is left unrecognised and
counted, because a schema that guesses is worse than one with holes in it: a
hole is visible.

```sh
cargo run --release -p eg-eval --example schema -- private/book.xlsb
```

## The corpus

The graph of a workbook whose ingest is measured in seconds is a few hundred
kilobytes of JSON, so the store is a directory rather than a database:
`manifest.json` plus one file per workbook, keyed by the blake3 of the source
file. A workbook that has not changed is a hit however it was copied; one that
has changed cannot be.

Answering cold means that ingest. Answering warm is a file read of about a
millisecond — four orders of magnitude apart, and the gigabytes the in-memory
workbook occupies are never touched to answer a corpus-level question, which is
the whole reason to keep a store.

### The formula groups, and a number that stopped being true

For most of this project the formula-group layer was left out of the store, on a
measured reason: hundreds of thousands of nodes and a hundred-odd megabytes of
near-identical text, wanted only when drilling into one workbook. That reason no
longer holds, and the way it stopped holding is worth writing down.

The measurement came from a workbook read through calamine *before* the fork's
fixes. Mis-decoded relative references gave a filled-down column a different
R1C1 shape on every row, so almost nothing grouped. Read correctly, the same
workbook groups thousands-fold better, and the layer that was a hundred
megabytes is a few hundred kilobytes — a fraction of a second to reload, on top
of a store whose whole point is a warm read too fast to notice.

So they are stored. What it buys is not the disk: rebuilding the layer costs a
full ingest of the source file — seconds of work and gigabytes of memory — so
without it no question about a formula could be answered from the corpus alone.
With it, the lexical index holds every formula group in the workbook and `eg
search` can find one when the file is not even present.

The old measurement is kept as the reason for a ceiling rather than a flat yes. This
layer has no natural bound — a workbook of one-off formulas groups into nothing,
and its group layer is as large as its formula count — so above
`MAX_STORED_FORMULA_GROUPS` (20,000, more than an order of magnitude above what
a well-grouping workbook needs) the layer is dropped at index time and rebuilt
on demand, as the whole layer used to be. Each stored graph records which kind
it is, so a loader never guesses.

Formula groups are still not *embedded*. That choice was made on the same stale
measurement, but it survives the correction on its own merits: a formula is exact
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

On a real workbook the region-level index is a few hundred documents and a
fraction of a megabyte, built and queried in well under a second — the layer
that decides whether a question is answerable costs almost nothing to keep.

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
cheap to index: a couple of thousand documents, a fraction of a megabyte, and a
search over them as fast as one over the regions. That measurement is why the
corpus stores the layer rather than rebuilding it.

```sh
cargo run --release -p eg-index --example formulas -- private/formulas private/book.xlsb vlookup
```

That measurement also found the one thing text relevance cannot do here. Nearly
every formula in a real workbook is a lookup, so a query for `vlookup` matches a
great many groups at the same BM25 score, and which ones surface is then
arbitrary. Each node carries how many cells it stands for, and the score is
multiplied by `1 + log10(1 + cells) / 4` — the same idea as an edge's weight,
that how much of the workbook rests on something is part of how much it matters.
The `vlookup` list then leads with the group standing for the most cells. The
multiplier tops out near 2.4x, far below the spread of real text scores, so it
orders ties without ever putting a big irrelevant node above a small exact
match.

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

The nodes worth embedding — sheets, tables, columns, defined names — are a few
hundred per workbook, which at the model's 384 dimensions is about a megabyte of
`f32` with the metadata beside it. Fifty such workbooks are tens of thousands of
vectors and a few tens of megabytes. A full scan of a real corpus takes a
fraction of a millisecond, and it is exact. An approximate index would add a
build step, a
tuning parameter, a recall cliff and a second on-disk format in exchange for
beating a number too small to see, so there is no HNSW here: an array of floats
per workbook and a loop over it.

Formula groups are stored and indexed lexically, and still not embedded. The
old reason was volume — hundreds of thousands of groups and most of a gigabyte
of vectors — and that number came from the same pre-fork reader; read correctly
the layer is small enough to embed in seconds. The reason that survives the
correction is the one about the text:
`=VLOOKUP(S2,LOOKUP!E$1:F$1048576,2,FALSE)` is not a sentence, and a sentence
embedding has nothing to say about it. Asking for it is a lexical
query, and every group is in the lexical index.

Embedding a workbook's nodes takes seconds, and the batching is why: batches are
padded to the longest text in them, so one wide table, whose document carries
every column header it has, was paying for all the short labels batched beside
it. Sorting by length before batching cut that time by nearly half, measured
around the model call alone — loading the graph and building the lexical index
are noise beside it, and folding them in would report a throughput that is
really a measure of tantivy.

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

On a real workbook an expansion is a fraction of a millisecond over the stored
graph, and the chain it prints is the point: a note headed *Provision for
debtors with…* is read by the working table, which in turn reads a dozen rate
tables on the TABLES sheet, each edge labelled with how many cell references
stand behind it.

### The measurement that shaped the walk

The graph's degree distribution decides whether a bounded k-hop expansion is
cheap or explosive, which is why `eg-graph` has been collecting it since P3a.
On a real workbook the dependency layer is sparse — a few hundred edges over a
few hundred nodes, with a maximum in-degree in the low tens. The most connected
nodes have out-degrees an order of magnitude larger than that, and every one of
them is a region pointing at its own columns.

So the explosion is real and lives entirely in `CONTAINS`. A plain k-hop walk
from a column reaches its region in one hop and every column of that region in
two — a large fraction of the workbook, none of it asked for. Containment is
therefore followed *inwards* — a column's table, its sheet, the workbook, a
path of at most three that costs no hop, because naming a node is not
travelling away from it — and outwards only when asked, and never from the
workbook root, whose children are the whole file.

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
       read by: [7] (… refs, another sheet), [26] (… refs, another sheet)
[7]    region    'TR450-6-WORK DOC'!A1:BM…
       in: TR450-6-WORK DOC
       reads: [9] (… refs, another sheet), [11] (… refs), [1] (… refs, …)
```

Nesting each node under whatever reached it would repeat the same table once per
path that found it, and an agent reading that cannot tell two mentions are one
table. Numbering also gives it a handle: *"the figure comes from [4]"* is
checkable against the list in a way that *"the figure comes from the rates
table"* is not. A passage of a few dozen nodes renders to a few kilobytes in
well under a millisecond, with the citations handed back as a list so a caller
can check an answer's references against what it was actually given.

No cell values appear. The workbook is gigabytes in memory and the ranges are
one read away, so a passage that inlined values would be both enormous and stale.
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

The asymmetry is what the measurements show. After a load counted in seconds,
precedents come back in microseconds, because they are in the formula's own
text. Dependents take seconds, because every formula in the file has to be
scanned — and they find every reference into that one lookup table.

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

Sweeping a real workbook is one pass, and it costs about as much as loading it.
The sweep reports how many formulas agreed with the value stored beside them,
how many differed, and how many it could not evaluate at all.

Zero disagreements. Every formula this crate can evaluate computes exactly what
Excel stored, to the last digit, millions of times over. What is left
unsupported is two honest gaps: `GETPIVOTDATA()`, which asks a pivot table
rather than the grid, and references into workbooks that are not open.

A whole column of a real workbook was unsupported until recently, and it was one
function. `PV()` discounts an overdue amount by the days it has been
outstanding, which is the arithmetic such a workbook exists to do: the
impairment on a debtor is the difference between what is owed and what that is
worth now. Modelling one function closed nearly all of what was left, and every
one of those formulas agrees with the value Excel stored beside it.

It did not start out that way. The first sweep agreed with well under
three-quarters of what it read, and the distance from there to here is four
defects in the XLSB *reader* and one in this crate. None of them failed. Each
was found by the same method — recompute a formula and disagree with the number
Excel had cached — and none of them could have been found by reading formulas,
because a wrong formula looks exactly like a right one.

Five fixes, in the order they were found, each one moving the agreement up:

1. relativity flags no longer read as part of a column
2. the two flags read the right way round
3. formula cells whose cached value is an error no longer skipped
4. references into other workbooks no longer resolved against this one — after
   which nothing disagreed at all
5. `PV()` modelled rather than refused, which cleared most of what was left
   unevaluable

Two of those are worth the detail. An XLSB reference stores its column in 14
bits and its relativity in the other two, and three of the four decoding paths
read the field whole, so a relative column 2 arrived as column 16,386 —
`=VLOOKUP(B2,HQ880_20240630!$XFF$1:$XFM$1048576,5,FALSE)`, where Excel's last
column is `XFD`. Masking the flags off made a large fraction of the workbook's
formulas readable and *raised* the disagreement count, because the two flags
then turned out to mean the opposite of what the reader believed:

```
=V5*BQ$1     column relative, row absolute — an ageing bucket × a rate
=V5*$AH5     column absolute, row relative — an ageing bucket × a percentage
```

Only the second is what Excel had stored. `AH` is that sheet's "% to provide"
column, and ageing bucket times percentage is what a provision is. Two bits,
deciding the meaning of an entire column of the workbook.

The last one is the quietest defect in this repository. A reference into another
workbook carries two indices — which workbook, and which sheet of it — and only
the second was being used, so a sheet index meant for last year's copy of this
file named a sheet of *this* one. `JOURNAL_PROV!$D$21` was a real sheet, a real
cell, and a dependency the graph recorded and nobody could have questioned. A
handful of formulas gave the wrong number and a couple gave the right one by
coincidence, the local cell happening to hold what the foreign cell held.

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
  level is sorted by its own internal dependencies — and a level is a *shortest*
  distance from the change, not a rank. In `D=A+C` where `C=B` and `B=A`, `D` is
  reached at level 1 and reads a cell that only moves at level 2. So a cell is
  judged again whenever an input of it moves, and holds what the last visit
  said. It is cheaper than it sounds — the revisits are a fraction of the cells
  the walk visits at all — and the alternative is a
  confidently wrong number: judged once, `D` would report the sum of the new `A`
  and the *stored* `C`, with nothing marking it as provisional. What cannot be
  ordered at all is a cycle, and a cycle is reported rather than iterated to a
  fixed point — Excel's iterative calculation is a setting this cannot see.
- **Saying what it could not answer.** A cell whose formula this crate does not
  model has no new value, and neither does anything reading it. Those come back
  as *no answer*, never as *unchanged* — leaving the stored value in place would
  report a smaller impact than the change really has.

On a real workbook, changing one interest rate — the one residential debtors are
discounted at — reaches over a million cells across six levels, and the report
says how many of them moved, how many held still, and how many it could not
answer for. That is the whole closure, which is not what the default asks for:
the ceiling on affected cells stops it a few levels in, at a fraction of the
cost, and says so.

The half that does not move is the interesting half, and the first level shows
it cleanly. That level is exactly the `PV()` column. Every one of those formulas
looks the rate up in the whole table, `$BR$4:$BS$12`, so every one of them reads
the cell that changed — and yet a sixth of them are looking up a different
category and come back with the number they had. A dependency graph is right
that all of them depend on that cell; only recomputing can say which of them a
change actually reaches.

A walk that has to stop says which limit stopped it, because "nothing else
moved" and "this did not look" are different answers:

```
  stopped at the ceiling on affected cells — the change reaches further than this
```

The cost is the scans, which is where it should be: tens of millions of formulas
across seven of them are most of the run. The walk keeps their number down by
pruning — a
cell that recomputes to what it already held cannot move anything reading it, so
it does not travel in the next frontier — and matching a reference against that
frontier is the inner loop, tens of millions of times against a set of hundreds
of thousands of cells. So the frontier is held as sorted rows per column, and a
reference costs a bounding-box reject and then a binary search per column it
spans, rather than a walk over either side.

### The quadratic that hid in the last two levels

For a while that paragraph was false and the closure took half an hour, and how
it got there is worth keeping. A substituted value lives in an overlay that every
read goes through, and the overlay was written for what a caller substitutes —
a handful of cells — so it answered "what have you got inside this range?" by
looking at all of them. Its own comment said so. Then the walk began putting
every cell it recomputes into that overlay, over a million of them, and a scan
that had been free became billions of cell comparisons.

None of it showed up until level 5. Nothing in the first four levels reads a
*range* at all — they are `PV` and `VLOOKUP` over single cells — so the overlay
was only ever asked for one address at a time, which is a hash lookup. Level 5
is where the aggregation sheets start, and they read whole columns. The first
four levels were quick; the last two took longer than everything before them put
together, many times over.

The overlay now carries a column-major index of its own addresses, so a range
read costs a bounded lookup per column: column-major because a spreadsheet range
is tall and narrow, and in row order a single column's own cells are separated
by every substitution on the rows between them. The walk also holds one
evaluator for its whole run instead of rebuilding the sheet-name map and an
empty lookup index per cell, a million times over — which needs the walk to say
when it overrides a cell, because a cached lookup column is only as good as the
values it was built from. Half an hour became well under a minute, with every
count identical.

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
| `tables` | the tables of a workbook and the columns of each, with their types |
| `query_table` | a total, count or average over the rows of one table |
| `schema` | which column keys into which table, read out of the lookup formulas |
| `what_if` | what else moves if a cell held a different number |

The surface is the pipeline: search, then read the context, then go down to
cells when the answer needs a number. A passage carries no values — it says
where to look — so an agent that wants one has to ask for it by citation, which
is also what makes the answer checkable.

Three resources with three costs sit behind that, which is why this is a server
and not a command. The corpus and the lexical index are memory-mapped and
cheap. The embedder is loaded on the first question that needs meaning rather
than words. A workbook is the expensive one — seconds of ingest and gigabytes of
memory for a large file — so it is opened only when a tool needs cells, and then
held: the second question about a workbook is far likelier than the first.

```
read_cells  INDICATORS!A45:D47   (opened private/book.xlsb in …s)
recompute   'TR450-6-WORK DOC'!AJ5   =V5*$AH5
              agrees with the stored value
                V5      …
                $AH5    …
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

### Whether the answers are any good

Every layer below this one fails loudly when it breaks: the reader is diffed
against a second reader, the graph's edges are re-derived from the cells, every
formula is recomputed against what Excel cached. The layer the project exists
for had nothing. A change to the tokenizer, the cell-count multiplier, the
fusion or the walk's budget could make answers worse and no number would move.

So there is a scorer. It takes questions whose right answer is already known and
reports two things, because they are different questions:

- **Search** — where the wanted node lands, as hit@1 and mean reciprocal rank.
  MRR is the one to watch: it moves when a right answer slips from second to
  third, and hit@5 does not.
- **Context** — whether the passage `eg ask` renders actually *cites* that node.
  This is the one that matters, because the passage is the product. A node
  ranked first and then squeezed out by the budget has answered nothing.

`cargo test -p eg-retrieve --test answers` is the committed floor: a small made-
up debtors workbook, eight questions phrased the way a person asks rather than
the way the sheet is spelled, and an assertion that the passage answers all of
them and that MRR stays above 0.85. It runs by word only, because a model
download is not something a test may depend on.

```sh
cargo run --release -p eg-retrieve --example answers -- corpus/ questions.json
```

The example is the same scoring against a real corpus and a question file of
your own — which is where the interesting answers are, and which is why the
question file for the reference workbook lives in `private/` with the workbook.

**Two things it found immediately.** Neither was visible before there was a
number:

- **Fusing the two searches was costing precision.** Over a question file for a
  real corpus, searching by word alone put the right node first markedly more
  often than fusing word and meaning did; the fusion demoted most of a dozen
  answers and rescued exactly one the lexical half missed entirely. Context
  recall favoured the fusion, so it was a trade — but it was being made blind.
  See below for what it turned out to be.
- **A question about the column a table is keyed by cannot be answered.** Region
  detection reads the leftmost column as row labels, so it heads nothing and
  gets no node: asking a debtors table about "customer" returns everything
  except the customer column. The committed suite keeps that question and
  asserts that the set of unanswered questions is *exactly* the set recorded as
  known gaps — so it fails if a new one appears, and also if this one starts
  working and nobody updated the record.

A dozen questions is a baseline, not a verdict.

### Failing loudly

The layers below this one fail loudly. Retrieval did not: a passage that missed
the right table read exactly like one that found it, and an agent had no way to
tell. Two attempts at a confidence score are worth recording because both were
worse than useless.

Warning whenever a question uses a word the workbook does not fires on four
questions in five — "present value of expected receipts" is a rank-one hit on a
workbook containing neither "present" nor "value". Thresholding instead on how
much of the question the top result accounts for flags a rank-one answer whose
column happens to be named in two words out of five. A dozen questions cannot
calibrate a classifier and pretending otherwise would just move the silent
failure somewhere else.

So there is no score. Every answer states what it was found on:

```
Matched: the top result matches "debt", "aged" of 3; "buckets" not in this
corpus at all; 2 of 2 results found by word and by meaning
```

against

```
Matched: the top result matches "provision", "doubtful", "debts" of 3;
3 of 3 results found by word and by meaning
```

The system still cannot say which of those answers is right. It can no longer
present them as though they were the same thing, which was the actual defect. An
agent reading the first one knows the passage rests on two common words and that
the distinctive one is not in the workbook at all.

One case does raise a banner, because there is no room to argue: when the result
carries *none* of the question's words, either because none of them is in the
corpus or because the ranking found something on other grounds entirely.

```
BLIND MATCH: none of "colour", "invoice", "paper" appears anywhere in this
corpus, so nothing below was found on the question. Treat it as a guess.
```

Costs one index probe per word of the question, each a fraction of a
millisecond against a search already under one.

### What the fusion was actually doing

The obvious suspect was `K`, the rank-fusion constant, at the published 60.
Over a ranking of eight, `1/(60+1)` to `1/(60+8)` spans 11% while a second
appearance adds 100%, so rank looks like noise and the fusion looks like a vote
on set membership. The span is real. The effect is not: sweeping `K` across its
whole plausible range moves MRR by a rounding error. It stays at 60, because
changing a constant that buys nothing is a second number to explain.

Two other things did matter.

**How deep each half is asked before fusing.** Absence from a ranking is the
only evidence the fusion has that one half dislikes a node, and missing from a
list of eight may only mean rank nine. Asking each half for 50 and cutting the
fused list afterwards costs one more index read.

**Which of two *exclusive* finds goes first.** This is narrower than "words beat
meaning" and is the whole of it. A node both halves ranked outscores either
half's exclusive find at any weight — that is the fusion working. But between a
node only the words found and a node only the embeddings found, unweighted RRF
scored a tie and broke it on the label. On a spreadsheet the words are the
better evidence: a column really is called `Total Debt`. Weighting them at 2
settles that tie the right way.

Scored three ways — by word only, as the fusion first shipped, and at depth 50
with words weighted 2 — the last recovers most of the lexical half's precision
while keeping the passage recall that made the fusion worth running. Word-only
still ranks best on its own terms and cites worst, which is the trade in one
line.

The weight plateaus from 1.5 to 3 and the questions cannot tell those apart, so
it is set to 2 — the middle of the plateau rather than its peak. Push it to 5
and the semantic half stops rescuing the questions it exists for, with passage
recall falling back to where word-only search had it.

```sh
cargo run --release -p eg-retrieve --example answers -- corpus/ questions.json --sweep
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

Over the ten thousand-odd formulas of a sheet, the two agree on all but a few
hundred. Almost every remaining difference is SheetJS naming a column past `XFD`
— the same defect this reader had, in the same field, found the same way — and a
handful are the two of them spelling "a sheet in a workbook that is not open"
differently. Neither reader is an authority. They simply do not have the same
bugs in the same places.

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
