use std::ffi::OsString;

use hogmap::filter::{Filter, FilterError, Match};
use hogmap::index::{EntryId, Index, Kind, MetricSummary, SizeMetric};
use hogmap::scan::Terminal;
use hogmap::treemap::{Layout, NodeRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanState {
    Scanning,
    Finished,
    Cancelled,
    Failed,
}

/// The producer's terminal wins: a cancellation requested while its terminal was already queued
/// still ends `Finished` or `Failed`. `cancelled` only explains a scan that has not terminated.
pub fn scan_state(running: bool, cancelled: bool, terminal: Option<Terminal>) -> ScanState {
    match terminal {
        Some(Terminal::Finished) => ScanState::Finished,
        Some(Terminal::Cancelled) => ScanState::Cancelled,
        Some(Terminal::Failed(_)) => ScanState::Failed,
        None if cancelled => ScanState::Cancelled,
        None if running => ScanState::Scanning,
        None => ScanState::Failed,
    }
}

/// Names are native components, not snapshot-local entry IDs. No navigation is persisted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Location {
    pub path: Vec<OsString>,
    pub members: Option<Members>,
}

/// The siblings of an opened aggregate. The names stay fixed when the map metric changes; the
/// metric that grouped them tells whether they are still known to be the small ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Members {
    pub names: Vec<OsString>,
    pub grouped_by: SizeMetric,
}

impl Location {
    pub fn directory(index: &Index, id: EntryId) -> Self {
        let mut path = Vec::new();
        let mut next = Some(id);
        while let Some(id) = next {
            let Some(entry) = index.get(id) else { break };
            if entry.parent.is_some() {
                path.push(entry.name.to_os_string());
            }
            next = entry.parent;
        }
        path.reverse();
        Self {
            path,
            members: None,
        }
    }

    pub fn resolve(&self, index: &Index) -> Option<(EntryId, Option<Vec<EntryId>>)> {
        let mut parent = index.root();
        for component in &self.path {
            parent = index
                .entries()
                .find(|(_, entry)| {
                    entry.parent == Some(parent)
                        && entry.name == component
                        && entry.kind == Kind::Directory
                })?
                .0;
        }
        let members = self.members.as_ref().map(|members| {
            index
                .entries()
                .filter(|(_, entry)| {
                    entry.parent == Some(parent)
                        && members.names.iter().any(|name| name == entry.name)
                })
                .map(|(id, _)| id)
                .collect()
        });
        Some((parent, members))
    }

    /// Whether the opened members are the ones `metric` grouped. After a switch to the other
    /// metric the same names stay open, but they are no longer known to be its small items.
    pub fn members_grouped_by(&self, metric: SizeMetric) -> bool {
        self.members
            .as_ref()
            .is_some_and(|members| members.grouped_by == metric)
    }

    pub fn up(&self) -> Self {
        let mut result = self.clone();
        if result.members.take().is_none() {
            result.path.pop();
        }
        result
    }
}

#[derive(Default)]
pub struct History {
    locations: Vec<Location>,
    cursor: usize,
}

impl History {
    pub fn current(&self) -> Location {
        self.locations.get(self.cursor).cloned().unwrap_or_default()
    }

    pub fn visit(&mut self, location: Location) {
        if self.locations.is_empty() {
            self.locations.push(Location::default());
        }
        if self.current() == location {
            return;
        }
        self.locations.truncate(self.cursor + 1);
        self.locations.push(location);
        if self.locations.len() > 128 {
            self.locations.remove(0);
        }
        self.cursor = self.locations.len() - 1;
    }

    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.locations.len()
    }
    pub fn back(&mut self) {
        if self.can_back() {
            self.cursor -= 1;
        }
    }
    pub fn forward(&mut self) {
        if self.can_forward() {
            self.cursor += 1;
        }
    }
}

/// The selection to keep after the map switches metric. An entry ID stays valid for the same
/// snapshot. An aggregate is only identified by its parent, so it survives only when the new
/// layout groups exactly the same members under that parent. Each metric orders members by its
/// own weights, so the comparison ignores order.
pub fn retained_selection(
    selected: Option<NodeRef>,
    before: Option<&Layout>,
    after: Option<&Layout>,
) -> Option<NodeRef> {
    let node = selected?;
    if matches!(node, NodeRef::Entry(_)) {
        return Some(node);
    }
    let members = |layout: &Layout| {
        layout
            .placed
            .iter()
            .find(|cell| cell.node == node)
            .map(|cell| {
                let mut members = cell.members.clone();
                members.sort_unstable();
                members
            })
    };
    let unchanged = before
        .and_then(members)
        .zip(after.and_then(members))
        .is_some_and(|(before, after)| before == after);
    unchanged.then_some(node)
}

pub fn other_metric(metric: SizeMetric) -> SizeMetric {
    match metric {
        SizeMetric::Logical => SizeMetric::Allocated,
        SizeMetric::Allocated => SizeMetric::Logical,
    }
}

