use hogmap::filter::{Filter, Match};
use hogmap::index::{FileIdentity, Index, IndexError, Kind, Sizes, State};

#[test]
fn incremental_matches_preserve_empty_context_unknowns_aliases_and_listed_logical_sizes() {
    use Match::{No, Unknown, Yes};
    let check = |index: &Index, filter: &Filter, expected: &[Match], counts| {
        let cached = filter.matches(index);
        assert_eq!(cached.matches, expected);
        assert_eq!((cached.matched_files, cached.unknown_files), counts);
        // Independent full evaluation, including directory context, without the snapshot cache.
        let mut full: Vec<_> = index
            .entries()
            .map(|(_, entry)| {
                if entry.kind == Kind::File {
                    filter.evaluate(entry.name, entry.sizes.logical)
                } else {
                    No
                }
            })
            .collect();
        for (id, entry) in index.entries().rev() {
            if let Some(parent) = entry.parent {
                let child = full[id.position()];
                let current = full[parent.position()];
                full[parent.position()] = if child == Yes || current == Yes {
                    Yes
                } else if child == Unknown || current == Unknown {
                    Unknown
                } else {
                    No
                };
            }
        }
        assert_eq!(cached.matches, full);
    };
    let empty = Filter::default();
    let real = Filter::parse("*.bin;>10b").unwrap();
    let mut index = Index::new("Filter fixture".into()).unwrap();
    let root = index.root();
    check(&index, &empty, &[No], (0, 0));
    let directory = index
        .insert(
            root,
            "empty".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Pending,
        )
        .unwrap();
    check(&index, &empty, &[No, No], (0, 0));
    check(&index, &real, &[No, No], (0, 0));
    let unknown = index
        .insert(
            directory,
            "mystery.bin".into(),
            Kind::File,
            Sizes {
                logical: None,
                allocated: Some(1),
            },
            None,
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Unknown, Unknown, Unknown], (0, 1));
    let identity = FileIdentity { volume: 1, file: 8 };
    index
        .insert(
            root,
            "first.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(20),
                allocated: Some(100),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Unknown, Unknown, Yes], (1, 1));
    index
        .insert(
            directory,
            "alias.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(5),
                allocated: Some(200),
            },
            Some(identity),
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Unknown, Unknown, Yes, No], (1, 1));
    check(&index, &empty, &[Yes, Yes, Yes, Yes, Yes], (3, 0));
    check(&index, &real, &[Yes, Unknown, Unknown, Yes, No], (1, 1));
    index
        .insert(
            directory,
            "big.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(30),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    index.set_state(unknown, State::Inaccessible).unwrap();
    index.set_state(directory, State::Complete).unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    index
        .apply_authoritative_sizes(
            identity,
            Sizes {
                logical: Some(40),
                allocated: Some(400),
            },
        )
        .unwrap();
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    assert_eq!(
        index.insert(
            root,
            "sealed".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete
        ),
        Err(IndexError::ResolvedSnapshot)
    );
    check(&index, &real, &[Yes, Yes, Unknown, Yes, No, Yes], (2, 1));
    check(
        &index,
        &Filter::parse("*.bin;<10b").unwrap(),
        &[Yes, Yes, Unknown, No, Yes, No],
        (1, 1),
    );
}
