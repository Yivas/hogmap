use std::ffi::{OsStr, OsString};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    Inaccessible,
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub known_logical_bytes: u64,
    pub unknown_size_files: usize,
    pub complete: bool,
}

/// Append-only metadata snapshot. IDs identify entries, not physical files.
#[derive(Debug)]
pub struct Index {
    entries: Vec<Entry>,
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
        self.entries.push(Entry {
            parent: Some(parent),
            name,
            kind,
            sizes,
            identity,
            state,
        });
        Ok(id)
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
        entry.state = state;
        Ok(())
    }

    /// Logical bytes count directory entries, including hardlinks. No physical total is inferred.
    pub fn summary(&self) -> Result<Summary, IndexError> {
        let mut result = Summary {
            files: 0,
            known_logical_bytes: 0,
            unknown_size_files: 0,
            complete: true,
        };
        for entry in &self.entries {
            result.complete &= entry.state == State::Complete;
            if entry.kind == Kind::File {
                result.files += 1;
                match entry.sizes.logical {
                    Some(bytes) => {
                        result.known_logical_bytes = result
                            .known_logical_bytes
                            .checked_add(bytes)
                            .ok_or(IndexError::SizeOverflow)?;
                    }
                    None => {
                        result.unknown_size_files += 1;
                        result.complete = false;
                    }
                }
            }
        }
        Ok(result)
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
    {
        return Err(IndexError::InvalidName);
    }
    Ok(())
}
