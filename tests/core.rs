use std::ffi::OsStr;

use hogmap::filter::{Filter, FilterError, MAX_CONDITIONS, MAX_EXPRESSION_BYTES, Match};
use hogmap::index::{FileIdentity, Index, IndexError, Kind, Sizes, State};

#[test]
fn filters_follow_the_documented_name_size_and_combination_rules() {
    for (expression, name, bytes, expected) in [
        ("", "notes.txt", None, Match::Yes),
        ("*.jpg", "PHOTO.JPG", None, Match::Yes),
        ("*.jpg", "notes.txt", Some(200), Match::No),
        ("|*.jpg", "PHOTO.JPG", None, Match::No),
        ("|*.jpg", "notes.txt", None, Match::Yes),
        (">100mb", "a.bin", Some(104_857_600), Match::No),
        (">100mb", "a.bin", Some(104_857_601), Match::Yes),
        ("*.jpg;>1mb", "photo.jpg", Some(1_048_577), Match::Yes),
        ("*.jpg;>1mb", "notes.txt", Some(1_048_577), Match::No),
        ("*.jpg;>1mb", "photo.jpg", Some(1_048_576), Match::No),
        (
            "*.jpg;*.png;>1mb;|draft*",
            "draft.png",
            Some(2_000_000),
            Match::No,
        ),
        (
            "*.jpg;*.png;>1mb;|draft*",
            "photo.png",
            Some(2_000_000),
            Match::Yes,
        ),
        (" >1KB ; <2kb ", "a.bin", Some(1500), Match::Yes),
        ("*.jpg;>1mb", "photo.jpg", None, Match::Unknown),
        ("*.jpg;>1mb", "notes.txt", None, Match::No),
        ("?a*ab", "éa123ab", Some(0), Match::Yes),
        ("*a*b", "aacab", Some(0), Match::Yes),
        ("é.txt", "É.txt", Some(0), Match::No),
        ("é.txt", "é.txt", Some(0), Match::Yes),
    ] {
        let filter = Filter::parse(expression).unwrap();
        assert_eq!(
            filter.evaluate(OsStr::new(name), bytes),
            expected,
            "{expression:?}, {name}"
        );
    }
}

#[test]
fn invalid_or_excessive_filters_are_rejected_before_use() {
    for expression in [
        "*.jpg;",
        ";*.jpg",
        "*.jpg;;>1mb",
        "|",
        "||*.jpg",
        "\\temp",
        "a/b",
        ":red",
        ">1year",
        ">1 mb",
        ">-1mb",
        ">1.5mb",
        ">1",
        ">=1mb",
        ">18446744073709551616b",
        ">18446744073709551615tb",
        "photo\0.jpg",
    ] {
        assert!(Filter::parse(expression).is_err(), "{expression:?}");
    }
    assert!(matches!(
        Filter::parse(&"a".repeat(MAX_EXPRESSION_BYTES + 1)),
        Err(FilterError::TooLong)
    ));
    let conditions = vec!["*"; MAX_CONDITIONS + 1].join(";");
    assert!(matches!(
        Filter::parse(&conditions),
        Err(FilterError::TooManyConditions)
    ));
}

#[test]
fn index_keeps_unknowns_and_logical_hardlinks_without_mutating_totals_when_filtering() {
    let mut index = Index::new("Synthetic volume".into()).unwrap();
    let root = index.root();
    let folder = index
        .insert(
            root,
            "Photos".into(),
            Kind::Directory,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    let identity = Some(FileIdentity {
        volume: 1,
        file: 42,
    });
    for name in ["one.jpg", "link.jpg"] {
        index
            .insert(
                folder,
                name.into(),
                Kind::File,
                Sizes {
                    logical: Some(12),
                    allocated: Some(4096),
                },
                identity,
                State::Complete,
            )
            .unwrap();
    }
    index
        .insert(
            root,
            "empty.txt".into(),
            Kind::File,
            Sizes {
                logical: Some(0),
                allocated: Some(0),
            },
            None,
            State::Complete,
        )
        .unwrap();
    index
        .insert(
            root,
            "unknown.bin".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Inaccessible,
        )
        .unwrap();
    index.set_state(root, State::Complete).unwrap();
    let before = index.summary().unwrap();
    assert_eq!(before.files, 4);
    assert_eq!(before.known_logical_bytes, 24);
    assert_eq!(before.unknown_size_files, 1);
    assert!(!before.complete);
    let filter = Filter::parse("*.jpg").unwrap();
    let matched: Vec<_> = index
        .entries()
        .filter(|(_, entry)| {
            entry.kind == Kind::File
                && filter.evaluate(&entry.name, entry.sizes.logical) == Match::Yes
        })
        .collect();
    assert_eq!(matched.len(), 2);
    assert_eq!(index.summary().unwrap(), before);
    assert_eq!(index.get(folder).unwrap().parent, Some(root));
}

#[test]
fn index_rejects_invalid_hierarchy_and_overflow_without_hiding_errors() {
    let mut index = Index::new("Synthetic".into()).unwrap();
    let root = index.root();
    let file = index
        .insert(
            root,
            "large.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(u64::MAX),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(
        index.insert(
            file,
            "child".into(),
            Kind::File,
            Sizes::default(),
            None,
            State::Pending
        ),
        Err(IndexError::NotDirectory)
    );
    for name in ["", ".", "..", "a/b", "a\\b", "C:", "nul\0name"] {
        assert_eq!(
            index.insert(
                root,
                name.into(),
                Kind::File,
                Sizes::default(),
                None,
                State::Pending
            ),
            Err(IndexError::InvalidName)
        );
    }
    assert_eq!(index.entries().count(), 2);
    index
        .insert(
            root,
            "another.bin".into(),
            Kind::File,
            Sizes {
                logical: Some(1),
                allocated: None,
            },
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(index.summary(), Err(IndexError::SizeOverflow));
}

#[cfg(windows)]
#[test]
fn non_unicode_windows_names_are_preserved_and_not_silently_excluded() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let name = OsString::from_wide(&[0xd800, b'.' as u16, b'j' as u16, b'p' as u16, b'g' as u16]);
    let mut index = Index::new("Synthetic".into()).unwrap();
    let id = index
        .insert(
            index.root(),
            name.clone(),
            Kind::File,
            Sizes::default(),
            None,
            State::Complete,
        )
        .unwrap();
    assert_eq!(index.get(id).unwrap().name, name);
    let filter = Filter::parse("*.jpg;>1mb").unwrap();
    assert_eq!(filter.evaluate(&name, None), Match::Unknown);
    assert_eq!(filter.evaluate(&name, Some(0)), Match::No);
}
