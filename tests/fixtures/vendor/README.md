# Vendored fixtures

These workbooks come from the [calamine](https://github.com/tafia/calamine) test
suite (MIT licensed; see `LICENSE-calamine.md`), and are vendored because they
were authored by **real Excel**.

That matters specifically for the `.xlsb` files. No open-source tool can write
XLSB — not LibreOffice, which imports it but has no export filter, and not any
Rust or Python library. Hand-rolling one for tests would encode our own reading
of the binary format rather than testing against what Excel actually emits, so
genuine Excel output is the only trustworthy input.

Each basename here exists as both `.xlsx` and `.xlsb`, and `issues` exists as
`.xls` as well, which is what makes them usable as format-parity fixtures. The
`.xlsx` member of each set is the reference: it stores values and formulas as
XML text, so it is the one whose expected content cannot silently drift.

`relative_references.xls` and `.xlsx` are the exception. They come from
calamine's suite too (added in tafia/calamine#712), but LibreOffice wrote the
`.xls` from the `.xlsx`, and the `.xlsx` was written by openpyxl, so it holds
formula text but no cached values. They are here for the one thing no Excel
fixture has: references that are absolute in one axis and relative in the
other, which is the only kind that tells the two relativity flags apart.
There is no `.xlsb` member.

`OOM_alloc.xls` is also from calamine's suite and has no twin. It is here
because it links to two other workbooks, which makes it the only fixture with
references that must not resolve to this workbook's own sheets.