/// The selected metric's totals overflowed. `other_available` is true only when the other
/// metric's summary and derived apparent total are representable; both metrics can overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricOverflow {
    pub metric: SizeMetric,
    pub other_available: bool,
}

/// Totals of the metric the map shows. An overflow is reported for that metric instead of being
/// replaced by zero or by the other metric's totals.
pub fn metric_totals(index: &Index, metric: SizeMetric) -> Result<MetricSummary, MetricOverflow> {
    // Both listed accounting and its derived apparent area must fit.
    index
        .summary_for(metric)
        .and_then(|summary| {
            summary.apparent_bytes()?;
            Ok(summary)
        })
        .map_err(|_| MetricOverflow {
            metric,
            other_available: index
                .summary_for(other_metric(metric))
                .and_then(|summary| summary.apparent_bytes())
                .is_ok(),
        })
}

/// The renderer paints descendants after parents; hit testing follows that same order.
pub fn hit_test(layout: &Layout, x: f32, y: f32) -> Option<usize> {
    layout.placed.iter().rposition(|cell| {
        x >= cell.rect.x && x < cell.rect.right() && y >= cell.rect.y && y < cell.rect.bottom()
    })
}

/// Returns the largest character prefix that fits, reserving width for an ellipsis.
/// Widths come from the renderer's font measurement, not guessed character counts.
pub fn fitting_prefix(widths: &[f32], available: f32, ellipsis: f32) -> Option<(usize, bool)> {
    if !available.is_finite() || available <= 0.0 {
        return None;
    }
    if widths.iter().sum::<f32>() <= available {
        return Some((widths.len(), false));
    }
    if ellipsis > available {
        return None;
    }
    let mut used = ellipsis;
    let mut count = 0;
    for width in widths {
        if used + width > available {
            break;
        }
        used += width;
        count += 1;
    }
    Some((count, true))
}

#[derive(Default)]
pub struct HighlightFilter {
    filter: Filter,
    pub error: Option<FilterError>,
}

impl HighlightFilter {
    pub fn set(&mut self, expression: &str) -> bool {
        match Filter::parse(expression) {
            Ok(filter) => {
                self.filter = filter;
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(error);
                false
            }
        }
    }

    pub fn matches(&self, index: &Index) -> Vec<Match> {
        let mut result: Vec<_> = index
            .entries()
            .map(|(_, entry)| {
                if entry.kind == Kind::File {
                    self.filter.evaluate(entry.name, entry.sizes.logical)
                } else {
                    Match::No
                }
            })
            .collect();
        for (id, entry) in index.entries().rev() {
            if let Some(parent) = entry.parent {
                result[parent.position()] =
                    combine(result[parent.position()], result[id.position()]);
            }
        }
        result
    }
}

/// Follows the parser's blank-expression rule, so a whitespace-only filter hides the counter.
pub fn show_match_count(expression: &str) -> bool {
    !expression.trim().is_empty()
}

/// Counts files only: directory matches provide context, not additional matching results.
pub fn match_counts(index: &Index, matches: &[Match]) -> (usize, usize) {
    index
        .entries()
        .filter(|(_, entry)| entry.kind == Kind::File)
        .fold((0, 0), |(yes, unknown), (id, _)| {
            match matches.get(id.position()) {
                Some(Match::Yes) => (yes + 1, unknown),
                Some(Match::Unknown) | None => (yes, unknown + 1),
                Some(Match::No) => (yes, unknown),
            }
        })
}

fn combine(left: Match, right: Match) -> Match {
    match (left, right) {
        (Match::Yes, _) | (_, Match::Yes) => Match::Yes,
        (Match::Unknown, _) | (_, Match::Unknown) => Match::Unknown,
        _ => Match::No,
    }
}

pub fn highlight(node: NodeRef, members: &[EntryId], matches: &[Match]) -> Match {
    match node {
        NodeRef::Entry(id) => matches
            .get(id.position())
            .copied()
            .unwrap_or(Match::Unknown),
        NodeRef::Aggregate { .. } => members.iter().fold(Match::No, |found, id| {
            combine(
                found,
                matches
                    .get(id.position())
                    .copied()
                    .unwrap_or(Match::Unknown),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hogmap::index::{Sizes, State};

    #[test]
    fn highlight_count_excludes_directory_context_and_keeps_unknown_files_separate() {
        let mut index = Index::new("sample".into()).unwrap();
        index
            .insert(
                index.root(),
                "known.bin".into(),
                Kind::File,
                Sizes {
                    logical: Some(2048),
                    allocated: None,
                },
                None,
                State::Complete,
            )
            .unwrap();
        index
            .insert(
                index.root(),
                "unknown.bin".into(),
                Kind::File,
                Sizes::default(),
                None,
                State::Complete,
            )
            .unwrap();
        let mut filter = HighlightFilter::default();
        assert!(filter.set(">1kb"));
        assert_eq!(match_counts(&index, &filter.matches(&index)), (1, 1));
    }
}
