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
    /// Entries grouped by an [`NodeRef::Aggregate`] or a [`NodeRef::Group`]; empty otherwise.
    pub members: Vec<EntryId>,
    /// FNV-1a over the member positions, so a group's widget identity follows its members and
    /// not just their summary. Computed once here; the UI hashes one number per cell per frame.
    pub member_signature: u64,
    pub group_name: Option<String>,
    /// Entries this cell stands for, counted by coverage class: complete, partial, unknown. A
    /// plain entry contributes one; an aggregate or group contributes its members. The layout
    /// already visits exactly those entries, so recording them here is exact by construction and
    /// saves the coverage strip from walking an aggregate again.
    pub states: [u32; 3],
    /// A directory whose children are drawn inside it. A directory that is not expanded (too
    /// small, past the drawn depth, or with nothing of positive size inside) shows an empty
    /// interior, so the UI centres its name and size there instead.
    pub expanded: bool,
}

/// The class the coverage strip shows for one entry.
pub(crate) fn coverage_class(entry: &Entry<'_>) -> usize {
    match entry.state {
        State::Partial | State::Inaccessible | State::Stale => 1,
        State::Pending => 2,
        State::Complete
            if entry.kind == Kind::File
                && (entry.sizes.logical.is_none() || entry.sizes.allocated.is_none()) =>
        {
            2
        }
        State::Complete => 0,
    }
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
        queue.push_back((root, root_placed.content, 1_usize, 0_usize));
    }
    while let Some((parent, content, depth, placed_at)) = queue.pop_front() {
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
        // At least one child of positive size is placed below: a retained cell or the
        // aggregate that stands for the rest.
        result.placed[placed_at].expanded = true;
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
        // The aggregate stands for exactly these entries, so their classes are recorded here
        // while they are already in hand.
        let mut aggregate_states = [0_u32; 3];
        let mut aggregate_unknown = false;
        let mut aggregate_signature = MemberSignature::new();
        let mut aggregate_members = Vec::with_capacity(child_ids.len() - keep);
        for id in &child_ids[keep..] {
            aggregate_states[index.coverage_class_of(*id) as usize] += 1;
            aggregate_unknown |= totals.get(id.position()).unknown > 0;
            aggregate_signature.push(*id);
            aggregate_members.push(*id);
        }
        // Parent totals already include all children. Visit only the retained prefix for its
        // complement, rather than summing a potentially million-entry small-item aggregate.
        // The cells are built in the same pass: both need the retained children's weights.
        let mut retained_weight = 0_u64;
        let mut cells: Vec<Cell> = Vec::with_capacity(keep + 1);
        for id in &child_ids[..keep] {
            let weight = totals.get(id.position()).known_sum;
            retained_weight = retained_weight
                .checked_add(weight)
                .ok_or(LayoutError::SizeOverflow)?;
            cells.push(Cell {
                node: Some(*id),
                weight,
            });
        }
        let aggregate_weight = total
            .checked_sub(retained_weight)
            .ok_or(LayoutError::SizeOverflow)?;
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
                        queue.push_back((id, placed.content, depth + 1, result.placed.len()));
                    }
                    result.placed.push(placed);
                }
                None => {
                    result.placed.push(Placed {
                        node: NodeRef::Aggregate { parent },
                        rect: cell_rect,
                        header: None,
                        content: cell_rect,
                        known_sum: aggregate_weight,
                        has_unknown: aggregate_unknown,
                        member_signature: aggregate_signature.finish(),
                        members: std::mem::take(&mut aggregate_members),
                        group_name: None,
                        states: aggregate_states,
                        expanded: false,
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
        let mut states = [0_u32; 3];
        let mut signature = MemberSignature::new();
        for id in &members {
            let value = totals.get(id.position());
            sum = sum
                .checked_add(value.known_sum)
                .ok_or(LayoutError::SizeOverflow)?;
            unknown |= value.unknown > 0;
            states[index.coverage_class_of(*id) as usize] += 1;
            signature.push(*id);
        }
        entries.push((name, members, sum, unknown, states, signature.finish()));
    }
    let weights: Vec<_> = entries
        .iter()
        .map(|(_, _, weight, _, _, _)| Cell {
            node: None,
            weight: *weight,
        })
        .collect();
    let rectangles = squarify(&weights, RectF64::from_rect(result.content));
    for (ordinal, ((name, members, known_sum, has_unknown, states, signature), rect)) in
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
            member_signature: signature,
            members,
            group_name: Some(name),
            states,
            expanded: false,
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
    let mut states = [0_u32; 3];
    states[coverage_class(entry)] = 1;
    Placed {
        node,
        rect,
        header,
        content,
        known_sum: totals.known_sum,
        has_unknown: totals.unknown > 0,
        member_signature: 0,
        members: Vec::new(),
        group_name: None,
        states,
        expanded: false,
    }
}

/// FNV-1a over member positions, kept as running state so a group or an aggregate can be signed
/// while it is being built. The layout already walks an aggregate's members to count their coverage
/// classes, and the signature needs the same members in the same order, so hashing them was a second
/// full walk.
struct MemberSignature(u64);

impl MemberSignature {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn push(&mut self, member: EntryId) {
        self.0 ^= member.position() as u64;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    const fn finish(&self) -> u64 {
        self.0
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

        // The row was chosen by the squarified criterion, which assumes its cells share the band
        // thickness and split the other axis. Placing each cell across the whole band instead
        // produced the thin strips this algorithm exists to avoid.
        if remaining.width <= remaining.height {
            let width = remaining.width;
            let row_height = if width > 0.0 {
                (row_area / width).min(remaining.height)
            } else {
                0.0
            };
            let mut x = remaining.x;
            for (index, cell) in row.iter().enumerate() {
                let cell_width = if row_height > 0.0 {
                    if index + 1 == row.len() {
                        (remaining.x + width - x).max(0.0)
                    } else {
                        (cell.weight as f64 * scale / row_height)
                            .min(remaining.x + width - x)
                            .max(0.0)
                    }
                } else {
                    0.0
                };
                result.push(
                    RectF64 {
                        x,
                        y: remaining.y,
                        width: cell_width,
                        height: row_height,
                    }
                    .to_rect(),
                );
                x += cell_width;
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
            let mut y = remaining.y;
            for (index, cell) in row.iter().enumerate() {
                let cell_height = if row_width > 0.0 {
                    if index + 1 == row.len() {
                        (remaining.y + height - y).max(0.0)
                    } else {
                        (cell.weight as f64 * scale / row_width)
                            .min(remaining.y + height - y)
                            .max(0.0)
                    }
                } else {
                    0.0
                };
                result.push(
                    RectF64 {
                        x: remaining.x,
                        y,
                        width: row_width,
                        height: cell_height,
                    }
                    .to_rect(),
                );
                y += cell_height;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn signature_of(members: &[EntryId]) -> u64 {
        let mut signature = MemberSignature::new();
        for member in members {
            signature.push(*member);
        }
        signature.finish()
    }

    #[test]
    fn a_group_signature_follows_its_members_not_their_summary() {
        let a = EntryId::at(0).unwrap();
        let b = EntryId::at(1).unwrap();
        assert_ne!(
            signature_of(&[a, b]),
            signature_of(&[b, a]),
            "another order is another set"
        );
        assert_ne!(
            signature_of(&[a, b]),
            signature_of(&[a]),
            "a subset is another set"
        );
        assert_ne!(
            signature_of(&[a]),
            signature_of(&[b]),
            "two single-member groups differ"
        );
    }

    #[test]
    fn the_member_signature_is_fnv1a_over_positions() {
        // Expected values from the published FNV-1a definition, computed outside this code, so a
        // shared mistake in the running state cannot confirm itself. One position is above 255 and
        // another is the largest one an entry can have, so folding a position into a narrower type
        // would show up here.
        let cases: &[(&[u32], u64)] = &[
            (&[0], 0xaf63_bd4c_8601_b7df),
            (&[1], 0xaf63_bc4c_8601_b62c),
            (&[300], 0xaf62_a14c_85ff_d54b),
            (&[0, 1], 0x0832_8707_b4eb_6e3a),
            (&[1, 0], 0x082f_2207_b4e8_8cc4),
            (&[u32::MAX - 1], 0x6b1d_733e_79fe_4821),
        ];
        for (positions, expected) in cases {
            let members: Vec<EntryId> = positions
                .iter()
                .map(|position| EntryId::at(*position as usize).unwrap())
                .collect();
            assert_eq!(signature_of(&members), *expected, "positions {positions:?}");
        }
    }
}
