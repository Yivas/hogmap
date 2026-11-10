use crate::name_arena::{NameArena, NameRef};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;

/// Win32 reports the maximum component length in UTF-16 units; NTFS uses 255.
const MAX_COMPONENT_UTF16_UNITS: usize = 255;
/// Bound on retained conflict names and metadata rereads, not on accounting conflicts.
pub const MAX_CONFLICT_REREADS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(NonZeroU32);

impl EntryId {
    pub fn position(self) -> usize {
        self.0.get() as usize - 1
    }
    pub(crate) fn at(position: usize) -> Option<Self> {
        u32::try_from(position)
            .ok()
            .and_then(|n| n.checked_add(1))
            .and_then(NonZeroU32::new)
            .map(Self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Complete,
    /// No `DirectoryDone` or `DirectoryFailed` for this directory was accepted before the scan
    /// ended, failed, was cancelled or lost its producer. It is not the same as inaccessible.
    Partial,
    Inaccessible,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub volume: u64,
    pub file: u128,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub logical: Option<u64>,
    pub allocated: Option<u64>,
}

/// Which byte count a summary or a layout weighs. `Allocated` is the observed allocation size of
/// an entry, not recoverable space and not a reconciliation with volume-used bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeMetric {
    Logical,
    Allocated,
}

impl SizeMetric {
    /// Position of this metric in the per-metric totals array.
    fn slot(self) -> usize {
        match self {
            Self::Logical => 0,
            Self::Allocated => 1,
        }
    }

    pub(crate) fn of(self, sizes: Sizes) -> Option<u64> {
        match self {
            Self::Logical => sizes.logical,
            Self::Allocated => sizes.allocated,
        }
    }
}

#[derive(Debug)]
pub struct Entry<'a> {
    pub parent: Option<EntryId>,
    pub name: &'a OsStr,
    pub kind: Kind,
    pub sizes: Sizes,
    pub identity: Option<FileIdentity>,
    pub state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    InvalidName,
    InvalidParent,
    NotDirectory,
    SizeOverflow,
    CapacityExceeded,
    NotConflicted,
    /// Authoritative charges belong to a finished snapshot; a rescan needs a new Index.
    ResolvedSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChargeResolution {
    Applied,
    MissingCharge,
    UnknownSize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityResolution {
    pub logical: ChargeResolution,
    pub allocated: ChargeResolution,
    pub still_conflicted: bool,
}

/// Logical listed totals. After authoritative correction, add the signed resolution delta
/// from summary_for(Logical) to known bytes to obtain unique + shared bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    /// Sum of the logical size of every visible file entry, hardlinks included.
    pub known_logical_bytes: u64,
    /// Logical bytes charged once per file identity; identity-less files count individually.
    pub unique_logical_bytes: u64,
    /// Logical bytes of later hardlink entries that were not charged again.
    pub shared_logical_bytes: u64,
    pub unknown_size_files: usize,
    pub complete: bool,
}

/// Totals for one size metric, kept as entries are appended.
#[derive(Debug, Default)]
struct MetricTotals {
    known_bytes: u64,
    unique_bytes: u64,
    shared_bytes: u64,
    unknown_size_files: usize,
    identity_missing_files: usize,
    conflicting_identities: usize,
    resolution_delta_bytes: i128,
    overflowed: bool,
}

/// File totals kept as entries are appended, so `summary()` does not walk them again.
#[derive(Debug, Default)]
struct FileTotals {
    files: usize,
    metrics: [MetricTotals; 2],
}

/// Totals for one size metric. Listed, unique and shared facts keep their original meanings.
/// Apparent area uses `apparent_bytes()`: known + delta = unique + shared, checked as u64.
/// `complete` covers states/known sizes, not identity consistency or physical attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricSummary {
    pub metric: SizeMetric,
    pub files: usize,
    /// Sum of every visible file entry with a known size for this metric, hardlinks included.
    pub known_bytes: u64,
    /// Bytes charged once per file identity; identity-less files count individually.
    pub unique_bytes: u64,
    /// Bytes of later entries that reused an already charged identity.
    pub shared_bytes: u64,
    pub unknown_size_files: usize,
    /// Known-size entries of this metric without an identity, counted individually.
    pub identity_missing_files: usize,
    /// Identities whose known samples for this metric disagree; each identity counts once.
    pub conflicting_identities: usize,
    /// Signed correction to the first listed charge: known + delta = unique + shared.
    /// Known/shared/unknown counts remain observations of the directory listings.
    pub resolution_delta_bytes: i128,
    pub complete: bool,
}

