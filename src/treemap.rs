//! Pure squarified layout over an [`Index`].
//!
//! Geometry is independent from the renderer: the input is an index subtree and
//! a rectangle, the output is placed cells. Weights are integer bytes of the
//! selected [`SizeMetric`]; `layout` and `layout_members` keep the logical path.
//! A file identity is charged once, at its first entry with a known size for
//! that metric, including an explicit authoritative correction to that charge.
//! A hardlink never contributes its bytes twice. Unknown sizes
//! get no area and are reported through `has_unknown`.
//!
//! Volume composition (free and unattributed space) is deliberately absent:
//! it needs volume capacity data that this module does not receive.

use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};

use crate::index::{Entry, EntryId, FileIdentity, Index, Kind, SizeMetric, State};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn area(&self) -> f32 {
        self.width * self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayoutParams {
    /// Cells expected to be smaller than this area join their parent's aggregate.
    pub min_pixel_area: f32,
    /// Header strip for directory cells; zero disables headers.
    pub header_height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRef {
    Entry(EntryId),
    /// Small siblings grouped under `parent`; the identity stays navigable.
    Aggregate {
        parent: EntryId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub node: NodeRef,
    pub rect: Rect,
    /// `None` when the directory is too short for a header or is not a directory.
    pub header: Option<Rect>,
    /// Area available to children; equals `rect` for files and aggregates.
    pub content: Rect,
    pub known_sum: u64,
    pub has_unknown: bool,
    /// Entries grouped by an [`NodeRef::Aggregate`]; empty otherwise.
    pub members: Vec<EntryId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub root: Rect,
    pub content: Rect,
    pub known_sum: u64,
    pub has_unknown: bool,
    pub placed: Vec<Placed>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    UnknownRoot,
    InvalidRect,
    InvalidParams,
    SizeOverflow,
}

/// Lays out a subtree by logical bytes, the compatible path for existing consumers.
pub fn layout(
    index: &Index,
    root: EntryId,
    rect: Rect,
    params: LayoutParams,
) -> Result<Layout, LayoutError> {
    layout_with_metric(index, root, rect, params, SizeMetric::Logical)
}

/// Lays out a subtree by `metric`. Unknown sizes of that metric get no area and set
/// `has_unknown`; they are never treated as zero.
pub fn layout_with_metric(
    index: &Index,
    root: EntryId,
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
) -> Result<Layout, LayoutError> {
    layout_selection(index, root, None, rect, params, metric)
}

/// Lays out selected siblings by logical bytes, keeping their original IDs and descendant trees.
/// Use the members of a placed aggregate to open its contents without unrelated siblings.
pub fn layout_members(
    index: &Index,
    parent: EntryId,
    members: &[EntryId],
    rect: Rect,
    params: LayoutParams,
) -> Result<Layout, LayoutError> {
    layout_members_with_metric(index, parent, members, rect, params, SizeMetric::Logical)
}

/// Lays out selected siblings by `metric`. Membership rules match `layout_members`.
pub fn layout_members_with_metric(
    index: &Index,
    parent: EntryId,
    members: &[EntryId],
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
) -> Result<Layout, LayoutError> {
    let mut seen = HashSet::new();
    if members.is_empty()
        || members.iter().any(|id| {
            !seen.insert(*id)
                || index
                    .get(*id)
                    .is_none_or(|entry| entry.parent != Some(parent))
        })
    {
        return Err(LayoutError::UnknownRoot);
    }
    layout_selection(index, parent, Some(members), rect, params, metric)
}

fn layout_selection(
    index: &Index,
    root: EntryId,
    members: Option<&[EntryId]>,
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
) -> Result<Layout, LayoutError> {
    validate_rect(rect)?;
    if !params.min_pixel_area.is_finite() || params.min_pixel_area <= 0.0 {
        return Err(LayoutError::InvalidParams);
    }
    if !params.header_height.is_finite() || params.header_height < 0.0 {
        return Err(LayoutError::InvalidParams);
    }
    let root_entry = index.get(root).ok_or(LayoutError::UnknownRoot)?;
    let mut children = children_map(index);
    let totals = subtree_totals(index, metric)?;
    let root_totals = if let Some(members) = members {
        children[root.position()] = members.to_vec();
        let mut selected = Totals::default();
        for id in members {
            let value = totals[id.position()];
            selected.known_sum = selected
                .known_sum
                .checked_add(value.known_sum)
                .ok_or(LayoutError::SizeOverflow)?;
            selected.has_unknown |= value.has_unknown;
        }
        selected
    } else {
        totals[root.position()]
    };
    let root_placed = place(
        NodeRef::Entry(root),
        &root_entry,
        rect,
        root_totals,
        params.header_height,
    );
    let mut result = Layout {
        root: rect,
        content: root_placed.content,
        known_sum: root_totals.known_sum,
        has_unknown: root_totals.has_unknown,
        placed: vec![root_placed.clone()],
    };

    let mut queue = VecDeque::new();
    if root_entry.kind == Kind::Directory {
        queue.push_back((root, root_placed.content));
    }
    while let Some((parent, content)) = queue.pop_front() {
        let Some(child_ids) = children.get(parent.position()) else {
            continue;
        };
        let mut weights: Vec<(EntryId, u64)> = child_ids
            .iter()
            .filter_map(|id| {
                let weight = totals.get(id.position())?.known_sum;
                (weight > 0).then_some((*id, weight))
            })
            .collect();
        if weights.is_empty() {
            continue;
        }
        weights.sort_by(|a, b| compare_children(index, a, b));
        let total = weights
            .iter()
            .try_fold(0_u64, |sum, (_, weight)| sum.checked_add(*weight))
            .ok_or(LayoutError::SizeOverflow)?;
        let content_area = f64::from(content.area());

        let mut keep = weights.len();
        while keep > 0 {
            let expected = content_area * weights[keep - 1].1 as f64 / total as f64;
            if expected >= f64::from(params.min_pixel_area) {
                break;
            }
            keep -= 1;
        }
        let aggregate_members: Vec<EntryId> = weights[keep..].iter().map(|(id, _)| *id).collect();
        let aggregate_weight = weights[keep..]
            .iter()
            .try_fold(0_u64, |sum, (_, weight)| sum.checked_add(*weight))
            .ok_or(LayoutError::SizeOverflow)?;

        let mut cells: Vec<Cell> = weights[..keep]
            .iter()
            .map(|(id, weight)| Cell {
                node: Some(*id),
                weight: *weight,
            })
            .collect();
        if !aggregate_members.is_empty() {
            cells.push(Cell {
                node: None,
                weight: aggregate_weight,
            });
        }

        let squarified = squarify(&cells, RectF64::from_rect(content));
        for (cell, cell_rect) in cells.iter().zip(squarified) {
            match cell.node {
                Some(id) => {
                    let entry = index.get(id).ok_or(LayoutError::UnknownRoot)?;
                    let cell_totals = totals[id.position()];
                    let placed = place(
                        NodeRef::Entry(id),
                        &entry,
                        cell_rect,
                        cell_totals,
                        params.header_height,
                    );
                    if entry.kind == Kind::Directory
                        && placed.content.area() >= params.min_pixel_area
                    {
                        queue.push_back((id, placed.content));
                    }
                    result.placed.push(placed);
                }
                None => {
                    let has_unknown = aggregate_members
                        .iter()
                        .any(|id| totals[id.position()].has_unknown);
                    result.placed.push(Placed {
                        node: NodeRef::Aggregate { parent },
                        rect: cell_rect,
                        header: None,
                        content: cell_rect,
                        known_sum: aggregate_weight,
                        has_unknown,
                        members: aggregate_members.clone(),
                    });
                }
            }
        }
    }
    Ok(result)
}

fn validate_rect(rect: Rect) -> Result<(), LayoutError> {
    if !rect.x.is_finite()
        || !rect.y.is_finite()
        || !rect.width.is_finite()
        || !rect.height.is_finite()
        || rect.width <= 0.0
        || rect.height <= 0.0
        || !rect.right().is_finite()
        || !rect.bottom().is_finite()
        || !rect.area().is_finite()
    {
        return Err(LayoutError::InvalidRect);
    }
    Ok(())
}

fn children_map(index: &Index) -> Vec<Vec<EntryId>> {
    let mut children = vec![Vec::new(); index.len()];
    for (id, entry) in index.entries() {
        if let Some(parent) = entry.parent {
            children[parent.position()].push(id);
        }
    }
    children
}

#[derive(Clone, Copy, Debug, Default)]
struct Totals {
    known_sum: u64,
    has_unknown: bool,
}

fn subtree_totals(index: &Index, metric: SizeMetric) -> Result<Vec<Totals>, LayoutError> {
    let mut seen_identities: HashSet<FileIdentity> = HashSet::new();
    let mut totals = vec![Totals::default(); index.len()];
    for (id, entry) in index.entries() {
        let mut known_sum = 0_u64;
        let mut has_unknown = entry.state != State::Complete;
        if entry.kind == Kind::File {
            match index.effective_size(&entry, metric) {
                Some(bytes) => {
                    let charge = match entry.identity {
                        Some(identity) if !seen_identities.insert(identity) => 0,
                        _ => bytes,
                    };
                    known_sum = charge;
                }
                None => has_unknown = true,
            }
        }
        totals[id.position()] = Totals {
            known_sum,
            has_unknown,
        };
    }

    // The index appends a child below an existing directory, so reverse insertion order
    // finalizes every descendant before its parent.
    for (id, entry) in index.entries().rev() {
        let parent = match entry.parent {
            Some(parent) => parent,
            None => continue,
        };
        let child = totals[id.position()];
        let parent_totals = &mut totals[parent.position()];
        parent_totals.known_sum = parent_totals
            .known_sum
            .checked_add(child.known_sum)
            .ok_or(LayoutError::SizeOverflow)?;
        parent_totals.has_unknown |= child.has_unknown;
    }
    Ok(totals)
}

fn compare_children(index: &Index, a: &(EntryId, u64), b: &(EntryId, u64)) -> Ordering {
    let name_order = match (index.get(a.0), index.get(b.0)) {
        (Some(a), Some(b)) => a.name.cmp(b.name),
        _ => a.0.cmp(&b.0),
    };
    b.1.cmp(&a.1).then(name_order).then_with(|| a.0.cmp(&b.0))
}

fn place(
    node: NodeRef,
    entry: &Entry<'_>,
    rect: Rect,
    totals: Totals,
    header_height: f32,
) -> Placed {
    let (header, content) = if entry.kind == Kind::Directory {
        split_header(rect, header_height)
    } else {
        (None, rect)
    };
    Placed {
        node,
        rect,
        header,
        content,
        known_sum: totals.known_sum,
        has_unknown: totals.has_unknown,
        members: Vec::new(),
    }
}

fn split_header(rect: Rect, header_height: f32) -> (Option<Rect>, Rect) {
    if header_height > 0.0 && rect.height >= 2.0 * header_height {
        let header = Rect::new(rect.x, rect.y, rect.width, header_height);
        let content = Rect::new(
            rect.x,
            rect.y + header_height,
            rect.width,
            rect.height - header_height,
        );
        (Some(header), content)
    } else {
        (None, rect)
    }
}

#[derive(Clone, Copy)]
struct Cell {
    node: Option<EntryId>,
    weight: u64,
}

#[derive(Clone, Copy)]
struct RectF64 {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl RectF64 {
    fn from_rect(rect: Rect) -> Self {
        Self {
            x: f64::from(rect.x),
            y: f64::from(rect.y),
            width: f64::from(rect.width),
            height: f64::from(rect.height),
        }
    }

    fn area(self) -> f64 {
        self.width.max(0.0) * self.height.max(0.0)
    }

    fn to_rect(self) -> Rect {
        Rect::new(
            self.x as f32,
            self.y as f32,
            self.width.max(0.0) as f32,
            self.height.max(0.0) as f32,
        )
    }
}

fn squarify(cells: &[Cell], area: RectF64) -> Vec<Rect> {
    if cells.is_empty() {
        return Vec::new();
    }
    let total_weight: f64 = cells.iter().map(|cell| cell.weight as f64).sum();
    if total_weight <= 0.0 {
        return Vec::new();
    }
    let scale = area.area() / total_weight;
    let mut remaining = area;
    let mut result = Vec::with_capacity(cells.len());
    let mut start = 0;
    while start < cells.len() {
        let short_side = remaining.width.min(remaining.height);
        let mut end = start + 1;
        let mut best = worst_ratio(&cells[start..end], scale, short_side);
        while end < cells.len() {
            let candidate = worst_ratio(&cells[start..=end], scale, short_side);
            if candidate <= best {
                best = candidate;
                end += 1;
            } else {
                break;
            }
        }
        let row = &cells[start..end];
        let row_weight: f64 = row.iter().map(|cell| cell.weight as f64).sum();
        let is_last_row = end == cells.len();
        let row_area = if is_last_row {
            remaining.area()
        } else {
            (row_weight * scale).min(remaining.area())
        };

        if remaining.width <= remaining.height {
            let width = remaining.width;
            let row_height = if width > 0.0 {
                (row_area / width).min(remaining.height)
            } else {
                0.0
            };
            let mut y = remaining.y;
            for (index, cell) in row.iter().enumerate() {
                let height = if width > 0.0 {
                    if index + 1 == row.len() {
                        (remaining.y + row_height - y).max(0.0)
                    } else {
                        (cell.weight as f64 * scale / width)
                            .min(remaining.y + row_height - y)
                            .max(0.0)
                    }
                } else {
                    0.0
                };
                result.push(
                    RectF64 {
                        x: remaining.x,
                        y,
                        width,
                        height,
                    }
                    .to_rect(),
                );
                y += height;
            }
            remaining = RectF64 {
                x: remaining.x,
                y: remaining.y + row_height,
                width,
                height: (remaining.height - row_height).max(0.0),
            };
        } else {
            let height = remaining.height;
            let row_width = if height > 0.0 {
                (row_area / height).min(remaining.width)
            } else {
                0.0
            };
            let mut x = remaining.x;
            for (index, cell) in row.iter().enumerate() {
                let width = if height > 0.0 {
                    if index + 1 == row.len() {
                        (remaining.x + row_width - x).max(0.0)
                    } else {
                        (cell.weight as f64 * scale / height)
                            .min(remaining.x + row_width - x)
                            .max(0.0)
                    }
                } else {
                    0.0
                };
                result.push(
                    RectF64 {
                        x,
                        y: remaining.y,
                        width,
                        height,
                    }
                    .to_rect(),
                );
                x += width;
            }
            remaining = RectF64 {
                x: remaining.x + row_width,
                y: remaining.y,
                width: (remaining.width - row_width).max(0.0),
                height,
            };
        }
        start = end;
    }
    result
}

fn worst_ratio(row: &[Cell], scale: f64, short_side: f64) -> f64 {
    let row_weight: f64 = row.iter().map(|cell| cell.weight as f64).sum();
    if row_weight <= 0.0 || short_side <= 0.0 {
        return f64::INFINITY;
    }
    let row_area = row_weight * scale;
    let max_area = row
        .iter()
        .map(|cell| cell.weight as f64 * scale)
        .fold(0.0, f64::max);
    let min_area = row
        .iter()
        .map(|cell| cell.weight as f64 * scale)
        .fold(f64::INFINITY, f64::min);
    (short_side * short_side * max_area / (row_area * row_area))
        .max(row_area * row_area / (short_side * short_side * min_area))
}
