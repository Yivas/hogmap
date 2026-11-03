use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

/// Win32 reports the maximum component length in UTF-16 units; NTFS uses 255.
const MAX_COMPONENT_UTF16_UNITS: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(usize);

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

#[derive(Debug)]
pub struct Entry {
    pub parent: Option<EntryId>,
    pub name: OsString,
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

/// File totals kept as entries are appended, so `summary()` does not walk them again.
#[derive(Debug, Default)]
struct FileTotals {
    files: usize,
    known_logical_bytes: u64,
    unique_logical_bytes: u64,
    shared_logical_bytes: u64,
    unknown_size_files: usize,
    overflowed: bool,
}

/// Append-only metadata snapshot. IDs identify entries, not physical files.
///
/// The index keeps one entry per charged file identity and a count of incomplete entries to
/// answer `summary()` in constant time; that set is the memory cost of hardlink accounting.
#[derive(Debug)]
pub struct Index {
    entries: Vec<Entry>,
    totals: FileTotals,
    charged_identities: HashSet<FileIdentity>,
    incomplete_entries: usize,
}

impl Index {
    pub fn new(root_name: OsString) -> Result<Self, IndexError> {
        validate_name(&root_name)?;
        Ok(Self {
            entries: vec![Entry {
                parent: None,
                name: root_name,
                kind: Kind::Directory,
                sizes: Sizes::default(),
                identity: None,
                state: State::Pending,
            }],
            totals: FileTotals::default(),
            charged_identities: HashSet::new(),
            incomplete_entries: 1,
        })
    }

    pub fn root(&self) -> EntryId {
        EntryId(0)
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
        let id = EntryId(self.entries.len());
        let entry = Entry {
            parent: Some(parent),
            name,
            kind,
            sizes,
            identity,
            state,
        };
        self.account(&entry);
        self.entries.push(entry);
        Ok(id)
    }

    fn account(&mut self, entry: &Entry) {
        if entry.state != State::Complete {
            self.incomplete_entries += 1;
        }
        if entry.kind != Kind::File {
            return;
        }
        self.totals.files += 1;
        let Some(bytes) = entry.sizes.logical else {
            self.totals.unknown_size_files += 1;
            return;
        };
        accumulate(
            &mut self.totals.known_logical_bytes,
            bytes,
            &mut self.totals.overflowed,
        );
        let first_charge = match entry.identity {
            Some(identity) => self.charged_identities.insert(identity),
            None => true,
        };
        if first_charge {
            accumulate(
                &mut self.totals.unique_logical_bytes,
                bytes,
                &mut self.totals.overflowed,
            );
        } else {
            accumulate(
                &mut self.totals.shared_logical_bytes,
                bytes,
                &mut self.totals.overflowed,
            );
        }
    }

    pub fn get(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(id.0)
    }

    pub fn entries(&self) -> impl Iterator<Item = (EntryId, &Entry)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(id, entry)| (EntryId(id), entry))
    }

    pub fn set_state(&mut self, id: EntryId, state: State) -> Result<(), IndexError> {
        let entry = self
            .entries
            .get_mut(id.0)
            .ok_or(IndexError::InvalidParent)?;
        let was_complete = entry.state == State::Complete;
        let is_complete = state == State::Complete;
        entry.state = state;
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
        if self.totals.overflowed {
            return Err(IndexError::SizeOverflow);
        }
        Ok(Summary {
            files: self.totals.files,
            known_logical_bytes: self.totals.known_logical_bytes,
            unique_logical_bytes: self.totals.unique_logical_bytes,
            shared_logical_bytes: self.totals.shared_logical_bytes,
            unknown_size_files: self.totals.unknown_size_files,
            complete: self.incomplete_entries == 0 && self.totals.unknown_size_files == 0,
        })
    }
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
