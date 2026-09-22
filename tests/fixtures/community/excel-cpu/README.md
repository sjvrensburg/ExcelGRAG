# excel-cpu

A 16-bit CPU implemented entirely in Excel formulas, by
[InkboxSoftware/excelCPU](https://github.com/InkboxSoftware/excelCPU),
dedicated to the public domain under CC0-1.0 (`LICENSE` in this directory,
copied from the upstream repository).

Committed here as a real, structurally alien test workbook — a spreadsheet
built as a computing device rather than a financial model, with a single
82,155-cell sheet of 82,044 formulas and heavy cross-sheet dependency chains,
unlike anything in `tests/fixtures/demo` or the reference workbook this
project was built against. Useful for exercising the reader, the graph, and
(once built) the structural validation gate against a workbook shaped nothing
like the ones ExcelGRAG's test suite otherwise sees.

Vendored as of 2026-09-22, from the `main` branch. Re-fetch from upstream if
either file changes there; ExcelGRAG does not modify them.

## What was checked before committing

- `eg-ingest --example audit`: loads cleanly, no format limitations beyond
  the usual (no cell styling from any format).
- `eg check CPU.xlsx`: 82,044 formulas, 83 recomputed and agree, one genuine
  disagreement (`SOS!B2 =IF(B2=0, 1, 0)`, a self-referential cell — exactly
  the shape "recompute never recurses" predicts will disagree, since
  recompute reads the stored value of `B2` as its own input rather than
  resolving the circularity), and the rest refused by name as unsupported
  functions (`ROW`, `SWITCH`, `FLOOR.MATH`, `INDIRECT` (volatile),
  `DEC2HEX`) — the project's stated invariant working as intended on a real,
  independently-authored workbook.
- `eg-graph --example lifting`: the lifted dependency edges agree exactly
  with the cells they were derived from, on every one of the 82,044
  formulas.
- `ROM.xlsx` and `instructionSet.xlsx` carry no formulas at all (pure data
  and a documentation table) and check out trivially.