impl MetricSummary {
    /// Per-name area total: only the charged record is corrected; aliases retain listed sizes.
    /// No physical attribution or error bound is implied. Overflow never wraps or saturates.
    pub fn apparent_bytes(&self) -> Result<u64, IndexError> {
        i128::from(self.known_bytes)
            .checked_add(self.resolution_delta_bytes)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(IndexError::SizeOverflow)
    }
}

/// Append-only metadata snapshot. IDs identify entries, not physical files.
///
/// The index keeps one accounting row per file identity and a count of incomplete entries to
/// answer `summary()` in constant time; that table is the memory cost of hardlink accounting.
#[derive(Debug)]
pub struct Index {
    entries: Vec<StoredEntry>,
    names: NameArena,
    identities: Vec<StoredIdentity>,
    totals: FileTotals,
    identity_charges: HashMap<StoredIdentity, IdentityCharge>,
    authoritative_sizes: HashMap<StoredIdentity, Sizes>,
    // Only conflicts retain a representative listed name; ordinary entries do not grow.
    conflict_names: HashMap<StoredIdentity, EntryId>,
    conflicting_identity_count: usize,
    incomplete_entries: usize,
    // One lazy metric cache belongs to this snapshot; mutations maintain it or invalidate it.
    pub(crate) layout_cache: std::sync::Mutex<Option<crate::treemap::LayoutData>>,
    pub(crate) filter_cache: std::sync::Mutex<Option<crate::filter::MatchCache>>,
}

impl Index {
    pub fn new(root_name: OsString) -> Result<Self, IndexError> {
        validate_name(&root_name)?;
        let mut names = NameArena::default();
        let name = names.push(&root_name).ok_or(IndexError::CapacityExceeded)?;
        Ok(Self {
            entries: vec![StoredEntry::new(
                None,
                name,
                Kind::Directory,
                Sizes::default(),
                u32::MAX,
                State::Pending,
            )],
            names,
            identities: Vec::new(),
            totals: FileTotals::default(),
            identity_charges: HashMap::new(),
            authoritative_sizes: HashMap::new(),
            conflict_names: HashMap::new(),
            conflicting_identity_count: 0,
            incomplete_entries: 1,
            layout_cache: std::sync::Mutex::new(None),
            filter_cache: std::sync::Mutex::new(None),
        })
    }

    /// Allocation-capacity estimate, excluding allocator headers and process overhead.
    pub fn memory_estimate(&self) -> IndexMemory {
        IndexMemory {
            entry_bytes: self.entries.len() * std::mem::size_of::<StoredEntry>(),
            entry_capacity_bytes: self.entries.capacity() * std::mem::size_of::<StoredEntry>(),
            name_bytes: self.names.len(),
            name_capacity_bytes: self.names.capacity(),
            name_boundary_capacity_bytes: self.names.boundary_capacity_bytes(),
            identity_bytes: self.identities.capacity() * std::mem::size_of::<StoredIdentity>(),
            layout_cache_bytes: self
                .layout_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map_or(0, |cache| cache.memory_bytes()),
            filter_cache_bytes: self
                .filter_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .map_or(0, |cache| cache.memory_bytes()),
            charged_identity_bytes: hash_bytes::<(StoredIdentity, IdentityCharge)>(
                self.identity_charges.capacity(),
            ) + hash_bytes::<(StoredIdentity, Sizes)>(
                self.authoritative_sizes.capacity(),
            ) + hash_bytes::<(StoredIdentity, EntryId)>(
                self.conflict_names.capacity(),
            ),
        }
    }

    pub fn root(&self) -> EntryId {
        EntryId::at(0).expect("root ID fits")
    }

