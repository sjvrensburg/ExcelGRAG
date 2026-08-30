//! Turning an expansion into text an agent can ground an answer in.
//!
//! The constraint that shapes this is not prose quality, it is *checkability*.
//! A passage that reads well and cannot be verified is worse than a table of
//! ranges, because it invites an answer nobody can trace. So every node carries
//! the A1 range it stands for, and nothing appears here that the graph did not
//! say — no summaries of what a column probably means, no inferred purpose.
//!
//! # Numbered nodes, relations by number
//!
//! The obvious rendering nests each node under the one that reached it, and
//! repeats a node once per path that found it. On the reference workbook that
//! turns 27 retrieved nodes into 40-odd lines, several of them the same table
//! written out again, and an agent reading it cannot tell that two mentions are
//! one table.
//!
//! So each node is listed once with a number, and relations are given as
//! numbers. It is shorter, it is unambiguous, and it gives the agent a handle:
//! "the figure comes from [4]" is checkable against the list in a way that "the
//! figure comes from the rates table" is not.
//!
//! # What is deliberately absent
//!
//! Cell values. The workbook is 6 GB in memory and the ranges are one read
//! away, so a passage that inlined values would be both enormous and stale.
//! What this gives an agent is where to look; P6 is what looks.

use std::collections::BTreeMap;
use std::fmt::Write;

use eg_graph::{EdgeKind, NodeKind};

use crate::expand::{Retrieved, RetrievedNode, Role, WorkbookContext};

/// How much text to produce.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// A ceiling on the node entries, in characters.
    ///
    /// Characters and not tokens: this crate has no tokenizer and should not
    /// pretend to, and every tokenizer disagrees anyway. A caller fitting a
    /// context window should divide by three and leave room.
    ///
    /// Three things are written whatever this says, and nothing else is:
    ///
    /// - The first node of the passage. A preamble with nothing under it would
    ///   be worse than one that overran.
    /// - The notice that workbooks are missing from the corpus, shortened to a
    ///   bare count if it has to be. A caller not told that data is absent will
    ///   present what is left as everything there was.
    /// - The closing line saying how much was cut, for the same reason.
    ///
    /// Together those are a few hundred characters at worst, and a caller
    /// sizing a window should leave room for them rather than treat this as a
    /// hard bound. `max_chars: 10` against a corpus full of stale hashes still
    /// returns a few hundred characters, all of it saying so.
    pub max_chars: usize,
    /// Show how many cell references stand behind each relation. On by default:
    /// the difference between an edge of weight 1 and one of weight 115,003 is
    /// the difference between a footnote and the spine of the model.
    pub weights: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            max_chars: 8_000,
            weights: true,
        }
    }
}

/// A rendered passage, and what had to be left out of it.
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    pub text: String,
    /// Ranges the passage cites, in the order they appear.
    ///
    /// Handed back separately so a caller can check an answer's citations
    /// against what it was actually given. An agent citing a range that is not
    /// in this list has invented it.
    pub citations: Vec<String>,
    /// Nodes dropped to fit `max_chars`. Named in the text too, because a
    /// passage that was cut and does not say so reads as a complete one.
    pub omitted: usize,
    /// Workbooks that got no room at all, and so are not in the text under any
    /// heading. Counted separately: a reader can see that a listed workbook was
    /// cut short, and has no way at all to notice one that never appeared.
    pub omitted_workbooks: usize,
}

