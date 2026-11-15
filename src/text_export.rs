//! Fixed UTF-8 report of the drawn map, not a recursive filesystem listing or a script.
use crate::index::{EntryId, Index, Kind, SizeMetric};
use crate::treemap::{Layout, NodeRef, Placed};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write;

pub struct View<'a> {
    pub index: &'a Index,
    pub layout: &'a Layout,
    pub location: &'a str,
    pub expression: &'a str,
    pub filtered: bool,
    pub metric: SizeMetric,
}

/// Escape controls, separators and lone surrogates so every cell occupies exactly one line.
pub fn escape(text: &OsStr) -> String {
    #[cfg(windows)]
    let units = {
        use std::os::windows::ffi::OsStrExt;
        text.encode_wide().collect::<Vec<_>>()
    };
    #[cfg(not(windows))]
    let units = text.to_string_lossy().encode_utf16().collect::<Vec<_>>();
    let mut output = String::new();
    for character in char::decode_utf16(units) {
        match character {
            Ok('\\') => output.push_str("\\\\"),
            Ok('\n') => output.push_str("\\n"),
            Ok('\r') => output.push_str("\\r"),
            Ok('\t') => output.push_str("\\t"),
            Ok(c) if !c.is_control() => output.push(c),
            Ok(c) => {
                let _ = write!(output, "\\u{{{:04x}}}", c as u32);
            }
            Err(c) => {
                let _ = write!(output, "\\u{{{:04x}}}", c.unpaired_surrogate());
            }
        }
    }
    output
}

pub fn render(view: View<'_>) -> String {
    let mut output = format!(
        "HogMap view text v1\nLocation: {}\nMetric: {}\nMode: {}\nExpression: {}\nScope: drawn cells only; aggregates are not expanded.\nEntries without area (unknown or zero size) are not listed; unknowns remain in folder and aggregate unknown marks.\nTree (two spaces per level)\tSize\tParent percentage\tKind\n",
        escape(OsStr::new(view.location)),
        match view.metric {
            SizeMetric::Logical => "logical",
            SizeMetric::Allocated => "observed allocated",
        },
        if view.filtered { "Filter" } else { "Highlight" },
        escape(OsStr::new(view.expression)),
    );
    // `Layout::placed` is breadth-first and always starts with the drawn root. A depth-first walk
    // of those same cells keeps every child directly under its parent, so the indentation
    // describes the hierarchy instead of the previous row's subtree.
    let cells = &view.layout.placed;
    let mut children: HashMap<EntryId, Vec<usize>> = HashMap::new();
    for (position, cell) in cells.iter().enumerate().skip(1) {
        let parent = match cell.node {
            NodeRef::Entry(id) => view.index.get(id).and_then(|entry| entry.parent),
            NodeRef::Aggregate { parent } => Some(parent),
        };
        if let Some(parent) = parent {
            children.entry(parent).or_default().push(position);
        }
    }
    let mut emitted = vec![false; cells.len()];
    let mut stack = Vec::new();
    if !cells.is_empty() {
        stack.push(Step {
            position: 0,
            depth: 0,
            parent_totals: None,
        });
    }
    while let Some(step) = stack.pop() {
        if std::mem::replace(&mut emitted[step.position], true) {
            continue;
        }
        let cell = &cells[step.position];
        push_row(
            &mut output,
            view.index,
            cell,
            step.depth,
            step.parent_totals,
        );
        if let NodeRef::Entry(id) = cell.node {
            let parent_totals = Some((cell.known_sum, cell.has_unknown));
            if let Some(children) = children.get(&id) {
                // Pushed reversed so the stack pops them in placement order.
                for child in children.iter().rev() {
                    stack.push(Step {
                        position: *child,
                        depth: step.depth + 1,
                        parent_totals,
                    });
                }
            }
        }
    }
    // A layout only places descendants of its drawn root; keep any stray cell rather than lose a
    // row, with the root's own depth and no parent percentage.
    for (position, cell) in cells.iter().enumerate() {
        if !emitted[position] {
            push_row(&mut output, view.index, cell, 0, None);
        }
    }
    output
}

/// One pending depth-first step: the cell, its ancestry depth and its parent's known totals.
struct Step {
    position: usize,
    depth: usize,
    parent_totals: Option<(u64, bool)>,
}

/// Writes one row. A cell whose entry is gone writes nothing; the caller still walks its children.
fn push_row(
    output: &mut String,
    index: &Index,
    cell: &Placed,
    depth: usize,
    parent_totals: Option<(u64, bool)>,
) {
    let (name, kind) = match cell.node {
        NodeRef::Entry(id) => {
            let Some(entry) = index.get(id) else {
                return;
            };
            (
                escape(entry.name),
                if entry.kind == Kind::Directory {
                    "folder"
                } else {
                    "file"
                },
            )
        }
        NodeRef::Aggregate { .. } => (
            format!("[{} grouped items; not expanded]", cell.members.len()),
            "aggregate",
        ),
    };
    let size = if cell.has_unknown && cell.known_sum == 0 {
        "unknown".into()
    } else {
        format!(
            "{} B{}",
            cell.known_sum,
            if cell.has_unknown { " + unknown" } else { "" }
        )
    };
    let percentage = match parent_totals {
        None => "-".into(),
        Some((bytes, unknown)) if bytes == 0 || unknown || cell.has_unknown => "unknown".into(),
        Some((bytes, _)) => {
            let tenths =
                (u128::from(cell.known_sum) * 1000 + u128::from(bytes) / 2) / u128::from(bytes);
            format!("{}.{:01}%", tenths / 10, tenths % 10)
        }
    };
    let _ = writeln!(
        output,
        "{}{name}\t{size}\t{percentage}\t{kind}",
        "  ".repeat(depth)
    );
}
