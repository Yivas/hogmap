use std::collections::HashMap;
use std::ffi::OsString;

use hogmap::filter::{Filter, FilterError, Match};
use hogmap::index::{EntryId, Index, Kind};
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
    pub members: Option<Vec<OsString>>,
}

impl Location {
    pub fn directory(index: &Index, id: EntryId) -> Self {
        let mut path = Vec::new();
        let mut next = Some(id);
        while let Some(id) = next {
            let Some(entry) = index.get(id) else { break };
            if entry.parent.is_some() {
                path.push(entry.name.clone());
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
                        && entry.name == *component
                        && entry.kind == Kind::Directory
                })?
                .0;
        }
        let members = self.members.as_ref().map(|names| {
            index
                .entries()
                .filter(|(_, entry)| entry.parent == Some(parent) && names.contains(&entry.name))
                .map(|(id, _)| id)
                .collect()
        });
        Some((parent, members))
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

    pub fn matches(&self, index: &Index) -> HashMap<EntryId, Match> {
        let mut result = HashMap::new();
        let entries: Vec<_> = index.entries().collect();
        for (id, entry) in &entries {
            result.insert(
                *id,
                if entry.kind == Kind::File {
                    self.filter.evaluate(&entry.name, entry.sizes.logical)
                } else {
                    Match::No
                },
            );
        }
        for (id, entry) in entries.iter().rev() {
            if let Some(parent) = entry.parent {
                let child = result[id];
                let parent_match = result.entry(parent).or_insert(Match::No);
                *parent_match = combine(*parent_match, child);
            }
        }
        result
    }
}

fn combine(left: Match, right: Match) -> Match {
    match (left, right) {
        (Match::Yes, _) | (_, Match::Yes) => Match::Yes,
        (Match::Unknown, _) | (_, Match::Unknown) => Match::Unknown,
        _ => Match::No,
    }
}

pub fn highlight(node: NodeRef, members: &[EntryId], matches: &HashMap<EntryId, Match>) -> Match {
    match node {
        NodeRef::Entry(id) => matches.get(&id).copied().unwrap_or(Match::Unknown),
        NodeRef::Aggregate { .. } => members.iter().fold(Match::No, |found, id| {
            combine(found, matches.get(id).copied().unwrap_or(Match::Unknown))
        }),
    }
}