/// Render an expansion.
pub fn render(found: &Retrieved, opts: &RenderOptions) -> Rendered {
    let mut out = Rendered::default();
    // Counted rather than measured off the string: `String::len` is bytes, the
    // ceiling is documented in characters, and a workbook with non-ASCII sheet
    // names would quietly get a smaller allowance than it asked for.
    let mut chars = 0usize;
    let mut wrote_an_entry = false;

    if !found.missing_workbooks.is_empty() {
        // One line, not one paragraph each. Twenty stale hashes used to be 2.2
        // KB of near-identical prose written before the ceiling was consulted,
        // which then left no room for any real workbook.
        //
        // Counted per workbook, which is what `missing_workbooks` holds. It
        // read "result(s)" before, so three hits into one evicted workbook
        // announced themselves as one — understating the loss to exactly the
        // reader the notice exists for.
        const NAMED: usize = 8;
        let count = found.missing_workbooks.len();
        let named: Vec<String> = found
            .missing_workbooks
            .iter()
            .take(NAMED)
            .map(|h| h.chars().take(8).collect())
            .collect();
        let more = count.saturating_sub(NAMED);
        let full = format!(
            "{count} workbook(s) matched by the search are no longer in the \
             corpus ({}{}); their context is missing. Reindex to recover \
             it.\n\n",
            named.join(", "),
            if more > 0 {
                format!(", and {more} more")
            } else {
                String::new()
            }
        );
        // Under a ceiling too small even for that, the hashes go and the fact
        // does not. Losing the names costs a reader a lookup; losing the notice
        // costs them the knowledge that anything is missing at all.
        let notice = if full.chars().count() <= opts.max_chars {
            full
        } else {
            format!("{count} workbook(s) matched by the search are missing from the corpus.\n\n")
        };
        chars += notice.chars().count();
        out.text.push_str(&notice);
    }

    for workbook in &found.workbooks {
        render_workbook(workbook, opts, &mut out, &mut chars, &mut wrote_an_entry);
    }

    if out.omitted > 0 {
        let whole = if out.omitted_workbooks > 0 {
            format!(
                ", including {} workbook(s) that do not appear above at all",
                out.omitted_workbooks
            )
        } else {
            String::new()
        };
        let _ = write!(
            out.text,
            "\n{} further node(s) were retrieved and left out to fit{whole}. \
             Raise the character ceiling, or expand fewer hops.\n",
            out.omitted
        );
    }
    out
}

/// One relation between two retrieved nodes, as the reader should see it.
struct Relation {
    other: usize,
    weight: u64,
    kind: EdgeKind,
}

fn render_workbook(
    workbook: &WorkbookContext,
    opts: &RenderOptions,
    out: &mut Rendered,
    chars: &mut usize,
    wrote_an_entry: &mut bool,
) {
    // Seeds first and in rank order, then everything else in the order the walk
    // found it, which is nearest-then-heaviest. So the numbering itself carries
    // the ranking, and an agent reading top-down reads most-relevant-first.
    let mut order: Vec<&RetrievedNode> = workbook.nodes.iter().filter(|n| n.is_seed()).collect();
    order.extend(workbook.nodes.iter().filter(|n| !n.is_seed()));
    if order.is_empty() {
        return;
    }

    // Node index in the graph -> position in this passage, one-based.
    let numbers: BTreeMap<u32, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node, i + 1))
        .collect();
    let (reads, read_by) = relations(workbook, &numbers);

    // Numbers line up, so a column of them reads as a column.
    let pad = order.len().to_string().len();
    let indent = " ".repeat(pad + 5);

    // How many entries fit, decided before any of them is written. Relations
    // are then rendered against that count, so a surviving entry can never
    // point at a number the passage does not list — the whole reason to know
    // the cut in advance.
    let entry = |i: usize, upto: usize| -> String {
        build_entry(
            workbook, &order, i, upto, &reads, &read_by, opts, pad, &indent,
        )
    };
    // The heading is one of two sentences and they are not the same length: a
    // cut workbook writes "4 of 30 node(s), the rest cut to fit" where a whole
    // one writes "30 node(s)", some twenty-six characters shorter.
    //
    // So it is measured twice. Optimistically first, against the short form,
    // because most workbooks fit whole and charging them for a sentence they
    // will not write cuts passages that had room — a complete 616-character
    // passage needed a 642 ceiling before this, and announced itself as cut.
    // Only when that pass comes up short is the longer form charged, and then
    // against the longest it could be, so the sentence finally written always
    // fits the space measured for it.
    let uncut = self::heading(workbook, order.len(), order.len())
        .chars()
        .count();
    let longest_cut = self::heading(workbook, order.len().saturating_sub(1), order.len())
        .chars()
        .count();

    let mut fits = count_fits(&order, uncut, *chars, *wrote_an_entry, opts, &entry);
    if fits < order.len() {
        fits = count_fits(&order, longest_cut, *chars, *wrote_an_entry, opts, &entry);
    }
    // Entries are measured with every relation present, which is the largest
    // they can be; filtering them afterwards only shrinks the passage.
    // Plus the blank line that closes the workbook, which is written whether or
    // not anything was cut and so belongs in the measurement.
    if fits == 0 {
        // Everything this workbook offered is omitted, and the workbook itself
        // never appears. Both have to be counted: a reader can see that a
        // listed workbook was cut short, and cannot notice one that is absent.
        out.omitted += order.len();
        out.omitted_workbooks += 1;
        return;
    }

    // Reporting `order.len()` here put "27 node(s)" above a list ending at
    // [4]: a passage contradicting itself, which the reader can only resolve
    // from a footer several workbooks further down.
    let heading = self::heading(workbook, fits, order.len());
    out.text.push_str(&heading);
    *chars += heading.chars().count();
    for (i, node) in order.iter().enumerate().take(fits) {
        let text = entry(i, fits);
        *chars += text.chars().count();
        out.text.push_str(&text);
        *wrote_an_entry = true;
        if let Some(a1) = &node.a1 {
            out.citations.push(a1.clone());
        }
    }
    out.omitted += order.len() - fits;
    out.text.push('\n');
    *chars += 1;
}

