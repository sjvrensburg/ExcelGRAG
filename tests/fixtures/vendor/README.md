# Vendored fixtures

These workbooks come from the [calamine](https://github.com/tafia/calamine) test
suite (MIT licensed; see `LICENSE-calamine.md`), and are vendored because they
were authored by **real Excel**.

That matters specifically for the `.xlsb` files. No open-source tool can write
XLSB — not LibreOffice, which imports it but has no export filter, and not any
Rust or Python library. Hand-rolling one for tests would encode our own reading
of the binary format rather than testing against what Excel actually emits, so
genuine Excel output is the only trustworthy input.

Each basename here exists as both `.xlsx` and `.xlsb`, which is what makes them
usable as format-parity pairs.
