# ExcelGRAG

Excel → Graph → GraphRAG. Turns spreadsheets into a queryable property graph so
agents can explore them and ground answers in specific cells.

Written in Rust, and the reason is XLSB. A 170 MB binary workbook with 43.5
million populated cells loads in about 8 seconds. `openpyxl` cannot open XLSB at
all, and the only Python library that can, `pyxlsb`, does not surface formulas.

## Status

Early. `eg-model` (addressing, cell/workbook model) and `eg-ingest` (loading) are
implemented and tested; the graph, index, retrieval, evaluation and MCP layers
are not yet built.

## Workspace

| Crate | Purpose | State |
|---|---|---|
| `eg-model` | Addressing, cell values, workbook model | implemented |
| `eg-ingest` | Loading xlsx/xlsm/xlsb/xls/ods via calamine | implemented |
| `eg-structure` | Region detection, header inference, formula grouping | stub |
| `eg-graph` | Graph build and persistence | stub |
| `eg-index` | Lexical (tantivy) and vector (fastembed) indexes | stub |
| `eg-retrieve` | Hybrid search, graph expansion, context rendering | stub |
| `eg-eval` | Formula evaluation and what-if | stub |
| `eg-mcp` | MCP server | stub |
| `eg-cli` | Command-line front-end | stub |

## The calamine fork

`eg-ingest` depends on a forked calamine at `../calamine`, wired in via
`[patch.crates-io]`. It fixes two silent bugs in both binary formats —
dropped shared and array formulas, and transposed `>=` / `>` — without which 70%
of the formulas in a real XLSB workbook go missing. Clone it alongside this repo:

```sh
git clone https://github.com/tafia/calamine.git ../calamine
cd ../calamine && git checkout xlsb-shared-formulas
```

The fix is ready to upstream; see `docs/upstream-issues.md`.

## Testing

```sh
cargo test --workspace
```

The most important test is format parity: the same logical workbook read as
`.xlsx` and as `.xlsb` must produce identical values *and* formulas. It has
already caught two real bugs — see `docs/upstream-issues.md`.

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
