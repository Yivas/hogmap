//! Ghosts are a derived presentation, never additions to the live scanner index or its authority.
//! Snapshot-local IDs are remapped through native component paths; replacement identities are
//! not disappeared items. Map totals including ghosts are not volume-capacity accounting.
use crate::filter::Metadata;
use crate::index::{EntryId, FileIdentity, Index, IndexError, Kind, Sizes, State};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

struct PreviousEntry {
    parent: Option<EntryId>,
    name: OsString,
    kind: Kind,
    sizes: Sizes,
    identity: Option<FileIdentity>,
    metadata: Metadata,
    tags: u8,
}
pub struct Baseline {
    root: PathBuf,
    entries: Vec<PreviousEntry>,
}
impl Baseline {
    pub fn capture(root: &Path, index: &Index) -> Self {
        Self {
            root: root.into(),
            entries: index
                .entries()
                .map(|(id, entry)| PreviousEntry {
                    parent: entry.parent,
                    name: entry.name.into(),
                    kind: entry.kind,
                    sizes: entry.sizes,
                    identity: entry.identity,
                    metadata: index.filter_metadata(id),
                    tags: index.tags(id).unwrap_or(0),
                })
                .collect(),
        }
    }
}
#[derive(Default)]
pub struct Ghosts {
    pub display: Option<Index>,
    pub weak_matches: HashSet<EntryId>,
    pub count: usize,
}
impl Ghosts {
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    /// `None` means that no live, same-root comparison is possible. No stored path is visited.
    pub fn compare(
        &mut self,
        before: Baseline,
        root: Option<&Path>,
        live: &Index,
        snapshot: bool,
    ) -> Result<bool, IndexError> {
        if snapshot || root != Some(before.root.as_path()) {
            return Ok(false);
        }
        let mut paths: Vec<Vec<OsString>> = Vec::with_capacity(live.len());
        let mut current = HashMap::new();
        for (id, entry) in live.entries() {
            let mut path = entry
                .parent
                .map_or_else(Vec::new, |parent| paths[parent.position()].clone());
            if entry.parent.is_some() {
                path.push(entry.name.into());
            }
            current.insert(path.clone(), id);
            paths.push(path);
        }
        let mut display = copy_live(live)?;
        let mut remap = vec![None; before.entries.len()];
        remap[0] = Some(live.root());
        let mut old_paths: Vec<Vec<OsString>> = Vec::with_capacity(before.entries.len());
        let mut count = 0;
        let mut weak_matches = HashSet::new();
        for (slot, entry) in before.entries.iter().enumerate() {
            let mut path = entry
                .parent
                .map_or_else(Vec::new, |parent| old_paths[parent.position()].clone());
            if entry.parent.is_some() {
                path.push(entry.name.clone());
            }
            old_paths.push(path.clone());
            if slot == 0 {
                continue;
            }
            if let Some(id) = current.get(&path).copied() {
                let found = live.get(id).expect("current path");
                if found.kind == entry.kind
                    && !entry
                        .identity
                        .zip(found.identity)
                        .is_some_and(|(a, b)| a != b)
                {
                    remap[slot] = Some(id);
                    if entry.identity.is_none() || found.identity.is_none() {
                        weak_matches.insert(id);
                    }
                }
                continue;
            }
            let Some(parent) = entry.parent.and_then(|id| remap[id.position()]) else {
                continue;
            };
            // A disappeared directory carries its old descendants. A present but unenumerated
            // parent proves nothing about absence, so that whole branch remains untouched.
            if !display
                .get(parent)
                .is_some_and(|parent| matches!(parent.state, State::Complete | State::Stale))
            {
                continue;
            }
            let id = display.insert(
                parent,
                entry.name.clone(),
                entry.kind,
                entry.sizes,
                entry.identity,
                State::Stale,
            )?;
            display.set_filter_metadata(id, entry.metadata)?;
            display
                .set_tags(id, entry.tags)
                .map_err(|_| IndexError::InvalidParent)?;
            remap[slot] = Some(id);
            count += 1;
        }
        // Corrections remain owned by the same first live charges; ghosts use their listed sizes.
        for (identity, sizes) in live.authoritative_corrections() {
            display.apply_authoritative_sizes(identity, sizes)?;
        }
        self.display = (count > 0).then_some(display);
        self.weak_matches = weak_matches;
        self.count = count;
        Ok(true)
    }
}
fn copy_live(live: &Index) -> Result<Index, IndexError> {
    let root = live.get(live.root()).expect("index root");
    let mut copy = Index::new(root.name.into())?;
    copy.set_state(copy.root(), root.state)?;
    for (id, entry) in live.entries() {
        if let Some(parent) = entry.parent {
            let new = copy.insert(
                parent,
                entry.name.into(),
                entry.kind,
                entry.sizes,
                entry.identity,
                entry.state,
            )?;
            debug_assert_eq!(new, id);
        }
        copy.set_filter_metadata(id, live.filter_metadata(id))?;
        if let Some(tags) = live.tag_assignment(id) {
            copy.set_tags(id, tags)
                .map_err(|_| IndexError::InvalidParent)?;
        }
    }
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_proven_absence_creates_last_sized_ghosts_and_clear_restores_the_live_tree() {
        let mut old = Index::new("Fixture".into()).unwrap();
        old.set_state(old.root(), State::Complete).unwrap();
        let identity = |file| Some(FileIdentity { volume: 1, file });
        old.insert(
            old.root(),
            "gone.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(60),
                allocated: Some(64),
            },
            identity(1),
            State::Complete,
        )
        .unwrap();
        old.insert(
            old.root(),
            "replaced.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(20),
                allocated: Some(32),
            },
            identity(2),
            State::Complete,
        )
        .unwrap();
        let folder = old
            .insert(
                old.root(),
                "partial".into(),
                Kind::Directory,
                Sizes::default(),
                None,
                State::Complete,
            )
            .unwrap();
        old.insert(
            folder,
            "unproven.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(90),
                allocated: Some(96),
            },
            None,
            State::Complete,
        )
        .unwrap();
        let mut live = Index::new("Fixture".into()).unwrap();
        live.set_state(live.root(), State::Complete).unwrap();
        live.insert(
            live.root(),
            "partial".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Partial,
        )
        .unwrap();
        live.insert(
            live.root(),
            "replaced.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(7),
                allocated: Some(8),
            },
            identity(3),
            State::Complete,
        )
        .unwrap();
        let mut ghosts = Ghosts::default();
        assert!(
            !ghosts
                .compare(
                    Baseline::capture(Path::new("fixture"), &old),
                    Some(Path::new("fixture")),
                    &live,
                    true
                )
                .unwrap()
        );
        assert!(ghosts.display.is_none());
        assert!(
            !ghosts
                .compare(
                    Baseline::capture(Path::new("fixture"), &old),
                    Some(Path::new("other")),
                    &live,
                    false
                )
                .unwrap()
        );
        assert!(
            ghosts
                .compare(
                    Baseline::capture(Path::new("fixture"), &old),
                    Some(Path::new("fixture")),
                    &live,
                    false
                )
                .unwrap()
        );
        assert_eq!(ghosts.count, 1);
        let display = ghosts.display.as_ref().unwrap();
        let stale: Vec<_> = display
            .entries()
            .filter(|(_, e)| e.state == State::Stale)
            .collect();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].1.name, "gone.bin");
        assert_eq!(
            stale[0].1.sizes,
            Sizes {
                logical: Some(60),
                allocated: Some(64)
            }
        );
        assert_eq!(display.summary().unwrap().known_logical_bytes, 67);
        assert_eq!(live.summary().unwrap().known_logical_bytes, 7);
        ghosts.clear();
        assert!(ghosts.display.is_none());
        assert_eq!(ghosts.count, 0);
    }
}