/// How many entries fit under `opts.max_chars`, charged for a heading of
/// `heading_chars` and for the blank line that closes the workbook.
///
/// Entries are measured with every relation present, which is the largest they
/// can be; filtering them to what survives the cut only shrinks the passage.
fn count_fits(
    order: &[&RetrievedNode],
    heading_chars: usize,
    already: usize,
    wrote_an_entry: bool,
    opts: &RenderOptions,
    entry: &dyn Fn(usize, usize) -> String,
) -> usize {
    let mut used = already + heading_chars + 1;
    let mut fits = 0usize;
    for i in 0..order.len() {
        let size = entry(i, order.len()).chars().count();
        // The very first entry of the whole passage always goes in. A preamble
        // with no nodes under it is not a smaller answer, it is no answer.
        let unconditional = !wrote_an_entry && i == 0;
        if !unconditional && used + size > opts.max_chars {
            break;
        }
        used += size;
        fits += 1;
    }
    fits
}

fn heading(workbook: &WorkbookContext, shown: usize, total: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {}", workbook.path);
    if workbook.truncated {
        let _ = writeln!(
            out,
            "\nThe walk hit its budget: this is part of the context around the \
             match, not all of it."
        );
    }
    let count = if shown < total {
        format!("{shown} of {total} node(s), the rest cut to fit")
    } else {
        format!("{total} node(s)")
    };
    let _ = writeln!(
        out,
        "\n{count}. A `*` marks something the search matched; the rest were \
         reached from it. Every range below is a live location in this \
         workbook, not a value.\n"
    );
    out
}

/// One entry, with its relations limited to entries numbered `upto` or lower.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    workbook: &WorkbookContext,
    order: &[&RetrievedNode],
    i: usize,
    upto: usize,
    reads: &BTreeMap<usize, Vec<Relation>>,
    read_by: &BTreeMap<usize, Vec<Relation>>,
    opts: &RenderOptions,
    pad: usize,
    indent: &str,
) -> String {
    let node = order[i];
    let number = i + 1;
    let mut entry = String::new();

    let _ = writeln!(
        entry,
        "[{number}]{:width$}{} {} {}{}",
        "",
        if node.is_seed() { "*" } else { " " },
        node.kind.as_str(),
        quoted(&node.label, node.a1.as_deref()),
        node.a1
            .as_deref()
            .map(|a1| format!("   {a1}"))
            .unwrap_or_default(),
        width = pad - number.to_string().len() + 1
    );

    // The workbook root is the heading above all of this, so repeating it on
    // every line costs a line's width per node and says nothing.
    let path: Vec<&str> = workbook
        .ancestry(node.node)
        .into_iter()
        .filter(|n| n.kind != NodeKind::Workbook)
        .map(|n| n.label.as_str())
        .collect();
    if !path.is_empty() {
        let _ = writeln!(entry, "{indent}in: {}", path.join(" > "));
    }
    if let Some(line) = relation_line("reads", reads.get(&number), upto, opts) {
        let _ = writeln!(entry, "{indent}{line}");
    }
    if let Some(line) = relation_line("read by", read_by.get(&number), upto, opts) {
        let _ = writeln!(entry, "{indent}{line}");
    }
    entry
}

