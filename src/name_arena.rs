//! Whole native names in a byte arena, resolved through its own boundary table.
#![allow(unsafe_code)]

use std::ffi::OsStr;

#[derive(Clone, Copy, Debug)]
pub(crate) struct NameRef(u32);

#[derive(Debug, Default)]
pub(crate) struct NameArena {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

fn checked_push(bytes: usize, length: usize, names: usize) -> Option<(NameRef, u32)> {
    let end = u32::try_from(bytes.checked_add(length)?).ok()?;
    u32::try_from(names.checked_add(1)?).ok()?;
    let slot = u32::try_from(names).ok()?;
    Some((NameRef(slot), end))
}

impl NameArena {
    pub(crate) fn push(&mut self, name: &OsStr) -> Option<NameRef> {
        let encoded = name.as_encoded_bytes();
        let (reference, end) = checked_push(self.bytes.len(), encoded.len(), self.ends.len())?;
        self.bytes.reserve(encoded.len());
        self.ends.reserve(1);
        self.bytes.extend_from_slice(encoded);
        self.ends.push(end);
        Some(reference)
    }

    pub(crate) fn get(&self, reference: NameRef) -> &OsStr {
        // A slot has no owner provenance: a foreign/expired slot either selects this arena's
        // whole name at the same position or panics here before any unchecked reconstruction.
        let slot = reference.0 as usize;
        let end = self.ends[slot] as usize;
        let start = slot
            .checked_sub(1)
            .map_or(0, |prior| self.ends[prior] as usize);
        // SAFETY: only push writes bytes/ends, copying whole OsStr encodings in this process.
        // Both boundaries come from this arena's table, never from NameRef. Consecutive ends
        // delimit exactly one original name, including empty names with equal ends. Appends and
        // reallocation preserve these bytes; the returned borrow prevents mutation or dropping
        // this arena while the view is live.

        unsafe { OsStr::from_encoded_bytes_unchecked(&self.bytes[start..end]) }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
    pub(crate) fn boundary_capacity_bytes(&self) -> usize {
        self.ends.capacity() * std::mem::size_of::<u32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_arena_and_many_whole_names() {
        let mut arena = NameArena::default();
        assert_eq!(arena.len(), 0);
        let empty = arena.push(OsStr::new("")).unwrap();
        let names: Vec<_> = (0..10_000).map(|n| format!("name-{n}-雪😀")).collect();
        let handles: Vec<_> = names
            .iter()
            .map(|name| arena.push(OsStr::new(name)).unwrap())
            .collect();
        assert_eq!(arena.get(empty), OsStr::new(""));
        for (handle, name) in handles.into_iter().zip(names) {
            assert_eq!(arena.get(handle), OsStr::new(&name));
        }
        let trailing_empty = arena.push(OsStr::new("")).unwrap();
        assert_eq!(arena.get(trailing_empty), OsStr::new(""));
    }

    #[test]
    fn foreign_and_expired_slots_only_select_local_whole_names() {
        let mut source = NameArena::default();
        let foreign_first = source.push(OsStr::new("a")).unwrap();
        let foreign_second = source.push(OsStr::new("bb")).unwrap();
        let foreign_third = source.push(OsStr::new("ccc")).unwrap();
        let mut target = NameArena::default();
        assert!(std::panic::catch_unwind(|| target.get(foreign_first)).is_err());
        target.push(OsStr::new("雪😀")).unwrap();
        assert_eq!(target.get(foreign_first), OsStr::new("雪😀"));
        drop(source);
        assert!(std::panic::catch_unwind(|| target.get(foreign_second)).is_err());
        target.push(OsStr::new("é")).unwrap();
        assert_eq!(target.get(foreign_first), OsStr::new("雪😀"));
        assert_eq!(target.get(foreign_second), OsStr::new("é"));
        assert!(std::panic::catch_unwind(|| target.get(foreign_third)).is_err());
    }

    #[test]
    fn offsets_are_checked_before_narrowing() {
        let limit = u32::MAX as usize;
        let (slot, end) = checked_push(limit - 3, 3, limit - 1).unwrap();
        assert_eq!(slot.0, u32::MAX - 1);
        assert_eq!(end, u32::MAX);
        assert!(checked_push(limit - 3, 4, 0).is_none());
        assert!(checked_push(usize::MAX, 1, 0).is_none());
        assert!(checked_push(0, 0, limit).is_none());
        assert!(checked_push(0, 0, usize::MAX).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn lone_surrogates_remain_separate_whole_names() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut arena = NameArena::default();
        let names = [
            vec![0xD800],
            vec![0xDC00],
            vec![65, 0xD801, 66],
            vec![0xD800, 0xDC00],
        ];
        let handles: Vec<_> = names
            .iter()
            .map(|units| arena.push(&OsString::from_wide(units)).unwrap())
            .collect();
        for (handle, units) in handles.into_iter().zip(names) {
            assert_eq!(arena.get(handle).encode_wide().collect::<Vec<_>>(), units);
        }
    }
}