    /// Appends only below an existing directory, so parent references cannot form cycles.
    pub fn insert(
        &mut self,
        parent: EntryId,
        name: OsString,
        kind: Kind,
        sizes: Sizes,
        identity: Option<FileIdentity>,
        state: State,
    ) -> Result<EntryId, IndexError> {
        if !self.authoritative_sizes.is_empty() {
            return Err(IndexError::ResolvedSnapshot);
        }
        let parent_entry = self.get(parent).ok_or(IndexError::InvalidParent)?;
        if parent_entry.kind != Kind::Directory {
            return Err(IndexError::NotDirectory);
        }
        validate_name(&name)?;
        let id = EntryId::at(self.entries.len()).ok_or(IndexError::CapacityExceeded)?;
        let identity_slot = if identity.is_some() {
            u32::try_from(self.identities.len())
                .ok()
                .filter(|slot| *slot != u32::MAX)
                .ok_or(IndexError::CapacityExceeded)?
        } else {
            u32::MAX
        };
        let name_ref = self.names.push(&name).ok_or(IndexError::CapacityExceeded)?;
        if let Some(identity) = identity {
            self.identities.push(identity.into());
        }
        self.account(
            id,
            &Entry {
                parent: Some(parent),
                name: &name,
                kind,
                sizes,
                identity,
                state,
            },
        );
        self.entries.push(StoredEntry::new(
            Some(parent),
            name_ref,
            kind,
            sizes,
            identity_slot,
            state,
        ));
        let cache = self
            .layout_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut cache) = cache
            && cache.append(self, id).is_ok()
        {
            *self
                .layout_cache
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache);
        }
        let cache = self
            .filter_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut cache) = cache {
            cache.append(self, id);
            *self
                .filter_cache
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache);
        }
        Ok(id)
    }

    fn account(&mut self, id: EntryId, entry: &Entry<'_>) {
        if entry.state != State::Complete {
            self.incomplete_entries += 1;
        }
        if entry.kind != Kind::File {
            return;
        }
        self.totals.files += 1;
        // Each metric charges independently: an unknown logical size must not skip a known
        // allocated size, and one metric's overflow must not hide the other's totals.
        let logical_conflict =
            self.account_metric(SizeMetric::Logical, entry.sizes.logical, entry.identity);
        let allocated_conflict =
            self.account_metric(SizeMetric::Allocated, entry.sizes.allocated, entry.identity);
        if logical_conflict || allocated_conflict {
            self.conflicting_identity_count += 1;
            if self.conflict_names.len() < MAX_CONFLICT_REREADS {
                self.conflict_names
                    .insert(entry.identity.expect("conflict has identity").into(), id);
            }
        }
    }

    fn account_metric(
        &mut self,
        metric: SizeMetric,
        size: Option<u64>,
        identity: Option<FileIdentity>,
    ) -> bool {
        let totals = &mut self.totals.metrics[metric.slot()];
        let Some(bytes) = size else {
            totals.unknown_size_files += 1;
            return false;
        };
        accumulate(&mut totals.known_bytes, bytes, &mut totals.overflowed);
        let Some(identity) = identity else {
            // Without an identity the size cannot be deduplicated, so it is charged on its own
            // and reported as a separate confidence gap.
            totals.identity_missing_files += 1;
            accumulate(&mut totals.unique_bytes, bytes, &mut totals.overflowed);
            return false;
        };
        let charge = self.identity_charges.entry(identity.into()).or_default();
        let was_conflicted = charge.logical.conflicted || charge.allocated.conflicted;
        let sample = charge.for_metric_mut(metric);
        let mut new_identity_conflict = false;
        match sample.first {
            None => {
                sample.first = Some(bytes);
                accumulate(&mut totals.unique_bytes, bytes, &mut totals.overflowed);
            }
            Some(first) => {
                if first != bytes && !sample.conflicted {
                    sample.conflicted = true;
                    totals.conflicting_identities += 1;
                    new_identity_conflict = !was_conflicted;
                }
                accumulate(&mut totals.shared_bytes, bytes, &mut totals.overflowed);
            }
        }
        new_identity_conflict
    }

    /// Each still-conflicting identity once, regardless of which metrics disagree.
    pub fn conflicting_identities(&self) -> impl Iterator<Item = FileIdentity> + '_ {
        self.identity_charges
            .iter()
            .filter_map(|(identity, charge)| {
                (charge.logical.conflicted || charge.allocated.conflicted)
                    .then_some((*identity).into())
            })
    }

    /// Constant-time count of the union of unresolved logical/allocated identities.
    pub fn conflicting_identity_count(&self) -> usize {
        self.conflicting_identity_count
    }

    /// At most MAX_CONFLICT_REREADS listed names, selected when an identity first disagrees.
    /// Selection is not a promise to retain its first listed hardlink or retry another name.
    pub fn conflict_candidates(&self) -> impl Iterator<Item = (FileIdentity, EntryId)> + '_ {
        self.conflict_names
            .iter()
            .map(|(identity, id)| ((*identity).into(), *id))
    }

    /// Apply verified handle sizes after the producer has finished. Listed records and their
    /// known/shared/unknown observations stay untouched. Only existing charges are corrected.
    /// The first successful correction seals insertion; set_state remains available.
    pub fn apply_authoritative_sizes(
        &mut self,
        identity: FileIdentity,
        sizes: Sizes,
    ) -> Result<IdentityResolution, IndexError> {
        let key = StoredIdentity::from(identity);
        let charge = self
            .identity_charges
            .get(&key)
            .ok_or(IndexError::NotConflicted)?;
        if !charge.logical.conflicted && !charge.allocated.conflicted {
            return Err(IndexError::NotConflicted);
        }
        let previous = self
            .authoritative_sizes
            .get(&key)
            .copied()
            .unwrap_or_default();
        let mut corrected = previous;
        let mut next = [(0_u64, 0_i128); 2];
        let mut statuses = [ChargeResolution::MissingCharge; 2];
        for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
            let slot = metric.slot();
            let Some(first) = charge.for_metric(metric).first else {
                continue;
            };
            let Some(value) = metric.of(sizes) else {
                statuses[slot] = ChargeResolution::UnknownSize;
                continue;
            };
            let totals = &self.totals.metrics[slot];
            if totals.overflowed {
                return Err(IndexError::SizeOverflow);
            }
            let old = metric.of(previous).unwrap_or(first);
            let unique = totals
                .unique_bytes
                .checked_sub(old)
                .and_then(|sum| sum.checked_add(value))
                .ok_or(IndexError::SizeOverflow)?;
            let delta = totals
                .resolution_delta_bytes
                .checked_add(i128::from(value) - i128::from(old))
                .ok_or(IndexError::SizeOverflow)?;
            next[slot] = (unique, delta);
            statuses[slot] = ChargeResolution::Applied;
            match metric {
                SizeMetric::Logical => corrected.logical = Some(value),
                SizeMetric::Allocated => corrected.allocated = Some(value),
            }
        }
        // Validate both updates before mutating either metric.
        if statuses.contains(&ChargeResolution::Applied) {
            *self
                .layout_cache
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            *self
                .filter_cache
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.authoritative_sizes.insert(key, corrected);
            let charge = self
                .identity_charges
                .get_mut(&key)
                .expect("validated charge");
            for metric in [SizeMetric::Logical, SizeMetric::Allocated] {
                let slot = metric.slot();
                if statuses[slot] != ChargeResolution::Applied {
                    continue;
                }
                let sample = charge.for_metric_mut(metric);
                let totals = &mut self.totals.metrics[slot];
                totals.unique_bytes = next[slot].0;
                totals.resolution_delta_bytes = next[slot].1;
                if sample.conflicted {
                    totals.conflicting_identities -= 1;
                    sample.conflicted = false;
                }
            }
        }
        let charge = self.identity_charges.get(&key).expect("validated charge");
        let still_conflicted = charge.logical.conflicted || charge.allocated.conflicted;
        if !still_conflicted {
            self.conflicting_identity_count -= 1;
            self.conflict_names.remove(&key);
        }
        Ok(IdentityResolution {
            logical: statuses[0],
            allocated: statuses[1],
            still_conflicted,
        })
    }

    /// Only the first known entry for this identity/metric may use this correction.
    /// Layout keeps every later alias at its listed size; raw entries stay untouched.
    pub(crate) fn effective_size(&self, entry: &Entry<'_>, metric: SizeMetric) -> Option<u64> {
        let listed = metric.of(entry.sizes)?;
        Some(
            entry
                .identity
                .and_then(|identity| {
                    self.authoritative_sizes
                        .get(&StoredIdentity::from(identity))
                })
                .and_then(|sizes| metric.of(*sizes))
                .unwrap_or(listed),
        )
    }

    pub(crate) fn has_authoritative_sizes(&self) -> bool {
        !self.authoritative_sizes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: EntryId) -> Option<Entry<'_>> {
        self.entries
            .get(id.position())
            .map(|entry| self.view(entry))
    }

    fn view(&self, entry: &StoredEntry) -> Entry<'_> {
        Entry {
            parent: entry.parent,
            name: self.names.get(entry.name),
            kind: if entry.flags & 1 == 0 {
                Kind::Directory
            } else {
                Kind::File
            },
            sizes: Sizes {
                logical: (entry.flags & 2 != 0).then_some(entry.logical),
                allocated: (entry.flags & 4 != 0).then_some(entry.allocated),
            },
            identity: self
                .identities
                .get(entry.identity as usize)
                .copied()
                .map(Into::into),
            state: entry.state(),
        }
    }

    pub fn entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = (EntryId, Entry<'_>)> + ExactSizeIterator {
        self.entries
            .iter()
            .enumerate()
            .map(|(id, entry)| (EntryId::at(id).expect("stored ID fits"), self.view(entry)))
    }

    pub fn set_state(&mut self, id: EntryId, state: State) -> Result<(), IndexError> {
        let entry = self
            .entries
            .get_mut(id.position())
            .ok_or(IndexError::InvalidParent)?;
        let previous = entry.state();
        let was_complete = previous == State::Complete;
        let is_complete = state == State::Complete;
        entry.set_state(state);
        match (was_complete, is_complete) {
            (true, false) => self.incomplete_entries += 1,
            (false, true) => self.incomplete_entries -= 1,
            _ => {}
        }
        let cache = self
            .layout_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut cache) = cache {
            cache.state_changed(self, id, previous, state);
            *self
                .layout_cache
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache);
        }
        Ok(())
    }

    /// Logical bytes count visible entries. `unique_logical_bytes` charges a hardlink once,
    /// at the first entry with that identity and a known size; no physical total is inferred.
    /// The totals are maintained on insertion, so this call does not walk the index again.
    pub fn summary(&self) -> Result<Summary, IndexError> {
        let totals = self.summary_for(SizeMetric::Logical)?;
        Ok(Summary {
            files: totals.files,
            known_logical_bytes: totals.known_bytes,
            unique_logical_bytes: totals.unique_bytes,
            shared_logical_bytes: totals.shared_bytes,
            unknown_size_files: totals.unknown_size_files,
            complete: totals.complete,
        })
    }

    /// Totals for one metric, maintained on insertion. Overflow of the requested metric returns
    /// `SizeOverflow`; the other metric keeps its own totals and overflow state.
    pub fn summary_for(&self, metric: SizeMetric) -> Result<MetricSummary, IndexError> {
        let totals = &self.totals.metrics[metric.slot()];
        if totals.overflowed {
            return Err(IndexError::SizeOverflow);
        }
        Ok(MetricSummary {
            metric,
            files: self.totals.files,
            known_bytes: totals.known_bytes,
            unique_bytes: totals.unique_bytes,
            shared_bytes: totals.shared_bytes,
            unknown_size_files: totals.unknown_size_files,
            identity_missing_files: totals.identity_missing_files,
            conflicting_identities: totals.conflicting_identities,
            resolution_delta_bytes: totals.resolution_delta_bytes,
            complete: self.incomplete_entries == 0 && totals.unknown_size_files == 0,
        })
    }
}