/// Both directions of every dependency the expansion recorded, by passage
/// number.
///
/// Read off the roles rather than re-walked: the roles are what the expansion
/// actually followed, and a renderer that went back to the graph could show an
/// edge the walk never took.
fn relations(
    workbook: &WorkbookContext,
    numbers: &BTreeMap<u32, usize>,
) -> (
    BTreeMap<usize, Vec<Relation>>,
    BTreeMap<usize, Vec<Relation>>,
) {
    let mut reads: BTreeMap<usize, Vec<Relation>> = BTreeMap::new();
    let mut read_by: BTreeMap<usize, Vec<Relation>> = BTreeMap::new();

    for node in &workbook.nodes {
        let Some(&here) = numbers.get(&node.node) else {
            continue;
        };
        let (reader, target, weight, kind) = match &node.role {
            // `of` reads this node.
            Role::Input { of, weight, kind } => {
                (numbers.get(of).copied(), Some(here), *weight, *kind)
            }
            // This node reads `on`.
            Role::Dependent { on, weight, kind } => {
                (Some(here), numbers.get(on).copied(), *weight, *kind)
            }
            _ => continue,
        };
        // Both ends have to be in the passage. A budget that dropped one leaves
        // a relation pointing at a number the reader cannot look up.
        let (Some(reader), Some(target)) = (reader, target) else {
            continue;
        };
        reads.entry(reader).or_default().push(Relation {
            other: target,
            weight,
            kind,
        });
        read_by.entry(target).or_default().push(Relation {
            other: reader,
            weight,
            kind,
        });
    }

    for list in reads.values_mut().chain(read_by.values_mut()) {
        list.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.other.cmp(&b.other)));
    }
    (reads, read_by)
}

fn relation_line(
    verb: &str,
    list: Option<&Vec<Relation>>,
    upto: usize,
    opts: &RenderOptions,
) -> Option<String> {
    let list = list?;
    let parts: Vec<String> = list
        .iter()
        // A relation to an entry the ceiling cut is dropped rather than
        // printed. A dangling `[37]` is worse than a missing one: the agent
        // will cite it.
        .filter(|r| r.other <= upto)
        .map(|r| {
            if opts.weights {
                format!(
                    "[{}] ({} refs{})",
                    r.other,
                    thousands(r.weight),
                    across(r.kind)
                )
            } else {
                format!("[{}]", r.other)
            }
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(format!("{verb}: {}", parts.join(", ")))
}

/// Named only where it is not the ordinary case, so the common line stays short.
fn across(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::CrossSheetRef => ", another sheet",
        EdgeKind::CrossWorkbookRef => ", another workbook",
        EdgeKind::ReferencesName => ", by name",
        _ => "",
    }
}

/// A label in quotes, unless it is the node's own range, which is already the
/// citation on the same line and reads as noise repeated.
///
/// Compared against the citation rather than guessed at from the shape. A
/// header reading `FY2024` is a perfectly good A1 reference — column FY, row
/// 2024 — so no test on the string can tell a range from a name, and the
/// version that tried blanked `Q1`, `H2` and `2024` off their own nodes.
fn quoted(label: &str, a1: Option<&str>) -> String {
    if label.is_empty() {
        return String::new();
    }
    if is_the_citation(label, a1) {
        return String::new();
    }
    format!("{label:?}")
}

/// Whether the label is exactly the local part of the citation beside it.
fn is_the_citation(label: &str, a1: Option<&str>) -> bool {
    let Some(a1) = a1 else { return false };
    match a1.rsplit_once('!') {
        Some((_, local)) => local == label,
        None => a1 == label,
    }
}

/// 115003 reads as a different number at a glance than 115,003 does, and the
/// weights here span six orders of magnitude.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separates_every_three_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(115_003), "115,003");
        assert_eq!(thousands(1_048_576), "1,048,576");
    }

    #[test]
    fn a_label_that_is_its_own_citation_is_not_printed_twice() {
        assert!(is_the_citation("A1:BM115004", Some("Sheet1!A1:BM115004")));
        assert!(is_the_citation("B7", Some("\'Q3 Sales\'!B7")));
        assert!(!is_the_citation("RATES", Some("LOOKUP!AE53:AG89")));
        assert!(!is_the_citation("A1:B2", None));
    }

    #[test]
    fn a_header_shaped_like_a_reference_keeps_its_name() {
        // Column FY, row 2024 is a real address, so no test on the string can
        // tell this from a range. Only the citation beside it can.
        for header in ["FY2024", "Q1", "H2", "2024"] {
            assert_eq!(
                quoted(header, Some("Sales!C2:C99")),
                format!("{header:?}"),
                "{header} lost its name"
            );
        }
    }
}
