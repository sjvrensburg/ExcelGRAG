# Upstream issues in calamine

Three defects found in [calamine](https://github.com/tafia/calamine) 0.36.1 while
building `eg-ingest`.

**Status:** issues 2 and 3 are fixed in a fork at `../calamine`, branch
`xlsb-shared-formulas`, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Both are fixed for `.xlsb` and `.xls`, in three commits — one per
format plus a performance fix — ready to upstream as a pull request. Issue 1 was our own bug and is fixed here.

All three were found by the format-parity test — reading the same logical
workbook as `.xlsx` and as `.xlsb` and requiring identical values and formulas —
or by auditing a real 170 MB XLSB workbook.

---

## 1. `used_cells()` returns range-relative coordinates (our bug, not theirs)

Not a calamine defect; calamine documents it clearly:

> The row and column are relative/index values rather than absolute cell
> positions.

But it is an easy and very damaging mistake to make, because it is invisible on
any sheet whose content starts at A1 — which is exactly what small test fixtures
tend to look like. Our first implementation misplaced every cell on every sheet
that starts anywhere else.

**Fix:** add `range.start()` back to each coordinate. The value range and the
formula range have *different* origins on the same sheet, so they must be
offset independently. See `attach_formulas` in `crates/eg-ingest/src/lib.rs`.

**Regression guard:** `parity.rs` compares against `issues.xlsx`, whose
`Sheet1` holds its only cell at A2, and whose `datatypes` sheet has formulas at
A3/A4 while values start at A1.

---

## 2. `>` and `>=` are transposed when decoding BIFF formulas

**Affects:** `.xlsb` and `.xls`. Not `.xlsx`, which stores formulas as text.

The BIFF Ptg operator assignments are:

| Code | Operator |
|------|----------|
| 0x09 | `<`  |
| 0x0A | `<=` |
| 0x0B | `=`  |
| 0x0C | `>=` |
| 0x0D | `>`  |
| 0x0E | `<>` |

calamine maps `0x0C => ">"` and `0x0D => ">="` — the two greater-than operators
the wrong way round. The same table appears in both `src/xlsb/mod.rs` (~line 808)
and `src/xls.rs` (~line 1528).

**Evidence:** in `tests/fixtures/vendor/issues.*`, the cell `datatypes!A4` stores
the authoritative text `A1&gt;A2` in the XLSX XML, while the XLSB read of the
same workbook yields `A1>=A2`.

**Impact:** silent and total. Every comparison in every formula read from a
binary workbook is inverted at the boundary — `>=` becomes `>` and vice versa.
Any threshold logic built on it is wrong, with no error raised.

**Fixed** in the fork, for both `.xlsb` and `.xls`.

ExcelGRAG previously carried a local `fix_binary_comparison_operators`
workaround. It has been removed: with the fork fixing the decoder at source,
applying the swap on top would transpose the operators straight back — a mistake
the parity test caught the moment the fork was wired in.

`binary_formats_decode_comparison_operators_correctly` in `parity.rs` guards it
now, asserting `datatypes!A4` decodes as `A1>A2` in all three formats. The XLSX
twin stores that text as XML, so the expected value cannot drift.

---

## 3. `PtgExp` is discarded, losing shared and array formulas — **blocker**

**Affects:** `.xlsb` and `.xls`. Both are fixed in the fork.

`parse_formula` treats `PtgExp` (0x01) as a no-op:

```rust
0x01 => {
    // PtgExp: array/shared formula, ignore
    debug!("ignoring PtgExp array/shared formula");
    stack.push(formula.len());
    rgce = &rgce[4..];
}
```

The cell then decodes to an empty string, and `worksheet_formula` drops every
cell whose text is empty. `PtgExp` is how Excel encodes *a member of a shared or
array formula group*: the token points at the group's master cell, whose formula
must be re-expanded at the member's offset.

Excel uses this heavily. It is the normal encoding for a column of repeated
formulas — precisely the structure that matters most to us.

### Measured on a real 170 MB workbook

43.5 million populated cells across 25 sheets:

| | Formula cells |
|---|---|
| Present in the file | 6,793,166 |
| Decoded by calamine | 2,012,750 |
| **Silently dropped** | **4,780,416 (70.4%)** |

Whole sheets recover 0%. On one sheet with 390,730 formula cells, a histogram of
the leading Ptg token shows **390,728 are `PtgExp`** and exactly 2 are ordinary
references — matching the 2 formulas calamine returned for that sheet.

Two independent methods agree on the totals: walking the raw BIFF12 record
stream in Python, and driving calamine's own `worksheet_cells_reader` /
`next_formula` and counting empty results. See
`crates/eg-ingest/examples/xlsb_formula_probe.rs`.

### Why this blocks the project

The dependency graph is the core of ExcelGRAG. Losing 70% of formulas means
losing 70% of its edges, and losing them *invisibly* — the cells still carry
their cached values, so nothing looks wrong. Formula grouping, precedent
tracing, and what-if analysis would all quietly operate on a fraction of the
model. This is worse than an outright failure, because the output stays
plausible.

It cannot be worked around from outside calamine: once `PtgExp` is dropped,
there is no way to tell which cells had one.

### The fix

`PtgExp` is 5 bytes: the token plus a 4-byte row naming the group's first row.
The group's real token stream lives in a `BrtShrFmla` (0x01AB) or `BrtArrFmla`
(0x01AA) record, whose payload is a 16-byte range — `rwFirst, rwLast, colFirst,
colLast` — followed by the token length and the tokens. `BrtArrFmla` carries an
extra flags byte before the length.

Two details made this more than a lookup:

- **A definition can appear after the members that use it**, so a single pass
  cannot resolve them. `XlsbCellsReader::formulas` collects members and
  definitions, then resolves at the end. `worksheet_formula` uses it, while
  `next_formula` keeps its old behaviour so the public API is unchanged.
- **Definitions use the relative `PtgRefN` and `PtgAreaN` tokens**, which
  calamine did not handle at all. They store signed offsets from the cell being
  evaluated, so `parse_formula` now takes an anchor cell and each member decodes
  the shared token stream at its own position. That is what makes every row of a
  filled-down column come out with its own references.

Excel chunks shared formula groups at about 64 rows, which is why one sheet of
the sample workbook has 6,106 definitions covering 390,728 member cells.

### Verification

On the same 170 MB workbook, through `worksheet_formula`:

| | Formula cells |
|---|---|
| Present in the file | 6,793,166 |
| Recovered after the fix | **6,793,166 (100%)** |

The count alone would not prove correctness — a mis-applied anchor gives the
same count with every row pointing at the wrong cells. So
`crates/eg-ingest/examples/shared_group_check.rs` checks the invariant that
actually matters: two vertically adjacent members of a group must differ by
exactly one row.

```
shared/array formula members found:      4,780,416
  left unresolved by the two-pass read:          0
adjacent member pairs compared:          4,779,791
  advance by exactly one row:            4,779,791
  WRONG:                                         0
  correctness: 100.0000%
```

All 276 of calamine's own tests still pass, and the fork adds fixture-free unit
tests for the N-class reference decoding, anchor shifting, the operator
assignments, and `PtgExp` recognition.

---

## The same defect in `.xls`

`src/xls.rs` had both problems independently, and they are fixed in a second
commit on the same branch.

The BIFF8/BIFF5 shape differs in three ways worth recording, since each one
produced a wrong answer before it was pinned down:

- Definitions live in `ShrFmla` (0x04BC) and `Array` (0x0221) records, whose
  range header is a **`RefU`** — `rwFirst(2) rwLast(2) colFirst(1) colLast(1)` —
  with **single-byte columns**. Reading it as the wider `Ref8U` yields plausible
  rows and nonsense columns, so nothing matches and every member silently stays
  unresolved. The formula begins at offset 8 for `ShrFmla` and 12 for `Array`.
- `PtgRefN` is 4 bytes in BIFF8 but 3 before it, and the two layouts **swap the
  relative-flag bits**: BIFF8 uses `fColRel = 0x8000` and `fRwRel = 0x4000` on
  the column field, while BIFF2-5 uses `fRwRel = 0x8000` and `fColRel = 0x4000`
  on the row field.
- `parse_dimensions` cannot be reused for these ranges. It accepts only the
  10- and 14-byte `Dimensions` record and returns an error otherwise — an error
  that propagates out of the sheet read and drops every formula on the sheet.

### Verification

Across calamine's own `.xls` fixtures, formulas recovered went from **496 to
869**, with **none of the previously decoded formulas lost** and exactly two
changed — both the operator fix, both now agreeing with the `issues.xlsx` twin.

Of the 373 newly resolved cells, the vertically adjacent ones advance by exactly
one row in 303 of 310 pairs. The seven exceptions are genuine group boundaries
where the formula changes shape; the long runs are uniform throughout, which is
what correct decoding looks like.

---

## Performance note

Resolving members needs care about how the definition is found. Excel splits a
filled-down column into groups of about 64 rows, so a large sheet holds
thousands of definitions and millions of members; scanning every definition per
member made the sample workbook take **43.7s** to load, against 7.9s when
shared formulas were not resolved at all.

Definitions are now indexed by column and located by binary search, which is
sound because groups within a column cannot overlap. That brings the same
workbook to **10.1s** while decoding 3.4x as many formulas — with identical
output.