#[derive(Debug)]
struct StoredEntry {
    logical: u64,
    allocated: u64,
    parent: Option<EntryId>,
    name: NameRef,
    identity: u32,
    flags: u8,
}
impl StoredEntry {
    fn new(
        parent: Option<EntryId>,
        name: NameRef,
        kind: Kind,
        sizes: Sizes,
        identity: u32,
        state: State,
    ) -> Self {
        let mut entry = Self {
            logical: sizes.logical.unwrap_or(0),
            allocated: sizes.allocated.unwrap_or(0),
            parent,
            name,
            identity,
            flags: u8::from(kind == Kind::File)
                | (u8::from(sizes.logical.is_some()) << 1)
                | (u8::from(sizes.allocated.is_some()) << 2),
        };
        entry.set_state(state);
        entry
    }
    fn set_state(&mut self, state: State) {
        let value = match state {
            State::Pending => 0,
            State::Complete => 1,
            State::Partial => 2,
            State::Inaccessible => 3,
            State::Stale => 4,
        };
        self.flags = (self.flags & 7) | (value << 3);
    }
    fn state(&self) -> State {
        match self.flags >> 3 {
            0 => State::Pending,
            1 => State::Complete,
            2 => State::Partial,
            3 => State::Inaccessible,
            4 => State::Stale,
            _ => unreachable!("internal state bits"),
        }
    }
}

