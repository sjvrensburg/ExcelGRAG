# Upstream issues in calamine

Eight defects found in [calamine](https://github.com/tafia/calamine) 0.36.1
while building ExcelGRAG.

**Status:** issues 2 and 3 are fixed in a fork at `../calamine`, branch
`xlsb-shared-formulas`, wired in through `[patch.crates-io]` in the workspace
`Cargo.toml`. Both are fixed for `.xlsb` and `.xls`, in two commits — one per
format — submitted as [tafia/calamine#712](https://github.com/tafia/calamine/pull/712).
Issue 1 was our own bug and is fixed here. Issue 4 was found later, while
building the graph, and is submitted separately as
[#713](https://github.com/tafia/calamine/pull/713) — unrelated code, so it did
not belong on a branch already under review. Issues 5, 6 and 7 were found later still, by
recomputing formulas rather than reading them, and sit on their own branches
`xlsb-relative-columns`, `xlsb-formula-error-cells` and
`xlsb-external-supbooks`; all three are pushed to the fork but not yet opened
upstream. Issue 8 was found by a six-way audit of ExcelGRAG's own code rather
than by parity or recompute, and is committed directly to `excelgrag` (it has
no independent topic branch) but not yet opened upstream.

The workspace patches in an `excelgrag` branch carrying both fixes, since
`[patch.crates-io]` takes a single source.

They were found by the format-parity test — reading the same logical workbook as
`.xlsx` and as `.xlsb` and requiring identical values and formulas — by auditing
a real 170 MB XLSB workbook, or, for issue 5, by recomputing that workbook's
formulas and comparing the answers with the values Excel had stored.

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

---

## What the review caught

A code review before opening the PR found a third bug in the `.xls` work, worth
recording because of *how* it hid.

The column offset in a BIFF8 `RgceLocRel` is **8 bits** wide, not the 14 bits
XLSB uses — the older format has 256 columns, not 16384. Masking 14 bits reads
the `-1` written as `0xFF` as `+255`, so `SUM(B38:I38)` decoded as
`SUM(IX38:JE38)`. Ninety formulas in calamine's own fixtures were wrong, and
they would have shipped.

**The row-advance invariant cannot catch this.** Every member of a group gets
the *same* wrong column, so consecutive rows still differ by exactly one row and
the check passes. Verifying rows says nothing about columns.

Two things exposed it:

- Generating a fixture with LibreOffice, which cannot write XLSB but *can* write
  `.xls`, and diffing against the `.xlsx` twin: `A1*2` against `IW1*2`.
- `crates/eg-ingest/examples/column_sanity.rs`, which checks that a sheet-local
  reference lands inside the columns the sheet actually uses. Column `JJ` in a
  256-column format is impossible on its face — the smell was visible in the
  earlier output and went unremarked.

Running that checker over the real workbook also retired the last unverified
assumption, that XLSB columns really are 14 bits: no reference exceeds XFD. It
found that unpatched calamine already emits out-of-range references at a higher
rate than the patched build (14.5% against 2.8%), including a column `USO` on a
sheet using `A..I`. That is pre-existing behaviour on other token types, not a
regression, and is left alone here.

---

## 4. A sheet name that needs quoting is written bare

**Affects:** `.xlsb` and `.xls`. Not `.xlsx`, whose formulas are stored as text
Excel already wrote correctly.

**Fixed** in the fork and submitted as
[tafia/calamine#713](https://github.com/tafia/calamine/pull/713), independently
of #712 and branched from `master`, so the two can be taken in either order.

Excel requires a sheet name containing a space, a hyphen, or other punctuation
to be quoted inside a formula: `'BP136-6-WORK DOC'!A1`. calamine concatenates
the name unquoted:

```rust
formula.push_str(&sheets[ixti as usize]);
formula.push('!');
```

(`src/xlsb/mod.rs`, four sites around lines 865–906.)

So the formula text we receive is `BP136-6-WORK DOC!A1`, which Excel would
reject and which no parser can read back correctly. A scanner sees a reference
to a sheet called `DOC`.

### Measured on the reference workbook

10 of its 25 sheets have names needing quotes, and 1,665 formula references use
one of them:

| Sheet | Bare uses | Read instead as |
|---|---|---|
| `BP136-6-WORK DOC` | 1,472 | `DOC` |
| `PIVOT CLASSIFIC` | 48 | `CLASSIFIC` |
| `IMPAIR_PROV_DEBT CLASS` | 45 | `CLASS` |
| `PIVOT PER SERVICE PER DAY` | 72 | `DAY` |
| `PIVOT PER SERVICE` | 13 | `SERVICE` |
| `PIVOT SERVICE IMPAIR` | 12 | `IMPAIR` |
| `PIVOT_SERVICE AND TYPE` | 3 | `TYPE` |

`crates/eg-ingest/examples/unquoted_sheets.rs` produces this table.

### Why the severity depends on luck

There are two outcomes, and only the first is visible:

- The fragment names no sheet, so the reference is counted as broken. That is
  what happens on this workbook: all 1,665 land in the graph's
  "references to sheets the workbook does not have" bucket.
- **The fragment matches a different real sheet**, and the dependency is
  attributed to it silently. A workbook with sheets `Q3 SALES` and `SALES` —
  an entirely ordinary pair — would wire every reference to the first into the
  second, with nothing to see. No count moves, no error is raised.

The checker reports whether any fragment collides. On this workbook, none does.

### The fix

`push_sheet_name` in `utils.rs`, beside the existing `push_column`, quotes when
the name is not a plain identifier and doubles any interior apostrophe. It also
quotes a name shaped like a cell reference — `Q1!A1` would otherwise read as
cell `Q1` of the current sheet — and `R` or `C` alone, which collide with R1C1
notation. The `#REF` marker the readers substitute for a missing sheet is an
error marker rather than a name, so it stays unquoted.

`.xls` is covered end to end by `tests/OOM_alloc.xls`, which has a sheet called
`EIM New Deals` and formulas referring to it. **No `.xlsb` fixture exercises it
and none can be authored**, since no open-source tool writes XLSB; that path
rests on the helper's unit tests and on the call sites being identical in shape.

### What it changed, measured

Rebuilding the graph on the reference workbook:

| | Before | After |
|---|---|---|
| Bare uses of a name needing quotes | 1,665 | **0** |
| References to a sheet that does not exist | 1,663 | **0** |
| `CROSS_SHEET_REF` edges | 53 | **95** |
| References scanned | 23,626,201 | 23,624,729 |

The last row is the part worth keeping. The count fell by **1,472 — exactly the
number of bare uses of `BP136-6-WORK DOC`** — because `BP136` is itself a valid
cell reference. The scanner was reading that prefix as a reference to cell
`BP136` *on the source sheet*, and the remainder as `DOC!AK$12`. So the defect
was not only losing 1,663 real references; it was **fabricating 1,472 that no
formula ever wrote**, and they landed in the "empty target" bucket, whose count
fell by the same 1,472.

That is the silent-misattribution case actually occurring, reached through a
prefix that parses as a cell rather than through a sheet-name collision. Nothing
about the output looked wrong.

## 5. A reference's relativity flags, read twice wrongly

**Affects:** `.xlsb`. Pre-existing upstream, in code untouched by #712.

**Fixed** in the fork on branch `xlsb-relative-columns`, branched from `master`
and independent of #712 and #713. Not yet opened upstream.

An `RgceLoc` column field is 14 bits of column and two of relativity. **Both
halves of that sentence were being read wrongly**, and the branch fixes them in
that order: the flags were being taken as part of the column, and once they were
not, they turned out to mean the opposite of what the reader believed.

MS-XLSB orders the field column-first, so `0x4000` marks the column relative and
`0x8000` the row — the reverse of the BIFF8 layout the `.xls` reader documents.
Same two bits, different format.

### 5a. The flags are read as part of the column

`PtgRef` masks them off.
`PtgArea`, `PtgRef3d` and `PtgArea3d` read the field whole and print both
components as absolute, under a `// TODO: check with relative columns`:

```rust
formula.push('$');
push_column(read_u16(&rgce[10..12]) as u32, &mut formula);
```

A relative column 2 is stored as `0x4002`, which taken as a column index is
16,386 — two past `XFD`, the last column a worksheet has — and `push_column`
renders it as a four-letter column no formula can name. Sheet names below are
neutralised; the rest is verbatim:

| Decoded | Actually |
|---|---|
| `=SUM($BTRO$4:$BTRT$4)` | `=SUM(C4:H4)` |
| `=Summary!$BTRO$25` | `=Summary!C25` |
| `=VLOOKUP(B2,Data!$XFF$1:$XFM$1048576,5,0)` | `=VLOOKUP(B2,Data!$B1:$I1048576,5,0)` |

The first is a row total in column I over the six columns to its left, which is
how you know the mask is right and not merely in range.

### Measured on the reference workbook

**855,637 of its 6,793,166 formulas — 12.6% — named a column that cannot
exist.** Nothing failed. A formula that is displayed still looks like a formula;
only something that reads it finds the reference points nowhere, which is why
this survived the graph, the index and the retrieval layer and was caught by the
first component to evaluate a formula rather than parse it.

### 5b. The two flags are the wrong way round

Masking them off leaves the question of what they mean, and the reader had them
swapped. For `PtgRef`, `PtgArea` and the 3-D forms that only moves the `$`
signs, since an absolute and a relative reference name the same cell. For
`PtgRefN` and `PtgAreaN` it decides whether the stored number is an index or an
offset from the cell being evaluated — so it changes which cell is read, and
those are the tokens every filled formula is built from.

The token for the second operand of `AJ5` on the reference workbook is:

```
field = 0x8021, raw_row = 0, anchor = (row 4, col 35)
```

Read as the reader had it — column relative, row absolute — that is column
35 + 33 = `BQ`, row 1: `=V5*BQ$1`, where `BQ1` holds 159.49 and the product is
93,958. Excel stored 295.87. Read correctly — column absolute, row relative —
it is column 33, row 4 + 0: `=V5*$AH5`, where `AH5` holds 0.502222 and the
product is 295.86915555555555, the stored value to the last digit. `AH` is that
sheet's "% to provide" column, and a provision of ageing bucket times percentage
is what the formula is for. Four sibling cells decode the same way and match
their stored values exactly; the row below stores 0, which only the relative row
explains.

**934,118 formulas — 13.7% of the workbook — moved from disagreeing with their
stored value to agreeing with it.**

### The fix

One `read_loc_col` splits the field, so a single place knows the layout, and
each component is marked `$` by its own flag through `push_a1`. `PtgRef` is
routed through both — a refactor, not a change: its masking and its flags
already matched, and `formula_xlsb` covers it, `issues.xlsb` decoding to
`B1+OneRange` before and after.

Nothing in either repository's fixtures discriminates the two flag readings: a
fixture would need a reference absolute in one axis and relative in the other,
and none has one. That is also why this survived — `B1+OneRange` is relative in
both axes, and both readings agree on it.

No fixture in either repository exercises a relative area or a 3-D reference,
and none can be authored for `.xlsb`. The branch carries unit tests for the
field split, for each of the four `$` combinations, for both corners of an area
and for the two 3-D forms; the end-to-end path rests on the reference workbook.

### What it changed, measured

Recomputing every formula and comparing with the value Excel stored:

| | Before | After 5a | After 5b |
|---|---|---|---|
| Agreed with the stored value | 71.9% | 83.8% | **97.6%** |
| Disagreed | 13.8% | 14.5% | **0.7%** |
| Not recomputable | 14.3% | 1.7% | **1.7%** |
| Failed to parse | 855,637 | 0 | **0** |

What is left of the third row is two functions `eg-eval` does not implement,
`PV()` and `GETPIVOTDATA()`. Nothing left in it is a decoding failure.

The middle column is worth reading twice: masking the flags off *raised* the
disagreement count, because 855,637 formulas that had been unreadable became
readable and then, being read with the flags reversed, wrong. A fix that makes
a number worse is not always a wrong fix — it can be one that stops hiding the
next defect.

## 6. A formula cell whose cached value is an error is skipped

**Affects:** `.xlsb`. Pre-existing upstream.

**Fixed** in the fork on branch `xlsb-formula-error-cells`, branched from
`master` and independent of every other branch. Not yet opened upstream.

`next_cell` pairs each literal record with its formula counterpart:
`BrtCellBool` with `BrtFmlaBool`, `BrtCellReal` with `BrtFmlaNum`, `BrtCellSt`
with `BrtFmlaString`. `BrtCellError` (`0x0003`) has no such pair, so
`BrtFmlaError` (`0x000B`) falls through the match's catch-all and the record is
skipped:

```rust
0x0004 | 0x000A => DataRef::Bool(self.buf[8] != 0), // BrtCellBool or BrtFmlaBool
0x0005 | 0x0009 => { .. }                           // BrtCellReal or BrtFmlaNum
0x0006 | 0x0008 => { .. }                           // BrtCellSt or BrtFmlaString
0x0003 => { .. }                                    // BrtCellError, alone
```

The cell is then not merely valueless but **absent**: `worksheet_range` has no
entry for it and `used_cells` never yields it, so nothing distinguishes it from
a blank. `worksheet_formula` still returns its formula, so the same coordinate
exists in one range and not the other.

That is an ordinary cell. Any `VLOOKUP` that misses stores `#N/A`, and a
workbook that looks things up has thousands of them: **48,006 on the reference
workbook**, whose stored values could not be read at all. Recomputing every
formula, agreement goes from 97.6% to **98.3%**.

### The fix

`0x000B` joins the `0x0003` arm. The error byte sits at the same offset in both
records — `next_formula_record` already reads the two together for that reason —
and nothing in that arm reads past it.

No fixture exercises it and none can be authored, for the usual reason. Verified
against the reference workbook, where a `VLOOKUP` column returning `#N/A` came
back blank before and comes back `#N/A` after.

### What was left

Eleven formulas out of 6,793,166 still disagreed after this. Ten of them were
issue 7 below. The eleventh was ours — Excel forces a subtraction whose
operands are equal to 15 significant digits to zero, and `eg-eval` returned the
`1.49e-8` between the doubles.

## 7. A reference into another workbook is resolved against this one

**Affects:** `.xlsb`. Pre-existing upstream.

**Fixed** in the fork on branch `xlsb-external-supbooks`, branched from `master`
and independent of every other branch. Not yet opened upstream.

An `Xti` carries two indices: a supporting book, and a tab within *that* book.
`BrtExternSheet` used only the second:

```rust
match read_i32(&xti[4..8]) {
    -2 => "#ThisWorkbook",
    -1 => "#InvalidWorkSheet",
    p if p >= 0 && (p as usize) < sheets.len() => &sheets[p as usize].0,
    _ => "#Unknown",
}
```

For a workbook that links to another one — last year's copy of itself, most
commonly — that names a real sheet of ours, records a real dependency, and
points it at the wrong place. **Nothing about the result looks wrong.** The
reference reads `JOURNAL_IMPAIR!$D$21`, `JOURNAL_IMPAIR` exists, and `$D$21` is
a cell on it.

### Measured on the reference workbook

Its externals block declares four supporting books — `BrtSupSelf` (`0x0165`)
first, then three `BrtSupBookSrc` (`0x0163`) — and its 18 `Xti` entries name
them:

| ixti | supbook | tab | resolved to | actually |
|---|---|---|---|---|
| 2 | 1 | 5 | `JOURNAL_IMPAIR` | sheet 6 of another workbook |
| 6 | 2 | 12 | `PIVOT PER SERVICE` | sheet 13 of another workbook |
| 16 | 3 | 26 | `#Unknown` | sheet 27 of another workbook |

Twelve formulas used them. Ten disagreed with the value Excel had stored; two
agreed by coincidence, the local cell happening to hold the same number as the
foreign one, which is the case that would never have been found by looking.

The proof is arithmetic rather than documentary. `Sheet1!AB2` reads

```
=IF($B2="ACTIVE",JOURNAL_IMPAIR!$D$21,JOURNAL_IMPAIR!$D$22)
```

and Excel stored 2. `JOURNAL_IMPAIR` is 13 rows long, so `$D$21` is empty and
the formula could not have produced 2 from it. `INDICATORS!D21:D24` — the sheet
the *self*-book `Xti` at index 3 names — holds `2, 0, 2, 1`, which is what all
six formulas of that family stored. The external books are copies of this
workbook, and their indicator sheet sits at a different tab index.

### The fix

Supporting books are tracked in declaration order, and a tab index resolves
against our sheets only when the `Xti` names ours. A reference into another
book is written `[1]#Sheet3`, in the shape an external reference already has,
because the sheet's own name lives in that workbook and it is not open. A
workbook that declares no supporting books keeps the old behaviour, every
reference being local. The two sentinels still outrank the book index.

`xti_sheet` is a free function with unit tests for each case. No fixture has an
external link and none can be authored for `.xlsb`; the end-to-end evidence is
that workbook.

### What it changed, measured

| | Before | After |
|---|---|---|
| Agreed with the stored value | 98.3% | **98.3%** |
| Disagreed | 11 | **0** |
| Not recomputable | 115,757 | 115,769 |

**Zero.** Every one of the 6,677,397 formulas `eg-eval` can evaluate now
computes exactly what Excel stored. What is left in the last row is 115,566
`PV()`, 191 `GETPIVOTDATA()` and the 12 references into workbooks that are not
open — three honest gaps, no decoding failures.

## 8. A declared table's totals row was subtracted by the wrong count, and neither count was exposed

**Affects:** `.xlsx`. Pre-existing upstream, and a missing feature alongside it.

**Fixed** in the fork, committed directly to `excelgrag`. Not yet opened
upstream.

Two related problems in the same code path, `Xlsx::read_tables()`
(`src/xlsx/mod.rs`):

**The range bug.** A declared table's `ref` range is shifted to exclude its
header and totals rows before becoming `Table::data()`:

```rust
let mut dims = get_dimension(table_meta.ref_cells.as_bytes())?;
if table_meta.header_row_count != 0 {
    dims.start.0 += table_meta.header_row_count;
}
if table_meta.totals_row_count != 0 {
    dims.end.0 -= table_meta.header_row_count;   // should be totals_row_count
}
```

The `dims.end` line subtracted `header_row_count` instead of
`totals_row_count`. For the overwhelmingly common shape — one header row, and
either no totals row or one — both counts are small integers and the two
tables ExcelGRAG's own fixtures happen to exercise never disagree, so this
passed every existing test. It surfaces exactly when the counts differ: a
table with **no** header row (`headerRowCount="0"`, "My table has headers"
unchecked in Excel) and a totals row (`totalsRowCount="1"`) subtracts 0
instead of 1, leaving the fabricated totals row inside `table.data()` — the
totals formula reads back as if it were one more row of data.

**The missing accessors.** Even correctly computed, neither count reached a
caller: `Table<T>` carried `name`, `sheet_name`, `columns`, `data` and nothing
else, so ExcelGRAG's ingest layer had no way to ask a table whether it
actually declared a header row — only whether `columns()` was non-empty, which
is **always** true, since Excel auto-names an unheaded table's columns
(`Column1`, `Column2`, …) rather than leaving them empty. A headerless table's
auto-generated column names were consequently read as if they were the row
above the table, annexing whatever real content happened to sit there.

### The fix

`InnerTableMetadata`'s already-parsed `header_row_count`/`totals_row_count`
are threaded through `Tables`, `TableMetadata`, `table_by_name` and
`table_by_name_ref` into two new fields on `Table<T>`, with public accessors
`Table::has_header_row()` and `Table::has_totals_row()` (`src/lib.rs`)
alongside the existing `name()`/`sheet_name()`/`columns()`/`data()`. The
`dims.end` line now subtracts `totals_row_count`.

No existing fixture has `headerRowCount="0"` or a nonzero `totalsRowCount` —
checked directly against every `.xlsx` fixture's `xl/tables/*.xml`, calamine's
own and ExcelGRAG's vendor set alike. XLSX table XML is plain OOXML, unlike
XLSB, so the regression test (`tests/test.rs`,
`test_headerless_table_with_totals_row`) authors one on the fly with
`rust_xlsxwriter` (a new dev-dependency) rather than needing a checked-in
fixture: a headerless, three-row table with a totals row, asserting
`!has_header_row()`, `has_totals_row()`, and that `data()` is exactly the
three data rows — reverting the `dims.end` line back to
`header_row_count` reproduces the original bug and fails it (`data.height()`
comes back `4`, the totals row counted as data).
