use crate::name_arena::{NameArena, NameRef};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;

/// Win32 reports the maximum component length in UTF-16 units; NTFS uses 255.
const MAX_COMPONENT_UTF16_UNITS: usize = 255;

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
}

/// Logical totals. `known_logical_bytes` counts visible entries, so it equals
/// `unique_logical_bytes + shared_logical_bytes` when no sum overflows.
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
    overflowed: bool,
}

/// File totals kept as entries are appended, so `summary()` does not walk them again.
#[derive(Debug, Default)]
struct FileTotals {
    files: usize,
    metrics: [MetricTotals; 2],
}

/// Totals for one size metric. `complete` covers entry states and known sizes of this metric only:
/// it does not prove consistent identities or an exclusive physical attribution.
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
    pub complete: bool,
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
    incomplete_entries: usize,
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
            incomplete_entries: 1,
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
            charged_identity_bytes: hash_bytes::<(StoredIdentity, IdentityCharge)>(
                self.identity_charges.capacity(),
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
        self.account(&Entry {
            parent: Some(parent),
            name: &name,
            kind,
            sizes,
            identity,
            state,
        });
        self.entries.push(StoredEntry::new(
            Some(parent),
            name_ref,
            kind,
            sizes,
            identity_slot,
            state,
        ));
        Ok(id)
    }

    fn account(&mut self, entry: &Entry<'_>) {
        if entry.state != State::Complete {
            self.incomplete_entries += 1;
        }
        if entry.kind != Kind::File {
            return;
        }
        self.totals.files += 1;
        // Each metric charges independently: an unknown logical size must not skip a known
        // allocated size, and one metric's overflow must not hide the other's totals.
        self.account_metric(SizeMetric::Logical, entry.sizes.logical, entry.identity);
        self.account_metric(SizeMetric::Allocated, entry.sizes.allocated, entry.identity);
    }

    fn account_metric(
        &mut self,
        metric: SizeMetric,
        size: Option<u64>,
        identity: Option<FileIdentity>,
    ) {
        let totals = &mut self.totals.metrics[metric.slot()];
        let Some(bytes) = size else {
            totals.unknown_size_files += 1;
            return;
        };
        accumulate(&mut totals.known_bytes, bytes, &mut totals.overflowed);
        let Some(identity) = identity else {
            // Without an identity the size cannot be deduplicated, so it is charged on its own
            // and reported as a separate confidence gap.
            totals.identity_missing_files += 1;
            accumulate(&mut totals.unique_bytes, bytes, &mut totals.overflowed);
            return;
        };
        let sample = self
            .identity_charges
            .entry(identity.into())
            .or_default()
            .for_metric_mut(metric);
        match sample.first {
            None => {
                sample.first = Some(bytes);
                accumulate(&mut totals.unique_bytes, bytes, &mut totals.overflowed);
            }
            Some(first) => {
                if first != bytes && !sample.conflicted {
                    sample.conflicted = true;
                    totals.conflicting_identities += 1;
                }
                accumulate(&mut totals.shared_bytes, bytes, &mut totals.overflowed);
            }
        }
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
        let was_complete = entry.state() == State::Complete;
        let is_complete = state == State::Complete;
        entry.set_state(state);
        match (was_complete, is_complete) {
            (true, false) => self.incomplete_entries += 1,
            (false, true) => self.incomplete_entries -= 1,
            _ => {}
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
}
impl IndexMemory {
    pub fn total_bytes(self) -> usize {
        self.entry_capacity_bytes
            + self.name_capacity_bytes
            + self.name_boundary_capacity_bytes
            + self.identity_bytes
            + self.charged_identity_bytes
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
fn utf16_units(encoded: &[u8]) -> usize {
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