// Avoid u128's 16-byte alignment padding in the identity vector and charge set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct StoredIdentity {
    volume: u64,
    low: u64,
    high: u64,
}
impl From<FileIdentity> for StoredIdentity {
    fn from(id: FileIdentity) -> Self {
        Self {
            volume: id.volume,
            low: id.file as u64,
            high: (id.file >> 64) as u64,
        }
    }
}

/// First charge and conflict state of one file identity for one metric.
#[derive(Clone, Copy, Debug, Default)]
struct MetricCharge {
    /// First known sample, including `Some(0)`; `None` means this metric has not charged yet.
    first: Option<u64>,
    /// A later known sample of the same identity disagreed with the first one.
    conflicted: bool,
}

/// One accounting row per file identity. Both metrics share the key but charge independently.
#[derive(Clone, Copy, Debug, Default)]
struct IdentityCharge {
    logical: MetricCharge,
    allocated: MetricCharge,
}

impl IdentityCharge {
    fn for_metric(&self, metric: SizeMetric) -> &MetricCharge {
        match metric {
            SizeMetric::Logical => &self.logical,
            SizeMetric::Allocated => &self.allocated,
        }
    }
    fn for_metric_mut(&mut self, metric: SizeMetric) -> &mut MetricCharge {
        match metric {
            SizeMetric::Logical => &mut self.logical,
            SizeMetric::Allocated => &mut self.allocated,
        }
    }
}

