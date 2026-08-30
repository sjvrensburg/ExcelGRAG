# ExcelGRAG

Excel → Graph → GraphRAG. Turns spreadsheets into a queryable property graph so
agents can explore them and ground answers in specific cells.

Written in Rust, and the reason is XLSB. A 170 MB binary workbook with 43.5
million populated cells loads in about 8 seconds. `openpyxl` cannot open XLSB at
all, and the only Python library that can, `pyxlsb`, does not surface formulas.

## Status

Early, but a workbook now goes all the way to a graph. `eg-model`, `eg-ingest`,
`eg-structure` and `eg-graph` are implemented and tested; the index, retrieval,
evaluation and MCP layers are not yet built.

## Workspace

| Crate | Purpose | State |
|---|---|---|
| `eg-model` | Addressing, cell values, workbook model | implemented |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods via calamine | implemented |
| `eg-structure` | Region detection, header inference, formula grouping | implemented |
| `eg-graph` | Graph build, reference lifting, invariants | implemented |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes | stub |
| `eg-retrieve` | Hybrid search, graph expansion, context rendering | stub |
| `eg-eval` | Formula evaluation and what-if | stub |
| `eg-mcp` | MCP server | stub |
| `eg-cli` | Command-line front-end | stub |

## The calamine fork

`eg-ingest` depends on a [forked calamine](https://github.com/sjvrensburg/calamine/tree/xlsb-shared-formulas),
wired in via `[patch.crates-io]` and pinned by `Cargo.lock`. Nothing extra to
clone — `cargo build` fetches it.

The fork fixes two silent bugs in both binary formats: dropped shared and array
formulas, and transposed `>=` / `>`. Without them 70% of the formulas in a real
XLSB workbook go missing, and every comparison is inverted.

A third defect is recorded but not fixed: a sheet name needing quotes is written
bare, so `'Q3 SALES'!A1` arrives as `Q3 SALES!A1` and reads as a sheet named
`SALES`.

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

On the real 170 MB workbook: **464,863 nodes and 926,928 edges in 10.7s**, using
119 MiB. Drop the formula-group nodes and it is **732 nodes, 850 edges, 0.1 MiB**
— with the identical dependency layer, because lifting reads formula cells and
not group nodes.

That dependency layer is 119 edges, standing for 2.5 million references. Most
references never leave the region they are written in.

```sh
cargo run --release -p eg-graph --example graph -- private/book.xlsb
cargo run --release -p eg-graph --example graph -- private/book.xlsb --no-groups
```

The example reports node and edge counts by kind, the reference breakdown,
measured memory, the degree distribution, references to sheets the workbook does
not have, and whether every invariant holds.

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
