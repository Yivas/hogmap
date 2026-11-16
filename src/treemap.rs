//! Pure squarified layout over an [`Index`].
//!
//! Geometry is independent from the renderer: the input is an index subtree and
//! a rectangle, the output is placed cells. Weights are integer bytes of the
//! selected [`SizeMetric`]; `layout` and `layout_members` keep the logical path.
//! Each listed name contributes its known size. Only the first known entry of
//! an identity/metric receives any authoritative charge correction; aliases keep
//! their listed sizes. Apparent area is not physical attribution. Unknown sizes
//! get no area and are reported through `has_unknown`.
//!
//! Volume composition (free and unattributed space) is deliberately absent:
//! it needs volume capacity data that this module does not receive.

use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};

use crate::index::{Entry, EntryId, FileIdentity, Index, Kind, SizeMetric, State};

/// Content smaller than this under a header band is not worth the band: the cell keeps its whole
/// rectangle and relies on the tooltip.
const MIN_CONTENT_UNDER_HEADER: f32 = 6.0;

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
    /// Levels below the drawn root that are laid out. Deeper directories stay whole rectangles:
    /// the map shows readable blocks and the user opens them by navigating, instead of tiling
    /// every descendant until the smallest cells carry punctuation. Zero draws only the root;
    /// `usize::MAX` disables the cap.
    pub max_depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRef {
    Entry(EntryId),
    /// Small siblings grouped under `parent`; the identity stays navigable.
    Aggregate {
        parent: EntryId,
    },
    /// Session-only extension/class group; `ordinal` belongs to this layout only.
    Group {
        parent: EntryId,
        ordinal: usize,
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
    pub group_name: Option<String>,
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
    layout_selection(index, root, None, rect, params, metric, None)
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
    layout_selection(index, parent, Some(members), rect, params, metric, None)
}

/// Filters geometry using previously evaluated decisions. Unknown files retain their observed
/// area and undecided tone; they are not matches. No names or expressions are evaluated here.
pub fn layout_filtered(
    index: &Index,
    root: EntryId,
    members: Option<&[EntryId]>,
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
    matches: &[crate::filter::Match],
) -> Result<Layout, LayoutError> {
    if matches.len() != index.len()
        || members.is_some_and(|members| {
            members.is_empty()
                || members.iter().any(|id| {
                    index
                        .get(*id)
                        .is_none_or(|entry| entry.parent != Some(root))
                })
        })
    {
        return Err(LayoutError::UnknownRoot);
    }
    if let Some(members) = members {
        let mut seen = HashSet::new();
        if members.iter().any(|id| !seen.insert(*id)) {
            return Err(LayoutError::UnknownRoot);
        }
    }
    layout_selection(index, root, members, rect, params, metric, Some(matches))
}

fn layout_selection(
    index: &Index,
    root: EntryId,
    members: Option<&[EntryId]>,
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
    matches: Option<&[crate::filter::Match]>,
) -> Result<Layout, LayoutError> {
    validate_rect(rect)?;
    if !params.min_pixel_area.is_finite() || params.min_pixel_area <= 0.0 {
        return Err(LayoutError::InvalidParams);
    }
    if !params.header_height.is_finite() || params.header_height < 0.0 {
        return Err(LayoutError::InvalidParams);
    }
    let root_entry = index.get(root).ok_or(LayoutError::UnknownRoot)?;
    let mut storage = index
        .layout_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if storage
        .as_ref()
        .is_none_or(|cache| cache.metric != metric || cache.matches.as_deref() != matches)
    {
        if let Some(cache) = storage.as_mut() {
            // Membership is snapshot-owned and independent of weights. Insertions maintain it
            // or discard this cache; authoritative corrections also discard the cache.
            let totals = subtree_totals(index, metric, matches)?;
            cache.totals = totals;
            cache.metric = metric;
            cache.matches = matches.map(<[_]>::to_vec);
            cache.sorted.fill(false);
        } else {
            *storage = Some(LayoutData::build(index, metric, matches)?);
        }
    }
    let data = storage.as_mut().expect("initialized layout data");
    let mut selected_children = members.map(<[EntryId]>::to_vec);
    if let Some(selected) = &mut selected_children {
        selected.sort_by(|a, b| {
            compare_children(
                index,
                &(*a, data.totals.get(a.position()).known_sum),
                &(*b, data.totals.get(b.position()).known_sum),
            )
        });
    }
    let totals = &data.totals;
    let root_totals = if let Some(members) = members {
        let mut selected = Totals::default();
        for id in members {
            let value = totals.get(id.position());
            selected.known_sum = selected
                .known_sum
                .checked_add(value.known_sum)
                .ok_or(LayoutError::SizeOverflow)?;
            selected.unknown += value.unknown;
        }
        selected
    } else {
        totals.get(root.position())
    };
    let root_placed = place(
        NodeRef::Entry(root),
        &root_entry,
        rect,
        root_totals,
        header_for_depth(0, params.header_height),
    );
    let mut result = Layout {
        root: rect,
        content: root_placed.content,
        known_sum: root_totals.known_sum,
        has_unknown: root_totals.unknown > 0,
        placed: vec![root_placed.clone()],
    };

    let mut queue = VecDeque::new();
    if root_entry.kind == Kind::Directory && params.max_depth > 0 {
        queue.push_back((root, root_placed.content, 1_usize));
    }
    while let Some((parent, content, depth)) = queue.pop_front() {
        let slot = parent.position();
        if !data.sorted[slot] {
            data.children[slot].sort_by(|a, b| {
                compare_children(
                    index,
                    &(*a, totals.get(a.position()).known_sum),
                    &(*b, totals.get(b.position()).known_sum),
                )
            });
            data.sorted[slot] = true;
        }
        let child_ids = if parent == root {
            selected_children.as_deref().unwrap_or(&data.children[slot])
        } else {
            &data.children[slot]
        };
        let positive = child_ids.partition_point(|id| totals.get(id.position()).known_sum > 0);
        if positive == 0 {
            continue;
        }
        let child_ids = &child_ids[..positive];
        let total = if parent == root {
            root_totals.known_sum
        } else {
            totals.get(slot).known_sum
        };
        let content_area = f64::from(content.area());
        let keep = child_ids.partition_point(|id| {
            content_area * totals.get(id.position()).known_sum as f64 / total as f64
                >= f64::from(params.min_pixel_area)
        });
        let mut aggregate_members = child_ids[keep..].to_vec();
        // Parent totals already include all children. Visit only the retained prefix for its
        // complement, rather than summing a potentially million-entry small-item aggregate.
        let retained_weight = child_ids[..keep]
            .iter()
            .try_fold(0_u64, |sum, id| {
                sum.checked_add(totals.get(id.position()).known_sum)
            })
            .ok_or(LayoutError::SizeOverflow)?;
        let aggregate_weight = total
            .checked_sub(retained_weight)
            .ok_or(LayoutError::SizeOverflow)?;
        let mut cells: Vec<Cell> = child_ids[..keep]
            .iter()
            .map(|id| Cell {
                node: Some(*id),
                weight: totals.get(id.position()).known_sum,
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
                    let cell_totals = totals.get(id.position());
                    let placed = place(
                        NodeRef::Entry(id),
                        &entry,
                        cell_rect,
                        cell_totals,
                        header_for_depth(depth, params.header_height),
                    );
                    if entry.kind == Kind::Directory
                        && depth < params.max_depth
                        && placed.content.area() >= params.min_pixel_area
                    {
                        queue.push_back((id, placed.content, depth + 1));
                    }
                    result.placed.push(placed);
                }
                None => {
                    let has_unknown = aggregate_members
                        .iter()
                        .any(|id| totals.get(id.position()).unknown > 0);
                    result.placed.push(Placed {
                        node: NodeRef::Aggregate { parent },
                        rect: cell_rect,
                        header: None,
                        content: cell_rect,
                        known_sum: aggregate_weight,
                        has_unknown,
                        members: std::mem::take(&mut aggregate_members),
                        group_name: None,
                    });
                }
            }
        }
    }
    Ok(result)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupBy {
    #[default]
    None,
    Extension,
    Class,
}

/// Groups immediate children, preserving real IDs in each navigable member list. Group weights
/// use the same corrected subtree totals as ordinary layouts; directories belong to Other.
pub fn layout_grouped(
    index: &Index,
    root: EntryId,
    rect: Rect,
    params: LayoutParams,
    metric: SizeMetric,
    grouping: (&crate::filter::ClassTable, GroupBy),
    matches: Option<&[crate::filter::Match]>,
) -> Result<Layout, LayoutError> {
    let (classes, by) = grouping;
    if matches.is_some_and(|values| values.len() != index.len()) {
        return Err(LayoutError::UnknownRoot);
    }
    let mut result = layout_selection(
        index,
        root,
        None,
        rect,
        LayoutParams {
            max_depth: 0,
            ..params
        },
        metric,
        matches,
    )?;
    let totals = subtree_totals(index, metric, matches)?;
    // A directory only remains after filtering if a file below it remains. Its scan-state
    // uncertainty alone must not manufacture an empty group.
    let mut retained = vec![false; index.len()];
    for (id, entry) in index.entries().rev() {
        if entry.kind == Kind::File {
            retained[id.position()] =
                matches.is_none_or(|values| values[id.position()] != crate::filter::Match::No);
        }
        if retained[id.position()]
            && let Some(parent) = entry.parent
        {
            retained[parent.position()] = true;
        }
    }
    let mut groups: std::collections::BTreeMap<String, Vec<EntryId>> =
        std::collections::BTreeMap::new();
    for (id, entry) in index
        .entries()
        .filter(|(_, entry)| entry.parent == Some(root))
    {
        if matches.is_some() && !retained[id.position()] {
            continue;
        }
        let extension = (entry.kind == Kind::File)
            .then(|| {
                entry
                    .name
                    .to_str()?
                    .rsplit_once('.')
                    .map(|(_, ext)| ext.to_ascii_lowercase())
            })
            .flatten()
            .filter(|ext| !ext.is_empty());
        let name = match by {
            GroupBy::Extension => extension.map(|ext| format!(".{ext}")),
            GroupBy::Class => extension.and_then(|ext| {
                classes
                    .entries()
                    .find(|(_, class)| {
                        class
                            .extensions
                            .iter()
                            .any(|candidate| candidate.eq_ignore_ascii_case(&ext))
                    })
                    .map(|(name, _)| name.to_owned())
            }),
            GroupBy::None => None,
        }
        .unwrap_or_else(|| "Other".into());
        groups.entry(name).or_default().push(id);
    }
    let mut entries = Vec::new();
    for (name, members) in groups {
        let mut sum = 0_u64;
        let mut unknown = false;
        for id in &members {
            let value = totals.get(id.position());
            sum = sum
                .checked_add(value.known_sum)
                .ok_or(LayoutError::SizeOverflow)?;
            unknown |= value.unknown > 0;
        }
        entries.push((name, members, sum, unknown));
    }
    let weights: Vec<_> = entries
        .iter()
        .map(|(_, _, weight, _)| Cell {
            node: None,
            weight: *weight,
        })
        .collect();
    let rectangles = squarify(&weights, RectF64::from_rect(result.content));
    for (ordinal, ((name, members, known_sum, has_unknown), rect)) in
        entries.into_iter().zip(rectangles).enumerate()
    {
        result.placed.push(Placed {
            node: NodeRef::Group {
                parent: root,
                ordinal,
            },
            rect,
            header: None,
            content: rect,
            known_sum,
            has_unknown,
            members,
            group_name: Some(name),
        });
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

#[derive(Clone, Copy, Debug, Default)]
struct Totals {
    known_sum: u64,
    unknown: u32,
}

#[derive(Debug)]
struct SubtreeTotals {
    known: Vec<u64>,
    unknown: Vec<u32>,
}

impl SubtreeTotals {
    fn new(len: usize) -> Self {
        let mut known = Vec::with_capacity(len + 4096);
        let mut unknown = Vec::with_capacity(len + 4096);
        known.resize(len, 0);
        unknown.resize(len, 0);
        Self { known, unknown }
    }

    fn get(&self, slot: usize) -> Totals {
        Totals {
            known_sum: self.known[slot],
            unknown: self.unknown[slot],
        }
    }

    fn add(&mut self, slot: usize, value: Totals) -> Result<(), LayoutError> {
        self.known[slot] = self.known[slot]
            .checked_add(value.known_sum)
            .ok_or(LayoutError::SizeOverflow)?;
        self.unknown[slot] += value.unknown;
        Ok(())
    }
}

/// Snapshot-owned, one metric at a time. Unknown counts allow completion to clear an ancestor
/// flag without rereading its other descendants. Only visited sibling lists are sorted.
#[derive(Debug)]
pub(crate) struct LayoutData {
    metric: SizeMetric,
    matches: Option<Vec<crate::filter::Match>>,
    children: Vec<Vec<EntryId>>,
    totals: SubtreeTotals,
    sorted: Vec<bool>,
    child_capacity_bytes: usize,
}
impl LayoutData {
    fn build(
        index: &Index,
        metric: SizeMetric,
        matches: Option<&[crate::filter::Match]>,
    ) -> Result<Self, LayoutError> {
        let mut children = Vec::with_capacity(index.len() + 4096);
        children.resize_with(index.len(), Vec::new);
        for (id, entry) in index.entries() {
            if let Some(parent) = entry.parent {
                children[parent.position()].push(id);
            }
        }
        let child_capacity_bytes = children
            .iter()
            .map(|list| list.capacity() * std::mem::size_of::<EntryId>())
            .sum();
        let mut sorted = Vec::with_capacity(index.len() + 4096);
        sorted.resize(index.len(), false);
        let totals = subtree_totals(index, metric, matches)?;
        Ok(Self {
            metric,
            matches: matches.map(<[_]>::to_vec),
            children,
            totals,
            sorted,
            child_capacity_bytes,
        })
    }
    pub(crate) fn memory_bytes(&self) -> usize {
        self.children.capacity() * std::mem::size_of::<Vec<EntryId>>()
            + self.totals.known.capacity() * std::mem::size_of::<u64>()
            + self.totals.unknown.capacity() * std::mem::size_of::<u32>()
            + self.sorted.capacity() * std::mem::size_of::<bool>()
            + self.child_capacity_bytes
            + self.matches.as_ref().map_or(0, |matches| {
                matches.capacity() * std::mem::size_of::<crate::filter::Match>()
            })
    }
    pub(crate) fn append(&mut self, index: &Index, id: EntryId) -> Result<(), LayoutError> {
        if self.matches.is_some() {
            return Err(LayoutError::UnknownRoot);
        }
        if self.children.len() == self.children.capacity() {
            self.children.reserve(4096);
        }
        if self.totals.known.len() == self.totals.known.capacity() {
            self.totals.known.reserve(4096);
        }
        if self.totals.unknown.len() == self.totals.unknown.capacity() {
            self.totals.unknown.reserve(4096);
        }
        let entry = index.get(id).expect("new index entry");
        let value = Totals {
            known_sum: if entry.kind == Kind::File {
                self.metric.of(entry.sizes).unwrap_or(0)
            } else {
                0
            },
            unknown: u32::from(
                entry.state != State::Complete
                    || (entry.kind == Kind::File && self.metric.of(entry.sizes).is_none()),
            ),
        };
        self.children.push(Vec::new());
        if self.sorted.len() == self.sorted.capacity() {
            self.sorted.reserve(4096);
        }
        self.sorted.push(false);
        self.totals.known.push(value.known_sum);
        self.totals.unknown.push(value.unknown);
        if let Some(parent) = entry.parent {
            let list = &mut self.children[parent.position()];
            let previous = list.capacity();
            list.push(id);
            self.child_capacity_bytes +=
                (list.capacity() - previous) * std::mem::size_of::<EntryId>();
        }
        let mut next = entry.parent;
        while let Some(parent) = next {
            let slot = parent.position();
            self.sorted[slot] = false;
            self.totals.add(slot, value)?;
            next = index.get(parent).expect("existing parent").parent;
        }
        Ok(())
    }
    pub(crate) fn state_changed(
        &mut self,
        index: &Index,
        id: EntryId,
        previous: State,
        next: State,
    ) {
        let entry = index.get(id).expect("existing state entry");
        if previous == next
            || (entry.kind == Kind::File
                && (self.metric.of(entry.sizes).is_none()
                    || self.matches.as_ref().is_some_and(|matches| {
                        matches[id.position()] != crate::filter::Match::Yes
                    })))
        {
            // Excluded files contribute no uncertainty; undecided files always contribute one.
            // Neither contribution changes when their scan state changes.
            return;
        }
        let before = previous != State::Complete;
        let after = next != State::Complete;
        if before == after {
            return;
        }
        let mut next = Some(id);
        while let Some(id) = next {
            if after {
                self.totals.unknown[id.position()] += 1;
            } else {
                self.totals.unknown[id.position()] -= 1;
            }
            next = index.get(id).expect("existing ancestor").parent;
        }
    }
}

fn subtree_totals(
    index: &Index,
    metric: SizeMetric,
    matches: Option<&[crate::filter::Match]>,
) -> Result<SubtreeTotals, LayoutError> {
    let mut totals = SubtreeTotals::new(index.len());
    let contribution = |id: EntryId, entry: &Entry<'_>, mut known_sum| {
        let mut has_unknown = entry.state != State::Complete
            || (entry.kind == Kind::File && metric.of(entry.sizes).is_none());
        if entry.kind == Kind::File
            && let Some(matches) = matches
        {
            match matches[id.position()] {
                crate::filter::Match::No => {
                    known_sum = 0;
                    has_unknown = false;
                }
                crate::filter::Match::Unknown => has_unknown = true,
                crate::filter::Match::Yes => {}
            }
        }
        Totals {
            known_sum,
            unknown: u32::from(has_unknown),
        }
    };
    let corrected = index.has_authoritative_sizes();
    if corrected {
        // Corrections belong to the first known insertion-order charge, even when excluded.
        // A reverse identity set would move them to the last alias and change its geometry.
        // Only a corrected identity needs that order: without a correction the first entry and
        // every later alias contribute their listed size alike, so tracking them would change
        // nothing and cost one set insert per file.
        let mut seen_identities: HashSet<FileIdentity> = HashSet::new();
        for (id, entry) in index.entries() {
            let known_sum = if entry.kind == Kind::File {
                metric.of(entry.sizes).map_or(0, |listed| {
                    let corrected_first = entry.identity.is_some_and(|identity| {
                        index.has_correction(identity) && seen_identities.insert(identity)
                    });
                    if corrected_first {
                        index
                            .effective_size(&entry, metric)
                            .expect("known listed size")
                    } else {
                        listed
                    }
                })
            } else {
                0
            };
            totals.add(id.position(), contribution(id, &entry, known_sum))?;
        }
    }
    // Parents precede children; one reverse pass finalizes each subtree before its parent.
    // At most one unknown per entry fits u32 because EntryId already bounds the index length.
    for (id, entry) in index.entries().rev() {
        if !corrected {
            let known_sum = if entry.kind == Kind::File {
                metric.of(entry.sizes).unwrap_or(0)
            } else {
                0
            };
            totals.add(id.position(), contribution(id, &entry, known_sum))?;
        }
        if let Some(parent) = entry.parent {
            totals.add(parent.position(), totals.get(id.position()))?;
        }
    }
    Ok(totals)
}

fn compare_children(index: &Index, a: &(EntryId, u64), b: &(EntryId, u64)) -> Ordering {
    // Zero-weight entries are outside the positive prefix: neither cells nor aggregate members
    // can expose their order. Avoid resolving native names for an excluded sibling tail.
    if a.1 == 0 && b.1 == 0 {
        return Ordering::Equal;
    }
    b.1.cmp(&a.1)
        .then_with(|| match (index.get(a.0), index.get(b.0)) {
            (Some(a), Some(b)) => a.name.cmp(b.name),
            _ => a.0.cmp(&b.0),
        })
        .then_with(|| a.0.cmp(&b.0))
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
        has_unknown: totals.unknown > 0,
        members: Vec::new(),
        group_name: None,
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
        // A band that leaves a sliver of content costs area and shows nothing. Small cells keep
        // their whole rectangle and are identified by the tooltip instead.
        if content.height >= MIN_CONTENT_UNDER_HEADER && content.width >= MIN_CONTENT_UNDER_HEADER {
            return (Some(header), content);
        }
    }
    (None, rect)
}

/// Nested bands shrink so a deep level does not spend the same pixels as the root. The depth tone
/// already separates levels; the band only has to stay legible.
fn header_for_depth(depth: usize, max: f32) -> f32 {
    let scale = match depth {
        0 => 1.0,
        1 => 0.8,
        // At the desktop's 20-point base this leaves a 15-point strip: a measured 13-point
        // text row plus one point of padding above and below, rather than an unlabeled band.
        _ => 0.75,
    };
    max * scale
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