impl From<StoredIdentity> for FileIdentity {
    fn from(id: StoredIdentity) -> Self {
        Self {
            volume: id.volume,
            file: u128::from(id.low) | (u128::from(id.high) << 64),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IndexMemory {
    pub entry_bytes: usize,
    pub entry_capacity_bytes: usize,
    pub name_bytes: usize,
    pub name_capacity_bytes: usize,
    pub name_boundary_capacity_bytes: usize,
    pub identity_bytes: usize,
    pub charged_identity_bytes: usize,
    pub layout_cache_bytes: usize,
    pub filter_cache_bytes: usize,
}
impl IndexMemory {
    pub fn total_bytes(self) -> usize {
        self.entry_capacity_bytes
            + self.name_capacity_bytes
            + self.name_boundary_capacity_bytes
            + self.identity_bytes
            + self.charged_identity_bytes
            + self.layout_cache_bytes
            + self.filter_cache_bytes
    }
}

// Approximate SwissTable buckets plus control bytes; excludes allocator headers.
pub(crate) fn hash_bytes<T>(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = (capacity * 8 / 7).next_power_of_two();
    buckets * (std::mem::size_of::<T>() + 1) + 16
}

fn accumulate(sum: &mut u64, bytes: u64, overflowed: &mut bool) {
    match sum.checked_add(bytes) {
        Some(total) => *sum = total,
        None => *overflowed = true,
    }
}

fn validate_name(name: &OsStr) -> Result<(), IndexError> {
    let bytes = name.as_encoded_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes
            .iter()
            .any(|byte| matches!(byte, 0 | b'/' | b'\\' | b':'))
        || utf16_units(bytes) > MAX_COMPONENT_UTF16_UNITS
    {
        return Err(IndexError::InvalidName);
    }
    Ok(())
}

/// Counts UTF-16 units in the WTF-8 bytes behind `OsStr::as_encoded_bytes` on Windows.
/// Each non-continuation byte starts one code point, and a four-byte sequence (a code point
/// above U+FFFF) takes one extra unit.
pub(crate) fn utf16_units(encoded: &[u8]) -> usize {
    let mut units = 0;
    for byte in encoded {
        if *byte & 0b1100_0000 != 0b1000_0000 {
            units += 1;
            if *byte & 0b1111_1000 == 0b1111_0000 {
                units += 1;
            }
        }
    }
    units
}
