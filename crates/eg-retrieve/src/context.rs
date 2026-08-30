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
    /// A ceiling on the passage, in characters.
    ///
    /// Characters and not tokens: this crate has no tokenizer and should not
    /// pretend to, and every tokenizer disagrees anyway. A caller fitting a
    /// context window should divide by three and leave room.
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
}

/// Render an expansion.
pub fn render(found: &Retrieved, opts: &RenderOptions) -> Rendered {
    let mut out = Rendered::default();

    for hash in &found.missing_workbooks {
        let _ = writeln!(
            out.text,
            "A result matched workbook {} which is no longer in the corpus; \
             its context is missing. Reindex to recover it.\n",
            hash.chars().take(8).collect::<String>()
        );
    }

    for workbook in &found.workbooks {
        render_workbook(workbook, opts, &mut out);
    }

    if out.omitted > 0 {
        let _ = write!(
            out.text,
            "\n{} further node(s) were retrieved and left out to fit. \
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

fn render_workbook(workbook: &WorkbookContext, opts: &RenderOptions, out: &mut Rendered) {
    // Seeds first and in rank order, then everything else in the order the walk
    // found it, which is heaviest-edge first. So the numbering itself carries
    // the ranking, and an agent reading top-down reads most-relevant-first.
    let mut order: Vec<&RetrievedNode> = workbook.nodes.iter().filter(|n| n.is_seed()).collect();
    order.extend(workbook.nodes.iter().filter(|n| !n.is_seed()));

    // Node index in the graph -> position in this passage, one-based.
    let numbers: BTreeMap<u32, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.node, i + 1))
        .collect();

    let (reads, read_by) = relations(workbook, &numbers);

    let _ = writeln!(out.text, "# {}", workbook.path);
    if workbook.truncated {
        let _ = writeln!(
            out.text,
            "\nThe walk hit its budget: this is part of the context around the \
             match, not all of it."
        );
    }
    let _ = writeln!(
        out.text,
        "\n{} node(s). A `*` marks something the search matched; the rest were \
         reached from it. Every range below is a live location in this \
         workbook, not a value.\n",
        order.len()
    );

    // Numbers line up, so a column of them reads as a column.
    let pad = order.len().to_string().len();
    let indent = " ".repeat(pad + 5);

    for (i, node) in order.iter().enumerate() {
        let number = i + 1;
        let mut entry = String::new();

        let _ = writeln!(
            entry,
            "[{number}]{:width$}{} {} {}{}",
            "",
            if node.is_seed() { "*" } else { " " },
            node.kind.as_str(),
            quoted(&node.label),
            node.a1
                .as_deref()
                .map(|a1| format!("   {a1}"))
                .unwrap_or_default(),
            width = pad - number.to_string().len() + 1
        );

        // The workbook root is the heading above all of this, so repeating it
        // on every line costs a line's width per node and says nothing.
        let path: Vec<&str> = workbook
            .ancestry(node.node)
            .into_iter()
            .filter(|n| n.kind != NodeKind::Workbook)
            .map(|n| n.label.as_str())
            .collect();
        if !path.is_empty() {
            let _ = writeln!(entry, "{indent}in: {}", path.join(" > "));
        }
        if let Some(line) = relation_line("reads", reads.get(&number), opts) {
            let _ = writeln!(entry, "{indent}{line}");
        }
        if let Some(line) = relation_line("read by", read_by.get(&number), opts) {
            let _ = writeln!(entry, "{indent}{line}");
        }

        // Whole entries are dropped, never half of one. A passage cut mid-way
        // through a citation is a passage that cites a range that does not
        // exist.
        //
        // The first entry always goes in, whatever the budget. A preamble with
        // no nodes under it is not a smaller answer, it is no answer, and a
        // caller who set the ceiling too low should get the best hit and a
        // count of what would not fit.
        if i > 0 && out.text.len() + entry.len() > opts.max_chars {
            out.omitted += order.len() - i;
            break;
        }
        out.text.push_str(&entry);
        if let Some(a1) = &node.a1 {
            out.citations.push(a1.clone());
        }
    }
    out.text.push('\n');
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

fn relation_line(verb: &str, list: Option<&Vec<Relation>>, opts: &RenderOptions) -> Option<String> {
    let list = list?;
    if list.is_empty() {
        return None;
    }
    let parts: Vec<String> = list
        .iter()
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

/// A label in quotes, unless it is a bare A1 range, which is already the
/// citation on the same line and reads as noise repeated.
fn quoted(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }
    if looks_like_a_range(label) {
        return String::new();
    }
    format!("{label:?}")
}

fn looks_like_a_range(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == ':' || c == '$')
        && label.chars().any(|c| c.is_ascii_digit())
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
    fn a_bare_range_is_not_repeated_as_a_label() {
        assert!(looks_like_a_range("A1:BM115004"));
        assert!(looks_like_a_range("B7"));
        assert!(!looks_like_a_range("RATES"));
        assert!(!looks_like_a_range("Total Debt"));
        assert!(!looks_like_a_range(""));
    }
}
